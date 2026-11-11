//! Assembled Reborn runtime: substrate + drivers + worker, started as one.
//!
//! This module is the "later slice" the crate-level docstring promises:
//! product-level wiring on top of the substrate facades exposed by
//! `build_runtime_substrate`. It is the **only** place in the workspace where
//! `ironclaw_turn_runner` (drivers, host factory, model gateway bridge),
//! `ironclaw_threads` (session thread service), and `ironclaw_llm` are
//! composed into a running agent.
//!
//! Downstream callers (the CLI, future channel adapters, e2e harnesses) reach
//! this assembly only through:
//!
//! - [`build_reborn_runtime`] — construct + start the runtime
//! - [`RebornRuntime`] — task-level handle (`new_conversation`,
//!   `send_user_message`, `shutdown`)
//!
//! They never name the underlying `TurnCoordinator`, `SessionThreadService`,
//! `LoopExitApplier`, `HostManagedModelGateway`, etc. directly. That is the
//! property that satisfies the "narrow Reborn public surface" requirement
//! pinned by `crates/app/ironclaw_architecture_tests/tests/reborn_dependency_boundaries.rs`.
//!
//! This file keeps the public types, the [`RebornRuntime`] struct, and the
//! `build_*` entrypoints. The rest lives in submodules: `assembly`
//! (`assemble_runtime`) and `assembly_support` (its helpers), `store_parts`,
//! `startup_guards`, `skill_context`; the handle's methods are split across
//! `handles` (production accessors and `shutdown`), `turns` (submission),
//! `turn_settlement` (waiting and cancellation), and `harness_accessors`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use ironclaw_auth::RebornProductAuthServices;
use ironclaw_conversations::RebornFilesystemConversationServices;
use thiserror::Error;
use tokio::sync::{Mutex, OwnedMutexGuard};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use ironclaw_assistant::{
    ApprovalBlockedTurnRun, ApprovalInteractionScope, ApprovalInteractionService,
    ApprovalNotificationBackfillProcessCommitObserver, ApprovalResolverPort,
    ApprovalTurnRunLocator, AuthInteractionService, AuthNotificationBackfillProcessCommitObserver,
    DefaultApprovalInteractionService, DefaultAuthInteractionService,
    OutboundPreferencesProductService, PersistentApprovalGranteeResolver,
    ResourceBlockBackfillProcessCommitObserver, RunOutcomeProcessCommitObserver,
    RunStateApprovalInteractionReadModel, SuggestionsProcessCommitObserver,
};
use ironclaw_event_log::{
    DurableAuditLog, DurableEventLog, EventError, NonBlockingEventSink, RuntimeEvent,
};
use ironclaw_extension_registry::{ExtensionRegistry, SharedExtensionRegistry};
use ironclaw_filesystem::{CompositeRootFilesystem, ScopedFilesystem};
use ironclaw_host_api::turn::{
    AcceptedMessageRef, EventCursor, IdempotencyKey, LoopGateRef, ReplyTargetBindingRef,
    SanitizedCancelReason, SourceBindingRef, TurnActor, TurnId, TurnRunId, TurnScope, TurnStatus,
};
use ironclaw_host_api::{
    audit::{ActionResultSummary, ActionSummary, AuditEnvelope, AuditStage, DecisionSummary},
    capability::EffectKind,
    capability_surface::CapabilitySurfacePolicy,
    http::RuntimeHttpEgress,
    ids::{
        AgentId, ApprovalRequestId, AuditEventId, CapabilityId, CorrelationId, ExtensionId,
        InvocationId, TenantId, ThreadId, UserId,
    },
    mount::MountView,
    process::RuntimeProcessError,
    resource::ResourceScope,
    scope::Principal,
};
use ironclaw_loop_contracts::{LoopHostMilestoneSink, LoopRunContext, RunProfileResolutionRequest};
use ironclaw_loop_host::ToolDisclosureMode;
use ironclaw_loop_host::{
    AwaitEdgeSettler, AwaitEdgeWriter, CapabilityResolveError, CapabilitySurfaceProfileResolver,
    EmptyUserProfileSource, FilesystemSkillBundleSource, HostIdentityContextSource,
    HostSkillContextSource, HostUserProfileSource, JsonSpawnSubagentInputCodec,
    LoopCapabilityInputResolver, LoopCapabilityPortFactory, LoopCapabilityResultWriter,
    ModelGatewayBackedSystemInferencePort,
};
use ironclaw_loop_host::{
    FirstPartySkillsExtension, FirstPartySkillsExtensionHandles, SelectableSkillContextSource,
    SkillActivationSelectorConfig, SkillExecutionAdapter, SkillInjectionMode,
};
use ironclaw_observability::live_latency_started_at;
use ironclaw_processes::{
    ProcessConcurrencyClass, ProcessConcurrencyLimits, ProcessGateQuery, ProcessGateQuerySource,
    ProcessLifecycleLookupSource, ProcessSuspensionKind,
};
use ironclaw_product_contracts::lifecycle_service::LifecycleProductSurfaceContext;
use ironclaw_product_contracts::operator_llm::{
    ActiveModelReader, LlmConfigService, LlmConfigServiceError,
};
use ironclaw_product_contracts::projection::ProjectionStream;
use ironclaw_product_contracts::surface::{ProductSurface, ProductSurfaceCaller};
use ironclaw_threads::{
    AcceptInboundMessageRequest, EnsureThreadRequest, InboundMessageReplayMetadata, MessageContent,
    MessageKind, MessageStatus, SessionThreadService, ThreadHistoryRequest, ThreadScope,
};
use ironclaw_turn_runner::loop_exit_applier::{
    ApprovalGateEvidenceStore, AwaitDependentRunEvidenceStore, ThreadCheckpointLoopExitEvidencePort,
};
use ironclaw_turn_runner::milestone_events::{
    DurableLoopHostMilestoneScope, DurableLoopHostMilestoneSink,
};
use ironclaw_turn_runner::runtime::{
    DefaultPlannedRuntimeBuildError, DefaultPlannedRuntimeConfig, DefaultPlannedRuntimeConfigError,
    DefaultPlannedRuntimeParts, ProcessRuntimeSystem, build_default_planned_runtime,
};
use ironclaw_turn_runner::subagent::await_edge::{
    boot_recovery::ScopeRecoveryDriver, resolver::AwaitEdgeResolver, store::AwaitEdgeStore,
};
use ironclaw_turn_runner::subagent::flavors::StaticSubagentDefinitionResolver;
use ironclaw_turns::{
    AgentTurnProcessRuntime, AgentTurnSpawnTreeRuntimePort, CancelRunRequest, CancelRunResponse,
    GetRunStateRequest, SubmitTurnRequest, SubmitTurnResponse, TurnCoordinator, TurnError,
    TurnEventProjectionSource, TurnRunState, TurnRunWake,
};

