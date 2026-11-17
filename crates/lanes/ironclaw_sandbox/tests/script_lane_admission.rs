//! Script lane admission: bounded concurrency, slots held by the worker until
//! the backend really finishes, and no late start after a timeout.
#![deny(clippy::let_underscore_must_use)]
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

use ironclaw_extension_registry::*;
use ironclaw_host_api::{
    host_port::HostPortCatalog,
    ids::{CapabilityId, InvocationId, TenantId, UserId},
    path::VirtualPath,
    resource::{ResourceEstimate, ResourceScope, RuntimeResourceErrorKind},
};
use ironclaw_resources::*;
use ironclaw_sandbox::*;
use serde_json::json;

/// Blocks each execution until the test opens the gate (drops the sender) or
/// 10 s pass; counts how many executions actually started and finished.
#[derive(Clone)]
struct GatedBackend {
    gate: Arc<Mutex<mpsc::Receiver<()>>>,
    started: Arc<AtomicUsize>,
    finished: Arc<AtomicUsize>,
}

impl GatedBackend {
    fn new() -> (mpsc::Sender<()>, Self) {
        let (sender, receiver) = mpsc::channel();
        (
            sender,
            Self {
                gate: Arc::new(Mutex::new(receiver)),
                started: Arc::new(AtomicUsize::new(0)),
                finished: Arc::new(AtomicUsize::new(0)),
            },
        )
    }
}

impl ScriptBackend for GatedBackend {
    fn execute(
        &self,
        _request: ScriptBackendRequest,
        _slot: ScriptExecutionSlot,
    ) -> Result<ScriptBackendOutput, String> {
        self.started.fetch_add(1, Ordering::SeqCst);
        let ended = self
            .gate
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(10));
        self.finished.fetch_add(1, Ordering::SeqCst);
        match ended {
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Ok(ScriptBackendOutput::json(json!({"ok": true})))
            }
            other => Err(format!("gate ended unexpectedly: {other:?}")),
        }
    }
}

fn lane_config(max_concurrent_executions: usize) -> ScriptRuntimeConfig {
    ScriptRuntimeConfig {
        max_wall_clock_ms: 50,
        teardown_grace_ms: 50,
        max_concurrent_executions,
        ..ScriptRuntimeConfig::for_testing()
    }
}

async fn run(
    runtime: &ScriptRuntime<GatedBackend>,
    governor: &InMemoryResourceGovernor,
) -> Result<ScriptExecutionResult, ScriptError> {
    let package = script_package();
    let capability_id = CapabilityId::new("script.echo").unwrap();
    runtime
        .execute_extension_json(
            &GovernorRuntimeBudget::new(governor),
            ScriptExecutionRequest {
                extension: &package.id,
                capabilities: &package.capabilities,
                runtime: &package.manifest.runtime,
                capability_id: &capability_id,
                scope: sample_scope(),
                estimate: ResourceEstimate::default().set_concurrency_slots(1),
                mounts: None,
                resource_reservation: None,
                invocation: ScriptInvocation { input: json!({}) },
            },
        )
        .await
}

