//! Model gateway wiring: saved model preference, the swappable gateway, trace
//! recording, and provider factories across live reloads and production boot.

use super::*;

#[tokio::test]
async fn standalone_cli_send_uses_saved_user_model_preference() {
    let root = tempfile::tempdir().expect("tempdir");
    let standalone_root = root.path().join("standalone");
    std::fs::create_dir_all(&standalone_root).expect("standalone root");
    std::fs::write(
        standalone_root.join(crate::factory::STANDALONE_SECRETS_MASTER_KEY_PATH),
        format!(
            "{}\n",
            ironclaw_secrets::keychain::generate_master_key_hex()
        ),
    )
    .expect("seed standalone secrets master key");
    let config_home_dir = root.path().join("config-home");
    std::fs::create_dir_all(&config_home_dir).expect("config home dir");
    let home = RebornHome::resolve_from_env_parts(
        Some(config_home_dir.as_os_str().to_os_string()),
        None,
        None,
    )
    .expect("valid reborn home");
    std::fs::write(
        home.config_file_path(),
        "[llm.default]\nprovider_id = \"ollama\"\nmodel = \"workspace-default\"\n",
    )
    .expect("write config.toml");

    let requests = Arc::new(StdMutex::new(Vec::new()));
    let gateway = Arc::new(RecordingGateway {
        reply: "preferred model reply".to_string(),
        requests: Arc::clone(&requests),
    });
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input("runtime-cli-model-owner", standalone_root)
            .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_boot_config(RebornBootConfig::new(home, RebornProfile::Standalone))
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-cli-model-tenant".to_string(),
        agent_id: "runtime-cli-model-agent".to_string(),
        source_binding_id: "runtime-cli-model-source".to_string(),
        reply_target_binding_id: "runtime-cli-model-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: RUNTIME_SEND_TIMEOUT,
    })
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let caller = ProductSurfaceCaller::new(
        TenantId::new("runtime-cli-model-tenant").expect("tenant"),
        UserId::new("runtime-cli-model-owner").expect("user"),
        Some(AgentId::new("runtime-cli-model-agent").expect("agent")),
        None,
    );
    let llm_config = runtime
        .llm_config_service
        .as_ref()
        .expect("boot config wires model selection");
    llm_config
        .set_user_model_policy(
            caller.clone().with_operator_config(true),
            SetUserModelPolicyRequest {
                workspace_default: "workspace-default".to_string(),
                allowed_models: vec![
                    "workspace-default".to_string(),
                    "preferred-model".to_string(),
                ],
                model_entries: Some(Vec::new()),
            },
        )
        .await
        .expect("model policy is stored");
    llm_config
        .set_user_model_preference(
            caller,
            SetUserModelPreferenceRequest {
                model: Some("preferred-model".to_string()),
            },
        )
        .await
        .expect("model preference is stored");

    let conversation = runtime.new_conversation().await.expect("conversation");
    runtime
        .send_user_message(&conversation, "use my saved model")
        .await
        .expect("CLI message sends");

    {
        let requests = requests.lock().expect("requests lock");
        assert_eq!(requests.len(), 1, "one model call should be made");
        let request = &requests[0];
        let route = request
            .resolved_model_route
            .as_ref()
            .expect("saved preference should reach the model gateway");
        assert!(route.is_advisory());
        assert_eq!(route.model_id(), "preferred-model");
    }
    runtime.shutdown().await.expect("runtime shutdown");
}

#[async_trait]
impl HostManagedModelGateway for ModelOutageGateway {
    async fn stream_model(
        &self,
        _request: HostManagedModelRequest,
    ) -> Result<HostManagedModelResponse, HostManagedModelError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(HostManagedModelError::safe(
            HostManagedModelErrorKind::Unavailable,
            "model service is unavailable",
        ))
    }
}

#[async_trait]
impl HostSkillContextSource for FailingSkillContextSource {
    async fn load_skill_context_candidates(
        &self,
        _run_context: &LoopRunContext,
    ) -> Result<Vec<HostSkillContextCandidate>, HostSkillContextBuildError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(HostSkillContextBuildError::SourceUnavailable)
    }
}

#[async_trait]
impl HostManagedModelGateway for ToolCallingGateway {
    async fn stream_model(
        &self,
        request: HostManagedModelRequest,
    ) -> Result<HostManagedModelResponse, HostManagedModelError> {
        *self
            .stream_model_calls
            .lock()
            .expect("tool gateway stream count lock poisoned") += 1;
        self.requests
            .lock()
            .expect("tool gateway requests lock poisoned")
            .push(request);
        Err(HostManagedModelError::safe(
            HostManagedModelErrorKind::InvalidRequest,
            "expected capability-aware model path",
        ))
    }