#[cfg(any(test, feature = "test-support"))]
use ironclaw_assistant::RebornOutboundDeliveryTargetId;
use ironclaw_host_runtime::HostRuntime;
use ironclaw_outbound::CommunicationPreferenceRepository;
#[cfg(any(test, feature = "test-support"))]
use ironclaw_outbound::OutboundDeliveryTargetRegistrationOutcome;
#[cfg(any(test, feature = "test-support"))]
use ironclaw_outbound::OutboundError;
use ironclaw_turns::ExternalToolCatalog;

use self::latency::{trace_runtime_latency_error, trace_runtime_latency_ok};
use self::runtime_turn_scheduler::RuntimeTurnScheduler;
use crate::builtin_capability_policy::BuiltinCapabilityPolicy;
use crate::deployment::{DeploymentConfig, TrafficPolicy};
use crate::factory::{
    ComposedAutoApproveSettingStore, ComposedPersistentApprovalPolicyStore,
    ComposedToolPermissionOverrideStore, builtin_extension_registry,
    filesystem_reborn_identity_store,
};
#[cfg(test)]
use crate::model_gateway_assembly::wrap_swappable_gateway;
use crate::model_gateway_assembly::{
    RebornLlmReloadParts, build_production_model_gateway, build_skill_learning_provider,
};
use crate::root::default_system_prompt::{
    DefaultSystemPromptIdentitySource, SystemPromptProtocols,
};
use ironclaw_assistant::projection::{RebornProjectionServices, build_reborn_projection_services};
use ironclaw_assistant::{
    OUTBOUND_NOTIFICATION_CHANNELS_SET_CAPABILITY_ID, RebornOutboundPreferencesService,
    outbound_delivery_synthetic_provider,
};
use ironclaw_assistant::{current_turn_gate_runs, first_turn_run_for_gate};
pub(crate) use ironclaw_auth::product_prompt::blocked_auth_flow_canceller;
pub use ironclaw_auth::product_prompt::product_auth_challenge_provider;
use ironclaw_extension_host::AdminConfigurationCatalogUse;
#[cfg(any(test, feature = "test-support"))]
use ironclaw_extension_host::channel_pairing::ChannelPairingConsumeOutcome;
use ironclaw_extension_host::channel_pairing::ChannelPairingRegistry;
use ironclaw_extension_host::extension_lifecycle::RebornLocalExtensionManagementPort;
use ironclaw_extension_manager::admin_configuration::{
    ComposedAdminConfigurationService, ComposedExtensionAdminConfigurationResolver,
};
#[cfg(any(test, feature = "test-support"))]
use ironclaw_outbound::{
    DeliveryTargetCapabilities, OutboundDeliveryTargetEntry, OutboundDeliveryTargetId,
    OutboundDeliveryTargetOwner, OutboundDeliveryTargetScope, OutboundDeliveryTargetSummary,
};
use ironclaw_outbound::{MutableOutboundDeliveryTargetRegistry, OutboundDeliveryTargetProvider};
use ironclaw_secrets::SecretStorePort;
use ironclaw_skills::ScopedSkillManagementPort;

#[cfg(any(test, feature = "test-support"))]
#[derive(Clone)]
struct StaticOutboundDeliveryTargetProvider {
    summary: OutboundDeliveryTargetSummary,
    capabilities: DeliveryTargetCapabilities,
    reply_target_binding_ref: ReplyTargetBindingRef,
}

