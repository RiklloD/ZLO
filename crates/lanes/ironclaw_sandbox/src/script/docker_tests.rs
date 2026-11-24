//! Docker script backend tests: argument shape, bounded reads, and the
//! bounded teardown of a timed-out script.
use std::io::Cursor;

#[cfg(unix)]
use std::{ffi::OsString, fs, sync::Mutex};

#[cfg(unix)]
use super::DockerScriptBackend;
use super::{
    docker_run_args, parse_labelled_containers, read_bounded, validate_docker_image_reference,
};
use crate::script::ScriptBackendRequest;
use crate::script::orphans::{LabelledContainer, ScriptInstance};
#[cfg(unix)]
use crate::script::{ScriptBackend, ScriptExecutionSlot};
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
        // `run` records its arguments and echoes JSON; the container guard's
        // removal check (`ps`) finds no container.
        "#!/bin/sh\nif [ \"$1\" != run ]; then exit 0; fi\nprintf '%s\\n' \"$@\" > \"$IRONCLAW_DOCKER_ARGS_FILE\"\ncat >/dev/null\nprintf '{\"ok\":true}'\n",
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
        .execute(sample_docker_request(), ScriptExecutionSlot::unmetered())
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
    assert_script_labels(&args[5..13]);
    assert_eq!(&args[13..16], &["--network", "none", "alpine:latest"]);
    assert_eq!(args[16], "script-echo");
    assert_eq!(args[17], "--json");
    assert_eq!(args[18], "--network=host");
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

    let args = docker_run_args(
        &request,
        "alpine:latest",
        "ironclaw-script-test",
        &ScriptInstance::current(),
    );

    assert_eq!(&args[..3], &["run", "--rm", "-i"]);
    assert_eq!(args[3], "--name");
    assert!(
        args[4].starts_with("ironclaw-script-"),
        "container must carry a host-assigned name: {}",
        args[4]
    );
    assert_script_labels(&args[5..13]);
    assert_eq!(&args[13..16], &["--network", "none", "alpine:latest"]);
    assert_eq!(args[16], "script-echo");
    assert_eq!(args[17], "--json");
    assert_eq!(args[18], "--network=host");
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

/// The four `--label` pairs: lane, instance, host, pid.
fn assert_script_labels<S: AsRef<str>>(labels: &[S]) {
    let labels: Vec<&str> = labels.iter().map(AsRef::as_ref).collect();
    assert_eq!(labels[0], "--label");
    assert_eq!(labels[1], "ironclaw.lane=script");
    assert_eq!(labels[2], "--label");
    assert!(labels[3].starts_with("ironclaw.instance="), "{}", labels[3]);
    assert_eq!(labels[4], "--label");
    assert!(labels[5].starts_with("ironclaw.host="), "{}", labels[5]);
    assert_eq!(labels[6], "--label");
    assert_eq!(labels[7], format!("ironclaw.pid={}", std::process::id()));
}