    async fn stream_model_with_capabilities(
        &self,
        request: HostManagedModelRequest,
        capabilities: Arc<dyn LoopCapabilityPort>,
    ) -> Result<HostManagedModelResponse, HostManagedModelError> {
        let call_index = {
            let mut calls = self.calls.lock().expect("tool gateway lock poisoned");
            let call_index = *calls;
            *calls += 1;
            call_index
        };
        self.requests
            .lock()
            .expect("tool gateway requests lock poisoned")
            .push(request.clone());
        if call_index == 1 {
            let tool_result = request
                .messages
                .iter()
                .find(|message| message.role == HostManagedModelMessageRole::ToolResult)
                .expect("second model call should include tool result");
            assert_standalone_result_reference(tool_result, "hello from tool");
            let provider_call = tool_result
                .tool_result_provider_call
                .as_ref()
                .expect("provider replay metadata");
            assert_eq!(provider_call.provider_call_id, "call-1");
            assert_eq!(
                provider_call.capability_id,
                CapabilityId::new("builtin.echo").unwrap()
            );
            return Ok(HostManagedModelResponse::assistant_reply("tool ok"));
        }

        let surface = capabilities
            .visible_capabilities(VisibleCapabilityRequest)
            .await
            .map_err(model_capability_error)?;
        let echo_id = CapabilityId::new("builtin.echo").expect("echo id");
        assert!(
            surface
                .descriptors
                .iter()
                .any(|descriptor| descriptor.capability_id == echo_id),
            "builtin echo must be visible through standalone runtime capability surface"
        );
        let echo_tool = capabilities
            .tool_definitions()
            .map_err(model_capability_error)?
            .into_iter()
            .find(|definition| definition.capability_id == echo_id)
            .expect("echo provider tool definition");
        let candidate = capabilities
            .register_provider_tool_call(RegisterProviderToolCallRequest::new(ProviderToolCall {
                provider_id: "test-provider".to_string(),
                provider_model_id: "test-model".to_string(),
                turn_id: Some("provider-turn-1".to_string()),
                id: "call-1".to_string(),
                name: echo_tool.name,
                arguments: serde_json::json!({"message": "hello from tool"}),
                response_reasoning: None,
                reasoning: None,
                signature: None,
            }))
            .await
            .map_err(model_capability_error)?;
        Ok(HostManagedModelResponse::capability_calls(
            vec![candidate],
            "",
        ))
    }
}

#[async_trait]
impl HostManagedModelGateway for SandboxShellCallingGateway {
    async fn stream_model(
        &self,
        _request: HostManagedModelRequest,
    ) -> Result<HostManagedModelResponse, HostManagedModelError> {
        Err(HostManagedModelError::safe(
            HostManagedModelErrorKind::InvalidRequest,
            "expected capability-aware model path",
        ))
    }

    async fn stream_model_with_capabilities(
        &self,
        request: HostManagedModelRequest,
        capabilities: Arc<dyn LoopCapabilityPort>,
    ) -> Result<HostManagedModelResponse, HostManagedModelError> {
        let call_index = {
            let mut calls = self.calls.lock().expect("shell gateway lock poisoned");
            let call_index = *calls;
            *calls += 1;
            call_index
        };
        if call_index == 1 {
            let tool_result = request
                .messages
                .iter()
                .find(|message| message.role == HostManagedModelMessageRole::ToolResult)
                .expect("second model call should include shell result");
            assert!(
                tool_result.content.contains("railway-sandbox-marker"),
                "shell result should come from the configured sandbox transport: {}",
                tool_result.content
            );
            let envelope: serde_json::Value = serde_json::from_str(&tool_result.content)
                .expect("tool result should be a structured reference envelope");
            let preview = envelope["model_observation"]["detail"]["preview"]
                .as_str()
                .expect("tool result should include an inline preview");
            let shell_output: serde_json::Value =
                serde_json::from_str(preview).expect("shell preview should be structured JSON");
            assert_eq!(
                shell_output["sandboxed"],
                serde_json::json!(true),
                "model-visible shell result must report sandbox execution"
            );
            return Ok(HostManagedModelResponse::assistant_reply(
                "sandbox shell ok",
            ));
        }

        let surface = capabilities
            .visible_capabilities(VisibleCapabilityRequest)
            .await
            .map_err(model_capability_error)?;
        let shell_id = CapabilityId::new(ironclaw_host_runtime::SHELL_CAPABILITY_ID)
            .expect("shell capability id");
        assert!(
            surface
                .descriptors
                .iter()
                .any(|descriptor| descriptor.capability_id == shell_id),
            "builtin shell must be visible for a sandboxed hosted profile"
        );
        let shell_tool = capabilities
            .tool_definitions()
            .map_err(model_capability_error)?
            .into_iter()
            .find(|definition| definition.capability_id == shell_id)
            .expect("shell provider tool definition");
        let candidate = capabilities
            .register_provider_tool_call(RegisterProviderToolCallRequest::new(ProviderToolCall {
                provider_id: "test-provider".to_string(),
                provider_model_id: "test-model".to_string(),
                turn_id: Some("provider-turn-shell".to_string()),
                id: "shell-call-1".to_string(),
                name: shell_tool.name,
                arguments: serde_json::json!({
                    "command": "printf railway-sandbox-marker",
                    "credential_contexts": []
                }),
                response_reasoning: None,
                reasoning: None,
                signature: None,
            }))
            .await
            .map_err(model_capability_error)?;
        Ok(HostManagedModelResponse::capability_calls(
            vec![candidate],
            "",
        ))
    }
}

