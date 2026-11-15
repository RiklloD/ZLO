//! Docker CLI backend for the script lane, with bounded timeout teardown.
//!
//! The backend runs `docker run` as a host child process. A script that
//! exceeds its wall-clock limit is torn down in bounded steps — kill the CLI
//! child, force-remove the container by its host-assigned name, wait a bounded
//! time for the child to exit, then join the pipe threads with a deadline —
//! and every step that cannot finish in time is abandoned rather than waited
//! on. Nothing here blocks without a deadline once the timeout fires, so a
//! wedged Docker CLI cannot hold the calling thread (and with it the resource
//! reservation) indefinitely.

use std::{
    fmt,
    io::{self, Read, Write},
    process::{Command, Stdio},
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use super::{ScriptBackend, ScriptBackendOutput, ScriptBackendRequest};

/// Docker label every script container carries, so an operator (or a later
/// sweep) can find script-lane containers left behind by a failed teardown.
pub(crate) const SCRIPT_CONTAINER_LABEL: &str = "ironclaw.lane=script";

/// Prefix of the host-assigned container name (`<prefix><uuid>`).
const SCRIPT_CONTAINER_NAME_PREFIX: &str = "ironclaw-script-";

/// How long the backend waits between process polls.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Bounds for each teardown step after a script times out. Their sum is the
/// worst-case time the backend spends past the wall-clock limit; it must stay
/// below `ScriptRuntimeConfig::teardown_grace_ms`, the runtime's outer
/// deadline (pinned by `teardown_bounds_fit_inside_the_default_runtime_grace`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TeardownBounds {
    /// Bounded wait for the CLI child to exit after `kill`.
    pub(crate) reap: Duration,
    /// Bound on the `docker rm -f` used to stop the container itself.
    pub(crate) force_remove: Duration,
    /// Bound on joining each stdin/stdout/stderr pipe thread.
    pub(crate) pipe_join: Duration,
}

impl TeardownBounds {
    pub(crate) const DEFAULT: Self = Self {
        reap: Duration::from_secs(2),
        force_remove: Duration::from_secs(5),
        pipe_join: Duration::from_secs(1),
    };
}

/// A spawned `docker run` child as the backend sees it.
pub(crate) trait ScriptChild: Send {
    fn take_stdin(&mut self) -> Option<Box<dyn Write + Send>>;
    fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>>;
    fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>>;
    /// `Ok(Some(code))` once exited (`-1` when killed by a signal).
    fn try_wait(&mut self) -> io::Result<Option<i32>>;
    fn kill(&mut self) -> io::Result<()>;
}

/// The Docker CLI operations the backend needs.
///
/// Test seam: the production implementation shells out to `docker`; the
/// crate's teardown tests substitute a CLI whose kill fails and whose pipes
/// never close.
pub(crate) trait DockerCli: Send + Sync {
    fn spawn_run(&self, args: &[String]) -> io::Result<Box<dyn ScriptChild>>;
    /// Stop and remove the named container, giving up after `bound`.
    fn force_remove(&self, container_name: &str, bound: Duration) -> Result<(), String>;
}

/// Docker CLI backend for V1 script execution.
///
/// This backend intentionally accepts only normalized manifest-derived command
/// fields. It does not expose raw Docker flags to extensions, does not mount host
/// paths, does not pass host environment variables, and disables container
/// network access by default.
#[derive(Clone)]
pub struct DockerScriptBackend {
    cli: Arc<dyn DockerCli>,
    bounds: TeardownBounds,
}

impl fmt::Debug for DockerScriptBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DockerScriptBackend")
            .field("bounds", &self.bounds)
            .finish_non_exhaustive()
    }
}

impl Default for DockerScriptBackend {
    fn default() -> Self {
        Self {
            cli: Arc::new(SystemDockerCli),
            bounds: TeardownBounds::DEFAULT,
        }
    }
}

impl ScriptBackend for DockerScriptBackend {
    fn execute(&self, request: ScriptBackendRequest) -> Result<ScriptBackendOutput, String> {
        if request.runner != "docker" {
            return Err(format!(
                "DockerScriptBackend cannot execute runner {}",
                request.runner
            ));
        }
        let image = request
            .image
            .clone()
            .ok_or_else(|| "DockerScriptBackend requires an image".to_string())?;
        validate_docker_image_reference(&image)?;

        let container_name = format!(
            "{SCRIPT_CONTAINER_NAME_PREFIX}{}",
            uuid::Uuid::new_v4().simple()
        );
        execute_docker_request(
            self.cli.as_ref(),
            self.bounds,
            &request,
            &image,
            &container_name,
        )
    }
}