#[cfg(any(test, feature = "test-support"))]
#[async_trait::async_trait]
impl OutboundDeliveryTargetProvider for StaticOutboundDeliveryTargetProvider {
    async fn list_outbound_delivery_targets(
        &self,
        caller: &OutboundDeliveryTargetScope,
    ) -> Result<Vec<OutboundDeliveryTargetEntry>, OutboundError> {
        // Static test/QA fixture available to whichever caller asks: it claims
        // the querying caller as owner so it always survives the registry's
        // caller-scoping filter. Real providers derive the owner from the
        // resolved resource instead.
        Ok(vec![OutboundDeliveryTargetEntry {
            summary: self.summary.clone(),
            capabilities: self.capabilities.clone(),
            destination: self.reply_target_binding_ref.clone(),
            owner: OutboundDeliveryTargetOwner::for_scope(caller),
        }])
    }
}
use crate::RebornCompositionProfile;
use crate::automation::trigger_poller::{
    TRIGGER_POLLER_SHUTDOWN_TIMEOUT, TriggerPollerCompositionDeps, TriggerPollerRuntimeHandle,
    spawn_trigger_poller,
};
use crate::factory::{RebornRuntimeStores, build_runtime_substrate};
use crate::runtime_input::{
    PollSettings, RebornRuntimeIdentity, RebornRuntimeInput, TriggerFireAccessGrant,
};
use crate::trigger_fire_access::IdentityMembershipTriggerFireChecker;
use crate::trigger_poller_assembly::{
    build_trigger_active_run_lookup, build_trigger_poller_services, poller_user_directory,
    validate_trigger_poller_authorization,
};
use crate::{RebornBuildError, RebornReadiness};
use ironclaw_triggers::{
    CompositeTriggerFireChecker, StaticOwnerTriggerFireChecker, TriggerFireAccessChecker,
};
use production::{
    EmptyCapabilitySurfaceResolver, EmptyIdentityContextSource,
    UnavailableApprovalInteractionService, UnavailableCapabilityIo,
    UnavailableCapabilityPortFactory,
};

mod approval;
mod approval_interaction_assembly;
mod assembly;
mod assembly_support;
use approval_interaction_assembly::ApprovalRequestGateEvidence;
#[cfg(feature = "test-support")]
pub(crate) use approval_interaction_assembly::build_approval_gate_evidence_for_test;
pub(crate) use approval_interaction_assembly::build_approval_interaction_service;
#[cfg(any(test, feature = "test-support"))]
use approval_interaction_assembly::{
    ProcessGateApprovalTurnRunLocator, RegistryPersistentApprovalGranteeResolver,
};
mod auth_interaction;
#[cfg(test)]
#[path = "runtime/tests/auth_interaction.rs"]
mod auth_interaction_tests;
pub(crate) mod capability_host;
mod cli_skills;
mod computer_use_images;
#[cfg(test)]
#[path = "runtime/tests/default_system_prompt.rs"]
mod default_system_prompt_tests;
mod handles;
mod harness_accessors;
mod latency;
#[cfg(test)]
#[path = "runtime/tests/outbound_delivery.rs"]
mod outbound_delivery_tests;
mod production;
mod runtime_turn_scheduler;
mod skill_context;
mod skills;
mod startup_guards;
mod store_parts;
#[cfg(any(test, feature = "test-support"))]
pub(crate) mod subagent_delivery_test_support;
#[cfg(feature = "test-support")]
#[path = "runtime/test_support.rs"]
mod test_support;
mod turn_settlement;
mod turns;

#[cfg(feature = "test-support")]
pub(crate) use capability_host::PROJECT_CREATE_CAPABILITY_ID;
#[cfg(feature = "test-support")]
pub(crate) use capability_host::RESULT_READ_CAPABILITY_ID_FOR_TEST;
#[cfg(any(test, feature = "test-support"))]
pub(crate) use capability_host::SKILL_ACTIVATE_CAPABILITY_ID;

pub use skills::{
    RebornSkillActivation, RebornSkillActivationMode, RebornSkillActivationSource,
    RebornSkillAsset, RebornSkillBundle, RebornSkillExecutionPlan, RebornSkillExecutionResult,
};

use skills::skill_asset_error;

use ironclaw_operator::ResolvedRebornLlm;
// Named only by `#[cfg(any(test, feature = "test-support"))]` accessors below,
// so the imports carry the same gate. Without it, any build that compiles this
// crate as a *dependency* with `test-support` off — e.g. `cargo clippy -p
// ironclaw --lib --bins`, where the dev-dependency that would have unified the
// feature on is not in the selected set — sees three unused imports and fails
// `-D warnings`. See #7119; the "Check production-target lints (workspace, no
// dev-dependency features)" step in code_style.yml keeps that shape linted so
// the class cannot come back invisibly.
#[cfg(any(test, feature = "test-support"))]
#[cfg(any(test, feature = "test-support"))]
use ironclaw_product_contracts::admin_users::AdminUserService;
#[cfg(any(test, feature = "test-support"))]
use ironclaw_product_contracts::channel_config::ChannelConfigProductService;
use ironclaw_product_contracts::delivery::ChannelDeliveryResolver;

/// Stable identifier for a Reborn CLI conversation. Wraps a `ThreadId`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConversationId(pub ThreadId);

/// Final-form assistant reply read back from the session thread service after
/// a `send_user_message` completes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantReply {
    pub conversation: ConversationId,
    pub run_id: TurnRunId,
    pub status: TurnStatus,
    pub failure_category: Option<String>,
    pub text: Option<String>,
}