#[async_trait]
impl HostManagedModelGateway for LargeEchoToolCallingGateway {
    async fn stream_model(
        &self,
        _request: HostManagedModelRequest,
    ) -> Result<HostManagedModelResponse, HostManagedModelError> {
        Err(HostManagedModelError::safe(
            HostManagedModelErrorKind::InvalidRequest,
            "expected capability-aware model path",
        ))
    }

    async fn stream_model_with_capabilities(
        &self,
        request: HostManagedModelRequest,
        capabilities: Arc<dyn LoopCapabilityPort>,
    ) -> Result<HostManagedModelResponse, HostManagedModelError> {
        let call_index = {
            let mut calls = self.calls.lock().expect("large echo gateway lock poisoned");
            let call_index = *calls;
            *calls += 1;
            call_index
        };
        if call_index == 1 {
            let tool_result = request
                .messages
                .iter()
                .find(|message| message.role == HostManagedModelMessageRole::ToolResult)
                .expect("second model call should include tool result");
            assert!(
                !tool_result.content.contains(LARGE_ECHO_TAIL),
                "raw tail must remain out of the model replay; got {} bytes",
                tool_result.content.len()
            );
            assert!(
                tool_result.content.contains("result_reference"),
                "model replay must carry a bounded result-reference observation"
            );
            assert!(
                tool_result.content.len() <= TOOL_RESULT_RECORD_READ_MAX_BYTES * 2,
                "tool result replay must stay within the envelope bound, got {} bytes",
                tool_result.content.len()
            );
            assert!(
                tool_result.content.contains("Secretary of the Treasury"),
                "the initial result-reference preview must retain ordinary document text"
            );
            let result_ref = match tool_result.tool_result_content.as_ref() {
                Some(HostManagedToolResultContent::Reference { envelope }) => {
                    envelope.result_ref.clone()
                }
                other => panic!("expected a result reference, got {other:?}"),
            };
            let result_read_id = CapabilityId::new("builtin.result_read").expect("reader id");
            let result_read_tool = capabilities
                .tool_definitions()
                .map_err(model_capability_error)?
                .into_iter()
                .find(|definition| definition.capability_id == result_read_id)
                .expect("result_read provider tool definition");
            let candidate = capabilities
                .register_provider_tool_call(RegisterProviderToolCallRequest::new(
                    ProviderToolCall {
                        provider_id: "test-provider".to_string(),
                        provider_model_id: "test-model".to_string(),
                        turn_id: Some("provider-turn-2".to_string()),
                        id: "call-2".to_string(),
                        name: result_read_tool.name,
                        arguments: serde_json::json!({
                            "result_ref": result_ref,
                            "offset": 0,
                            "max_bytes": 2048,
                        }),
                        response_reasoning: None,
                        reasoning: None,
                        signature: None,
                    },
                ))
                .await
                .map_err(model_capability_error)?;
            return Ok(HostManagedModelResponse::capability_calls(
                vec![candidate],
                "",
            ));
        }
        if call_index == 2 {
            let tool_result = request
                .messages
                .iter()
                .rev()
                .find(|message| {
                    message.role == HostManagedModelMessageRole::ToolResult
                        && message
                            .tool_result_provider_call
                            .as_ref()
                            .is_some_and(|call| {
                                call.capability_id.as_str() == "builtin.result_read"
                            })
                })
                .expect("third model call should include result_read output");
            assert!(
                tool_result.content.contains(LARGE_ECHO_MESSAGE),
                "result_read must expose its bounded chunk to the model"
            );
            assert!(
                !tool_result.content.contains(LARGE_ECHO_TAIL),
                "the result_read response must remain bounded"
            );
            let observation: serde_json::Value =
                serde_json::from_str(&tool_result.content).expect("result_read observation");
            let detail = &observation["model_observation"]["detail"];
            assert_eq!(
                detail["result_ref"], observation["result_ref"],
                "result_read replay must expose only the original pageable result reference"
            );
            assert!(
                detail["total_bytes"]
                    .as_u64()
                    .is_some_and(|total_bytes| total_bytes > 2048),
                "result_read replay must expose total bytes for continuation: {}",
                tool_result.content
            );
            assert_eq!(
                detail["next_offset"].as_u64(),
                Some(2048),
                "result_read replay must expose the next offset for continuation"
            );
            return Ok(HostManagedModelResponse::assistant_reply("tool ok"));
        }
        let echo_id = CapabilityId::new("builtin.echo").expect("echo id");
        let echo_tool = capabilities
            .tool_definitions()
            .map_err(model_capability_error)?
            .into_iter()
            .find(|definition| definition.capability_id == echo_id)
            .expect("echo provider tool definition");
        // Larger than both the observer preview and model replay preview.
        let big_message = large_echo_message();
        let candidate = capabilities
            .register_provider_tool_call(RegisterProviderToolCallRequest::new(ProviderToolCall {
                provider_id: "test-provider".to_string(),
                provider_model_id: "test-model".to_string(),
                turn_id: Some("provider-turn-1".to_string()),
                id: "call-1".to_string(),
                name: echo_tool.name,
                arguments: serde_json::json!({ "message": big_message }),
                response_reasoning: None,
                reasoning: None,
                signature: None,
            }))
            .await
            .map_err(model_capability_error)?;
        Ok(HostManagedModelResponse::capability_calls(
            vec![candidate],
            "",
        ))
    }
}

