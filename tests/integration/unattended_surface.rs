//! Mutating synthetic capabilities (`t3_*`, Supermemory write, computer use,
//! project create, …) against the people who can and cannot approve them.
//!
//! Synthetic capabilities are appended to a run's surface after the host
//! catalog's approval/effect narrowing, so that narrowing cannot reach them;
//! each one therefore declares whether it is read-only or mutating, and a run
//! with no approval channel at all (suggestion generation: unbound,
//! `require_no_approval`) drops every mutating one. Every run that has a
//! channel — interactive chat, a scheduled trigger fire, a spawned child run —
//! keeps them behind the ordinary approval gate: with auto-approve off the
//! call parks and the user is asked, never a direct engine call.

use crate::reborn_support;
use crate::support;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use ironclaw_assistant::{CREATE_THREAD_COMMAND, SUBMIT_TURN_COMMAND};
use ironclaw_composition::{
    RebornRuntime, RebornRuntimeIdentity, RebornRuntimeInput, TriggerPollerSettings,
    build_reborn_runtime, standalone_runtime_policy,
};
use ironclaw_conversations::{AdapterInstallationId, AdapterKind};
use ironclaw_extension_contracts::external::ExternalActorRef;
use ironclaw_host_api::ids::{AgentId, SecretHandle, TenantId, ThreadId, UserId};
use ironclaw_host_api::resource::ResourceScope;
use ironclaw_host_api::turn::{AcceptedMessageRef, TurnRunId, TurnScope, TurnStatus};
use ironclaw_llm::testing::provider_chain_over;
use ironclaw_llm::{LlmProvider, Role, SessionConfig, create_session_manager};
use ironclaw_loop_host::{HostManagedModelGateway, LlmModelProfilePolicy, LlmProviderModelGateway};
use ironclaw_network::{
    NetworkHttpEgress, NetworkHttpError, NetworkHttpRequest, NetworkHttpResponse,
};
use ironclaw_product_contracts::computer_use::COMPUTER_USE_TOOL_NAME_PREFIX;
use ironclaw_product_contracts::inbound_requests::{
    ProductCreateThreadRequest, ProductSubmitTurnRequest, RebornSuggestionsGenerateRequest,
    RebornSuggestionsListRequest,
};
use ironclaw_product_contracts::notification_inbox::{
    NOTIFICATIONS_VIEW, ProductListNotificationsResponse, ProductNotification,
    ProductNotificationKind,
};
use ironclaw_product_contracts::operator_secrets::{
    OperatorSecretValueStore, OperatorSecretValueStoreError,
};
use ironclaw_product_contracts::product_wire::{
    RebornSubmitTurnResponse, RebornSuggestionGenerationStatus,
};
use ironclaw_product_contracts::profiles::{
    PROFILE_CREATE_COMMAND, PROFILE_THREAD_CREATE_COMMAND, ProfileThreadCreateRequest,
    ProfileThreadCreateResponse, ProfileWriteRequest,
};
use ironclaw_product_contracts::suggestions::{
    SUGGESTIONS_GENERATE_COMMAND, SUGGESTIONS_LIST_VIEW,
};
use ironclaw_product_contracts::surface::{
    BoundProductSurface, ProductSurfaceCaller, ProductSurfaceQueryRequest,
};
use ironclaw_supermemory::{
    SUPERMEMORY_SEARCH_TOOL_NAME, SUPERMEMORY_WRITE_TOOL_NAME, StaticSupermemoryBindingSource,
    Supermemory, SupermemoryBinding, SupermemoryClient, SupermemoryConfig,
};
use ironclaw_triggers::{
    TRIGGER_TRUSTED_ADAPTER_INSTALLATION_ID, TRIGGER_TRUSTED_ADAPTER_KIND,
    TRIGGER_TRUSTED_EXTERNAL_ACTOR_NAMESPACE, TriggerId, TriggerPollerWorkerConfig, TriggerRecord,
    TriggerSchedule, TriggerSourceKind, TriggerState,
};
use ironclaw_turns::{GetRunStateRequest, TurnCoordinator, TurnSpawnTreePort};
use reborn_support::reply::RebornScriptedReply;
use reborn_support::scripted_provider::{SCRIPTED_MODEL_NAME, scripted_trace_llm};
use secrecy::SecretString;
use serde_json::json;
use support::trace_llm::TraceLlm;
use tempfile::{TempDir, tempdir};

type HarnessResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const SUGGESTION_CARD: &str = r#"{"suggestions":[{"title":"Triage inbox","description":"Review and prioritize unread email.","suggested_prompt":"Triage my unread inbox.","icon":"web","sources":["Web Search"]}]}"#;

