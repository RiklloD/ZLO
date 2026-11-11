//! NEAR AI credential bootstrap: the auth-capture server, session-token and
//! stored API-key bootstrap of the root gateway and the NEAR AI MCP server, and
//! restart behavior.

use super::*;

const NEARAI_AUTH_CAPTURE_MAX_REQUEST_BYTES: usize = 50 * 1024 * 1024;
const NEARAI_AUTH_CAPTURE_IO_TIMEOUT: Duration = Duration::from_secs(5);
const NEARAI_AUTH_CAPTURE_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

async fn write_nearai_auth_capture_bytes(
    stream: &mut tokio::net::TcpStream,
    response: &[u8],
) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;

    match tokio::time::timeout(NEARAI_AUTH_CAPTURE_IO_TIMEOUT, stream.write_all(response)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(format!("write auth capture response failed: {error}")),
        Err(_) => Err(format!(
            "write auth capture response timed out after {:?}",
            NEARAI_AUTH_CAPTURE_IO_TIMEOUT
        )),
    }
}

async fn write_nearai_auth_capture_response(
    stream: &mut tokio::net::TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> Result<(), String> {
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    write_nearai_auth_capture_bytes(stream, response.as_bytes()).await
}

async fn start_nearai_auth_capture_server() -> (String, tokio::sync::oneshot::Receiver<String>) {
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpSocket;

    let socket = TcpSocket::new_v4().expect("test server socket");
    socket
        .bind("127.0.0.1:0".parse().expect("test server address"))
        .expect("test server binds");
    let listener = socket.listen(1024).expect("test server listens");
    let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
    let (auth_tx, auth_rx) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        let mut auth_tx = Some(auth_tx);
        'connections: loop {
            let (mut stream, _) =
                match tokio::time::timeout(NEARAI_AUTH_CAPTURE_IDLE_TIMEOUT, listener.accept())
                    .await
                {
                    Ok(Ok(accepted)) => accepted,
                    Ok(Err(error)) => panic!("accept test request: {error}"),
                    Err(_) => break,
                };
            let mut buffer = Vec::new();
            let mut header_end = None;
            loop {
                let mut chunk = [0_u8; 1024];
                let read = match tokio::time::timeout(
                    NEARAI_AUTH_CAPTURE_IO_TIMEOUT,
                    stream.read(&mut chunk),
                )
                .await
                {
                    Ok(Ok(read)) => read,
                    Ok(Err(error)) => panic!("read test request: {error}"),
                    Err(_) => {
                        write_nearai_auth_capture_response(
                            &mut stream,
                            "408 Request Timeout",
                            "text/plain",
                            "request read timed out",
                        )
                        .await
                        .expect("write auth capture read timeout response");
                        continue 'connections;
                    }
                };
                if read == 0 {
                    break;
                }
                if buffer.len().saturating_add(read) > NEARAI_AUTH_CAPTURE_MAX_REQUEST_BYTES {
                    write_nearai_auth_capture_response(
                        &mut stream,
                        "413 Payload Too Large",
                        "text/plain",
                        "request too large",
                    )
                    .await
                    .expect("write auth capture oversized request response");
                    continue 'connections;
                }
                buffer.extend_from_slice(&chunk[..read]);
                if let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                    header_end = Some(index + 4);
                    break;
                }
            }

            let Some(header_end) = header_end else {
                write_nearai_auth_capture_response(
                    &mut stream,
                    "400 Bad Request",
                    "text/plain",
                    "incomplete request headers",
                )
                .await
                .expect("write auth capture incomplete headers response");
                continue;
            };
            let headers = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
            let content_length = match headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            {
                Some((_, value)) => match value.trim().parse::<usize>() {
                    Ok(length) => length,
                    Err(_) => {
                        write_nearai_auth_capture_response(
                            &mut stream,
                            "400 Bad Request",
                            "text/plain",
                            "invalid content-length",
                        )
                        .await
                        .expect("write auth capture invalid content-length response");
                        continue;
                    }
                },
                None => {
                    write_nearai_auth_capture_response(
                        &mut stream,
                        "400 Bad Request",
                        "text/plain",
                        "missing content-length",
                    )
                    .await
                    .expect("write auth capture missing content-length response");
                    continue;
                }
            };
            let Some(request_len) = header_end.checked_add(content_length) else {
                write_nearai_auth_capture_response(
                    &mut stream,
                    "413 Payload Too Large",
                    "text/plain",
                    "request too large",
                )
                .await
                .expect("write auth capture overflow response");
                continue;
            };
            if request_len > NEARAI_AUTH_CAPTURE_MAX_REQUEST_BYTES {
                write_nearai_auth_capture_response(
                    &mut stream,
                    "413 Payload Too Large",
                    "text/plain",
                    "request too large",
                )
                .await
                .expect("write auth capture oversized content-length response");
                continue;
            }
            while buffer.len() < request_len {
                let mut chunk = [0_u8; 1024];
                let read = match tokio::time::timeout(
                    NEARAI_AUTH_CAPTURE_IO_TIMEOUT,
                    stream.read(&mut chunk),
                )
                .await
                {
                    Ok(Ok(read)) => read,
                    Ok(Err(error)) => panic!("read test body: {error}"),
                    Err(_) => {
                        write_nearai_auth_capture_response(
                            &mut stream,
                            "408 Request Timeout",
                            "text/plain",
                            "request body read timed out",
                        )
                        .await
                        .expect("write auth capture body timeout response");
                        continue 'connections;
                    }
                };
                if read == 0 {
                    write_nearai_auth_capture_response(
                        &mut stream,
                        "400 Bad Request",
                        "text/plain",
                        "incomplete request body",
                    )
                    .await
                    .expect("write auth capture incomplete body response");
                    continue 'connections;
                }
                let remaining = request_len - buffer.len();
                buffer.extend_from_slice(&chunk[..read.min(remaining)]);
            }

            let body = &buffer[header_end..request_len];
            let request_json = if body.is_empty() {
                None
            } else {
                match serde_json::from_slice::<serde_json::Value>(body) {
                    Ok(value) => Some(value),
                    Err(_) => {
                        write_nearai_auth_capture_response(
                            &mut stream,
                            "400 Bad Request",
                            "text/plain",
                            "invalid json body",
                        )
                        .await
                        .expect("write auth capture invalid json response");
                        continue;
                    }
                }
            };
            let wants_stream = request_json
                .as_ref()
                .and_then(|value| value.get("stream"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let request_line = headers.lines().next().unwrap_or_default();
            let auth_header = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                .map(|(_, value)| value.trim())
                .unwrap_or_default()
                .to_string();
            let is_chat_completion = request_line.contains("/v1/chat/completions");
            if is_chat_completion && wants_stream {
                let body = concat!(
                    r#"data: {"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#,
                    "\n\n",
                    "data: [DONE]\n\n"
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n{}",
                    body
                );
                write_nearai_auth_capture_bytes(&mut stream, response.as_bytes())
                    .await
                    .expect("write test streaming response");
            } else {
                let body = if is_chat_completion {
                    r#"{"choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#
                } else {
                    r#"{"data":[]}"#
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                write_nearai_auth_capture_bytes(&mut stream, response.as_bytes())
                    .await
                    .expect("write test response");
            }

            if is_chat_completion {
                if let Some(auth_tx) = auth_tx.take() {
                    #[allow(clippy::let_underscore_must_use)]
                    // oneshot send; dropped receiver is expected
                    let _ = auth_tx.send(auth_header);
                }
                break;
            }
        }
    });

    (base_url, auth_rx)
}