#[async_trait]
impl HostManagedModelGateway for AuthGateToolCallingGateway {
    async fn stream_model(
        &self,
        request: HostManagedModelRequest,
    ) -> Result<HostManagedModelResponse, HostManagedModelError> {
        self.requests
            .lock()
            .expect("auth-gate gateway requests lock poisoned")
            .push(request);
        Err(HostManagedModelError::safe(
            HostManagedModelErrorKind::InvalidRequest,
            "expected capability-aware model path",
        ))
    }

    async fn stream_model_with_capabilities(
        &self,
        request: HostManagedModelRequest,
        capabilities: Arc<dyn LoopCapabilityPort>,
    ) -> Result<HostManagedModelResponse, HostManagedModelError> {
        self.requests
            .lock()
            .expect("auth-gate gateway requests lock poisoned")
            .push(request);
        let notion_search_id = CapabilityId::new("notion.notion-search").expect("notion search id");
        let notion_tool = capabilities
            .tool_definitions()
            .map_err(model_capability_error)?
            .into_iter()
            .find(|definition| definition.capability_id == notion_search_id)
            .expect("activated Notion capability should be visible");
        let candidate = capabilities
            .register_provider_tool_call(RegisterProviderToolCallRequest::new(ProviderToolCall {
                provider_id: "test-provider".to_string(),
                provider_model_id: "test-model".to_string(),
                turn_id: Some("provider-turn-auth-gate".to_string()),
                id: "call-auth-gate".to_string(),
                name: notion_tool.name,
                arguments: serde_json::json!({ "query": "project notes" }),
                response_reasoning: None,
                reasoning: None,
                signature: None,
            }))
            .await
            .map_err(model_capability_error)?;
        Ok(HostManagedModelResponse::capability_calls(
            vec![candidate],
            "",
        ))
    }
}

#[async_trait]
impl HostManagedModelGateway for WorkspaceListingGateway {
    async fn stream_model(
        &self,
        request: HostManagedModelRequest,
    ) -> Result<HostManagedModelResponse, HostManagedModelError> {
        self.requests
            .lock()
            .expect("workspace gateway requests lock poisoned")
            .push(request);
        Err(HostManagedModelError::safe(
            HostManagedModelErrorKind::InvalidRequest,
            "expected capability-aware model path",
        ))
    }