/// Every mutating synthetic tool name a run can be offered on this fixture
/// (computer-use tools are matched by prefix).
fn mutating_synthetics_in(tools: &BTreeSet<String>) -> Vec<String> {
    tools
        .iter()
        .filter(|name| {
            name.starts_with("t3_")
                || name.starts_with(COMPUTER_USE_TOOL_NAME_PREFIX)
                || name.as_str() == SUPERMEMORY_WRITE_TOOL_NAME
                || name.as_str() == "builtin__project_create"
                || name.as_str() == "builtin__notification_channels_set"
                || name.as_str() == "builtin__skill_activate"
        })
        .cloned()
        .collect()
}

/// An operator key store holding only the Supermemory key, so Supermemory is
/// active for every run (recall/capture are off and the egress refuses).
struct KeyOnlySecrets;

#[async_trait]
impl OperatorSecretValueStore for KeyOnlySecrets {
    async fn put(
        &self,
        _handle: &SecretHandle,
        _value: SecretString,
    ) -> Result<(), OperatorSecretValueStoreError> {
        Ok(())
    }
    async fn contains(
        &self,
        _handle: &SecretHandle,
    ) -> Result<bool, OperatorSecretValueStoreError> {
        Ok(true)
    }
    async fn handles(&self) -> Result<Vec<SecretHandle>, OperatorSecretValueStoreError> {
        Ok(Vec::new())
    }
    async fn read(
        &self,
        _handle: &SecretHandle,
    ) -> Result<Option<SecretString>, OperatorSecretValueStoreError> {
        Ok(Some(SecretString::from("sm-test-key".to_string())))
    }
    async fn delete(&self, _handle: &SecretHandle) -> Result<bool, OperatorSecretValueStoreError> {
        Ok(false)
    }
}

/// Supermemory must never be reached over the network in these tests.
struct RefusingEgress;

#[async_trait]
impl NetworkHttpEgress for RefusingEgress {
    async fn execute(
        &self,
        _request: NetworkHttpRequest,
    ) -> Result<NetworkHttpResponse, NetworkHttpError> {
        Err(NetworkHttpError::Transport {
            reason: "unattended_surface tests make no network calls".to_string(),
            request_bytes: 0,
            response_bytes: 0,
        })
    }
}

fn test_supermemory() -> HarnessResult<Arc<Supermemory>> {
    let config = SupermemoryConfig {
        auto_recall: false,
        auto_capture: false,
        ..SupermemoryConfig::default()
    };
    let client =
        SupermemoryClient::new(&config, Arc::new(RefusingEgress), Arc::new(KeyOnlySecrets))?;
    let binding = SupermemoryBinding {
        primary_container: "unattended-itest".to_string(),
        read_containers: Vec::new(),
        instructions: None,
    };
    Ok(Arc::new(Supermemory::new(
        client,
        Arc::new(StaticSupermemoryBindingSource::new(Some(binding))),
        config,
    )))
}

fn test_computer_use() -> Arc<ironclaw_computer_use::ComputerUse> {
    // The binary is never spawned: listing tools needs no session.
    Arc::new(ironclaw_computer_use::ComputerUse::new(
        ironclaw_computer_use::ComputerUseConfig::new("/nonexistent/computer-use-engine"),
    ))
}

struct Fixture {
    root: TempDir,
    _session_root: TempDir,
    tenant_id: TenantId,
    agent_id: AgentId,
    user_id: UserId,
    scripted_llm: Arc<TraceLlm>,
    gateway: Arc<dyn HostManagedModelGateway>,
}

impl Fixture {
    async fn new(script: impl IntoIterator<Item = RebornScriptedReply>) -> HarnessResult<Self> {
        let session_root = tempdir()?;
        let scripted_llm: Arc<TraceLlm> = Arc::new(scripted_trace_llm(script));
        let raw: Arc<dyn LlmProvider> = scripted_llm.clone();
        let session = create_session_manager(SessionConfig {
            session_path: session_root.path().join("unattended.session.json"),
            ..SessionConfig::default()
        })
        .await;
        let provider = provider_chain_over(
            raw,
            &ironclaw_llm::testing::nearai_test_config(SCRIPTED_MODEL_NAME),
            session,
        )
        .await?;
        let model_profile_id = ironclaw_loop_contracts::ModelProfileId::new("interactive_model")
            .map_err(|reason| format!("invalid model profile id: {reason}"))?;
        let gateway: Arc<dyn HostManagedModelGateway> = Arc::new(LlmProviderModelGateway::new(
            provider,
            LlmModelProfilePolicy::new().allow_model_profile(model_profile_id, None),
        ));
        Ok(Self {
            root: tempdir()?,
            _session_root: session_root,
            tenant_id: TenantId::new("unattended-itest-tenant")?,
            agent_id: AgentId::new("unattended-itest-agent")?,
            user_id: UserId::new("unattended-itest-user")?,
            scripted_llm,
            gateway,
        })
    }