impl AssistantReply {
    /// True when a caller can treat the reply as a successful single-shot
    /// response. Recovery/failed/cancelled runs may still produce diagnostics,
    /// but they did not produce the requested assistant text.
    pub fn is_successful_final_reply(&self) -> bool {
        self.status == TurnStatus::Completed && self.text.is_some()
    }
}

/// Accepted-turn handle returned by `RebornRuntime::submit_user_turn`. Holds
/// the per-conversation send lock for its lifetime so the caller's wait phase
/// retains the same mutual exclusion the inline submit path used to.
struct SubmittedTurn {
    _send_guard: OwnedMutexGuard<()>,
    scope: TurnScope,
    run_id: TurnRunId,
    accepted_message_ref: AcceptedMessageRef,
}

/// Outcome of driving a single turn that may pause on a gate.
///
/// Test/recording-support only — produced by
/// [`RebornRuntime::send_user_message_until_gate`], which mirrors the
/// production [`RebornRuntime::send_user_message`] submit path but returns when
/// the run first reaches a terminal status *or* parks on a `Blocked*` gate,
/// instead of waiting only for a terminal status. Gate *resolution* stays on
/// the WebUI `ProductSurface` facade (`resolve_gate`) per the #3094 seam;
/// this type only observes where a run paused.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Clone)]
pub enum RebornTurnDriveOutcome {
    /// The run reached a terminal status without pausing on a gate.
    Terminal(AssistantReply),
    /// The run parked on a user-resolvable gate (auth/approval/resource) and is
    /// awaiting resolution through the facade. `gate_ref` is required: the
    /// blocked-reason contract carries a `TurnGateRef` for every such block, so its
    /// absence is an invariant violation, not a valid recorder outcome.
    BlockedOnGate {
        run_id: TurnRunId,
        status: TurnStatus,
        gate_ref: ironclaw_host_api::turn::TurnGateRef,
        partial_text: Option<String>,
    },
}