    async fn stream_model_with_capabilities(
        &self,
        request: HostManagedModelRequest,
        capabilities: Arc<dyn LoopCapabilityPort>,
    ) -> Result<HostManagedModelResponse, HostManagedModelError> {
        let call_index = {
            let mut calls = self.calls.lock().expect("workspace gateway lock poisoned");
            let call_index = *calls;
            *calls += 1;
            call_index
        };
        self.requests
            .lock()
            .expect("workspace gateway requests lock poisoned")
            .push(request.clone());
        if call_index > 0 {
            let tool_result = request
                .messages
                .iter()
                .find(|message| message.role == HostManagedModelMessageRole::ToolResult)
                .expect("second model call should include tool result");
            assert_standalone_result_reference(tool_result, "workspace-sentinel.txt");
            return Ok(HostManagedModelResponse::assistant_reply("workspace ok"));
        }

        let list_dir_id = CapabilityId::new("builtin.list_dir").expect("list_dir id");
        let list_dir_tool = capabilities
            .tool_definitions()
            .map_err(model_capability_error)?
            .into_iter()
            .find(|definition| definition.capability_id == list_dir_id)
            .expect("list_dir provider tool definition");
        let candidate = capabilities
            .register_provider_tool_call(RegisterProviderToolCallRequest::new(ProviderToolCall {
                provider_id: "test-provider".to_string(),
                provider_model_id: "test-model".to_string(),
                turn_id: Some("provider-turn-1".to_string()),
                id: "call-1".to_string(),
                name: list_dir_tool.name,
                arguments: serde_json::json!({"path": "/workspace"}),
                response_reasoning: None,
                reasoning: None,
                signature: None,
            }))
            .await
            .map_err(model_capability_error)?;
        Ok(HostManagedModelResponse::capability_calls(
            vec![candidate],
            "",
        ))
    }
}

impl Drop for RuntimeEnvGuard {
    fn drop(&mut self) {
        for previous in self.previous.iter().rev() {
            ironclaw_common::env_helpers::restore_runtime_env(previous.snapshot.clone());
            if !std::thread::panicking() {
                debug_assert_eq!(
                    ironclaw_common::env_helpers::env_or_override(previous.name),
                    previous.effective.clone(),
                    "RuntimeEnvGuard failed to restore {}",
                    previous.name
                );
            }
        }
    }
}

#[tokio::test]
async fn swappable_gateway_uses_current_active_model_for_requests() {
    let provider = Arc::new(RecordingLlmProvider::new("boot-model"));
    let raw: Arc<dyn ironclaw_llm::LlmProvider> = provider.clone();
    let session =
        ironclaw_llm::create_session_manager(ironclaw_llm::SessionConfig::default()).await;
    let bundle =
        crate::runtime::wrap_swappable_gateway(raw, session, None, None).expect("gateway bundle");

    bundle
        .gateway
        .stream_model(nearai_gateway_test_request())
        .await
        .expect("first request");
    bundle
        .reload
        .reload_handle
        .primary_provider()
        .set_model("reloaded-model")
        .expect("set active model");
    bundle
        .gateway
        .stream_model(nearai_gateway_test_request())
        .await
        .expect("second request");

    let requests = provider
        .requests
        .lock()
        .expect("recording provider request lock poisoned");
    assert_eq!(
        *requests,
        vec![
            Some("boot-model".to_string()),
            Some("reloaded-model".to_string())
        ],
        "production gateway must not keep sending the model selected at boot"
    );
}