    /// The production-composed runtime with Supermemory and computer use on
    /// and progressive disclosure off, so the tools the model receives are
    /// the permitted set rather than a token-budgeted preview of it.
    async fn runtime(&self, with_trigger_poller: bool) -> HarnessResult<Box<RebornRuntime>> {
        let build = ironclaw_composition::local_filesystem_build_input(
            self.user_id.as_str(),
            self.root.path().join("local-dev"),
        )
        .with_local_runtime_identity(self.tenant_id.clone(), self.agent_id.clone())
        .with_runtime_policy(standalone_runtime_policy()?)
        .with_bundled_first_party_for_test();
        let mut input = RebornRuntimeInput::from_build_input(build)
            .with_identity(RebornRuntimeIdentity {
                tenant_id: self.tenant_id.as_str().to_string(),
                agent_id: self.agent_id.as_str().to_string(),
                source_binding_id: "unattended-itest-source".to_string(),
                reply_target_binding_id: "unattended-itest-reply".to_string(),
            })
            .with_model_gateway_override(Arc::clone(&self.gateway))
            .with_tool_disclosure(ironclaw_loop_host::ToolDisclosureMode::Off)
            .with_supermemory_for_test(test_supermemory()?)
            .with_computer_use_for_test(test_computer_use());
        if with_trigger_poller {
            input = input.with_trigger_poller_settings(
                TriggerPollerSettings::enabled_with_tenant_scoped_authorizer_for_test()
                    .with_worker_config(
                        TriggerPollerWorkerConfig::default()
                            .set_poll_interval(Duration::from_millis(20)),
                    ),
            );
        }
        Ok(Box::new(build_reborn_runtime(input).await?))
    }

    fn surface(&self, runtime: &RebornRuntime) -> HarnessResult<BoundProductSurface> {
        Ok(BoundProductSurface::new(
            runtime.product_surface(None)?,
            ProductSurfaceCaller::new(
                self.tenant_id.clone(),
                self.user_id.clone(),
                Some(self.agent_id.clone()),
                None,
            ),
        ))
    }

    fn settings_scope(&self) -> ResourceScope {
        ResourceScope {
            tenant_id: self.tenant_id.clone(),
            user_id: self.user_id.clone(),
            agent_id: None,
            project_id: None,
            mission_id: None,
            thread_id: None,
            invocation_id: ironclaw_host_api::ids::InvocationId::new(),
        }
    }

    async fn set_auto_approve(&self, runtime: &RebornRuntime, enabled: bool) -> HarnessResult<()> {
        let store = runtime
            .standalone_auto_approve_settings_for_test()
            .ok_or("runtime exposes no auto-approve settings store")?;
        store
            .set(ironclaw_approvals::AutoApproveSettingInput {
                updated_by: ironclaw_host_api::scope::Principal::User(self.user_id.clone()),
                scope: self.settings_scope(),
                enabled,
            })
            .await
            .map_err(|error| format!("auto-approve write failed: {error}"))?;
        Ok(())
    }

    /// Set a per-tool permission override (`disabled`, `ask_each_time`, …).
    async fn set_tool_override(
        &self,
        runtime: &RebornRuntime,
        capability_id: &str,
        state: ironclaw_approvals::ToolPermissionOverride,
    ) -> HarnessResult<()> {
        runtime
            .standalone_tool_permission_overrides_for_test()
            .ok_or("runtime exposes no tool permission overrides")?
            .set(ironclaw_approvals::CapabilityPermissionOverrideInput {
                scope: self.settings_scope(),
                capability_id: ironclaw_host_api::ids::CapabilityId::new(capability_id)?,
                state,
                updated_by: ironclaw_host_api::scope::Principal::User(self.user_id.clone()),
            })
            .await?;
        Ok(())
    }

    /// The tool result the model was given for `tool` on any call.
    fn tool_result(&self, tool: &str) -> HarnessResult<String> {
        self.scripted_llm
            .captured_requests()
            .into_iter()
            .flatten()
            .find(|message| message.role == Role::Tool && message.name.as_deref() == Some(tool))
            .map(|message| message.content)
            .ok_or_else(|| format!("the model never got a result for {tool}").into())
    }

    fn thread_scope(&self, thread_id: ThreadId) -> TurnScope {
        TurnScope::new_with_owner(
            self.tenant_id.clone(),
            Some(self.agent_id.clone()),
            None,
            thread_id,
            Some(self.user_id.clone()),
        )
    }

