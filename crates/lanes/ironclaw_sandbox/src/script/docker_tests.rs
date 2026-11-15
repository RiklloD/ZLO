//! Docker script backend tests: argument shape, bounded reads, and the
//! bounded teardown of a timed-out script.
use std::io::Cursor;

#[cfg(unix)]
use std::{ffi::OsString, fs, sync::Mutex};

#[cfg(unix)]
use super::DockerScriptBackend;
use super::{
    SCRIPT_CONTAINER_LABEL, docker_run_args, read_bounded, validate_docker_image_reference,
};
#[cfg(unix)]
use crate::script::ScriptBackend;
use crate::script::ScriptBackendRequest;
use ironclaw_host_api::{
    ids::{CapabilityId, InvocationId, TenantId, UserId},
    resource::ResourceScope,
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[cfg(unix)]
static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn read_bounded_rejects_output_larger_than_limit() {
    let err = read_bounded(Cursor::new(b"abcdef"), 4).unwrap_err();
    assert!(err.contains("output exceeded 4 bytes"));
}

#[test]
fn read_bounded_allows_output_at_limit() {
    let output = read_bounded(Cursor::new(b"abcd"), 4).unwrap();
    assert_eq!(output, b"abcd");
}

#[test]
fn docker_image_reference_rejects_cli_flag_injection() {
    let err = validate_docker_image_reference("--network=host").unwrap_err();
    assert!(err.contains("must not start with '-'"));
}

#[test]
fn docker_image_reference_rejects_whitespace() {
    let err = validate_docker_image_reference("alpine --privileged").unwrap_err();
    assert!(err.contains("must not contain whitespace"));
}

#[test]
#[cfg(unix)]
fn docker_backend_disables_ambient_network_before_image_and_manifest_args() {
    let fake_bin = tempfile::tempdir().unwrap();
    let args_file = fake_bin.path().join("docker-args.txt");
    let docker_path = fake_bin.path().join("docker");
    fs::write(
        &docker_path,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$IRONCLAW_DOCKER_ARGS_FILE\"\ncat >/dev/null\nprintf '{\"ok\":true}'\n",
    )
    .unwrap();
    let mut permissions = fs::metadata(&docker_path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&docker_path, permissions).unwrap();

    let _lock = ENV_LOCK.lock().unwrap();
    let old_path = std::env::var_os("PATH");
    let old_args_file = std::env::var_os("IRONCLAW_DOCKER_ARGS_FILE");
    let _restore = EnvRestore(vec![
        ("PATH", old_path.clone()),
        ("IRONCLAW_DOCKER_ARGS_FILE", old_args_file),
    ]);
    let mut paths = vec![fake_bin.path().to_path_buf()];
    if let Some(old_path) = old_path.as_ref() {
        paths.extend(std::env::split_paths(old_path));
    }
    let path = std::env::join_paths(paths).unwrap();
    unsafe {
        std::env::set_var("PATH", path);
        std::env::set_var("IRONCLAW_DOCKER_ARGS_FILE", &args_file);
    }

    let output = DockerScriptBackend::default()
        .execute(sample_docker_request())
        .unwrap();

    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout, br#"{"ok":true}"#);
    let recorded = fs::read_to_string(args_file).unwrap();
    let args = recorded.lines().collect::<Vec<_>>();
    assert_eq!(&args[..3], &["run", "--rm", "-i"]);
    assert_eq!(args[3], "--name");
    assert!(
        args[4].starts_with("ironclaw-script-"),
        "container must carry a host-assigned name: {}",
        args[4]
    );
    assert_eq!(
        &args[5..10],
        &[
            "--label",
            SCRIPT_CONTAINER_LABEL,
            "--network",
            "none",
            "alpine:latest"
        ]
    );
    assert_eq!(args[10], "script-echo");
    assert_eq!(args[11], "--json");
    assert_eq!(args[12], "--network=host");
    let image_index = args
        .iter()
        .position(|arg| *arg == "alpine:latest")
        .expect("image must be present");
    let network_index = args
        .iter()
        .position(|arg| *arg == "--network")
        .expect("network flag must be present");
    let command_index = args
        .iter()
        .position(|arg| *arg == "script-echo")
        .expect("command must be present");
    let manifest_arg_index = args
        .iter()
        .position(|arg| *arg == "--network=host")
        .expect("manifest arg must be preserved after the command");
    assert!(network_index < image_index);
    assert!(image_index < command_index);
    assert!(command_index < manifest_arg_index);
}

#[test]
fn docker_run_args_disable_ambient_network_before_image_and_manifest_args() {
    let request = sample_docker_request();

    let args = docker_run_args(&request, "alpine:latest", "ironclaw-script-test");

    assert_eq!(&args[..3], &["run", "--rm", "-i"]);
    assert_eq!(args[3], "--name");
    assert!(
        args[4].starts_with("ironclaw-script-"),
        "container must carry a host-assigned name: {}",
        args[4]
    );
    assert_eq!(
        &args[5..10],
        &[
            "--label",
            SCRIPT_CONTAINER_LABEL,
            "--network",
            "none",
            "alpine:latest"
        ]
    );
    assert_eq!(args[10], "script-echo");
    assert_eq!(args[11], "--json");
    assert_eq!(args[12], "--network=host");
    let image_index = args
        .iter()
        .position(|arg| arg == "alpine:latest")
        .expect("image must be present");
    let network_index = args
        .iter()
        .position(|arg| arg == "--network")
        .expect("network flag must be present");
    let command_index = args
        .iter()
        .position(|arg| arg == "script-echo")
        .expect("command must be present");
    let manifest_arg_index = args
        .iter()
        .position(|arg| arg == "--network=host")
        .expect("manifest arg must be preserved after the command");
    assert!(network_index < image_index);
    assert!(image_index < command_index);
    assert!(command_index < manifest_arg_index);
}

fn sample_docker_request() -> ScriptBackendRequest {
    ScriptBackendRequest {
        provider: ironclaw_host_api::ids::ExtensionId::new("script").unwrap(),
        capability_id: CapabilityId::new("script.echo").unwrap(),
        scope: ResourceScope {
            tenant_id: TenantId::new("tenant1").unwrap(),
            user_id: UserId::new("user1").unwrap(),
            agent_id: None,
            project_id: None,
            mission_id: None,
            thread_id: None,
            invocation_id: InvocationId::new(),
        },
        runner: "docker".to_string(),
        image: Some("alpine:latest".to_string()),
        command: "script-echo".to_string(),
        args: vec!["--json".to_string(), "--network=host".to_string()],
        stdin_json: "{}".to_string(),
        max_stdout_bytes: 1024,
        max_stderr_bytes: 1024,
        max_wall_clock_ms: 1_000,
    }
}

#[cfg(unix)]
struct EnvRestore(Vec<(&'static str, Option<OsString>)>);

#[cfg(unix)]
impl Drop for EnvRestore {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            unsafe {
                if let Some(value) = value {
                    std::env::set_var(key, value);
                } else {
                    std::env::remove_var(key);
                }
            }
        }
    }
}

mod teardown {
    use std::{
        io::{self, Cursor, Read, Write},
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        time::{Duration, Instant},
    };

    use ironclaw_extension_registry::{
        CapabilityProviderHostApiContract, ExtensionManifest, ExtensionPackage,
        HostApiContractRegistry, ManifestSource,
    };
    use ironclaw_host_api::{
        host_port::HostPortCatalog,
        ids::{CapabilityId, InvocationId, TenantId, UserId},
        path::VirtualPath,
        resource::{ResourceEstimate, ResourceScope},
    };
    use ironclaw_resources::{
        GovernorRuntimeBudget, InMemoryResourceGovernor, ResourceAccount, ResourceTally,
    };
    use serde_json::json;

    use super::super::{DockerCli, DockerScriptBackend, ScriptChild, TeardownBounds};
    use crate::script::{
        ScriptError, ScriptExecutionRequest, ScriptInvocation, ScriptRuntime, ScriptRuntimeConfig,
    };

    const TEST_BOUNDS: TeardownBounds = TeardownBounds {
        reap: Duration::from_millis(50),
        force_remove: Duration::from_millis(50),
        pipe_join: Duration::from_millis(50),
    };

    /// A Docker CLI whose child refuses `kill`, whose stdout never closes,
    /// and which exits only if `exits_after_remove` and the container was
    /// force-removed.
    struct WedgedDockerCli {
        exits_after_remove: bool,
        kill_fails: bool,
        removed: Arc<AtomicBool>,
        removed_names: Mutex<Vec<String>>,
        spawned_args: Mutex<Vec<Vec<String>>>,
        kill_attempts: Arc<Mutex<u32>>,
        // Held so the stalled stdout stays open until the test ends.
        stall_handles: Mutex<Vec<mpsc::Sender<()>>>,
    }

    impl WedgedDockerCli {
        fn new(kill_fails: bool, exits_after_remove: bool) -> Arc<Self> {
            Arc::new(Self {
                exits_after_remove,
                kill_fails,
                removed: Arc::new(AtomicBool::new(false)),
                removed_names: Mutex::new(Vec::new()),
                spawned_args: Mutex::new(Vec::new()),
                kill_attempts: Arc::new(Mutex::new(0)),
                stall_handles: Mutex::new(Vec::new()),
            })
        }
    }

    impl DockerCli for WedgedDockerCli {
        fn spawn_run(&self, args: &[String]) -> io::Result<Box<dyn ScriptChild>> {
            self.spawned_args.lock().unwrap().push(args.to_vec());
            let (stall_sender, stall_receiver) = mpsc::channel();
            self.stall_handles.lock().unwrap().push(stall_sender);
            Ok(Box::new(WedgedChild {
                stdout: Some(Box::new(StalledReader(stall_receiver))),
                kill_fails: self.kill_fails,
                kill_attempts: Arc::clone(&self.kill_attempts),
                removed: Arc::clone(&self.removed),
                exits_after_remove: self.exits_after_remove,
            }))
        }

        fn force_remove(&self, container_name: &str, _bound: Duration) -> Result<(), String> {
            self.removed_names
                .lock()
                .unwrap()
                .push(container_name.to_string());
            self.removed.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    struct WedgedChild {
        stdout: Option<Box<dyn Read + Send>>,
        kill_fails: bool,
        kill_attempts: Arc<Mutex<u32>>,
        removed: Arc<AtomicBool>,
        exits_after_remove: bool,
    }

    impl ScriptChild for WedgedChild {
        fn take_stdin(&mut self) -> Option<Box<dyn Write + Send>> {
            Some(Box::new(io::sink()))
        }

        fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
            self.stdout.take()
        }

        fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>> {
            Some(Box::new(Cursor::new(Vec::new())))
        }

        fn try_wait(&mut self) -> io::Result<Option<i32>> {
            if self.exits_after_remove && self.removed.load(Ordering::SeqCst) {
                return Ok(Some(137));
            }
            Ok(None)
        }

        fn kill(&mut self) -> io::Result<()> {
            *self.kill_attempts.lock().unwrap() += 1;
            if self.kill_fails {
                Err(io::Error::other("injected kill failure"))
            } else {
                Ok(())
            }
        }
    }

    /// A pipe that blocks until its sender is dropped.
    struct StalledReader(mpsc::Receiver<()>);

    impl Read for StalledReader {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            match self.0.recv() {
                Ok(()) => Err(io::Error::other("unexpected stall signal")),
                Err(_) => Ok(0),
            }
        }
    }

    /// Drive one timed-out script through `ScriptRuntime` (the backend's
    /// caller) over the real resource governor.
    async fn run_timed_out_script(
        cli: Arc<WedgedDockerCli>,
    ) -> (Result<(), ScriptError>, Duration, ResourceTally) {
        let runtime = ScriptRuntime::new(
            ScriptRuntimeConfig {
                max_wall_clock_ms: 50,
                // Generous: the backend's own bounded teardown must return
                // first, so the assertion is about the backend, not the
                // runtime's outer deadline.
                teardown_grace_ms: 10_000,
                ..ScriptRuntimeConfig::for_testing()
            },
            DockerScriptBackend {
                cli,
                bounds: TEST_BOUNDS,
            },
        );
        let governor = InMemoryResourceGovernor::new();
        let scope = sample_scope();
        let account = ResourceAccount::tenant(scope.tenant_id.clone());
        let package = script_package();
        let capability_id = CapabilityId::new("script.echo").unwrap();

        let started = Instant::now();
        let result = runtime
            .execute_extension_json(
                &GovernorRuntimeBudget::new(&governor),
                ScriptExecutionRequest {
                    extension: &package.id,
                    capabilities: &package.capabilities,
                    runtime: &package.manifest.runtime,
                    capability_id: &capability_id,
                    scope,
                    estimate: ResourceEstimate::default().set_concurrency_slots(1),
                    mounts: None,
                    resource_reservation: None,
                    invocation: ScriptInvocation { input: json!({}) },
                },
            )
            .await
            .map(|_| ());
        (result, started.elapsed(), governor.reserved_for(&account))
    }

    #[tokio::test]
    async fn timed_out_script_with_failed_kill_and_stalled_pipe_returns_within_bound() {
        let cli = WedgedDockerCli::new(true, false);

        let (result, elapsed, reserved) = run_timed_out_script(Arc::clone(&cli)).await;

        assert!(
            elapsed < Duration::from_secs(3),
            "teardown must be bounded even when kill fails and a pipe never closes: {elapsed:?}"
        );
        match result {
            Err(ScriptError::Backend { reason }) => {
                assert!(reason.contains("timed out"), "{reason}");
            }
            other => panic!("expected the script timeout outcome, got {other:?}"),
        }
        assert_eq!(
            reserved,
            ResourceTally::default(),
            "the reservation must be released after a wedged teardown"
        );
        assert_eq!(*cli.kill_attempts.lock().unwrap(), 1);
        let spawned = cli.spawned_args.lock().unwrap();
        let name_index = spawned[0].iter().position(|arg| arg == "--name").unwrap();
        assert_eq!(
            *cli.removed_names.lock().unwrap(),
            vec![spawned[0][name_index + 1].clone()],
            "the container must be force-removed by the name it was started with"
        );
    }

    #[tokio::test]
    async fn timed_out_script_container_is_removed_even_when_the_cli_kill_succeeds() {
        // Killing `docker run` does not stop the container; without the
        // force-remove a timed-out script kept running in the background.
        let cli = WedgedDockerCli::new(false, true);

        let (result, elapsed, reserved) = run_timed_out_script(Arc::clone(&cli)).await;

        assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
        assert!(
            matches!(result, Err(ScriptError::Backend { .. })),
            "{result:?}"
        );
        assert_eq!(reserved, ResourceTally::default());
        assert_eq!(cli.removed_names.lock().unwrap().len(), 1);
    }

    #[test]
    fn teardown_bounds_fit_inside_the_default_runtime_grace() {
        let grace = Duration::from_millis(ScriptRuntimeConfig::default().teardown_grace_ms);
        assert!(
            worst_case(TeardownBounds::DEFAULT) < grace,
            "the runtime must not abandon a backend that is still inside its bounded teardown"
        );
    }

    /// Worst case: two reap waits (before and after the force-remove), one
    /// force-remove, and three pipe joins.
    fn worst_case(bounds: TeardownBounds) -> Duration {
        bounds.reap * 2 + bounds.force_remove + bounds.pipe_join * 3
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
        let root =
            VirtualPath::new(format!("/system/extensions/{}", manifest.id.as_str())).unwrap();
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
}
