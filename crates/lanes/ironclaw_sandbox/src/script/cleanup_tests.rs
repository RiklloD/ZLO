//! Bounded, owned cleanup under a wedged Docker daemon: every `docker`
//! child is owned until waited, a failed worker spawn keeps orphans owned,
//! and one absolute deadline bounds the whole teardown.

use std::{
    io::{self, Cursor, Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use ironclaw_host_api::{
    ids::{CapabilityId, ExtensionId, InvocationId, TenantId, UserId},
    resource::ResourceScope,
};

use super::docker::{
    DockerCli, DockerScriptBackend, ProcessLauncher, ScriptChild, SystemDockerCli, TeardownBounds,
};
use super::orphans::{
    LabelledContainer, Orphan, OrphanBounds, OrphanReaper, ScriptInstance, WorkerSpawner,
    lock_held, thread_spawner,
};
use super::{ScriptBackend, ScriptBackendRequest, ScriptExecutionSlot};

#[derive(Default)]
struct Counters {
    spawned: AtomicUsize,
    waited: AtomicUsize,
    leaked_unwaited: AtomicUsize,
}

/// A process that runs until killed, then takes `dies_after` more polls to
/// exit — or exits at once once the daemon is no longer wedged.
struct WedgedChild {
    killed: bool,
    polls_after_kill: usize,
    dies_after: usize,
    wedged: Arc<AtomicBool>,
    waited: bool,
    counters: Arc<Counters>,
}

impl WedgedChild {
    fn exit(&mut self) -> io::Result<Option<i32>> {
        if !self.waited {
            self.waited = true;
            self.counters.waited.fetch_add(1, Ordering::SeqCst);
        }
        Ok(Some(0))
    }
}

impl ScriptChild for WedgedChild {
    fn take_stdin(&mut self) -> Option<Box<dyn Write + Send>> {
        Some(Box::new(io::sink()))
    }
    fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
        Some(Box::new(Cursor::new(Vec::new())))
    }
    fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>> {
        Some(Box::new(Cursor::new(Vec::new())))
    }
    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        if self.waited || !self.wedged.load(Ordering::SeqCst) {
            return self.exit();
        }
        if !self.killed {
            return Ok(None);
        }
        self.polls_after_kill += 1;
        if self.polls_after_kill >= self.dies_after {
            self.exit()
        } else {
            Ok(None)
        }
    }
    fn kill(&mut self) -> io::Result<()> {
        self.killed = true;
        Ok(())
    }
}

impl Drop for WedgedChild {
    fn drop(&mut self) {
        // Dropping a std Child never waits it: this would be a zombie.
        if !self.waited {
            self.counters.leaked_unwaited.fetch_add(1, Ordering::SeqCst);
        }
    }
}

/// Launches `docker` helpers that hang while `wedged` is set.
struct WedgedLauncher {
    wedged: Arc<AtomicBool>,
    counters: Arc<Counters>,
}