    /// Tool names the model received on its `index`-th call.
    fn tools_at(&self, index: usize) -> HarnessResult<BTreeSet<String>> {
        let captured = self.scripted_llm.captured_tool_definitions();
        let call = captured
            .get(index)
            .ok_or_else(|| format!("no model call #{index}; captured {}", captured.len()))?;
        Ok(call.iter().map(|tool| tool.name.clone()).collect())
    }

    async fn submit_chat(
        &self,
        runtime: &RebornRuntime,
        text: &str,
    ) -> HarnessResult<(TurnScope, TurnRunId)> {
        let surface = self.surface(runtime)?;
        let thread = CREATE_THREAD_COMMAND
            .invoke_on(
                &surface,
                ProductCreateThreadRequest {
                    client_action_id: Some(uuid::Uuid::new_v4().to_string()),
                    ..ProductCreateThreadRequest::default()
                },
                ironclaw_host_api::ids::ActivityId::new(),
            )
            .await?
            .thread;
        self.submit_chat_in(runtime, thread.thread_id.as_str(), text)
            .await
    }

    async fn submit_chat_in(
        &self,
        runtime: &RebornRuntime,
        thread_id: &str,
        text: &str,
    ) -> HarnessResult<(TurnScope, TurnRunId)> {
        let surface = self.surface(runtime)?;
        let response = SUBMIT_TURN_COMMAND
            .invoke_on(
                &surface,
                ProductSubmitTurnRequest {
                    client_action_id: Some(uuid::Uuid::new_v4().to_string()),
                    thread_id: Some(thread_id.to_string()),
                    content: Some(text.to_string()),
                    ..ProductSubmitTurnRequest::default()
                },
                ironclaw_host_api::ids::ActivityId::new(),
            )
            .await?;
        let RebornSubmitTurnResponse::Submitted {
            thread_id, run_id, ..
        } = response
        else {
            return Err(format!("chat turn was not submitted: {response:?}").into());
        };
        Ok((self.thread_scope(thread_id), run_id))
    }
}