async fn send_nearai_auth_capture_raw_request(base_url: &str, request: String) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let address = base_url
        .strip_prefix("http://")
        .expect("capture server URL has http prefix");
    let mut stream = tokio::net::TcpStream::connect(address)
        .await
        .expect("connect to capture server");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write raw capture request");
    stream.shutdown().await.expect("finish raw capture request");

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .await
        .expect("read raw capture response");
    response
}

#[tokio::test]
async fn nearai_auth_capture_server_rejects_incomplete_body() {
    let (base_url, _auth_rx) = start_nearai_auth_capture_server().await;
    let response = send_nearai_auth_capture_raw_request(
        &base_url,
        "POST /v1/chat/completions HTTP/1.1\r\nhost: localhost\r\ncontent-length: 32\r\n\r\n{\"stream\":true"
            .to_string(),
    )
    .await;

    assert!(
        response.starts_with("HTTP/1.1 400 Bad Request"),
        "expected incomplete body to be rejected, got: {response:?}"
    );
}

#[tokio::test]
async fn nearai_auth_capture_server_rejects_oversized_content_length() {
    let (base_url, _auth_rx) = start_nearai_auth_capture_server().await;
    let response = send_nearai_auth_capture_raw_request(
        &base_url,
        format!(
            "POST /v1/chat/completions HTTP/1.1\r\nhost: localhost\r\ncontent-length: {}\r\n\r\n",
            NEARAI_AUTH_CAPTURE_MAX_REQUEST_BYTES + 1
        ),
    )
    .await;

    assert!(
        response.starts_with("HTTP/1.1 413 Payload Too Large"),
        "expected oversized body to be rejected, got: {response:?}"
    );
}