/// Errors returned by `RebornRuntime` methods.
#[derive(Debug, Error)]
pub enum RebornRuntimeError {
    #[error("reborn runtime build failed: {0}")]
    Build(#[from] RebornBuildError),
    #[error("turn coordinator unavailable for assembled runtime")]
    TurnCoordinatorUnavailable,
    #[error("host runtime unavailable for assembled runtime")]
    HostRuntimeUnavailable,
    #[error("turn submission failed: {0}")]
    TurnSubmission(String),
    #[error("turn submission rejected: {reason}")]
    TurnRejected { reason: String },
    #[error("session thread service error: {0}")]
    ThreadService(String),
    #[error("turn coordinator error: {0}")]
    TurnCoordinator(String),
    #[error("run did not reach a terminal state within {timeout:?}")]
    RunTimeout { timeout: Duration },
    #[error("run cancelled by caller")]
    OperationCancelled,
    #[error("invalid scope or identifier: {reason}")]
    InvalidArgument { reason: String },
    #[error("malformed runtime configuration: {reason}")]
    MalformedConfig { reason: String },
    #[error("malformed planned-runtime configuration: {0}")]
    PlannedRuntimeConfig(#[from] DefaultPlannedRuntimeConfigError),
    #[error("llm provider construction failed: {0}")]
    LlmProvider(String),
    #[error("turn-runner worker is no longer running")]
    WorkerStopped,
    #[error("skill execution unavailable for assembled runtime")]
    SkillExecutionUnavailable,
    #[error("skill execution failed: {0}")]
    SkillExecution(String),
    #[error("user sandbox shutdown failed: {0}")]
    UserSandboxShutdown(#[source] RuntimeProcessError),
    #[error("runtime event flush failed: {0}")]
    RuntimeEventFlush(#[source] EventError),
}

impl From<TurnError> for RebornRuntimeError {
    fn from(value: TurnError) -> Self {
        Self::TurnCoordinator(value.to_string())
    }
}

impl From<DefaultPlannedRuntimeBuildError> for RebornRuntimeError {
    fn from(value: DefaultPlannedRuntimeBuildError) -> Self {
        Self::InvalidArgument {
            reason: value.to_string(),
        }
    }
}

fn cli_model_resolution_error(error: LlmConfigServiceError) -> RebornRuntimeError {
    match error {
        LlmConfigServiceError::InvalidRequest { reason, .. } => {
            RebornRuntimeError::TurnRejected { reason }
        }
        LlmConfigServiceError::NotFound => RebornRuntimeError::TurnRejected {
            reason: "requested model is unavailable".to_string(),
        },
        LlmConfigServiceError::Unavailable => {
            RebornRuntimeError::LlmProvider("model selection is unavailable".to_string())
        }
        LlmConfigServiceError::Internal => {
            RebornRuntimeError::LlmProvider("model selection failed".to_string())
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) struct OutboundTestStores {
    state: Arc<dyn ironclaw_outbound::OutboundStateStorePort>,
    reply_attachment_intents: Arc<dyn ironclaw_outbound::ReplyAttachmentIntentPort>,
}

/// Started, running Reborn agent runtime.
///
/// `RebornRuntime` is the single user-facing handle returned by
/// [`build_reborn_runtime`]. Downstream code never reaches into the substrate
/// or worker machinery: it talks to the runtime through task-level methods.
pub(crate) struct SubagentDeliveryRuntime {
    pub(crate) input_queue:
        Arc<ironclaw_loop_host::FilesystemHostInputQueue<CompositeRootFilesystem>>,
    // These handles are retained by the composed runtime so the test-support
    // view can drive the same resolver/store that production wiring uses. The
    // production runtime reaches them through the bound runner graph; the
    // underscore names make that intentional ownership explicit to clippy.
    pub(crate) _resolver: Arc<AwaitEdgeResolver<dyn SessionThreadService>>,
    pub(crate) _store: Arc<AwaitEdgeStore>,
}

pub struct RebornRuntime {
    pub(crate) host_runtime: Arc<dyn HostRuntime>,
    user_sandbox_process_port: Option<Arc<ironclaw_host_runtime::UserSandboxProcessPort>>,
    pub(crate) product_auth: Arc<RebornProductAuthServices>,
    pub(crate) readiness: RebornReadiness,
    pub(crate) skill_management: Arc<ScopedSkillManagementPort>,
    pub(crate) extension_lifecycle_surface_context: LifecycleProductSurfaceContext,
    pub(crate) secret_store: Arc<dyn SecretStorePort>,
    pub(crate) scoped_filesystem: Arc<ScopedFilesystem<CompositeRootFilesystem>>,
    pub(crate) suggestions_store: Arc<dyn ironclaw_assistant::SuggestionsStore>,
    pub(crate) llm_config_service: Option<Arc<ironclaw_operator::RebornLlmConfigService>>,
    pub(crate) admin_secret_provisioner: Arc<dyn ironclaw_assistant::AdminSecretProvisioner>,
    pub(crate) project_service:
        Arc<dyn ironclaw_product_contracts::project_service::ProjectService>,
    pub(crate) diagnostic_store: Arc<dyn ironclaw_assistant::inspector_store::DiagnosticStorePort>,
    pub(crate) trigger_repository: Arc<dyn ironclaw_triggers::TriggerRepository>,
    /// Thread orchestration engine; `None` off libSQL. Started (with
    /// IronClaw's agent loop as its runtime) by [`RebornRuntime::product_surface`].
    // arch-exempt: optional_arc, libSQL-only engine absent on other backends, plan #orchestrator-stage-3
    pub(crate) orchestrator: Option<Arc<ironclaw_orchestrator::engine::Engine>>,
    /// Approvals, live stream and MCP tokens of CLI threads; set with `orchestrator`.
    // arch-exempt: optional_arc, libSQL-only like the engine it serves, plan #v3-cli-bridge
    pub(crate) cli_bridge: Option<Arc<ironclaw_cli_providers::bridge::CliBridge>>,
    /// See `factory`'s field of the same name.
    pub(crate) workspace_host_root: Option<std::path::PathBuf>,
    pub(crate) computer_use: Option<Arc<ironclaw_computer_use::ComputerUse>>,
    /// Late-bound manual-fire runner shared by product automation actions and
    /// the scheduler so both surfaces execute through the same worker graph.
    pub(crate) trigger_manual_fire_runner: Arc<dyn ironclaw_triggers::TriggerManualFireRunner>,
    #[cfg(any(test, feature = "test-support"))]
    #[allow(
        dead_code,
        reason = "held for test-support rebinding after runtime construction"
    )]
    pub(crate) trigger_process_lifecycle_source: Arc<
        std::sync::RwLock<
            Arc<dyn ironclaw_processes::ProcessLifecycleLookupSource<Error = TurnError>>,
        >,
    >,
    /// Sibling rebindable slot for the trigger delivery-target service; the
    /// test-support repoint seam swaps both slots together.
    pub(crate) broadcast_budget_event_sink: Arc<ironclaw_resources::BroadcastBudgetEventSink>,
    pub(crate) external_tool_catalog: Arc<dyn ExternalToolCatalog>,
    pub(crate) persistent_approval_policies: Arc<ComposedPersistentApprovalPolicyStore>,
    pub(crate) tool_permission_overrides: Arc<ComposedToolPermissionOverrideStore>,
    pub(crate) auto_approve_settings: Arc<ComposedAutoApproveSettingStore>,
    pub(crate) extension_registry: Arc<ExtensionRegistry>,
    pub(crate) shared_extension_registry: Arc<SharedExtensionRegistry>,
    pub(crate) skill_auto_activate_learned: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) extension_management: Arc<RebornLocalExtensionManagementPort>,
    pub(crate) runtime_http_egress: Option<Arc<dyn RuntimeHttpEgress>>,
    /// Durable nonce and signed-manifest replay state shared by CLI IronHub
    /// installs and the optional deep-link gateway.
    pub(crate) ironhub_link_state: Arc<ironclaw_extension_manager::ironhub::IronhubLinkStateStore>,
    pub(crate) ironhub_manifest_url: ironclaw_extension_manager::ironhub::IronhubManifestUrl,
    /// Single composed IronHub deep-link service. `None` is the default-off
    /// registration gate; the same option controls facade and route wiring.
    pub(crate) ironhub_link_service:
        Option<Arc<dyn ironclaw_product_contracts::ironhub::IronhubLinkService>>,
    pub(crate) owner_user_id: UserId,
    pub(crate) extension_filesystem: Arc<CompositeRootFilesystem>,
    /// What the Memory page and the hooks view are served from.
    pub(crate) memory_page: crate::memory_page_parts::MemoryPageParts,
    pub(crate) session_inbound_ledger: Arc<dyn ironclaw_assistant::IdempotencyLedger>,
    pub(crate) session_channel_directory:
        Arc<dyn ironclaw_product_contracts::session_ingress::SessionChannelDirectory>,
    pub(crate) session_channel_extension_id: Option<String>,
    /// Boot-time reply-publication recovery sweep, owned so shutdown stops
    /// it before releasing publication leases.
    pub(crate) reply_publication_recovery: Option<tokio::task::JoinHandle<()>>,
    /// The deployment's single workspace scoping decision, carried so the WebUI
    /// attachment handle addresses the same subtree as agent tool writes.
    pub(crate) workspace_mount_policy: crate::runtime_mounts::WorkspaceMountPolicy,
    pub(crate) system_extensions_lifecycle_mounts: MountView,
    pub(crate) outbound_preferences: Arc<dyn CommunicationPreferenceRepository>,
    pub(crate) notification_inbox: Arc<dyn ironclaw_notifications::NotificationInboxStorePort>,
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) outbound_state: OutboundTestStores,
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) triggered_run_delivery: Arc<dyn ironclaw_outbound::TriggeredRunDeliveryStore>,
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) delivered_gate_routes: Arc<dyn ironclaw_outbound::DeliveredGateRouteStore>,
    /// The delivery coordinator (present exactly when channel egress is
    /// configured). It owns reply publication; shutdown hands held
    /// publication leases back so another publisher can resume open replies
    /// at once.
    pub(crate) delivery_coordinator: Option<Arc<ironclaw_assistant::DeliveryCoordinator>>,
    pub(crate) channel_facade_slot:
        Arc<std::sync::OnceLock<Arc<dyn ironclaw_auth::ChannelConnectionService>>>,
    pub(crate) admin_configuration: Arc<ComposedAdminConfigurationService>,
    pub(crate) admin_configuration_uses: Arc<Vec<AdminConfigurationCatalogUse>>,
    pub(crate) channel_config_service: Arc<ComposedExtensionAdminConfigurationResolver>,
    pub(crate) channel_identity_store: Arc<ironclaw_extension_host::FilesystemChannelIdentityStore>,
    pub(crate) channel_dm_target_store:
        Arc<ironclaw_extension_host::FilesystemChannelDmTargetStore>,
    pub(crate) extension_ingress:
        Option<ironclaw_extension_host::extension_ingress::ExtensionIngressParts>,
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) deployment_channels: Arc<ironclaw_extension_host::DeploymentChannelRegistry>,
    pub(crate) channel_pairing: Option<Arc<ChannelPairingRegistry>>,
    pub(crate) channel_delivery_resolver: Option<Arc<dyn ChannelDeliveryResolver>>,
    /// Host-owned per-user delivery registrations (design §8). Always wired:
    /// a coordinator that cannot answer "is this user enrolled?" must still
    /// answer it, and a deployment with no enrollment-requiring channel gets
    /// the no-op that answers "nobody is".
    pub(crate) delivery_registrations:
        Arc<dyn ironclaw_product_contracts::delivery::DeliveryRegistrationService>,
    /// Publishes the non-secret bootstrap document a channel's client needs
    /// in order to enroll — the public half of a credential the host already
    /// holds, published generically rather than through a per-channel status
    /// document.
    pub(crate) delivery_client_bootstrap: Arc<dyn ironclaw_assistant::DeliveryClientBootstrap>,
    #[cfg(feature = "test-support")]
    pub(crate) channel_egress_credential_bridges:
        Option<Arc<ironclaw_extension_host::channel_egress::BridgedChannelEgressCredentials>>,
    turn_coordinator: Arc<dyn TurnCoordinator>,
    /// Generic channel host assembly (extension-runtime P6 S2), held so the
    /// reconcile loop lives exactly as long as the runtime.
    _channel_host_assembly:
        Option<Arc<ironclaw_extension_host::channel_host::GenericChannelHostAssembly>>,
    /// The product-side workflow factory that assembly builds every graph
    /// through (§12.11 D-A), held beside it for the same reason: it owns the
    /// per-extension durable state builder and every triggered driver's
    /// services, so the composed runtime keeps a handle on it rather than
    /// reaching it only through the graphs it produced.
    channel_workflow_factory: Option<Arc<ironclaw_assistant::RebornChannelWorkflowFactory>>,
    pub(crate) process_lifecycle_lookup_source:
        Arc<dyn ProcessLifecycleLookupSource<Error = TurnError>>,
    pub(crate) _process_gate_query_source: Arc<dyn ProcessGateQuerySource<Error = TurnError>>,
    turn_tree_store: Arc<dyn AgentTurnSpawnTreeRuntimePort>,
    thread_service: Arc<dyn SessionThreadService>,
    pub(crate) subagent_delivery: SubagentDeliveryRuntime,
    thread_scope: ThreadScope,
    turn_scheduler: RuntimeTurnScheduler,
    trigger_poller_handle: Option<TriggerPollerRuntimeHandle>,
    credential_refresh_worker_handle: Option<ironclaw_auth::KeepaliveSweepHandle>,
    trace_flush_worker: ironclaw_trace_commons::capture::TraceQueueFlushWorkerHandle,
    skill_learning_extraction_tasks:
        Option<Arc<ironclaw_extension_host::skill_learning::SkillLearningExtractionTasks>>,
    #[cfg(any(test, feature = "test-support"))]
    trigger_conversation_pairing:
        Option<Arc<dyn ironclaw_conversations::ConversationActorPairingService>>,
    pub(crate) outbound_delivery_target_registry:
        Option<Arc<MutableOutboundDeliveryTargetRegistry>>,
    budget_event_projection: Option<crate::observability::budget_events::BudgetEventProjection>,
    poll_settings: PollSettings,
    /// Mints the one-time API bearer on admin user creation. Read by
    /// `runtime.product_surface` when wiring the admin surface. `None` leaves the
    /// admin create path reporting the token minter unavailable.
    admin_api_token_minter:
        Option<Arc<dyn ironclaw_product_contracts::admin_users::AdminApiTokenMinter>>,
    actor_user_id: UserId,
    source_binding_ref: SourceBindingRef,
    reply_target_binding_ref: ReplyTargetBindingRef,
    projection_services: RebornProjectionServices,
    approval_interaction_service: Arc<dyn ApprovalInteractionService>,
    auth_interaction_service: Arc<dyn AuthInteractionService>,
    #[cfg(any(test, feature = "test-support"))]
    interaction_service_test_parts: Option<InteractionServiceTestParts>,
    runtime_event_sink: Arc<dyn NonBlockingEventSink>,
    default_run_profile_id: String,
    send_locks: Mutex<HashMap<ConversationId, Arc<Mutex<()>>>>,
    #[cfg(feature = "test-support")]
    pub(crate) skill_context_source: Option<Arc<dyn HostSkillContextSource>>,
    pub(crate) skill_activation_source: Option<Arc<ComposedSelectableSkillContextSource>>,
    skill_execution_adapter: Option<Arc<ComposedSkillExecutionAdapter>>,
    /// `@extension` mentions shared with the runner's tool disclosure.
    tool_mentions: Arc<ironclaw_loop_host::TurnToolMentions>,
    /// Operator boot config, carried so the product surface can compose the
    /// LLM-config settings service over `providers.json` / `config.toml`.
    boot: Option<ironclaw_config::RebornBootConfig>,
    /// Hot-swap handle for the live LLM provider, when one was wired at boot.
    llm_reload: Option<RebornLlmReloadParts>,
}