/// The LLM-provider-instrumentation seam: when a caller installs a factory
/// via `ResolvedRebornLlm::with_provider_factory` (how the bench wraps an
/// instrumented provider to capture reasoning / tokens / cost / system-prompt
/// / tool definitions), the gateway must drive the factory's output. Here the
/// factory ignores the config-built provider and returns a counting mock, so
/// if the factory were not applied the gateway would drive the config-built
/// provider (dead endpoint) instead of returning the mock's sentinel.
#[tokio::test]
async fn wrap_swappable_gateway_applies_provider_factory() {
    let session_dir = tempfile::tempdir().expect("session tempdir");
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mock: Arc<dyn ironclaw_llm::LlmProvider> = Arc::new(CountingOverrideProvider {
        calls: Arc::clone(&calls),
    });

    let config = ironclaw_llm::LlmConfig {
        backend: "nearai".to_string(),
        session: ironclaw_llm::SessionConfig {
            auth_base_url: "http://127.0.0.1:1".to_string(),
            session_path: session_dir.path().join("session.json"),
        },
        nearai: ironclaw_llm::NearAiConfig {
            model: "config-model-should-not-be-used".to_string(),
            cheap_model: None,
            base_url: "http://127.0.0.1:1".to_string(),
            api_key: None,
            fallback_model: None,
            max_retries: 0,
            circuit_breaker_threshold: None,
            circuit_breaker_recovery_secs: 30,
            response_cache_enabled: false,
            response_cache_ttl_secs: 3600,
            response_cache_max_entries: 1000,
            failover_cooldown_secs: 300,
            failover_cooldown_threshold: 3,
            smart_routing_cascade: false,
            unsupported_params: Vec::new(),
        },
        provider: None,
        bedrock: None,
        gemini_oauth: None,
        openai_codex: None,
        request_timeout_secs: 5,
        cheap_model: None,
        smart_routing_cascade: false,
        max_retries: 0,
        circuit_breaker_threshold: None,
        circuit_breaker_recovery_secs: 30,
        response_cache_enabled: false,
        response_cache_ttl_secs: 3600,
        response_cache_max_entries: 1000,
    };

    let factory_mock = Arc::clone(&mock);
    let session = ironclaw_llm::create_session_manager(config.session.clone()).await;
    let built = ironclaw_llm::build_static_provider_chain(&config, Arc::clone(&session))
        .await
        .expect("provider chain builds from config");
    let bundle = crate::runtime::wrap_swappable_gateway(
        built,
        session,
        Some(Arc::new(move |_built| Arc::clone(&factory_mock))),
        None,
    )
    .expect("gateway builds with the provider factory");

    let response = bundle
        .gateway
        .stream_model(nearai_gateway_test_request())
        .await
        .expect("gateway drives the factory-produced provider");

    assert_eq!(
        response.safe_text_deltas,
        vec!["override-driven".to_string()],
        "gateway must return the factory provider's response, not the config-built one"
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the override provider should be invoked exactly once"
    );
}

/// Provider wrapper that counts model calls and delegates to its inner — a
/// stand-in for the bench's instrumentation wrapper. Unlike
/// `CountingOverrideProvider`, it wraps `inner` so swapping the inner (via a
/// live reload of a `SwappableLlmProvider`) is observable through it.
struct CountingWrapperProvider {
    inner: Arc<dyn ironclaw_llm::LlmProvider>,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait::async_trait]
impl ironclaw_llm::LlmProvider for CountingWrapperProvider {
    fn model_name(&self) -> &str {
        self.inner.model_name()
    }

    fn cost_per_token(&self) -> (rust_decimal::Decimal, rust_decimal::Decimal) {
        self.inner.cost_per_token()
    }

    async fn complete(
        &self,
        request: ironclaw_llm::CompletionRequest,
    ) -> Result<ironclaw_llm::CompletionResponse, ironclaw_llm::LlmError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.complete(request).await
    }

    async fn complete_with_tools(
        &self,
        request: ironclaw_llm::ToolCompletionRequest,
    ) -> Result<ironclaw_llm::ToolCompletionResponse, ironclaw_llm::LlmError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.complete_with_tools(request).await
    }
}

/// Minimal nearai `LlmConfig` pointed at a dead endpoint: it *builds* lazily
/// (no connection at construction) but any model call errors. Enough to
/// exercise gateway/reload wiring without a network.
fn dead_endpoint_nearai_config(session_path: std::path::PathBuf) -> ironclaw_llm::LlmConfig {
    ironclaw_llm::LlmConfig {
        backend: "nearai".to_string(),
        session: ironclaw_llm::SessionConfig {
            auth_base_url: "http://127.0.0.1:1".to_string(),
            session_path,
        },
        nearai: ironclaw_llm::NearAiConfig {
            model: "config-model".to_string(),
            cheap_model: None,
            base_url: "http://127.0.0.1:1".to_string(),
            api_key: None,
            fallback_model: None,
            max_retries: 0,
            circuit_breaker_threshold: None,
            circuit_breaker_recovery_secs: 30,
            response_cache_enabled: false,
            response_cache_ttl_secs: 3600,
            response_cache_max_entries: 1000,
            failover_cooldown_secs: 300,
            failover_cooldown_threshold: 3,
            smart_routing_cascade: false,
            unsupported_params: Vec::new(),
        },
        provider: None,
        bedrock: None,
        gemini_oauth: None,
        openai_codex: None,
        request_timeout_secs: 5,
        cheap_model: None,
        smart_routing_cascade: false,
        max_retries: 0,
        circuit_breaker_threshold: None,
        circuit_breaker_recovery_secs: 30,
        response_cache_enabled: false,
        response_cache_ttl_secs: 3600,
        response_cache_max_entries: 1000,
    }
}