async fn wait_for_status(
    coordinator: &Arc<dyn TurnCoordinator>,
    scope: TurnScope,
    run_id: TurnRunId,
    expected: TurnStatus,
) -> HarnessResult<ironclaw_turns::TurnRunState> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let state = coordinator
            .get_run_state(GetRunStateRequest {
                scope: scope.clone(),
                run_id,
            })
            .await?;
        if state.status == expected {
            return Ok(state);
        }
        if state.status.is_terminal() || std::time::Instant::now() > deadline {
            return Err(format!(
                "run {run_id} reached {:?} (failure {:?}), expected {expected:?}",
                state.status, state.failure
            )
            .into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The approval notification the owner's Inbox receives for a parked run.
async fn wait_for_approval_notification(
    fixture: &Fixture,
    runtime: &RebornRuntime,
) -> HarnessResult<ProductNotification> {
    let surface = runtime.product_surface(None)?;
    let caller = ProductSurfaceCaller::new(
        fixture.tenant_id.clone(),
        fixture.user_id.clone(),
        Some(fixture.agent_id.clone()),
        None,
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let page = surface
            .query(
                caller.clone(),
                ProductSurfaceQueryRequest {
                    view_id: NOTIFICATIONS_VIEW.id.to_string(),
                    input: json!({ "limit": 20 }),
                    cursor: None,
                    limit: None,
                },
            )
            .await?;
        if let Some(payload) = page.items.into_iter().next() {
            let response: ProductListNotificationsResponse = serde_json::from_value(payload)?;
            if let Some(notification) = response
                .notifications
                .into_iter()
                .find(|notification| notification.kind == ProductNotificationKind::ApprovalRequired)
            {
                return Ok(notification);
            }
        }
        if std::time::Instant::now() > deadline {
            return Err("no approval notification reached the owner's Inbox".into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The status of the run an approval notification names.
async fn blocked_run(
    fixture: &Fixture,
    runtime: &RebornRuntime,
    notification: &ProductNotification,
) -> HarnessResult<TurnStatus> {
    let thread_id = notification
        .thread_id
        .as_deref()
        .ok_or("the approval notification names its thread")?;
    let run_id = notification
        .turn_run_id
        .as_deref()
        .ok_or("the approval notification names its run")?;
    let state = runtime
        .product_turn_coordinator_for_test()
        .get_run_state(GetRunStateRequest {
            scope: fixture.thread_scope(ThreadId::new(thread_id)?),
            run_id: TurnRunId::parse(run_id)?,
        })
        .await?;
    Ok(state.status)
}

async fn generate_suggestions(fixture: &Fixture, runtime: &RebornRuntime) -> HarnessResult<()> {
    let surface = fixture.surface(runtime)?;
    SUGGESTIONS_GENERATE_COMMAND
        .invoke_on(
            &surface,
            RebornSuggestionsGenerateRequest {
                client_action_id: format!("unattended-{}", uuid::Uuid::new_v4()),
            },
            ironclaw_host_api::ids::ActivityId::new(),
        )
        .await?;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let response = SUGGESTIONS_LIST_VIEW
            .query_on(&surface, RebornSuggestionsListRequest::default(), None)
            .await?;
        match response.status {
            RebornSuggestionGenerationStatus::Ready => return Ok(()),
            RebornSuggestionGenerationStatus::Failed => {
                return Err("suggestion generation failed".into());
            }
            _ if std::time::Instant::now() > deadline => {
                return Err("suggestion generation did not become ready".into());
            }
            _ => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
}

/// Supermemory, computer use and the `t3_*` tools are all wired (an attended
/// chat run is offered them), but suggestion generation — a run nobody can
/// approve for — never is, even with auto-approve ON: their handlers ask for
/// approval at call time, which would only park it. Read-only synthetics
/// (`supermemory_search`, `history_search`) stay.
#[tokio::test(flavor = "multi_thread")]
async fn optional_mutating_synthetics_never_reach_an_unattended_run() -> HarnessResult<()> {
    let fixture = Fixture::new([
        RebornScriptedReply::text("I recommend triaging the inbox."),
        RebornScriptedReply::text(SUGGESTION_CARD),
        RebornScriptedReply::text("Hello."),
    ])
    .await?;
    let runtime = fixture.runtime(false).await?;

    generate_suggestions(&fixture, &runtime).await?;
    let unattended = fixture.tools_at(0)?;

    let (scope, run_id) = fixture.submit_chat(&runtime, "hello").await?;
    wait_for_status(
        &runtime.product_turn_coordinator_for_test(),
        scope,
        run_id,
        TurnStatus::Completed,
    )
    .await?;
    let attended = fixture.tools_at(2)?;

    for wired in [SUPERMEMORY_WRITE_TOOL_NAME, "t3_delegate"] {
        assert!(
            attended.contains(wired),
            "control: an attended chat run must be offered {wired}; got {attended:?}"
        );
    }
    assert!(
        attended
            .iter()
            .any(|name| name.starts_with(COMPUTER_USE_TOOL_NAME_PREFIX)),
        "control: an attended chat run must be offered computer use; got {attended:?}"
    );
    assert_eq!(
        mutating_synthetics_in(&unattended),
        Vec::<String>::new(),
        "an unattended run must not be offered any mutating synthetic capability"
    );
    for read_only in [SUPERMEMORY_SEARCH_TOOL_NAME, "history_search"] {
        assert!(
            unattended.contains(read_only),
            "read-only synthetic {read_only} must stay on the unattended surface; \
             got {unattended:?}"
        );
    }
    runtime.shutdown().await?;
    Ok(())
}

/// The model calls `t3_delegate` anyway in an unattended run: it is refused
/// before dispatch (outside the surface), never executed, and the run still
/// finishes.
#[tokio::test(flavor = "multi_thread")]
async fn unattended_run_refuses_a_t3_delegate_call() -> HarnessResult<()> {
    let fixture = Fixture::new([
        RebornScriptedReply::tool_call(
            "t3_delegate",
            json!({"task": "unattended-delegate-must-not-run"}),
        ),
        RebornScriptedReply::text("I recommend triaging the inbox."),
        RebornScriptedReply::text(SUGGESTION_CARD),
    ])
    .await?;
    let runtime = fixture.runtime(false).await?;
    generate_suggestions(&fixture, &runtime).await?;

    let calls = fixture.scripted_llm.captured_requests();
    let refusal = calls
        .get(1)
        .and_then(|messages| {
            messages.iter().find(|message| {
                message.role == Role::Tool && message.name.as_deref() == Some("t3_delegate")
            })
        })
        .ok_or_else(|| format!("no tool result for the refused t3_delegate call; got {calls:?}"))?;
    assert!(
        refusal
            .content
            .contains("None of this response's tool calls were executed"),
        "t3_delegate must be refused before dispatch in an unattended run; got: {}",
        refusal.content
    );
    assert!(
        !refusal.content.contains("taskId"),
        "no delegated task may exist; got: {}",
        refusal.content
    );
    runtime.shutdown().await?;
    Ok(())
}

/// A scheduled trigger fire has an approval channel: with auto-approve off,
/// its `t3_delegate` call parks on the ordinary approval gate and the user is
/// notified, exactly as for shell or write_file — the engine is not reached.
#[tokio::test(flavor = "multi_thread")]
async fn scheduled_trigger_t3_call_parks_on_an_approval_and_notifies() -> HarnessResult<()> {
    let fixture = Fixture::new([RebornScriptedReply::tool_call(
        "t3_delegate",
        json!({"task": "scheduled-delegate-awaits-approval"}),
    )])
    .await?;
    let runtime = fixture.runtime(true).await?;
    fixture.set_auto_approve(&runtime, false).await?;

    runtime
        .trigger_conversation_pairing()
        .ok_or("poller-enabled runtime exposes conversation pairing")?
        .pair_external_actor(
            fixture.tenant_id.clone(),
            AdapterKind::new(TRIGGER_TRUSTED_ADAPTER_KIND)?,
            AdapterInstallationId::new(TRIGGER_TRUSTED_ADAPTER_INSTALLATION_ID)?,
            ExternalActorRef::new(
                TRIGGER_TRUSTED_EXTERNAL_ACTOR_NAMESPACE,
                fixture.user_id.as_str(),
                None::<String>,
            )?,
            fixture.user_id.clone(),
        )
        .await?;
    let due = Utc::now() - chrono::Duration::seconds(120);
    let trigger_id = TriggerId::new();
    let repository = runtime.trigger_repository();
    repository
        .upsert_trigger(TriggerRecord {
            trigger_id,
            tenant_id: fixture.tenant_id.clone(),
            creator_user_id: fixture.user_id.clone(),
            agent_id: Some(fixture.agent_id.clone()),
            project_id: None,
            name: "unattended-itest".to_string(),
            source: TriggerSourceKind::Schedule,
            schedule: TriggerSchedule::once(due, "UTC")?,
            prompt: "Check for anything new.".to_string(),
            execution_spec: None,
            delivery_target: None,
            state: TriggerState::Scheduled,
            next_run_at: due,
            last_run_at: None,
            last_fired_slot: None,
            last_status: None,
            active_fire_slot: None,
            active_run_ref: None,
            created_at: Utc::now(),
        })
        .await?;
    let notification = wait_for_approval_notification(&fixture, &runtime).await?;
    let tools = fixture.tools_at(0)?;
    let parked = blocked_run(&fixture, &runtime, &notification).await?;
    runtime.shutdown().await?;

    assert!(
        tools.contains("t3_delegate"),
        "a scheduled run keeps its approval channel, so it is offered t3_delegate; \
         got {tools:?}"
    );
    assert_eq!(parked, TurnStatus::BlockedApproval);
    assert_eq!(
        fixture.scripted_llm.captured_requests().len(),
        1,
        "the fire stops at the gate: no delegation result reaches the model"
    );
    Ok(())
}

/// A spawned child run's approval pauses reach its owner: with auto-approve
/// off, its `t3_delegate` call parks on the ordinary approval gate and the
/// user is notified — the engine is not reached.
#[tokio::test(flavor = "multi_thread")]
async fn child_run_t3_call_parks_on_an_approval_and_notifies() -> HarnessResult<()> {
    let fixture = Fixture::new([
        RebornScriptedReply::text("parent done"),
        RebornScriptedReply::tool_call(
            "t3_delegate",
            json!({"task": "child-delegate-awaits-approval"}),
        ),
    ])
    .await?;
    let runtime = fixture.runtime(false).await?;
    fixture.set_auto_approve(&runtime, false).await?;
    let coordinator = runtime.product_turn_coordinator_for_test();

    let (parent_scope, parent_run_id) = fixture.submit_chat(&runtime, "start").await?;
    wait_for_status(
        &coordinator,
        parent_scope.clone(),
        parent_run_id,
        TurnStatus::Completed,
    )
    .await?;
    let parent_tools = fixture.tools_at(0)?;
    assert!(
        parent_tools.contains("t3_delegate"),
        "control: the attended parent is offered t3_delegate; got {parent_tools:?}"
    );

    let child_run_id = TurnRunId::new();
    let child_thread_id = ThreadId::new(format!("subagent-{}", child_run_id.as_uuid().simple()))?;
    let child_scope = fixture.thread_scope(child_thread_id.clone());
    let thread_scope = ironclaw_threads::ThreadScope {
        tenant_id: fixture.tenant_id.clone(),
        agent_id: fixture.agent_id.clone(),
        project_id: None,
        owner_user_id: Some(fixture.user_id.clone()),
        mission_id: None,
    };
    let threads = runtime.session_thread_service();
    threads
        .ensure_thread(ironclaw_threads::EnsureThreadRequest {
            scope: thread_scope.clone(),
            thread_id: Some(child_thread_id.clone()),
            created_by_actor_id: fixture.user_id.as_str().to_string(),
            title: None,
            metadata_json: None,
        })
        .await?;
    let message = threads
        .accept_inbound_message(ironclaw_threads::AcceptInboundMessageRequest {
            scope: thread_scope,
            thread_id: child_thread_id,
            actor_id: fixture.user_id.as_str().to_string(),
            source_binding_id: Some("unattended-itest-child".to_string()),
            reply_target_binding_id: None,
            external_event_id: Some("child".to_string()),
            content: ironclaw_threads::MessageContent::text("do the child task"),
        })
        .await?;
    runtime
        .turn_spawn_tree_for_test()
        .submit_child_run(ironclaw_turns::SubmitChildRunRequest {
            parent_scope,
            parent_run_id,
            child_scope: child_scope.clone(),
            actor: ironclaw_turns::TurnActor::new(fixture.user_id.clone()),
            accepted_message_ref: AcceptedMessageRef::new(format!("msg:{}", message.message_id))?,
            requested_run_profile: None,
            output_contract: None,
            idempotency_key: ironclaw_turns::IdempotencyKey::new("unattended-itest-child")?,
            received_at: Utc::now(),
            requested_run_id: Some(child_run_id),
            process_dependency: None,
            process_input: None,
            spawn_tree_descendant_cap: 4,
        })
        .await?;
    let child = wait_for_status(
        &coordinator,
        child_scope,
        child_run_id,
        TurnStatus::BlockedApproval,
    )
    .await?;
    let gate = child.gate_ref.ok_or("a blocked approval names its gate")?;
    let notification = wait_for_approval_notification(&fixture, &runtime).await?;
    runtime.shutdown().await?;

    assert!(
        gate.as_str().starts_with("gate:approval-"),
        "the child's t3_delegate must park on an approval gate; got {gate:?}"
    );
    assert_eq!(
        notification.turn_run_id.as_deref(),
        Some(child_run_id.to_string().as_str()),
        "the owner is notified about the child's pending approval"
    );
    assert_eq!(
        fixture.scripted_llm.captured_requests().len(),
        2,
        "the child stops at the gate: no delegation result reaches the model"
    );
    Ok(())
}

/// Attended chat with auto-approve off: `t3_delegate` goes through the normal
/// approval gate — the run parks on an approval, and nothing is delegated
/// before the user answers.
#[tokio::test(flavor = "multi_thread")]
async fn attended_t3_delegate_waits_for_approval_when_auto_approve_is_off() -> HarnessResult<()> {
    let fixture = Fixture::new([RebornScriptedReply::tool_call(
        "t3_delegate",
        json!({"task": "attended-delegate-awaits-approval"}),
    )])
    .await?;
    let runtime = fixture.runtime(false).await?;
    fixture.set_auto_approve(&runtime, false).await?;

    let (scope, run_id) = fixture.submit_chat(&runtime, "delegate the report").await?;
    let state = wait_for_status(
        &runtime.product_turn_coordinator_for_test(),
        scope,
        run_id,
        TurnStatus::BlockedApproval,
    )
    .await?;
    let gate = state.gate_ref.ok_or("a blocked approval names its gate")?;
    assert!(
        gate.as_str().starts_with("gate:approval-"),
        "t3_delegate must park on an approval gate; got {gate:?}"
    );
    assert_eq!(
        fixture.scripted_llm.captured_requests().len(),
        1,
        "the run must stop at the gate: no tool result reaches the model before approval"
    );
    runtime.shutdown().await?;
    Ok(())
}

/// Finding 1: `project_create` and `skill_activate` are mutating synthetics
/// whose handlers used to run directly. With auto-approve off, each call parks
/// on the approval gate at the synthetic dispatch boundary — nothing runs.
#[tokio::test(flavor = "multi_thread")]
async fn project_create_and_skill_activate_wait_for_approval_when_auto_approve_is_off()
-> HarnessResult<()> {
    let fixture = Fixture::new([
        RebornScriptedReply::tool_call("builtin.project_create", json!({"name": "Gated Project"})),
        RebornScriptedReply::tool_call("builtin.skill_activate", json!({"skill": "triage"})),
    ])
    .await?;
    let runtime = fixture.runtime(false).await?;
    fixture.set_auto_approve(&runtime, false).await?;
    let coordinator = runtime.product_turn_coordinator_for_test();

    for (text, tool) in [
        ("make a project", "builtin__project_create"),
        ("activate triage", "builtin__skill_activate"),
    ] {
        let (scope, run_id) = fixture.submit_chat(&runtime, text).await?;
        let state = wait_for_status(&coordinator, scope, run_id, TurnStatus::BlockedApproval)
            .await
            .map_err(|error| format!("{tool}: {error}"))?;
        let gate = state.gate_ref.ok_or("a blocked approval names its gate")?;
        assert!(
            gate.as_str().starts_with("gate:approval-"),
            "{tool} must park on an approval gate; got {gate:?}"
        );
    }
    let projects = projects_of(&fixture, &runtime).await?;
    runtime.shutdown().await?;
    assert!(
        projects.is_empty(),
        "no project before approval; got {projects:?}"
    );
    Ok(())
}

/// Finding 1: a per-tool `disabled` setting refuses `project_create` and
/// `skill_activate` even with auto-approve ON — the model is told, and no
/// project is created.
#[tokio::test(flavor = "multi_thread")]
async fn disabled_project_create_and_skill_activate_are_refused_without_side_effect()
-> HarnessResult<()> {
    let fixture = Fixture::new([
        RebornScriptedReply::tool_call(
            "builtin.project_create",
            json!({"name": "Disabled Project"}),
        ),
        RebornScriptedReply::tool_call("builtin.skill_activate", json!({"skill": "triage"})),
        RebornScriptedReply::text("Both tools are disabled."),
    ])
    .await?;
    let runtime = fixture.runtime(false).await?;
    for capability in ["builtin.project_create", "builtin.skill_activate"] {
        fixture
            .set_tool_override(
                &runtime,
                capability,
                ironclaw_approvals::ToolPermissionOverride::Disabled,
            )
            .await?;
    }

    let (scope, run_id) = fixture.submit_chat(&runtime, "make a project").await?;
    let coordinator = runtime.product_turn_coordinator_for_test();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while fixture.scripted_llm.captured_requests().len() < 3 {
        let state = coordinator
            .get_run_state(GetRunStateRequest {
                scope: scope.clone(),
                run_id,
            })
            .await?;
        if state.status.is_terminal() || std::time::Instant::now() > deadline {
            return Err(format!("run reached {:?} before both refusals", state.status).into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let projects = projects_of(&fixture, &runtime).await?;
    let project_refusal = fixture.tool_result("builtin__project_create")?;
    let skill_refusal = fixture.tool_result("builtin__skill_activate")?;
    runtime.shutdown().await?;

    for (tool, refusal) in [
        ("project_create", project_refusal),
        ("skill_activate", skill_refusal),
    ] {
        assert!(
            refusal.contains("disabled by tool approval settings"),
            "a disabled {tool} must be refused, not run; got: {refusal}"
        );
    }
    assert!(
        projects.is_empty(),
        "a refused project_create creates nothing; got {projects:?}"
    );
    Ok(())
}

/// Finding 2: the gate asks with the thread's own scope, so a thread whose
/// profile is "Ask first" parks a `t3_delegate` call even though global
/// auto-approve is ON.
#[tokio::test(flavor = "multi_thread")]
async fn an_ask_first_thread_profile_wins_over_global_auto_approve() -> HarnessResult<()> {
    let fixture = Fixture::new([RebornScriptedReply::tool_call(
        "t3_delegate",
        json!({"task": "ask-first-delegate-awaits-approval"}),
    )])
    .await?;
    let runtime = fixture.runtime(false).await?;
    let surface = fixture.surface(&runtime)?;
    PROFILE_CREATE_COMMAND
        .invoke_on(
            &surface,
            serde_json::from_value::<ProfileWriteRequest>(json!({
                "profile_id": "ask-first",
                "display_name": "Ask first",
                "approval_mode": "ask_first",
            }))?,
            ironclaw_host_api::ids::ActivityId::new(),
        )
        .await?;
    let created: ProfileThreadCreateResponse = serde_json::from_value(
        PROFILE_THREAD_CREATE_COMMAND
            .invoke_on(
                &surface,
                ProfileThreadCreateRequest {
                    profile_id: "ask-first".to_string(),
                    client_action_id: Some(uuid::Uuid::new_v4().to_string()),
                },
                ironclaw_host_api::ids::ActivityId::new(),
            )
            .await?,
    )?;

    let (scope, run_id) = fixture
        .submit_chat_in(&runtime, &created.thread_id, "delegate the report")
        .await?;
    let state = wait_for_status(
        &runtime.product_turn_coordinator_for_test(),
        scope,
        run_id,
        TurnStatus::BlockedApproval,
    )
    .await?;
    runtime.shutdown().await?;
    assert!(
        state
            .gate_ref
            .is_some_and(|gate| gate.as_str().starts_with("gate:approval-")),
        "an Ask-first thread must park t3_delegate on an approval gate"
    );
    Ok(())
}

async fn projects_of(fixture: &Fixture, runtime: &RebornRuntime) -> HarnessResult<Vec<String>> {
    let projects = runtime
        .standalone_project_service_for_test()
        .ok_or("runtime exposes no project service")?
        .list_projects(
            ironclaw_product_contracts::workspace_views::ProjectCaller {
                tenant_id: fixture.tenant_id.clone(),
                user_id: fixture.user_id.clone(),
            },
            ironclaw_product_contracts::workspace_views::RebornListProjectsRequest { limit: None },
        )
        .await?;
    Ok(projects
        .projects
        .into_iter()
        .map(|project| project.name)
        .filter(|name| name.contains("Project"))
        .collect())
}