#[test]
fn docker_ps_listing_parses_names_and_labels() {
    let listing = "ironclaw-script-a\tinst-1\thost-a\t42\nironclaw-script-b\t\t\t\n\n";

    assert_eq!(
        parse_labelled_containers(listing),
        vec![
            LabelledContainer {
                name: "ironclaw-script-a".to_string(),
                instance: Some("inst-1".to_string()),
                host: Some("host-a".to_string()),
                pid: Some(42),
            },
            LabelledContainer {
                name: "ironclaw-script-b".to_string(),
                instance: None,
                host: None,
                pid: None,
            },
        ]
    );
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
            atomic::{AtomicBool, AtomicUsize, Ordering},
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
    use crate::script::orphans::{
        LabelledContainer, Orphan, OrphanBounds, OrphanReaper, ScriptInstance,
        reap_previous_instances,
    };
    use crate::script::{
        ScriptError, ScriptExecutionRequest, ScriptInvocation, ScriptRuntime, ScriptRuntimeConfig,
    };

    fn test_backend(cli: Arc<WedgedDockerCli>) -> DockerScriptBackend {
        DockerScriptBackend::from_parts(
            cli,
            TEST_BOUNDS,
            OrphanBounds {
                backoff: vec![Duration::from_millis(20)],
                command: Duration::from_millis(50),
            },
            ScriptInstance::current(),
        )
    }

    const TEST_BOUNDS: TeardownBounds = TeardownBounds {
        total: Duration::from_millis(400),
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
        /// `docker rm -f` calls that fail (leaving the container) before
        /// removal starts working.
        rm_failures_left: AtomicUsize,
        listing: Mutex<Vec<LabelledContainer>>,
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
                rm_failures_left: AtomicUsize::new(0),
                listing: Mutex::new(Vec::new()),
            })
        }

        fn with_rm_failures(self: Arc<Self>, failures: usize) -> Arc<Self> {
            self.rm_failures_left.store(failures, Ordering::SeqCst);
            self
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
            if self
                .rm_failures_left
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                    left.checked_sub(1)
                })
                .is_ok()
            {
                return Err("injected docker rm failure".to_string());
            }
            self.removed.store(true, Ordering::SeqCst);
            Ok(())
        }

        fn container_exists(
            &self,
            _container_name: &str,
            _bound: Duration,
        ) -> Result<bool, String> {
            Ok(!self.removed.load(Ordering::SeqCst))
        }

        fn list_script_containers(
            &self,
            _bound: Duration,
        ) -> Result<Vec<LabelledContainer>, String> {
            Ok(self.listing.lock().unwrap().clone())
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
        run_script(&timeout_runtime(test_backend(cli), 4)).await
    }

    fn timeout_runtime(
        backend: DockerScriptBackend,
        max_concurrent_executions: usize,
    ) -> ScriptRuntime<DockerScriptBackend> {
        ScriptRuntime::new(
            ScriptRuntimeConfig {
                max_wall_clock_ms: 50,
                // Generous: the backend's own bounded teardown must return
                // first, so the assertion is about the backend, not the
                // runtime's outer deadline.
                teardown_grace_ms: 10_000,
                max_concurrent_executions,
                ..ScriptRuntimeConfig::for_testing()
            },
            backend,
        )
    }

    async fn run_script(
        runtime: &ScriptRuntime<DockerScriptBackend>,
    ) -> (Result<(), ScriptError>, Duration, ResourceTally) {
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
        // Teardown's kill, then the guard's before handing the never-exiting
        // child to the reaper.
        assert!(*cli.kill_attempts.lock().unwrap() >= 1);
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
            TeardownBounds::DEFAULT.total < grace,
            "the runtime must not abandon a backend that is still inside its bounded teardown"
        );
    }

    fn held(reaper: &OrphanReaper) -> usize {
        crate::script::orphans::lock_held(&reaper.state).len()
    }

    #[tokio::test]
    async fn unverified_container_removal_keeps_the_slot_until_the_reaper_verifies_it() {
        // Teardown's rm fails, then the reaper's first two attempts fail too.
        let cli = WedgedDockerCli::new(false, true).with_rm_failures(3);
        let backend = test_backend(Arc::clone(&cli));
        let reaper = Arc::clone(&backend.reaper);
        let runtime = timeout_runtime(backend, 1);

        let (first, _, reserved) = run_script(&runtime).await;
        assert!(
            matches!(first, Err(ScriptError::Backend { .. })),
            "{first:?}"
        );
        assert_eq!(reserved, ResourceTally::default());
        assert_eq!(held(&reaper), 1, "the unverified container is the reaper's");

        // The container may still be running, so its slot is not re-admitted.
        let (second, _, _) = run_script(&runtime).await;
        assert!(
            matches!(second, Err(ScriptError::Resource(_))),
            "{second:?}"
        );
        assert_eq!(cli.spawned_args.lock().unwrap().len(), 1);

        // The reaper retries until removal is verified, then frees the slot.
        let deadline = Instant::now() + Duration::from_secs(5);
        while held(&reaper) > 0 {
            assert!(
                Instant::now() < deadline,
                "the reaper never verified removal"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(cli.removed_names.lock().unwrap().len() >= 4);
        let (third, _, _) = run_script(&runtime).await;
        assert!(
            matches!(third, Err(ScriptError::Backend { .. })),
            "the freed slot admits the next call: {third:?}"
        );
        assert_eq!(cli.spawned_args.lock().unwrap().len(), 2);
    }

    #[test]
    fn startup_reaper_removes_only_dead_previous_instances_on_this_host() {
        let current = ScriptInstance {
            instance: "current".to_string(),
            host: "this-host".to_string(),
            pid: 100,
        };
        const LIVE_PID: u32 = 200;
        let container =
            |name: &str, instance: Option<&str>, host: Option<&str>, pid: Option<u32>| {
                LabelledContainer {
                    name: name.to_string(),
                    instance: instance.map(str::to_string),
                    host: host.map(str::to_string),
                    pid,
                }
            };
        let cli = WedgedDockerCli::new(false, true);
        *cli.listing.lock().unwrap() = vec![
            container("dead-previous", Some("old"), Some("this-host"), Some(300)),
            container("ours", Some("current"), Some("this-host"), Some(100)),
            container("other-host", Some("old"), Some("elsewhere"), Some(300)),
            container(
                "live-sibling",
                Some("other"),
                Some("this-host"),
                Some(LIVE_PID),
            ),
            container("unlabelled", None, None, None),
        ];

        let removed =
            reap_previous_instances(cli.as_ref(), &current, Duration::from_millis(50), &|pid| {
                pid == LIVE_PID
            })
            .unwrap();

        assert_eq!(removed, 1);
        assert_eq!(
            *cli.removed_names.lock().unwrap(),
            vec!["dead-previous".to_string()]
        );
    }

    /// Every exit path of a spawned script settles through the container
    /// guard: the slot is released only once the container is verified gone
    /// and the CLI child waited; otherwise the reaper holds it.
    mod lifecycle {
        use std::sync::atomic::AtomicBool;

        use super::*;

        #[derive(Clone, Copy, PartialEq, Eq)]
        enum ChildMode {
            /// `try_wait` errors (a client hiccup) until the child is killed.
            PollError,
            /// Exits at once with `{"ok":true}`; the container lingers.
            ExitsOk,
            /// The first `try_wait` panics, unwinding through the backend.
            PanicOnFirstPoll,
            /// Exits only after this many polls following a kill.
            SlowExitAfterKill(usize),
        }

        #[derive(Default)]
        struct Counters {
            spawned: AtomicUsize,
            waited: AtomicUsize,
            leaked_unwaited: AtomicUsize,
        }

        /// A CLI whose container exists until a `docker rm -f` succeeds.
        struct ScriptedCli {
            mode: ChildMode,
            container_present: Arc<AtomicBool>,
            rm_failures_left: AtomicUsize,
            counters: Arc<Counters>,
        }

        impl ScriptedCli {
            fn new(mode: ChildMode, rm_failures: usize) -> Arc<Self> {
                Arc::new(Self {
                    mode,
                    container_present: Arc::new(AtomicBool::new(false)),
                    rm_failures_left: AtomicUsize::new(rm_failures),
                    counters: Arc::new(Counters::default()),
                })
            }
        }

        impl DockerCli for ScriptedCli {
            fn spawn_run(&self, _args: &[String]) -> io::Result<Box<dyn ScriptChild>> {
                self.counters.spawned.fetch_add(1, Ordering::SeqCst);
                self.container_present.store(true, Ordering::SeqCst);
                Ok(Box::new(ScriptedChild {
                    mode: self.mode,
                    killed: false,
                    polls: 0,
                    polls_after_kill: 0,
                    waited: false,
                    counters: Arc::clone(&self.counters),
                }))
            }

            fn force_remove(&self, _name: &str, _bound: Duration) -> Result<(), String> {
                if self
                    .rm_failures_left
                    .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                        left.checked_sub(1)
                    })
                    .is_ok()
                {
                    return Err("injected docker rm failure".to_string());
                }
                self.container_present.store(false, Ordering::SeqCst);
                Ok(())
            }

            fn container_exists(&self, _name: &str, _bound: Duration) -> Result<bool, String> {
                Ok(self.container_present.load(Ordering::SeqCst))
            }

            fn list_script_containers(
                &self,
                _bound: Duration,
            ) -> Result<Vec<LabelledContainer>, String> {
                Ok(Vec::new())
            }
        }

        struct ScriptedChild {
            mode: ChildMode,
            killed: bool,
            polls: usize,
            polls_after_kill: usize,
            waited: bool,
            counters: Arc<Counters>,
        }

        impl ScriptedChild {
            fn exit(&mut self, code: i32) -> io::Result<Option<i32>> {
                if !self.waited {
                    self.waited = true;
                    self.counters.waited.fetch_add(1, Ordering::SeqCst);
                }
                Ok(Some(code))
            }
        }

        impl ScriptChild for ScriptedChild {
            fn take_stdin(&mut self) -> Option<Box<dyn Write + Send>> {
                Some(Box::new(io::sink()))
            }

            fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
                Some(Box::new(Cursor::new(br#"{"ok":true}"#.to_vec())))
            }

            fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>> {
                Some(Box::new(Cursor::new(Vec::new())))
            }

            fn try_wait(&mut self) -> io::Result<Option<i32>> {
                self.polls += 1;
                if self.waited {
                    return Ok(Some(0));
                }
                match self.mode {
                    ChildMode::ExitsOk => self.exit(0),
                    ChildMode::PollError if self.killed => self.exit(137),
                    ChildMode::PollError => Err(io::Error::other("injected poll failure")),
                    ChildMode::PanicOnFirstPoll if self.polls == 1 => {
                        panic!("injected panic after spawn")
                    }
                    ChildMode::PanicOnFirstPoll => self.exit(137),
                    ChildMode::SlowExitAfterKill(_) if !self.killed => Ok(None),
                    ChildMode::SlowExitAfterKill(polls) => {
                        self.polls_after_kill += 1;
                        if self.polls_after_kill >= polls {
                            self.exit(137)
                        } else {
                            Ok(None)
                        }
                    }
                }
            }

            fn kill(&mut self) -> io::Result<()> {
                self.killed = true;
                Ok(())
            }
        }

        impl Drop for ScriptedChild {
            fn drop(&mut self) {
                // Dropping a std Child does not wait it: this is a zombie.
                if !self.waited {
                    self.counters.leaked_unwaited.fetch_add(1, Ordering::SeqCst);
                }
            }
        }

        fn backend(cli: Arc<ScriptedCli>) -> DockerScriptBackend {
            DockerScriptBackend::from_parts(
                cli,
                TEST_BOUNDS,
                OrphanBounds {
                    backoff: vec![Duration::from_millis(5), Duration::from_millis(20)],
                    command: Duration::from_millis(50),
                },
                ScriptInstance::current(),
            )
        }

        async fn wait_until_released(reaper: &OrphanReaper) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while held(reaper) > 0 {
                assert!(
                    Instant::now() < deadline,
                    "the reaper never settled the orphan"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }

        /// Drive `mode` through the runtime with a single-slot lane and
        /// assert the slot stays held until the reaper settles the orphan.
        async fn slot_held_until_reaped(
            mode: ChildMode,
            rm_failures: usize,
        ) -> (Result<(), ScriptError>, Arc<ScriptedCli>) {
            let cli = ScriptedCli::new(mode, rm_failures);
            let backend = backend(Arc::clone(&cli));
            let reaper = Arc::clone(&backend.reaper);
            let runtime = timeout_runtime(backend, 1);

            let (first, _, reserved) = run_script(&runtime).await;
            assert_eq!(reserved, ResourceTally::default());
            assert_eq!(held(&reaper), 1, "the unsettled container is the reaper's");
            let (second, _, _) = run_script(&runtime).await;
            assert!(
                matches!(second, Err(ScriptError::Resource(_))),
                "capacity must stay held while the container may run: {second:?}"
            );

            wait_until_released(&reaper).await;
            assert!(!cli.container_present.load(Ordering::SeqCst));
            let (third, _, _) = run_script(&runtime).await;
            assert!(
                !matches!(third, Err(ScriptError::Resource(_))),
                "the settled slot admits the next call: {third:?}"
            );
            (first, cli)
        }

        #[tokio::test]
        async fn a_polling_failure_keeps_the_slot_until_the_container_is_verified_gone() {
            let (first, cli) = slot_held_until_reaped(ChildMode::PollError, 3).await;
            assert!(
                matches!(first, Err(ScriptError::Backend { .. })),
                "{first:?}"
            );
            assert_eq!(cli.counters.leaked_unwaited.load(Ordering::SeqCst), 0);
        }

        #[tokio::test]
        async fn a_normal_exit_with_the_container_still_present_keeps_the_slot() {
            let (first, _cli) = slot_held_until_reaped(ChildMode::ExitsOk, 2).await;
            assert!(first.is_ok(), "the script itself succeeded: {first:?}");
        }

        #[tokio::test]
        async fn a_panic_after_spawn_keeps_the_slot_until_settled() {
            let (first, cli) = slot_held_until_reaped(ChildMode::PanicOnFirstPoll, 2).await;
            assert!(
                matches!(first, Err(ScriptError::Backend { .. })),
                "{first:?}"
            );
            assert_eq!(cli.counters.leaked_unwaited.load(Ordering::SeqCst), 0);
        }

        #[tokio::test]
        async fn repeated_timeouts_leave_no_unwaited_cli_children() {
            // Each killed CLI exits only well after the guard's bounded waits,
            // so the reaper must own and wait every one of them.
            let cli = ScriptedCli::new(ChildMode::SlowExitAfterKill(40), 0);
            let backend = backend(Arc::clone(&cli));
            let reaper = Arc::clone(&backend.reaper);
            let runtime = timeout_runtime(backend, 8);

            for _ in 0..5 {
                let (result, _, _) = run_script(&runtime).await;
                assert!(
                    matches!(result, Err(ScriptError::Backend { .. })),
                    "{result:?}"
                );
            }
            wait_until_released(&reaper).await;

            let counters = &cli.counters;
            assert_eq!(counters.spawned.load(Ordering::SeqCst), 5);
            assert_eq!(
                counters.waited.load(Ordering::SeqCst),
                5,
                "every CLI child is waited"
            );
            assert_eq!(
                counters.leaked_unwaited.load(Ordering::SeqCst),
                0,
                "no zombies"
            );
        }

        #[tokio::test]
        async fn a_panicking_reaper_attempt_does_not_stop_the_retries() {
            // The orphan's child panics on its first reaper poll.
            let cli = ScriptedCli::new(ChildMode::SlowExitAfterKill(1), 0);
            let reaper = Arc::new(OrphanReaper::new(
                cli.clone(),
                OrphanBounds {
                    backoff: vec![Duration::from_millis(5)],
                    command: Duration::from_millis(50),
                },
                crate::script::orphans::thread_spawner(),
            ));
            let panicking = PanicOnceChild {
                panicked: false,
                inner: cli.spawn_run(&[]).unwrap(),
            };
            reaper.adopt(Orphan {
                container_name: "orphan".to_string(),
                slot: crate::script::ScriptExecutionSlot::unmetered(),
                child: Some(Box::new(panicking)),
                container_gone: false,
            });

            wait_until_released(&reaper).await;
            assert_eq!(cli.counters.waited.load(Ordering::SeqCst), 1);
        }

        struct PanicOnceChild {
            panicked: bool,
            inner: Box<dyn ScriptChild>,
        }

        impl ScriptChild for PanicOnceChild {
            fn take_stdin(&mut self) -> Option<Box<dyn Write + Send>> {
                self.inner.take_stdin()
            }
            fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
                self.inner.take_stdout()
            }
            fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>> {
                self.inner.take_stderr()
            }
            fn try_wait(&mut self) -> io::Result<Option<i32>> {
                if !self.panicked {
                    self.panicked = true;
                    panic!("injected reaper attempt panic");
                }
                self.inner.try_wait()
            }
            fn kill(&mut self) -> io::Result<()> {
                self.inner.kill()
            }
        }
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