pub(crate) fn validate_docker_image_reference(image: &str) -> Result<(), String> {
    if image.is_empty() {
        return Err("Docker image reference must not be empty".to_string());
    }
    if image.starts_with('-') {
        return Err("Docker image reference must not start with '-'".to_string());
    }
    if image.chars().any(char::is_whitespace) {
        return Err("Docker image reference must not contain whitespace".to_string());
    }
    Ok(())
}

/// A pipe thread whose result is delivered over a channel, so the backend can
/// join it with a deadline and abandon it if the pipe never closes.
struct PipeThread<T> {
    name: &'static str,
    result: mpsc::Receiver<Result<T, String>>,
}

impl<T: Send + 'static> PipeThread<T> {
    fn spawn(
        name: &'static str,
        work: impl FnOnce() -> Result<T, String> + Send + 'static,
    ) -> Self {
        let (sender, result) = mpsc::sync_channel(1);
        thread::spawn(move || {
            // The receiver is gone only when the backend already abandoned
            // this pipe; the value has no reader left.
            if sender.send(work()).is_err() {
                tracing::debug!(pipe = name, "script pipe finished after being abandoned");
            }
        });
        Self { name, result }
    }

    /// Join with a deadline. `Err` covers a panicked thread (the sender was
    /// dropped without a value), an I/O failure, and an elapsed deadline; in
    /// the last case the thread is detached and left to finish on its own.
    fn join(self, bound: Duration) -> Result<T, String> {
        match self.result.recv_timeout(bound) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                tracing::debug!(
                    pipe = self.name,
                    bound_ms = bound.as_millis() as u64,
                    "abandoning a script pipe thread that did not finish in time"
                );
                Err(format!("script {} pipe did not close", self.name))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(format!("script {} pipe thread panicked", self.name))
            }
        }
    }
}

fn execute_docker_request(
    cli: &dyn DockerCli,
    bounds: TeardownBounds,
    request: &ScriptBackendRequest,
    image: &str,
    container_name: &str,
) -> Result<ScriptBackendOutput, String> {
    let started = Instant::now();
    let mut child = cli
        .spawn_run(&docker_run_args(request, image, container_name))
        .map_err(|error| error.to_string())?;

    let stdout = child
        .take_stdout()
        .ok_or_else(|| "Docker child stdout was not captured".to_string())?;
    let stderr = child
        .take_stderr()
        .ok_or_else(|| "Docker child stderr was not captured".to_string())?;
    let stdout_limit = request.max_stdout_bytes;
    let stderr_limit = request.max_stderr_bytes;
    let stdout_reader = PipeThread::spawn("stdout", move || read_bounded(stdout, stdout_limit));
    let stderr_reader = PipeThread::spawn("stderr", move || read_bounded(stderr, stderr_limit));

    let stdin_json = request.stdin_json.clone();
    let stdin_writer = child.take_stdin().map(|mut stdin| {
        PipeThread::spawn("stdin", move || {
            stdin
                .write_all(stdin_json.as_bytes())
                .map_err(|error| error.to_string())
            // `stdin` drops here, closing the pipe so the script sees EOF.
        })
    });

    let timeout = Duration::from_millis(request.max_wall_clock_ms);
    let exit_code = loop {
        if let Some(code) = child.try_wait().map_err(|error| error.to_string())? {
            break code;
        }
        if started.elapsed() >= timeout {
            tear_down_timed_out_script(
                cli,
                bounds,
                child,
                container_name,
                [Some(stdout_reader), Some(stderr_reader)],
                stdin_writer,
            );
            return Err(format!(
                "script timed out after {} ms",
                request.max_wall_clock_ms
            ));
        }
        thread::sleep(POLL_INTERVAL);
    };

    // The child exited, so its pipe ends are closed unless something else
    // still holds them; join with the same bound instead of trusting that.
    if let Some(stdin_writer) = stdin_writer {
        stdin_writer.join(bounds.pipe_join)?;
    }
    let stdout = stdout_reader.join(bounds.pipe_join)?;
    let stderr = stderr_reader.join(bounds.pipe_join)?;

    Ok(ScriptBackendOutput {
        exit_code,
        stdout,
        stderr,
        wall_clock_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
    })
}

