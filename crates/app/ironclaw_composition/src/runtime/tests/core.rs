use std::sync::{
    Arc, Mutex as StdMutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use crate::test_support::{TEST_SESSION_EXTENSION_ID, with_test_authenticated_session_channel};
use async_trait::async_trait;
use chrono::Utc;
use ironclaw_auth::{GOOGLE_CALENDAR_EVENTS_SCOPE, GOOGLE_CALENDAR_READONLY_SCOPE};
use ironclaw_event_log::{
    EventError, EventSink, NonBlockingEventSink, RuntimeEvent, RuntimeEventKind,
};

#[derive(Default)]
struct OverloadedEventSink {
    attempted_events: StdMutex<Vec<RuntimeEvent>>,
}

use ironclaw_assistant::{
    CREATE_THREAD_COMMAND, LifecyclePackageKind, LifecyclePackageRef, LifecycleProductPayload,
    LifecycleReadinessBlocker, ProductSurfaceCommandDescriptor, RESOLVE_GATE_COMMAND,
    RebornExtensionCredentialSetup, RebornSetupExtensionResponse, RebornSkillListResponse,
    RebornStreamEventsRequest, RebornStreamEventsResponse, RebornSubmitTurnResponse,
    SUBMIT_TURN_COMMAND, approval_gate_ref,
};
use ironclaw_extension_contracts::state::{InstallationState, LifecyclePublicState};
use ironclaw_host_api::ids::ProjectId;
use ironclaw_host_api::turn::{
    AcceptedMessageRef, IdempotencyKey, LoopResultRef, SanitizedCancelReason, TurnActor, TurnId,
    TurnRunId, TurnScope, TurnStatus,
};
use ironclaw_host_api::{
    ids::{
        ActivityId, AgentId, ApprovalRequestId, CapabilityId, InvocationId, TenantId, ThreadId,
        UserId,
    },
    resolution::Resolution,
    resource::ResourceScope,
    runtime_policy::{
        ApprovalPolicy, AuditMode, DeploymentMode, EffectiveRuntimePolicy, FilesystemBackendKind,
        NetworkMode, ProcessBackendKind, RuntimeProfile, SecretMode,
    },
    scope::Principal,
};
use ironclaw_loop_contracts::{
    InMemoryRunProfileResolver, LoopCapabilityPort, LoopRunContext, ModelProfileId,
    ProviderToolCall, RegisterProviderToolCallRequest, RunProfileResolutionRequest,
    RunProfileResolver, SkillVisibility, VisibleCapabilityRequest,
};
use ironclaw_loop_host::{
    HostManagedModelError, HostManagedModelErrorKind, HostManagedModelGateway,
    HostManagedModelMessage, HostManagedModelMessageRole, HostManagedModelRequest,
    HostManagedModelResponse, HostManagedToolResultContent, HostSkillContextBuildError,
    HostSkillContextCandidate, HostSkillContextSource, ModelCost, SpawnSubagentMode,
    SubagentKindId, SubagentThreadKind, SubagentThreadMetadata, ToolDisclosureMode,
};
use ironclaw_product_contracts::inbound_requests::{
    ProductCreateThreadRequest, ProductListAutomationsRequest, ProductResolveGateRequest,
    ProductSetupExtensionRequest, ProductSubmitTurnRequest,
};
use ironclaw_product_contracts::notification_inbox::{
    NOTIFICATIONS_ARCHIVE_COMMAND, NOTIFICATIONS_MARK_READ_COMMAND, NOTIFICATIONS_VIEW,
    ProductListNotificationsResponse, ProductNotificationMutationRequest,
};
use ironclaw_product_contracts::operator_llm::{
    LlmConfigService, SetUserModelPolicyRequest, SetUserModelPreferenceRequest,
};
use ironclaw_product_contracts::outbound::{ProductOutboundPayload, ProductProjectionItem};
use ironclaw_product_contracts::surface::{
    ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceErrorCode, ProductSurfaceErrorKind,
};
use ironclaw_product_contracts::views::{RebornViewPage, RebornViewQuery};
use ironclaw_skills::SkillTrust;
use ironclaw_threads::{
    AppendToolResultReferenceRequest, EnsureThreadRequest, LoadContextMessagesRequest, MessageKind,
    MessageStatus, TOOL_RESULT_RECORD_READ_MAX_BYTES, ThreadHistoryRequest, ThreadScope,
    ToolResultSafeSummary,
};
use ironclaw_turns::{
    AllowAllTurnAdmissionPolicy, GetRunStateRequest, SubmitChildRunRequest, SubmitTurnRequest,
    SubmitTurnResponse,
};
use rust_decimal_macros::dec;

use crate::RebornRuntimeProcessBinding;
use crate::observability::hooks::HooksActivationConfig;
use crate::runtime_input::{
    PollSettings, RebornRuntimeIdentity, RebornRuntimeInput, TriggerPollerSettings,
};
use crate::{RebornCompositionProfile, RebornReadiness, RebornReadinessState, RebornRuntimeError};
use ironclaw_config::{RebornBootConfig, RebornHome, RebornProfile};
use ironclaw_triggers::{
    TriggerFireAccessCheck, TriggerFireAccessChecker, TriggerFireAccessDecision,
    TriggerFireAccessError,
};

use super::{RebornSkillActivationSource, build_reborn_runtime};

const RUNTIME_POLL_TIMEOUT: Duration = Duration::from_secs(10);
const RUNTIME_SEND_TIMEOUT: Duration = Duration::from_secs(15);
const PRODUCTION_SHAPED_BUILD_TIMEOUT: Duration = Duration::from_secs(120);

async fn stop_turn_runner_worker_for_manual_state_test(runtime: &super::RebornRuntime) {
    runtime.turn_scheduler.stop_for_test().await;
}

fn standalone_runtime_policy() -> EffectiveRuntimePolicy {
    EffectiveRuntimePolicy {
        deployment: DeploymentMode::LocalSingleUser,
        requested_profile: RuntimeProfile::LocalHost,
        resolved_profile: RuntimeProfile::LocalHost,
        filesystem_backend: FilesystemBackendKind::HostWorkspace,
        process_backend: ProcessBackendKind::LocalHost,
        network_mode: NetworkMode::DirectLogged,
        secret_mode: SecretMode::ScrubbedEnv,
        approval_policy: ApprovalPolicy::AskDestructive,
        audit_mode: AuditMode::LocalMinimal,
    }
}

#[derive(Debug)]
struct RecordingGateway {
    reply: String,
    requests: Arc<StdMutex<Vec<HostManagedModelRequest>>>,
}

#[derive(Debug, Default)]
struct ModelOutageGateway {
    calls: AtomicUsize,
}

#[derive(Debug, Default)]
struct FailingSkillContextSource {
    calls: AtomicUsize,
}

#[derive(Debug, Default)]
struct ToolCallingGateway {
    calls: StdMutex<usize>,
    stream_model_calls: StdMutex<usize>,
    requests: StdMutex<Vec<HostManagedModelRequest>>,
}

#[derive(Debug, Default)]
struct SandboxShellCallingGateway {
    calls: StdMutex<usize>,
}

#[derive(Debug, Default)]
struct AuthGateToolCallingGateway {
    requests: StdMutex<Vec<HostManagedModelRequest>>,
}

#[derive(Debug, Default)]
struct WorkspaceListingGateway {
    calls: StdMutex<usize>,
    requests: StdMutex<Vec<HostManagedModelRequest>>,
}

// Standalone model replay is a bounded reference observation: for a
// result under the inline first-look preview cap (issue #5838,
// the standalone result-preview limit), the raw content legitimately
// appears inline in `detail.preview` so the model does not need a
// follow-up `result_read` call; only content beyond the cap requires one.
// Both fixtures below are well under the cap.
fn assert_standalone_result_reference(tool_result: &HostManagedModelMessage, raw_marker: &str) {
    assert!(
        tool_result.content.contains(raw_marker),
        "a result under the first-look preview cap should appear inline in model replay: {}",
        tool_result.content
    );
    let Some(HostManagedToolResultContent::Reference { envelope }) =
        tool_result.tool_result_content.as_ref()
    else {
        panic!(
            "model replay should carry a result-reference envelope, got {:?}",
            tool_result.tool_result_content
        );
    };
    assert_eq!(envelope.version, 1);
    assert!(envelope.result_ref.starts_with("result:"));
    let observation = envelope
        .model_observation
        .as_ref()
        .expect("result-reference replay should include a model observation");
    assert_eq!(observation["schema_version"], serde_json::json!(1));
    assert_eq!(observation["status"], serde_json::json!("success"));
    assert_eq!(
        observation["detail"]["kind"],
        serde_json::json!("result_reference")
    );
    assert_eq!(
        observation["detail"]["result_ref"],
        serde_json::json!(envelope.result_ref)
    );
}

struct StaticSkillContextSource {
    candidates: Vec<HostSkillContextCandidate>,
}

#[derive(Debug)]
struct AllowingTriggerFireAccessChecker;

impl StaticSkillContextSource {
    fn new(candidates: Vec<HostSkillContextCandidate>) -> Self {
        Self { candidates }
    }
}

#[async_trait]
impl TriggerFireAccessChecker for AllowingTriggerFireAccessChecker {
    async fn check_trigger_fire_access(
        &self,
        _request: TriggerFireAccessCheck,
    ) -> Result<TriggerFireAccessDecision, TriggerFireAccessError> {
        Ok(TriggerFireAccessDecision::Allowed)
    }
}

#[async_trait]
impl HostSkillContextSource for StaticSkillContextSource {
    async fn load_skill_context_candidates(
        &self,
        _run_context: &LoopRunContext,
    ) -> Result<Vec<HostSkillContextCandidate>, HostSkillContextBuildError> {
        Ok(self.candidates.clone())
    }
}

#[async_trait]
impl HostManagedModelGateway for RecordingGateway {
    async fn stream_model(
        &self,
        request: HostManagedModelRequest,
    ) -> Result<HostManagedModelResponse, HostManagedModelError> {
        self.requests
            .lock()
            .expect("recording gateway requests lock poisoned")
            .push(request);
        Ok(HostManagedModelResponse::assistant_reply(
            self.reply.clone(),
        ))
    }
}

/// A long echo argument, sized well over `TOOL_RESULT_RECORD_READ_MAX_BYTES`
/// (not just the old hardcoded 2KiB), so the default-observer test can
/// prove the payload is truncated before the observer sees it.
const LARGE_ECHO_MESSAGE: &str = "PAYLOAD0123456789ABCDEF_";
const LARGE_ECHO_TAIL: &str = "UNREPLAYED_RAW_TOOL_RESULT_TAIL";

fn large_echo_message() -> String {
    let repeat_count = TOOL_RESULT_RECORD_READ_MAX_BYTES / LARGE_ECHO_MESSAGE.len() + 1;
    format!(
        "Secretary of the Treasury: {}{}",
        LARGE_ECHO_MESSAGE.repeat(repeat_count),
        LARGE_ECHO_TAIL
    )
}

#[derive(Debug, Default)]
struct LargeEchoToolCallingGateway {
    calls: StdMutex<usize>,
}

fn model_capability_error(error: impl std::fmt::Display) -> HostManagedModelError {
    let safe_summary = error.to_string();
    HostManagedModelError::safe(HostManagedModelErrorKind::Unavailable, safe_summary)
}

static RUNTIME_ENV_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct RuntimeEnvGuardEntry {
    name: &'static str,
    effective: Option<String>,
    snapshot: ironclaw_common::env_helpers::RuntimeEnvSnapshot,
}

struct RuntimeEnvGuard {
    // Serializes tokio tests that mutate the runtime env overlay. The
    // set/remove helpers lock only the separate override map, not
    // ENV_MUTEX, so restoration can safely run while this guard is held.
    _async_lock: tokio::sync::MutexGuard<'static, ()>,
    _env_lock: std::sync::MutexGuard<'static, ()>,
    previous: Vec<RuntimeEnvGuardEntry>,
}

impl RuntimeEnvGuard {
    async fn set(name: &'static str, value: &str) -> Self {
        Self::with([(name, Some(value))]).await
    }

    async fn with<const N: usize>(vars: [(&'static str, Option<&str>); N]) -> Self {
        let async_lock = RUNTIME_ENV_TEST_LOCK.lock().await;
        let env_lock = ironclaw_common::env_helpers::lock_env();
        let previous = vars
            .iter()
            .map(|(name, _)| RuntimeEnvGuardEntry {
                name,
                effective: ironclaw_common::env_helpers::env_or_override(name),
                snapshot: ironclaw_common::env_helpers::snapshot_runtime_env(name),
            })
            .collect::<Vec<_>>();
        for (name, value) in vars {
            match value {
                Some(value) => ironclaw_common::env_helpers::set_runtime_env(name, value),
                None => ironclaw_common::env_helpers::mask_runtime_env(name),
            }
        }
        Self {
            _async_lock: async_lock,
            _env_lock: env_lock,
            previous,
        }
    }
}

fn nearai_gateway_test_request() -> HostManagedModelRequest {
    HostManagedModelRequest {
        model_profile_id: ironclaw_loop_contracts::ModelProfileId::new("interactive_model")
            .expect("model profile id"),
        messages: vec![ironclaw_loop_host::HostManagedModelMessage {
            role: HostManagedModelMessageRole::User,
            content: "hello model".to_string(),
            content_ref: ironclaw_host_api::turn::LoopMessageRef::new(
                "msg:22222222-2222-2222-2222-222222222222",
            )
            .expect("message ref"),
            tool_result_provider_call: None,
            tool_result_content: None,
            image_parts: Vec::new(),
        }],
        surface_version: None,
        resolved_model_route: None,
        fallback_index: 0,
        run_id: TurnRunId::new(),
        turn_id: TurnId::new(),
        thread_id: None,
        tool_choice: None,
        response_format: None,
    }
}

#[derive(Debug)]
struct RecordingLlmProvider {
    active_model: StdMutex<String>,
    requests: StdMutex<Vec<Option<String>>>,
}

impl RecordingLlmProvider {
    fn new(active_model: &str) -> Self {
        Self {
            active_model: StdMutex::new(active_model.to_string()),
            requests: StdMutex::new(Vec::new()),
        }
    }
}

fn skill_md(name: &str, description: &str, prompt: &str) -> String {
    format!(
        "---\nname: {name}\ndescription: {description}\nactivation:\n  keywords: [\"{name}\"]\n---\n\n{prompt}"
    )
}

/// Seed a skill where the runtime actually reads one: the DB-backed virtual filesystem.
///
/// Seeding the host disk instead is now testing nothing — every skill mount derives from
/// `db_backed_skill_grants`, so a disk-seeded skill is correctly invisible (nearai/ironclaw#7168).
/// Migrations are idempotent, so this runs before the runtime is built.
async fn seed_db_skill(
    storage_root: &std::path::Path,
    virtual_dir: &str,
    files: &[(&str, String)],
) {
    std::fs::create_dir_all(storage_root).expect("storage root");
    let db_path = crate::filesystem_assembly::standalone_db_path(storage_root);
    let db = std::sync::Arc::new(
        libsql::Builder::new_local(&db_path)
            .build()
            .await
            .expect("open libsql database"),
    );
    let vfs = ironclaw_filesystem::LibSqlRootFilesystem::new(db).expect("libsql root filesystem");
    vfs.run_migrations().await.expect("libsql migrations");
    for (relative_path, contents) in files {
        let path =
            ironclaw_host_api::path::VirtualPath::new(format!("{virtual_dir}/{relative_path}"))
                .expect("virtual path");
        ironclaw_filesystem::RootFilesystem::write_file(&vfs, &path, contents.as_bytes())
            .await
            .expect("write seeded skill file");
    }
}

async fn seed_user_skill(
    storage_root: &std::path::Path,
    tenant_id: &str,
    user_id: &str,
    name: &str,
    skill_md: String,
) {
    seed_user_skill_with_files(storage_root, tenant_id, user_id, name, skill_md, &[]).await
}

async fn seed_user_skill_with_files(
    storage_root: &std::path::Path,
    tenant_id: &str,
    user_id: &str,
    name: &str,
    skill_md: String,
    extra_files: &[(&str, String)],
) {
    let mut files = vec![("SKILL.md", skill_md)];
    files.extend(extra_files.iter().map(|(p, c)| (*p, c.clone())));
    seed_db_skill(
        storage_root,
        &format!("/tenants/{tenant_id}/users/{user_id}/skills/{name}"),
        &files,
    )
    .await
}

async fn seed_tenant_shared_skill(
    storage_root: &std::path::Path,
    tenant_id: &str,
    name: &str,
    skill_md: String,
) {
    seed_db_skill(
        storage_root,
        &format!("/tenants/{tenant_id}/tenant-shared/skills/{name}"),
        &[("SKILL.md", skill_md)],
    )
    .await
}

fn skill_md_with_setup_marker(name: &str, description: &str, marker: &str, prompt: &str) -> String {
    format!(
        "---\nname: {name}\ndescription: {description}\nactivation:\n  keywords: [\"{name}\"]\n  setup_marker: \"{marker}\"\n---\n\n{prompt}"
    )
}

fn recorded_request_count(requests: &StdMutex<Vec<HostManagedModelRequest>>) -> usize {
    requests
        .lock()
        .expect("recording gateway requests lock poisoned")
        .len()
}

/// Counts how many times the runtime drives this provider and answers with a
/// fixed sentinel, so a test can prove an injected provider — not one built
/// from config — is the one the gateway actually calls.
struct CountingOverrideProvider {
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

/// Records both trajectory callbacks so the e2e test can assert the
/// observer fires through a real `build_reborn_runtime` turn — driving the
/// input hook (`HostRuntimeLoopCapabilityPort`) and the result hook
/// (`StagedCapabilityIo::write_capability_result`) on the actual dispatch
/// path, not a direct helper call.
#[derive(Debug, Default)]
struct RecordingTrajectoryObserver {
    inputs: StdMutex<Vec<(String, String, serde_json::Value)>>,
    results: StdMutex<Vec<(String, String, serde_json::Value)>>,
}

async fn invoke_product_command<T, O>(
    api: &dyn ironclaw_product_contracts::surface::ProductSurface,
    caller: ProductSurfaceCaller,
    command: ProductSurfaceCommandDescriptor<T, O>,
    input: T,
) -> Result<O, ProductSurfaceError>
where
    T: serde::Serialize,
    O: serde::de::DeserializeOwned,
{
    let input = serde_json::to_value(input).map_err(ProductSurfaceError::internal_from)?;
    let response = ironclaw_product_contracts::surface::ProductSurface::invoke(
        api,
        caller,
        ironclaw_product_contracts::surface::ProductSurfaceInvokeRequest {
            operation_id: command.capability_id()?,
            input,
            activity_id: ActivityId::new(),
        },
    )
    .await?;
    serde_json::from_value(response.output).map_err(ProductSurfaceError::internal_from)
}

async fn invoke_product_capability<T>(
    api: &dyn ironclaw_product_contracts::surface::ProductSurface,
    caller: ProductSurfaceCaller,
    capability_id: &str,
    input: T,
) -> Result<Resolution, ProductSurfaceError>
where
    T: serde::Serialize,
{
    let input = serde_json::to_value(input).map_err(ProductSurfaceError::internal_from)?;
    let response = ironclaw_product_contracts::surface::ProductSurface::invoke(
        api,
        caller,
        ironclaw_product_contracts::surface::ProductSurfaceInvokeRequest {
            operation_id: CapabilityId::new(capability_id).expect("capability id"),
            input,
            activity_id: ActivityId::new(),
        },
    )
    .await?;
    serde_json::from_value(response.output).map_err(ProductSurfaceError::internal_from)
}
// arch-exempt: large_file, runtime composition contract coverage remains centralized, plan #6175

// Two-thread skill fixtures: thread 1 authors a skill carrying a script, thread 2 runs it.
//
// Distilled from ten live demo runs, each of which failed somewhere in this chain and none of which
// a hermetic test caught: the install vanished (#7168); a missing `description:` made the skill
// invisible to discovery forever; the staged script could not be executed because the bundle lives
// in the database; and once staging landed, the path the model was TOLD still missed. So these
// assert the whole chain rather than a layer -- a layer-at-a-time test passed through all ten.
/// The skill an agent writes when asked for eGFR — the exact shape every demo produced.
const SKILL_MD: &str = "---\nname: egfr-calc\ndescription: Compute eGFR from serum creatinine with the 2021 race-free CKD-EPI equation and assign a KDIGO stage.\n---\n\n# eGFR\n\nRun the bundled script:\n\n```bash\npython3 scripts/egfr.py --scr 1.3 --age 62 --female\n```\n";

/// A real script, so \"it ran\" means the process produced this output and not the model's arithmetic.
const SKILL_SCRIPT: &str = r#"#!/usr/bin/env python3
import argparse

parser = argparse.ArgumentParser()
parser.add_argument("--scr", type=float, required=True)
parser.add_argument("--age", type=float, required=True)
parser.add_argument("--female", action="store_true")
args = parser.parse_args()

kappa = 0.7 if args.female else 0.9
alpha = -0.241 if args.female else -0.302
factor = 1.012 if args.female else 1.0
ratio = args.scr / kappa
egfr = 142 * (min(ratio, 1) ** alpha) * (max(ratio, 1) ** -1.200) * (0.9938 ** args.age) * factor
egfr = round(egfr, 1)
stage = (
    "G1" if egfr >= 90 else
    "G2" if egfr >= 60 else
    "G3a" if egfr >= 45 else
    "G3b" if egfr >= 30 else
    "G4" if egfr >= 15 else "G5"
)
print(f"FIXTURE-OK eGFR={egfr} stage={stage}")
"#;

const TENANT: &str = "two-thread-tenant";
const OWNER: &str = "two-thread-owner";
const AGENT: &str = "two-thread-agent";

/// A mocked model doing what the demo's second thread does: read the activated body, take the
/// workdir it ADVERTISES, and run the skill's command there through the real `builtin.shell`.
///
/// The point is path provenance. The sibling fixture walks the workspace and runs the script with
/// `std::process::Command`, which proves the bytes landed but says nothing about the string handed
/// to the model — and a wrong string is what shipped once. Parsed from the body, so a wrong
/// advertised path fails this test.
#[derive(Debug, Default)]
struct SkillShellGateway {
    calls: StdMutex<usize>,
    /// The workdir the body advertised, as the model read it.
    advertised_workdir: StdMutex<Option<String>>,
    /// The shell's own stdout, replayed back to the model on the following call.
    shell_output: StdMutex<Option<String>>,
}

fn two_thread_caller() -> ProductSurfaceCaller {
    ProductSurfaceCaller::new(
        TenantId::new(TENANT).expect("tenant"),
        UserId::new(OWNER).expect("user"),
        Some(AgentId::new(AGENT).expect("agent")),
        None,
    )
}

fn two_thread_runtime_input(storage_root: std::path::PathBuf) -> RebornRuntimeInput {
    let gateway = Arc::new(RecordingGateway {
        reply: "fixture reply".to_string(),
        requests: Arc::new(std::sync::Mutex::new(Vec::new())),
    });
    RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(OWNER, storage_root)
            .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: TENANT.to_string(),
        agent_id: AGENT.to_string(),
        source_binding_id: "two-thread-source".to_string(),
        reply_target_binding_id: "two-thread-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: Duration::from_secs(5),
    })
    .with_model_gateway_override(gateway)
}

// Scenario-grouped test modules; each pulls the shared fixtures above in
// through `use super::*`.
#[path = "core/llm_gateway.rs"]
mod llm_gateway;
#[path = "core/nearai_bootstrap.rs"]
mod nearai_bootstrap;
#[path = "core/production_build.rs"]
mod production_build;
#[path = "core/scheduler.rs"]
mod scheduler;
#[path = "core/send_message.rs"]
mod send_message;
#[path = "core/skill_selection.rs"]
mod skill_selection;
#[path = "core/skills.rs"]
mod skills;
#[path = "core/startup_guards.rs"]
mod startup_guards;
#[path = "core/tool_calls.rs"]
mod tool_calls;
#[path = "core/trigger_poller.rs"]
mod trigger_poller;
#[path = "core/webui_bundle.rs"]
mod webui_bundle;
#[path = "core/wiring.rs"]
mod wiring;