impl ironclaw_extension_manager::extension_lifecycle_command::RebornExtensionLifecycleRuntime
    for RebornRuntime
{
    fn skill_management(&self) -> Arc<ironclaw_skills::ScopedSkillManagementPort> {
        Arc::clone(&self.skill_management)
    }

    fn extension_management(
        &self,
    ) -> Arc<ironclaw_extension_host::extension_lifecycle::RebornLocalExtensionManagementPort> {
        Arc::clone(&self.extension_management)
    }

    fn runtime_credential_accounts(
        &self,
    ) -> Arc<dyn ironclaw_auth::RuntimeCredentialAccountSelectionService> {
        self.product_auth
            .runtime_credential_account_selection_service()
    }

    fn extension_lifecycle_surface_context(&self) -> LifecycleProductSurfaceContext {
        self.extension_lifecycle_surface_context.clone()
    }
}

impl ironclaw_extension_manager::ironhub::RebornIronHubRuntime for RebornRuntime {
    fn ironhub_skill_management(&self) -> Arc<ironclaw_skills::ScopedSkillManagementPort> {
        Arc::clone(&self.skill_management)
    }

    fn ironhub_extension_management(
        &self,
    ) -> Arc<ironclaw_extension_host::ExtensionLifecycleManager> {
        Arc::clone(&self.extension_management)
    }