/// Regression guard for Firat's review: the provider factory (caller
/// instrumentation) must survive a live config reload. `wrap_swappable_gateway`
/// wraps the factory over the `SwappableLlmProvider`, so reloading — which
/// swaps the swappable's *inner* — keeps the wrapper in the call path. If the
/// factory were applied to the bare provider instead, the first reload would
/// silently drop instrumentation and this test's post-reload count would stay
/// at 1.
#[tokio::test]
async fn provider_factory_survives_live_reload() {
    let session_dir = tempfile::tempdir().expect("session tempdir");
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls_for_factory = Arc::clone(&calls);
    let factory: ironclaw_operator::RebornProviderFactory = Arc::new(move |inner| {
        Arc::new(CountingWrapperProvider {
            inner,
            calls: Arc::clone(&calls_for_factory),
        }) as Arc<dyn ironclaw_llm::LlmProvider>
    });

    let config = dead_endpoint_nearai_config(session_dir.path().join("session.json"));
    let session = ironclaw_llm::create_session_manager(config.session.clone()).await;
    let built = ironclaw_llm::build_static_provider_chain(&config, Arc::clone(&session))
        .await
        .expect("provider chain builds from config");
    let bundle = crate::runtime::wrap_swappable_gateway(built, session, Some(factory), None)
        .expect("gateway builds with the provider factory");

    // First model call routes through the instrumentation wrapper. The dead
    // endpoint makes the underlying call error, but the wrapper counts before
    // delegating, so the result is irrelevant — only that it was observed.
    #[expect(
        clippy::let_underscore_must_use,
        reason = "dead endpoint errors by design; only the wrapper's observation count matters"
    )]
    let _ = bundle
        .gateway
        .stream_model(nearai_gateway_test_request())
        .await;
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the instrumentation wrapper should observe the first model call"
    );

    // Live config reload: rebuild the chain and atomically swap the
    // swappable's inner provider — exactly what the WebUI settings path does.
    bundle
        .reload
        .reload_handle
        .reload(&config, Arc::clone(&bundle.reload.session))
        .await
        .expect("live reload rebuilds the provider chain");

    #[expect(
        clippy::let_underscore_must_use,
        reason = "dead endpoint errors by design; only the wrapper's observation count matters"
    )]
    let _ = bundle
        .gateway
        .stream_model(nearai_gateway_test_request())
        .await;
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "the instrumentation wrapper must still observe model calls after a live reload"
    );
}

/// Regression guard for the trace-recording gap: `IRONCLAW_RECORD_TRACE=1` on
/// the serve/run path must place a `RecordingLlm` in the turn provider chain.
/// The runtime builds turns through `wrap_swappable_gateway`, which never calls
/// `RecordingLlm::from_env`, and hot-reloads through
/// `build_provider_chain_components`, which also does not — so the recorder is
/// wired *only* via `ResolvedRebornLlm::with_env_trace_recording`. Nothing
/// pinned "serve + IRONCLAW_RECORD_TRACE ⇒ recorder attached" before, which is
/// exactly why the env "enabled" recording yet serve emitted nothing (the
/// committed reborn_qa fixtures were recorded through the in-process harness,
/// whose `build_provider_chain` path *does* wire the recorder).
///
/// This asserts the gate at the exact serve/run resolution seam. That the
/// attached factory actually wraps a recorder which records and flushes to disk
/// incrementally (no explicit `flush()`, matching serve's signalled shutdown)
/// is proven in
/// `ironclaw_llm::recording::tests::complete_flushes_incrementally_without_explicit_flush`
/// — the crate that owns `RecordingLlm` and can set real env vars, which this
/// `#![forbid(unsafe_code)]` crate cannot.
#[tokio::test]
async fn env_trace_recording_attaches_recorder_factory_only_when_enabled() {
    let session_dir = tempfile::tempdir().expect("session tempdir");
    let config = dead_endpoint_nearai_config(session_dir.path().join("session.json"));

    // Disabled: no factory attached; the resolved LLM is returned unchanged.
    {
        let _guard = RuntimeEnvGuard::with([("IRONCLAW_RECORD_TRACE", None)]).await;
        let disabled = ironclaw_operator::ResolvedRebornLlm::from_llm_config(config.clone())
            .with_env_trace_recording();
        assert!(
            disabled.provider_factory().is_none(),
            "no recording factory should attach when IRONCLAW_RECORD_TRACE is unset"
        );
    }

    // Enabled: the serve/run resolution path attaches the recording factory.
    {
        let _guard = RuntimeEnvGuard::set("IRONCLAW_RECORD_TRACE", "1").await;
        let enabled = ironclaw_operator::ResolvedRebornLlm::from_llm_config(config)
            .with_env_trace_recording();
        assert!(
            enabled.provider_factory().is_some(),
            "IRONCLAW_RECORD_TRACE must attach the recording provider factory on the \
             serve/run resolution path"
        );
    }
}