/// Bounded teardown of a timed-out script. Every step is best-effort and
/// bounded; failures are traced at debug level and never extend the wait.
fn tear_down_timed_out_script(
    cli: &dyn DockerCli,
    bounds: TeardownBounds,
    mut child: Box<dyn ScriptChild>,
    container_name: &str,
    readers: [Option<PipeThread<Vec<u8>>>; 2],
    stdin_writer: Option<PipeThread<()>>,
) {
    if let Err(error) = child.kill() {
        tracing::debug!(%error, "killing a timed-out script's docker child failed");
    }
    let mut exited = wait_for_exit(child.as_mut(), bounds.reap);

    // Killing the CLI does not stop the container (`--rm` only applies once
    // the CLI observes the exit), so the container is removed by name whether
    // or not the kill worked. This is also what unblocks a CLI that survived
    // its kill: once the container is gone, `docker run` returns.
    if let Err(error) = cli.force_remove(container_name, bounds.force_remove) {
        tracing::debug!(%error, "force-removing a timed-out script container failed");
    }
    if !exited {
        exited = wait_for_exit(child.as_mut(), bounds.reap);
    }
    if !exited {
        tracing::debug!(
            "abandoning a timed-out script's docker child that did not exit after kill and force-remove"
        );
    }

    // Pipe results are discarded: the script already timed out. Each join is
    // bounded, and a thread stuck on a pipe that never closes is abandoned.
    let readers = readers.into_iter().flatten();
    for reader in readers {
        if let Err(error) = reader.join(bounds.pipe_join) {
            tracing::debug!(%error, "timed-out script output pipe did not drain");
        }
    }
    if let Some(stdin_writer) = stdin_writer
        && let Err(error) = stdin_writer.join(bounds.pipe_join)
    {
        tracing::debug!(%error, "timed-out script input pipe did not drain");
    }
}

/// Poll `try_wait` until the child exits or `bound` elapses.
fn wait_for_exit(child: &mut dyn ScriptChild, bound: Duration) -> bool {
    let deadline = Instant::now() + bound;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => {}
            Err(error) => {
                tracing::debug!(%error, "polling a timed-out script's docker child failed");
                return false;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

pub(crate) fn docker_run_args(
    request: &ScriptBackendRequest,
    image: &str,
    container_name: &str,
) -> Vec<String> {
    let mut args = vec![
        "run".to_string(),
        "--rm".to_string(),
        "-i".to_string(),
        "--name".to_string(),
        container_name.to_string(),
        "--label".to_string(),
        SCRIPT_CONTAINER_LABEL.to_string(),
        "--network".to_string(),
        "none".to_string(),
        image.to_string(),
        request.command.clone(),
    ];
    args.extend(request.args.clone());
    args
}

pub(crate) fn read_bounded<R>(mut reader: R, limit: u64) -> Result<Vec<u8>, String>
where
    R: Read,
{
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| error.to_string())?;
        if read == 0 {
            return Ok(output);
        }
        if output.len().saturating_add(read) > limit {
            return Err(format!("output exceeded {limit} bytes"));
        }
        output.extend_from_slice(&buffer[..read]);
    }
}

/// The real Docker CLI.
struct SystemDockerCli;

impl DockerCli for SystemDockerCli {
    fn spawn_run(&self, args: &[String]) -> io::Result<Box<dyn ScriptChild>> {
        let child = Command::new("docker")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        Ok(Box::new(SystemScriptChild(child)))
    }

    fn force_remove(&self, container_name: &str, bound: Duration) -> Result<(), String> {
        let mut child = Command::new("docker")
            .args(["rm", "-f", container_name])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("spawning docker rm failed: {error}"))?;
        let deadline = Instant::now() + bound;
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(()),
                Ok(Some(status)) => return Err(format!("docker rm exited with {status}")),
                Ok(None) => {}
                Err(error) => return Err(format!("polling docker rm failed: {error}")),
            }
            if Instant::now() >= deadline {
                if let Err(error) = child.kill() {
                    tracing::debug!(%error, "killing a stuck docker rm failed");
                }
                return Err("docker rm did not finish in time".to_string());
            }
            thread::sleep(POLL_INTERVAL);
        }
    }
}

struct SystemScriptChild(std::process::Child);

impl ScriptChild for SystemScriptChild {
    fn take_stdin(&mut self) -> Option<Box<dyn Write + Send>> {
        self.0
            .stdin
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn Write + Send>)
    }

    fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
        self.0
            .stdout
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>)
    }

    fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>> {
        self.0
            .stderr
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>)
    }

    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        Ok(self.0.try_wait()?.map(|status| status.code().unwrap_or(-1)))
    }

    fn kill(&mut self) -> io::Result<()> {
        self.0.kill()
    }
}

#[cfg(test)]
#[path = "docker_tests.rs"]
mod tests;