async fn wait_until(mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done() {
        assert!(Instant::now() < deadline, "condition not reached in time");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn a_stuck_backend_keeps_its_slot_so_admission_stays_bounded() {
    let (open_gate, backend) = GatedBackend::new();
    let runtime = ScriptRuntime::new(lane_config(1), backend.clone());
    let governor = InMemoryResourceGovernor::new();
    let account = ResourceAccount::tenant(sample_scope().tenant_id);

    let first = run(&runtime, &governor).await;
    assert!(
        matches!(first, Err(ScriptError::Timeout { .. })),
        "{first:?}"
    );
    assert_eq!(backend.started.load(Ordering::SeqCst), 1);

    // The timed-out backend is still running and still owns the only slot:
    // the next call is refused at admission, never queued or started.
    let second = run(&runtime, &governor).await;
    match second {
        Err(ScriptError::Resource(error)) => {
            assert_eq!(error.kind(), RuntimeResourceErrorKind::LimitExceeded)
        }
        other => panic!("expected the lane to be full, got {other:?}"),
    }
    assert_eq!(backend.started.load(Ordering::SeqCst), 1);
    assert_eq!(governor.reserved_for(&account), ResourceTally::default());

    // Once the stuck backend really finishes, its slot comes back.
    drop(open_gate);
    let finished = Arc::clone(&backend.finished);
    wait_until(|| finished.load(Ordering::SeqCst) == 1).await;
    let mut third = run(&runtime, &governor).await;
    for _ in 0..100 {
        // The worker drops its slot just after the backend returns.
        if !matches!(third, Err(ScriptError::Resource(_))) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
        third = run(&runtime, &governor).await;
    }
    assert!(third.is_ok(), "{third:?}");
    assert_eq!(backend.started.load(Ordering::SeqCst), 2);
}

#[test]
fn a_timed_out_job_queued_behind_a_saturated_blocking_pool_never_starts() {
    let tokio = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    tokio.block_on(async {
        // Occupy the only blocking thread so the script job can only queue.
        let (release_pool, held) = mpsc::channel::<()>();
        let occupier = tokio::task::spawn_blocking(move || {
            // Released (sender dropped) or 10 s: either ends the occupation.
            let ended = held.recv_timeout(Duration::from_secs(10));
            ended.is_err()
        });
        let (_open_gate, backend) = GatedBackend::new();
        let runtime = ScriptRuntime::new(lane_config(4), backend.clone());
        let governor = InMemoryResourceGovernor::new();

        let timed_out = run(&runtime, &governor).await;
        assert!(
            matches!(timed_out, Err(ScriptError::Timeout { .. })),
            "{timed_out:?}"
        );

        // Free the pool: the queued job now gets a thread, sees it expired,
        // and must refuse to start the script.
        drop(release_pool);
        assert!(occupier.await.unwrap());
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            backend.started.load(Ordering::SeqCst),
            0,
            "a job whose call already timed out must never start"
        );
    });
}

fn script_package() -> ExtensionPackage {
    let mut contracts = HostApiContractRegistry::new();
    contracts
        .register(Arc::new(CapabilityProviderHostApiContract::new().unwrap()))
        .unwrap();
    let manifest = ExtensionManifest::parse(
        SCRIPT_MANIFEST,
        ManifestSource::InstalledLocal,
        &HostPortCatalog::empty(),
        &contracts,
    )
    .unwrap();
    let root = VirtualPath::new(format!("/system/extensions/{}", manifest.id.as_str())).unwrap();
    ExtensionPackage::from_manifest(manifest, root).unwrap()
}

fn sample_scope() -> ResourceScope {
    ResourceScope {
        tenant_id: TenantId::new("tenant1").unwrap(),
        user_id: UserId::new("user1").unwrap(),
        agent_id: None,
        project_id: None,
        mission_id: None,
        thread_id: None,
        invocation_id: InvocationId::new(),
    }
}

const SCRIPT_MANIFEST: &str = r#"schema_version = "reborn.extension_manifest.v2"
id = "script"
name = "Script Echo"
version = "0.1.0"
description = "Script demo extension"
trust = "untrusted"

[runtime]
kind = "script"
runner = "docker"
image = "alpine:latest"
command = "script-echo"
args = ["--json"]

[[host_api]]
id = "ironclaw.capability_provider/v1"
section = "capability_provider.tools"

[capability_provider.tools]

[[capability_provider.tools.capabilities]]
id = "script.echo"
description = "Echo text"
effects = ["dispatch_capability"]
default_permission = "allow"
visibility = "api"
input_schema_ref = "schemas/script/echo.input.v1.json"
output_schema_ref = "schemas/script/echo.output.v1.json"
"#;