impl ProcessLauncher for WedgedLauncher {
    fn launch(&self, _args: &[String], _interactive: bool) -> io::Result<Box<dyn ScriptChild>> {
        self.counters.spawned.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(WedgedChild {
            killed: false,
            polls_after_kill: 0,
            dies_after: 1_000,
            wedged: Arc::clone(&self.wedged),
            waited: false,
            counters: Arc::clone(&self.counters),
        }))
    }
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_wedged_daemon_never_leaves_helper_children_unwaited() {
    let wedged = Arc::new(AtomicBool::new(true));
    let counters = Arc::new(Counters::default());
    let cli = Arc::new(SystemDockerCli::new(Arc::new(WedgedLauncher {
        wedged: Arc::clone(&wedged),
        counters: Arc::clone(&counters),
    })));
    let reaper = OrphanReaper::new(
        cli.clone(),
        OrphanBounds {
            backoff: vec![Duration::from_millis(5)],
            command: Duration::from_millis(30),
        },
        thread_spawner(),
    );
    reaper.adopt(Orphan {
        container_name: "wedged".to_string(),
        slot: ScriptExecutionSlot::unmetered(),
        child: None,
        container_gone: false,
    });

    // Every `rm`/`ps` the reaper retries hangs and is abandoned at its bound.
    wait_until("the reaper retried repeatedly", || {
        counters.spawned.load(Ordering::SeqCst) >= 10
    });
    assert_eq!(
        counters.leaked_unwaited.load(Ordering::SeqCst),
        0,
        "no helper child may be dropped unwaited"
    );

    // The daemon recovers: the orphan settles and every helper is waited.
    wedged.store(false, Ordering::SeqCst);
    wait_until("the orphan settled", || lock_held(&reaper.state).is_empty());
    wait_until("the janitor drained", || {
        cli.janitor.state.held().is_empty()
    });
    assert_eq!(
        counters.waited.load(Ordering::SeqCst),
        counters.spawned.load(Ordering::SeqCst),
        "every helper child was waited"
    );
    assert_eq!(counters.leaked_unwaited.load(Ordering::SeqCst), 0);
}

/// A CLI whose container is gone; only the orphans' children need settling.
struct GoneCli;

impl DockerCli for GoneCli {
    fn spawn_run(&self, _args: &[String]) -> io::Result<Box<dyn ScriptChild>> {
        Err(io::Error::other("not used"))
    }
    fn force_remove(&self, _name: &str, _bound: Duration) -> Result<(), String> {
        Ok(())
    }
    fn container_exists(&self, _name: &str, _bound: Duration) -> Result<bool, String> {
        Ok(false)
    }
    fn list_script_containers(&self, _bound: Duration) -> Result<Vec<LabelledContainer>, String> {
        Ok(Vec::new())
    }
}

#[test]
fn a_failed_reaper_worker_spawn_keeps_the_child_and_slot_owned() {
    let counters = Arc::new(Counters::default());
    let launcher = WedgedLauncher {
        wedged: Arc::new(AtomicBool::new(false)),
        counters: Arc::clone(&counters),
    };
    let spawn_attempts = Arc::new(AtomicUsize::new(0));
    let attempts = Arc::clone(&spawn_attempts);
    let real = thread_spawner();
    // The first worker spawn fails; later ones work.
    let spawner: WorkerSpawner = Arc::new(move |name, work| {
        if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(io::Error::other("injected thread spawn failure"));
        }
        real(name, work)
    });
    let reaper = OrphanReaper::new(
        Arc::new(GoneCli),
        OrphanBounds {
            backoff: vec![Duration::from_millis(5)],
            command: Duration::from_millis(30),
        },
        spawner,
    );
    let orphan = |name: &str| Orphan {
        container_name: name.to_string(),
        slot: ScriptExecutionSlot::unmetered(),
        child: Some(launcher.launch(&[], true).unwrap()),
        container_gone: false,
    };

    reaper.adopt(orphan("first"));
    {
        let held = lock_held(&reaper.state);
        let first = held.get("first").expect("the orphan stays held");
        assert!(first.child.is_some(), "its child stays owned, not dropped");
    }
    assert_eq!(counters.leaked_unwaited.load(Ordering::SeqCst), 0);

    // The next adoption retries the spawn; the worker settles both.
    reaper.adopt(orphan("second"));
    wait_until("both orphans settled", || {
        lock_held(&reaper.state).is_empty()
    });
    assert_eq!(counters.waited.load(Ordering::SeqCst), 2);
    assert_eq!(counters.leaked_unwaited.load(Ordering::SeqCst), 0);
}

/// Everything wedged: the CLI child never exits (even when killed), its
/// output pipe never closes, and every `rm`/`ps` uses its whole bound.
struct FullyWedgedCli {
    // Held so the stalled pipe stays open until the test ends.
    stalls: Mutex<Vec<mpsc::Sender<()>>>,
}

struct NeverExits;