    fn ironhub_runtime_http_egress(&self) -> Option<Arc<dyn RuntimeHttpEgress>> {
        self.runtime_http_egress.clone()
    }

    fn ironhub_link_state(
        &self,
    ) -> Arc<ironclaw_extension_manager::ironhub::IronhubLinkStateStore> {
        Arc::clone(&self.ironhub_link_state)
    }

    fn ironhub_manifest_url(&self) -> ironclaw_extension_manager::ironhub::IronhubManifestUrl {
        self.ironhub_manifest_url.clone()
    }

    fn ironhub_surface_context(&self) -> LifecycleProductSurfaceContext {
        self.extension_lifecycle_surface_context.clone()
    }
}

pub(crate) type ComposedSelectableSkillContextSource =
    SelectableSkillContextSource<FilesystemSkillBundleSource<CompositeRootFilesystem>>;
type ComposedSkillExecutionAdapter =
    SkillExecutionAdapter<FilesystemSkillBundleSource<CompositeRootFilesystem>>;

mod trigger_execution_preflight;
use trigger_execution_preflight::StructuredTriggerExecutionPreflight;

#[cfg(any(test, feature = "test-support"))]
#[allow(
    dead_code,
    reason = "test-support parts are consumed selectively by integration harnesses"
)]
pub(crate) struct InteractionServiceTestParts {
    approval_requests: Arc<crate::factory::ComposedApprovalRequestStore>,
    capability_leases: Arc<crate::factory::ComposedCapabilityLeaseStore>,
    extension_registry: Arc<ExtensionRegistry>,
    workspace_mounts: crate::runtime_mounts::WorkspaceMountPolicy,
    memory_mounts: MountView,
    system_extensions_lifecycle_mounts: MountView,
    persistent_approval_policies: Arc<ComposedPersistentApprovalPolicyStore>,
    tool_permission_overrides: Arc<ComposedToolPermissionOverrideStore>,
    extension_management: Arc<RebornLocalExtensionManagementPort>,
    skill_management: Arc<ScopedSkillManagementPort>,
    admin_configuration_resolver: Arc<ComposedExtensionAdminConfigurationResolver>,
    product_auth: Arc<RebornProductAuthServices>,
    builtin_capability_policy: Arc<BuiltinCapabilityPolicy>,
}