#[tokio::test]
async fn nearai_auth_capture_server_rejects_missing_content_length() {
    let (base_url, _auth_rx) = start_nearai_auth_capture_server().await;
    let response = send_nearai_auth_capture_raw_request(
        &base_url,
        "POST /v1/chat/completions HTTP/1.1\r\nhost: localhost\r\n\r\n{}".to_string(),
    )
    .await;

    assert!(
        response.starts_with("HTTP/1.1 400 Bad Request"),
        "expected missing content-length to be rejected, got: {response:?}"
    );
    assert!(
        response.contains("missing content-length"),
        "expected missing content-length diagnostic, got: {response:?}"
    );
}

#[async_trait]
impl ironclaw_llm::LlmProvider for RecordingLlmProvider {
    fn model_name(&self) -> &str {
        "recording-provider"
    }

    fn cost_per_token(&self) -> (rust_decimal::Decimal, rust_decimal::Decimal) {
        (rust_decimal::Decimal::ZERO, rust_decimal::Decimal::ZERO)
    }

    async fn complete(
        &self,
        request: ironclaw_llm::CompletionRequest,
    ) -> Result<ironclaw_llm::CompletionResponse, ironclaw_llm::LlmError> {
        self.requests
            .lock()
            .expect("recording provider request lock poisoned")
            .push(request.model);
        Ok(ironclaw_llm::CompletionResponse {
            content: "ok".to_string(),
            input_tokens: 1,
            output_tokens: 1,
            finish_reason: ironclaw_llm::FinishReason::Stop,
            reasoning: None,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        })
    }

    async fn complete_with_tools(
        &self,
        request: ironclaw_llm::ToolCompletionRequest,
    ) -> Result<ironclaw_llm::ToolCompletionResponse, ironclaw_llm::LlmError> {
        self.requests
            .lock()
            .expect("recording provider request lock poisoned")
            .push(request.model);
        Ok(ironclaw_llm::ToolCompletionResponse {
            content: Some("ok".to_string()),
            tool_calls: Vec::new(),
            input_tokens: 1,
            output_tokens: 1,
            finish_reason: ironclaw_llm::FinishReason::Stop,
            reasoning: None,
            reasoning_details: None,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        })
    }

    fn active_model_name(&self) -> String {
        self.active_model
            .lock()
            .expect("recording provider active-model lock poisoned")
            .clone()
    }

    fn set_model(&self, model: &str) -> Result<(), ironclaw_llm::LlmError> {
        *self
            .active_model
            .lock()
            .expect("recording provider active-model lock poisoned") = model.to_string();
        Ok(())
    }
}