impl ScriptChild for NeverExits {
    fn take_stdin(&mut self) -> Option<Box<dyn Write + Send>> {
        Some(Box::new(io::sink()))
    }
    fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
        None
    }
    fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>> {
        None
    }
    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        Ok(None)
    }
    fn kill(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct StalledReader(mpsc::Receiver<()>);

impl Read for StalledReader {
    fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
        // Ends only when the test drops the sender.
        match self.0.recv() {
            Ok(()) => Err(io::Error::other("unexpected")),
            Err(_) => Ok(0),
        }
    }
}

struct StalledPipesChild {
    stdout: Option<Box<dyn Read + Send>>,
    stderr: Option<Box<dyn Read + Send>>,
}

impl ScriptChild for StalledPipesChild {
    fn take_stdin(&mut self) -> Option<Box<dyn Write + Send>> {
        NeverExits.take_stdin()
    }
    fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
        self.stdout.take()
    }
    fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>> {
        self.stderr.take()
    }
    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        Ok(None)
    }
    fn kill(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl DockerCli for FullyWedgedCli {
    fn spawn_run(&self, _args: &[String]) -> io::Result<Box<dyn ScriptChild>> {
        let mut stalls = self.stalls.lock().unwrap();
        let mut pipe = || {
            let (sender, receiver) = mpsc::channel();
            stalls.push(sender);
            Some(Box::new(StalledReader(receiver)) as Box<dyn Read + Send>)
        };
        Ok(Box::new(StalledPipesChild {
            stdout: pipe(),
            stderr: pipe(),
        }))
    }
    fn force_remove(&self, _name: &str, bound: Duration) -> Result<(), String> {
        thread::sleep(bound);
        Err("docker rm hung".to_string())
    }
    fn container_exists(&self, _name: &str, bound: Duration) -> Result<bool, String> {
        thread::sleep(bound);
        Err("docker ps hung".to_string())
    }
    fn list_script_containers(&self, _bound: Duration) -> Result<Vec<LabelledContainer>, String> {
        Ok(Vec::new())
    }
}

#[test]
fn one_cleanup_deadline_bounds_teardown_drop_and_pipe_joins() {
    let bounds = TeardownBounds {
        total: Duration::from_millis(300),
        reap: Duration::from_millis(100),
        force_remove: Duration::from_millis(200),
        pipe_join: Duration::from_millis(150),
    };
    let cli = Arc::new(FullyWedgedCli {
        stalls: Mutex::new(Vec::new()),
    });
    let backend = DockerScriptBackend::from_parts(
        cli.clone(),
        bounds,
        OrphanBounds {
            backoff: vec![Duration::from_secs(60)],
            command: Duration::from_millis(30),
        },
        ScriptInstance::current(),
    );
    let wall_clock = Duration::from_millis(50);

    let started = Instant::now();
    let result = backend.execute(request(wall_clock), ScriptExecutionSlot::unmetered());
    let elapsed = started.elapsed();

    assert!(result.is_err(), "the script timed out");
    // Timeout detection polls at 10 ms; allow that plus scheduling slack.
    let epsilon = Duration::from_millis(150);
    assert!(
        elapsed <= wall_clock + bounds.total + epsilon,
        "teardown, the guard's drop, and pipe joins must all fit one cleanup deadline: {elapsed:?}"
    );
    assert_eq!(
        lock_held(&backend.reaper.state).len(),
        1,
        "what the deadline left unsettled is the reaper's"
    );
    drop(cli.stalls.lock().unwrap().drain(..));
}

fn request(wall_clock: Duration) -> ScriptBackendRequest {
    ScriptBackendRequest {
        provider: ExtensionId::new("script").unwrap(),
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
        args: Vec::new(),
        stdin_json: "{}".to_string(),
        max_stdout_bytes: 1024,
        max_stderr_bytes: 1024,
        max_wall_clock_ms: u64::try_from(wall_clock.as_millis()).unwrap(),
    }
}