/// Test-support forwarder for the `result_read` synthetic-capability wrap
/// (durable tool-result projection seam, issue #5838). Bridges the private
/// `capability_host` module to `test_support.rs`; mirrors the `project_create`
/// forwarder above.
#[cfg(feature = "test-support")]
pub(crate) fn wrap_result_read_capability_for_test(
    inner: std::sync::Arc<dyn ironclaw_loop_contracts::LoopCapabilityPort>,
    thread_service: std::sync::Arc<dyn ironclaw_threads::SessionThreadService>,
    fallback_user_id: ironclaw_host_api::ids::UserId,
    run_context: ironclaw_loop_contracts::LoopRunContext,
    input_resolver: std::sync::Arc<dyn ironclaw_loop_host::LoopCapabilityInputResolver>,
    result_writer: std::sync::Arc<dyn ironclaw_loop_host::LoopCapabilityResultWriter>,
) -> Result<
    std::sync::Arc<dyn ironclaw_loop_contracts::LoopCapabilityPort>,
    ironclaw_loop_contracts::AgentLoopHostError,
> {
    capability_host::wrap_result_read_capability_for_test(
        inner,
        thread_service,
        fallback_user_id,
        run_context,
        input_resolver,
        result_writer,
    )
}

/// Test-support forwarder (harness-port-seam P1 seam) for
/// `create_refreshing_capability_port`
/// (`refreshing_capability_port.rs:75`), production's sole capability-port
/// factory. Bridges the private `capability_host` module to `test_support`; mirrors
/// the `outbound_delivery` forwarder above. For tests only -- gated behind
/// `test-support`, ships zero bytes in production builds.
#[cfg(feature = "test-support")]
pub(crate) async fn create_refreshing_capability_port_for_test(
    parts: crate::test_support::RefreshingCapabilityPortTestParts,
) -> Result<
    std::sync::Arc<dyn ironclaw_loop_contracts::LoopCapabilityPort>,
    ironclaw_loop_contracts::AgentLoopHostError,
> {
    capability_host::create_refreshing_capability_port_for_test(parts).await
}

/// Test-support forwarder exposing production's real `StagedCapabilityIo`
/// wiring (`capability_host.rs`'s `staged_capability_io_for_test`, which mirrors
/// `capability_wiring`'s `new_with_durable_previews` call). Bridges the
/// private `capability_host` module to `test_support`; mirrors the
/// `create_refreshing_capability_port_for_test` forwarder above.
/// For tests only -- gated behind `test-support`, ships zero bytes in
/// production builds.
#[cfg(feature = "test-support")]
pub(crate) fn staged_capability_io_for_test(
    thread_service: std::sync::Arc<dyn ironclaw_threads::SessionThreadService>,
    fallback_user_id: ironclaw_host_api::ids::UserId,
) -> (
    std::sync::Arc<dyn ironclaw_loop_host::LoopCapabilityInputResolver>,
    std::sync::Arc<dyn ironclaw_loop_host::LoopCapabilityResultWriter>,
) {
    capability_host::staged_capability_io_for_test(thread_service, fallback_user_id)
}

#[cfg(feature = "test-support")]
pub(crate) fn staged_capability_io_with_observer_for_test(
    thread_service: std::sync::Arc<dyn ironclaw_threads::SessionThreadService>,
    fallback_user_id: ironclaw_host_api::ids::UserId,
    observer: Option<std::sync::Arc<dyn crate::RebornTrajectoryObserver>>,
) -> (
    std::sync::Arc<dyn ironclaw_loop_host::LoopCapabilityInputResolver>,
    std::sync::Arc<dyn ironclaw_loop_host::LoopCapabilityResultWriter>,
) {
    capability_host::staged_capability_io_with_observer_for_test(
        thread_service,
        fallback_user_id,
        observer,
    )
}

pub async fn build_reborn_runtime(
    input: RebornRuntimeInput,
) -> Result<RebornRuntime, RebornRuntimeError> {
    build_runtime(input).await
}

pub async fn build_runtime(input: RebornRuntimeInput) -> Result<RebornRuntime, RebornRuntimeError> {
    let (runtime, _) = build_runtime_with_resource_governor(input).await?;
    Ok(runtime)
}

type BuiltRuntime = (RebornRuntime, Arc<dyn ironclaw_resources::ResourceGovernor>);

/// Boxed: the assembly future is several MiB, past a 2 MiB worker or test
/// thread stack when awaited inline.
pub(crate) async fn build_runtime_with_resource_governor(
    input: RebornRuntimeInput,
) -> Result<BuiltRuntime, RebornRuntimeError> {
    Box::pin(assembly::assemble_runtime(input)).await
}

#[cfg(test)]
#[path = "runtime/tests/core.rs"]
mod tests;