#[tokio::test]
async fn root_llm_gateway_bootstraps_nearai_session_token_from_env() {
    let _token_guard = RuntimeEnvGuard::set("NEARAI_SESSION_TOKEN", "sess_reborn_env_token").await;
    let session_dir = tempfile::tempdir().expect("session tempdir");
    let (base_url, auth_rx) = start_nearai_auth_capture_server().await;

    let config = ironclaw_llm::LlmConfig {
        backend: "nearai".to_string(),
        session: ironclaw_llm::SessionConfig {
            auth_base_url: base_url.clone(),
            session_path: session_dir.path().join("session.json"),
        },
        nearai: ironclaw_llm::NearAiConfig {
            model: "test-model".to_string(),
            cheap_model: None,
            base_url,
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
    let session = ironclaw_llm::create_session_manager(config.session.clone()).await;
    let built = ironclaw_llm::build_static_provider_chain(&config, Arc::clone(&session))
        .await
        .expect("provider chain builds from config");
    let bundle =
        crate::runtime::wrap_swappable_gateway(built, session, None, None).expect("gateway builds");
    let response = bundle
        .gateway
        .stream_model(nearai_gateway_test_request())
        .await
        .expect("gateway calls NEAR AI provider");

    assert_eq!(response.safe_text_deltas, vec!["ok".to_string()]);
    let auth_header = tokio::time::timeout(Duration::from_secs(2), auth_rx)
        .await
        .expect("chat request should be captured")
        .expect("auth header should be sent by capture server");
    assert_eq!(auth_header, "Bearer sess_reborn_env_token");
}

#[tokio::test]
async fn runtime_nearai_mcp_bootstraps_from_nearai_session_token() {
    let _env_guard = RuntimeEnvGuard::with([
        ("NEARAI_SESSION_TOKEN", Some("sess_reborn_mcp_token")),
        ("NEARAI_API_KEY", None),
        ("NEARAI_BASE_URL", Some("https://cloud-api.nearai.example")),
    ])
    .await;
    let root = tempfile::tempdir().expect("tempdir");
    let session_dir = tempfile::tempdir().expect("session tempdir");
    let standalone_root = root.path().join("standalone");

    let config = ironclaw_llm::LlmConfig {
        backend: "nearai".to_string(),
        session: ironclaw_llm::SessionConfig {
            auth_base_url: "https://private.nearai.example".to_string(),
            session_path: session_dir.path().join("session.json"),
        },
        nearai: ironclaw_llm::NearAiConfig {
            model: "test-model".to_string(),
            cheap_model: None,
            base_url: "https://private.nearai.example".to_string(),
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
    let llm = ironclaw_operator::ResolvedRebornLlm::from_llm_config(config);

    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-nearai-session-mcp-owner",
            standalone_root,
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_resolved_llm(llm)
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-nearai-session-mcp-tenant".to_string(),
        agent_id: "runtime-nearai-session-mcp-agent".to_string(),
        source_binding_id: "runtime-nearai-session-mcp-source".to_string(),
        reply_target_binding_id: "runtime-nearai-session-mcp-reply".to_string(),
    });

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let extension_management = &runtime.extension_management;
    let nearai_ref =
        LifecyclePackageRef::new(LifecyclePackageKind::Extension, "nearai").expect("valid ref");
    let projection = extension_management
        .project(
            nearai_ref,
            extension_management.tenant_operator_user_id_for_test(),
        )
        .await
        .expect("NEAR AI MCP projected");
    assert_eq!(projection.phase, InstallationState::Active);

    let capabilities = extension_management
        .active_model_visible_capabilities()
        .await
        .expect("active capabilities");
    assert!(
        capabilities
            .iter()
            .any(|capability| capability.id.as_str() == "nearai.web_search"),
        "nearai.web_search should be active with NEAR AI session-token config"
    );
    stop_turn_runner_worker_for_manual_state_test(&runtime).await;
}

#[tokio::test]
async fn runtime_nearai_mcp_bootstraps_from_stored_nearai_api_key() {
    let _env_guard = RuntimeEnvGuard::with([
        ("NEARAI_SESSION_TOKEN", None),
        ("NEARAI_API_KEY", None),
        ("NEARAI_BASE_URL", Some("https://cloud-api.nearai.example")),
    ])
    .await;
    let root = tempfile::tempdir().expect("tempdir");
    let standalone_root = root.path().join("standalone");
    let session_dir = tempfile::tempdir().expect("session tempdir");

    let services = crate::factory::build_runtime_substrate(
        crate::deployment::local_filesystem_build_input(
            "runtime-nearai-stored-mcp-owner",
            standalone_root.clone(),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .await
    .expect("services build for stored key seed");
    ironclaw_operator::LlmKeyStore::new(crate::RuntimeOperatorSecretValueStore::shared(
        services.secret_store(),
    ))
    .put(
        "nearai",
        ironclaw_secrets::SecretMaterial::from("sk-reborn-stored-nearai-mcp-key"),
    )
    .await
    .expect("stored key seeded");
    drop(services);

    let config = ironclaw_llm::LlmConfig {
        backend: "nearai".to_string(),
        session: ironclaw_llm::SessionConfig {
            auth_base_url: "https://private.nearai.example".to_string(),
            session_path: session_dir.path().join("session.json"),
        },
        nearai: ironclaw_llm::NearAiConfig {
            model: "test-model".to_string(),
            cheap_model: None,
            base_url: "https://cloud-api.nearai.example".to_string(),
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
    let llm = ironclaw_operator::ResolvedRebornLlm::from_llm_config(config);

    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-nearai-stored-mcp-owner",
            standalone_root,
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_resolved_llm(llm)
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-nearai-stored-mcp-tenant".to_string(),
        agent_id: "runtime-nearai-stored-mcp-agent".to_string(),
        source_binding_id: "runtime-nearai-stored-mcp-source".to_string(),
        reply_target_binding_id: "runtime-nearai-stored-mcp-reply".to_string(),
    });

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let extension_management = &runtime.extension_management;
    let nearai_ref =
        LifecyclePackageRef::new(LifecyclePackageKind::Extension, "nearai").expect("valid ref");
    let projection = extension_management
        .project(
            nearai_ref,
            extension_management.tenant_operator_user_id_for_test(),
        )
        .await
        .expect("NEAR AI MCP projected");
    assert_eq!(projection.phase, InstallationState::Active);

    let capabilities = extension_management
        .active_model_visible_capabilities()
        .await
        .expect("active capabilities");
    assert!(
        capabilities
            .iter()
            .any(|capability| capability.id.as_str() == "nearai.web_search"),
        "nearai.web_search should be active with stored NEAR AI API key config"
    );
    stop_turn_runner_worker_for_manual_state_test(&runtime).await;
}

async fn nearai_mcp_runtime_access_secret(
    runtime: &crate::runtime::RebornRuntime,
    owner_scope: ResourceScope,
) -> String {
    let product_auth = &runtime.product_auth;
    let auth_scope = ironclaw_auth::AuthProductScope::credential_owner(
        &owner_scope,
        ironclaw_auth::AuthSurface::Api,
    );
    let accounts = product_auth
        .credential_account_record_source()
        .accounts_for_owner(&auth_scope)
        .await
        .expect("NEAR AI product-auth accounts");
    let account = accounts
        .into_iter()
        .find(|account| {
            account.provider.as_str() == "nearai"
                && account.status == ironclaw_auth::CredentialAccountStatus::Configured
        })
        .expect("configured NEAR AI product-auth account");

    assert_eq!(account.scope.resource.tenant_id, owner_scope.tenant_id);
    assert_eq!(account.scope.resource.user_id, owner_scope.user_id);
    assert_eq!(account.scope.resource.agent_id, owner_scope.agent_id);
    assert_eq!(account.scope.resource.project_id, owner_scope.project_id);

    let handle = account.access_secret.expect("NEAR AI access secret");
    let store = runtime.secret_store();
    let lease = store
        .lease_once(&account.scope.resource, &handle)
        .await
        .expect("NEAR AI access secret lease");
    let material = store
        .consume(&account.scope.resource, lease.id)
        .await
        .expect("NEAR AI access secret material");
    secrecy::ExposeSecret::expose_secret(&material).to_string()
}

#[tokio::test]
async fn runtime_nearai_mcp_prebuild_api_key_is_not_replaced_by_stored_key() {
    let _env_guard = RuntimeEnvGuard::with([
        ("NEARAI_SESSION_TOKEN", None),
        ("NEARAI_API_KEY", None),
        ("NEARAI_BASE_URL", Some("https://cloud-api.nearai.example")),
    ])
    .await;
    let root = tempfile::tempdir().expect("tempdir");
    let standalone_root = root.path().join("standalone");
    let session_dir = tempfile::tempdir().expect("session tempdir");
    let owner = "runtime-nearai-prebuild-mcp-owner";
    let tenant = "runtime-nearai-prebuild-mcp-tenant";
    let agent = "runtime-nearai-prebuild-mcp-agent";

    let services = crate::factory::build_runtime_substrate(
        crate::deployment::local_filesystem_build_input(owner, standalone_root.clone())
            .with_runtime_policy(standalone_runtime_policy()),
    )
    .await
    .expect("services build for stored key seed");
    ironclaw_operator::LlmKeyStore::new(crate::RuntimeOperatorSecretValueStore::shared(
        services.secret_store(),
    ))
    .put(
        "nearai",
        ironclaw_secrets::SecretMaterial::from("sk-post-build-stored-nearai-mcp-key"),
    )
    .await
    .expect("stored key seeded");
    drop(services);

    let config = ironclaw_llm::LlmConfig {
        backend: "nearai".to_string(),
        session: ironclaw_llm::SessionConfig {
            auth_base_url: "https://private.nearai.example".to_string(),
            session_path: session_dir.path().join("session.json"),
        },
        nearai: ironclaw_llm::NearAiConfig {
            model: "test-model".to_string(),
            cheap_model: None,
            base_url: "https://cloud-api.nearai.example".to_string(),
            api_key: Some(secrecy::SecretString::from("sk-prebuild-nearai-mcp-key")),
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
    let llm = ironclaw_operator::ResolvedRebornLlm::from_llm_config(config);

    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(owner, standalone_root)
            .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_resolved_llm(llm)
    .with_identity(RebornRuntimeIdentity {
        tenant_id: tenant.to_string(),
        agent_id: agent.to_string(),
        source_binding_id: "runtime-nearai-prebuild-mcp-source".to_string(),
        reply_target_binding_id: "runtime-nearai-prebuild-mcp-reply".to_string(),
    });

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let owner_scope = ResourceScope {
        tenant_id: TenantId::new(tenant).expect("tenant"),
        user_id: UserId::new(owner).expect("owner"),
        agent_id: Some(AgentId::new(agent).expect("agent")),
        project_id: None::<ProjectId>,
        mission_id: None,
        thread_id: None,
        invocation_id: InvocationId::new(),
    };
    let material = nearai_mcp_runtime_access_secret(&runtime, owner_scope).await;

    assert_eq!(material, "sk-prebuild-nearai-mcp-key");
    stop_turn_runner_worker_for_manual_state_test(&runtime).await;
}

#[async_trait::async_trait]
impl ironclaw_llm::LlmProvider for CountingOverrideProvider {
    fn model_name(&self) -> &str {
        "mock-override-model"
    }

    fn cost_per_token(&self) -> (rust_decimal::Decimal, rust_decimal::Decimal) {
        (rust_decimal::Decimal::ZERO, rust_decimal::Decimal::ZERO)
    }

    async fn complete(
        &self,
        _request: ironclaw_llm::CompletionRequest,
    ) -> Result<ironclaw_llm::CompletionResponse, ironclaw_llm::LlmError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ironclaw_llm::CompletionResponse {
            content: "override-driven".to_string(),
            input_tokens: 0,
            output_tokens: 0,
            finish_reason: ironclaw_llm::FinishReason::Stop,
            reasoning: None,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        })
    }

    async fn complete_with_tools(
        &self,
        _request: ironclaw_llm::ToolCompletionRequest,
    ) -> Result<ironclaw_llm::ToolCompletionResponse, ironclaw_llm::LlmError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(ironclaw_llm::ToolCompletionResponse {
            content: Some("override-driven".to_string()),
            tool_calls: Vec::new(),
            input_tokens: 0,
            output_tokens: 0,
            finish_reason: ironclaw_llm::FinishReason::Stop,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            reasoning: None,
            reasoning_details: None,
        })
    }
}

/// Regression pin for the journey-critical fix (PR #6174): a provider
/// selected purely through `config.toml` + a stored API key (no env var set)
/// must reach the turn-serving provider. This exercises the ONLY mechanism
/// that now applies a stored key to the live gateway — the post-construction
/// `RebornLlmReloadAdapter::reload()` invoked once inside
/// `build_reborn_runtime` — by supplying a real `boot` config (so the
/// reload adapter can re-resolve `[llm.default]` from disk) instead of
/// pre-baking the stored key into a directly-supplied `ResolvedRebornLlm`
/// (which no longer feeds the gateway at all).
#[tokio::test]
async fn standalone_runtime_startup_uses_stored_nearai_api_key_after_restart() {
    // NOTE on isolation: this test does not need to override
    // `NEARAI_SESSION_PATH` / `NEARAI_AUTH_URL` (both env-only inputs to
    // `ironclaw_llm::resolution::nearai_session_config`, which the reload
    // adapter's config-file re-resolution invokes). `NearAiChatProvider::
    // resolve_bearer_token` checks `config.nearai.api_key` FIRST, before
    // ever touching the session manager — and `apply_stored_api_key` (called
    // by `RebornLlmReloadAdapter::reload`) sets exactly that field from the
    // seeded key below. So the session/auth-url defaults are constructed but
    // never read from disk or contacted over the network.
    let _env_guard = RuntimeEnvGuard::with([
        ("NEARAI_SESSION_TOKEN", None),
        ("NEARAI_API_KEY", None),
        ("NEARAI_BASE_URL", Some("https://cloud-api.nearai.example")),
    ])
    .await;
    let (base_url, auth_rx) = start_nearai_auth_capture_server().await;

    let root = tempfile::tempdir().expect("tempdir");
    let standalone_root = root.path().join("standalone");
    let config_home_dir = root.path().join("config-home");
    std::fs::create_dir_all(&config_home_dir).expect("config home dir");

    let services = crate::factory::build_runtime_substrate(
        crate::deployment::local_filesystem_build_input(
            "runtime-nearai-stored-key-owner",
            standalone_root.clone(),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .await
    .expect("services build for stored key seed");
    ironclaw_operator::LlmKeyStore::new(crate::RuntimeOperatorSecretValueStore::shared(
        services.secret_store(),
    ))
    .put(
        "nearai",
        ironclaw_secrets::SecretMaterial::from("sk-reborn-stored-nearai-key"),
    )
    .await
    .expect("stored key seeded");
    drop(services);

    // Provider selection lives entirely in config.toml (mirrors an
    // onboard-style setup): no env var carries the key, only the
    // encrypted secret store does. `base_url` is overridden to the local
    // capture server so the live reload's re-built provider chain actually
    // calls it.
    std::fs::write(
        RebornHome::resolve_from_env_parts(
            Some(config_home_dir.as_os_str().to_os_string()),
            None,
            None,
        )
        .expect("valid reborn home")
        .config_file_path(),
        format!(
            "[llm.default]\nprovider_id = \"nearai\"\nmodel = \"test-model\"\nbase_url = \"{base_url}\"\n"
        ),
    )
    .expect("write config.toml");
    let boot = RebornBootConfig::new(
        RebornHome::resolve_from_env_parts(
            Some(config_home_dir.as_os_str().to_os_string()),
            None,
            None,
        )
        .expect("valid reborn home"),
        RebornProfile::Standalone,
    );

    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-nearai-stored-key-owner",
            standalone_root,
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_boot_config(boot)
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-nearai-stored-key-tenant".to_string(),
        agent_id: "runtime-nearai-stored-key-agent".to_string(),
        source_binding_id: "runtime-nearai-stored-key-source".to_string(),
        reply_target_binding_id: "runtime-nearai-stored-key-reply".to_string(),
    });

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let conversation = runtime.new_conversation().await.expect("conversation");
    let reply = runtime
        .send_user_message(&conversation, "hi")
        .await
        .expect("message sends");

    assert!(reply.is_successful_final_reply(), "reply: {reply:?}");
    let auth_header = tokio::time::timeout(Duration::from_secs(5), auth_rx)
        .await
        .expect("chat request should be captured")
        .expect("auth header should be sent by capture server");
    assert_eq!(auth_header, "Bearer sk-reborn-stored-nearai-key");

    runtime.shutdown().await.expect("runtime shutdown");
}