/// Regression guard for the benchmark instrumentation seam: a
/// `ResolvedRebornLlm` carrying a `provider_factory` must have that factory
/// invoked during `build_reborn_runtime`, i.e. the caller's instrumentation
/// wrapper is threaded into the cold-boot gateway.
///
/// PR #6174 collapsed the boot path to `build_placeholder_llm_gateway()`, which
/// hardcoded `None` for the factory, so `ResolvedRebornLlm::with_provider_factory`
/// silently never ran on the production path — the benchmark harness saw every
/// task fail with zero model calls (no instrumented provider). The
/// `provider_factory_survives_live_reload` test above exercises the
/// `wrap_swappable_gateway` helper directly with `Some(..)`, so it cannot catch
/// a boot path that never calls the helper with a factory at all. This drives
/// the real caller (`build_reborn_runtime`) instead.
#[tokio::test]
async fn provider_factory_runs_during_production_boot() {
    let _env_guard =
        RuntimeEnvGuard::with([("NEARAI_BASE_URL", Some("https://cloud-api.nearai.example"))])
            .await;
    let root = tempfile::tempdir().expect("tempdir");
    let session_dir = tempfile::tempdir().expect("session tempdir");
    let standalone_root = root.path().join("standalone");

    let factory_ran = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let factory_ran_for_closure = Arc::clone(&factory_ran);
    // Identity decorator that only records that it was constructed: the factory
    // runs once, at gateway construction, to wrap the swappable provider.
    let factory: ironclaw_operator::RebornProviderFactory = Arc::new(move |inner| {
        factory_ran_for_closure.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        inner
    });

    let config = dead_endpoint_nearai_config(session_dir.path().join("session.json"));
    let llm = ironclaw_operator::ResolvedRebornLlm::from_llm_config(config)
        .with_provider_factory(factory);

    // No `boot` config is supplied, so the boot-time reload is skipped and the
    // dead endpoint is never contacted; the factory still wraps the swappable
    // at cold-boot construction.
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "provider-factory-boot-owner",
            standalone_root,
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_resolved_llm(llm)
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "provider-factory-boot-tenant".to_string(),
        agent_id: "provider-factory-boot-agent".to_string(),
        source_binding_id: "provider-factory-boot-source".to_string(),
        reply_target_binding_id: "provider-factory-boot-reply".to_string(),
    });

    let _runtime = build_reborn_runtime(input).await.expect("runtime builds");

    assert_eq!(
        factory_ran.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the caller's provider_factory must be invoked once during boot so \
         instrumentation wraps the swappable gateway (regression: #6174 dropped it)"
    );
}

/// Pins the provider wrap site in `wrap_swappable_gateway`: a gateway call
/// lands in the process-wide registry under the provider's served model.
#[tokio::test]
async fn swappable_gateway_records_llm_requests_into_operator_metrics() {
    const MODEL: &str = "operator-metrics-pin-model";
    let raw: Arc<dyn ironclaw_llm::LlmProvider> = Arc::new(RecordingLlmProvider::new(MODEL));
    let provider_id = raw.provider_id();
    let session =
        ironclaw_llm::create_session_manager(ironclaw_llm::SessionConfig::default()).await;
    let bundle =
        crate::runtime::wrap_swappable_gateway(raw, session, None, None).expect("gateway bundle");
    bundle
        .gateway
        .stream_model(nearai_gateway_test_request())
        .await
        .expect("gateway request");

    let rendered = ironclaw_operator::operator_metrics().render_prometheus();
    assert!(
        rendered.lines().any(|line| {
            line.starts_with("ironclaw_llm_requests_total{")
                && line.contains(&format!("model=\"{MODEL}\""))
                && line.contains("outcome=\"success\"")
                && line.contains(&format!("provider=\"{provider_id}\""))
        }),
        "gateway request recorded by provider and model:\n{rendered}"
    );
}
