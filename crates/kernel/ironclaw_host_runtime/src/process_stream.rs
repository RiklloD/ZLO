//! A host command whose output is read while it runs
//! ([`crate::HostProcessPort::spawn_streaming`]), for an operator console.
//!
//! The command runs in its own process group. Ending it — [`HostProcessStream::kill`],
//! the deadline, or dropping the stream — kills the whole group, so whatever
//! the command started ends with it. Nothing here buffers output: each read
//! is handed to the caller, which owns the bound on what it keeps.

use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::{Child, ChildStderr, ChildStdout};
use tokio::time::Instant;

use crate::process_port::kill_process_group;

const READ_CHUNK: usize = 8 * 1024;

/// What a streamed command did next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostProcessEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    /// The command ended; `code` is `None` when a signal ended it (a kill).
    /// Always the last event.
    Exited {
        code: Option<i32>,
        timed_out: bool,
    },
}

/// A running host command; see the module docs.
#[derive(Debug)]
pub struct HostProcessStream {
    child: Child,
    /// The process group leader while it has not been reaped (a reaped pid
    /// may be reused, so it is never signalled after that).
    group: Option<u32>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    deadline: Instant,
    timed_out: bool,
    done: bool,
}

async fn read_some<R: AsyncRead + Unpin>(
    reader: &mut Option<R>,
    buf: &mut [u8],
) -> std::io::Result<usize> {
    match reader {
        Some(reader) => reader.read(buf).await,
        None => std::future::pending().await,
    }
}

impl HostProcessStream {
    pub(crate) fn new(mut child: Child, timeout: Duration) -> Self {
        let group = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        Self {
            child,
            group,
            stdout,
            stderr,
            deadline: Instant::now() + timeout,
            timed_out: false,
            done: false,
        }
    }

    /// The next output chunk, or the exit; `None` after the exit was returned.
    pub async fn next(&mut self) -> Option<HostProcessEvent> {
        if self.done {
            return None;
        }
        let mut out = vec![0u8; READ_CHUNK];
        let mut err = vec![0u8; READ_CHUNK];
        loop {
            if self.stdout.is_none() && self.stderr.is_none() {
                return Some(self.wait_exit().await);
            }
            let Self {
                stdout,
                stderr,
                deadline,
                ..
            } = self;
            tokio::select! {
                read = read_some(stdout, &mut out) => match read {
                    Ok(n) if n > 0 => return Some(HostProcessEvent::Stdout(out[..n].to_vec())),
                    Ok(_) => *stdout = None,
                    Err(error) => {
                        tracing::debug!(%error, "streamed command stdout closed");
                        *stdout = None;
                    }
                },
                read = read_some(stderr, &mut err) => match read {
                    Ok(n) if n > 0 => return Some(HostProcessEvent::Stderr(err[..n].to_vec())),
                    Ok(_) => *stderr = None,
                    Err(error) => {
                        tracing::debug!(%error, "streamed command stderr closed");
                        *stderr = None;
                    }
                },
                () = tokio::time::sleep_until(*deadline) => {
                    self.timed_out = true;
                    self.kill();
                    // The group is dead: its pipes close and the loop ends.
                    self.deadline = Instant::now() + Duration::from_secs(5);
                }
            }
        }
    }

    async fn wait_exit(&mut self) -> HostProcessEvent {
        let status = tokio::select! {
            status = self.child.wait() => status,
            () = tokio::time::sleep_until(self.deadline) => {
                self.timed_out = true;
                self.kill();
                self.child.wait().await
            }
        };
        self.group = None;
        self.done = true;
        let code = match status {
            Ok(status) => status.code(),
            Err(error) => {
                tracing::debug!(%error, "streamed command wait failed");
                None
            }
        };
        HostProcessEvent::Exited {
            code,
            timed_out: self.timed_out,
        }
    }

    /// Kill the command and everything in its process group. Idempotent;
    /// [`Self::next`] then reports the exit.
    pub fn kill(&mut self) {
        if let Some(pid) = self.group {
            kill_process_group(pid);
        }
        if let Err(error) = self.child.start_kill() {
            tracing::debug!(%error, "streamed command already ended");
        }
    }
}

impl Drop for HostProcessStream {
    fn drop(&mut self) {
        if !self.done {
            self.kill();
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::collections::HashMap;

    use ironclaw_host_api::process::CommandExecutionRequest;
    use ironclaw_host_api::resource::ResourceScope;

    use super::*;
    use crate::HostProcessPort;

    fn request(command: &str, dir: &std::path::Path, timeout: u64) -> CommandExecutionRequest {
        CommandExecutionRequest {
            scope: ResourceScope::system(),
            mounts: None,
            command: command.to_string(),
            workdir: Some(dir.to_string_lossy().into_owned()),
            timeout_secs: Some(timeout),
            extra_env: HashMap::new(),
        }
    }

    async fn drain(stream: &mut HostProcessStream) -> (String, String, Option<i32>, bool) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        while let Some(event) = stream.next().await {
            match event {
                HostProcessEvent::Stdout(b) => out.extend(b),
                HostProcessEvent::Stderr(b) => err.extend(b),
                HostProcessEvent::Exited { code, timed_out } => {
                    return (
                        String::from_utf8_lossy(&out).into_owned(),
                        String::from_utf8_lossy(&err).into_owned(),
                        code,
                        timed_out,
                    );
                }
            }
        }
        panic!("no exit event");
    }

    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 only checks that the process exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    #[tokio::test]
    async fn streams_both_outputs_and_the_exit_code_in_a_scrubbed_environment() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: test-only; nothing else in this process reads it concurrently.
        unsafe { std::env::set_var("IRONCLAW_STREAM_TEST_SECRET", "hunter2") };
        let port = HostProcessPort::new();
        let mut stream = port
            .spawn_streaming(request(
                "echo out; echo err >&2; echo \"secret=${IRONCLAW_STREAM_TEST_SECRET:-}\"; exit 3",
                dir.path(),
                30,
            ))
            .await
            .unwrap();
        let (out, err, code, timed_out) = drain(&mut stream).await;
        assert_eq!(
            out, "out\nsecret=\n",
            "the host environment is not inherited"
        );
        assert_eq!(err, "err\n");
        assert_eq!(code, Some(3));
        assert!(!timed_out);
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn kill_and_drop_end_the_whole_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let port = HostProcessPort::new();
        for drop_it in [false, true] {
            let mut stream = port
                .spawn_streaming(request("sleep 60 & echo $!; wait", dir.path(), 120))
                .await
                .unwrap();
            let Some(HostProcessEvent::Stdout(first)) = stream.next().await else {
                panic!("expected the child pid");
            };
            let pid: i32 = String::from_utf8_lossy(&first).trim().parse().unwrap();
            assert!(alive(pid));
            if drop_it {
                drop(stream);
            } else {
                stream.kill();
                let (_, _, code, _) = drain(&mut stream).await;
                assert_eq!(code, None, "a killed command has no exit code");
            }
            let mut gone = false;
            for _ in 0..50 {
                if !alive(pid) {
                    gone = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            // A killed-but-unreaped grandchild is a zombie; `ps` tells it apart.
            let state = std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .unwrap();
            let state = String::from_utf8_lossy(&state.stdout);
            assert!(
                gone || state.trim().starts_with('Z') || state.trim().is_empty(),
                "background child survived (drop={drop_it}): {state}"
            );
        }
    }

    #[tokio::test]
    async fn the_deadline_kills_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let mut stream = HostProcessPort::new()
            .spawn_streaming(request("sleep 30", dir.path(), 1))
            .await
            .unwrap();
        let (_, _, code, timed_out) = drain(&mut stream).await;
        assert!(timed_out);
        assert_eq!(code, None);
    }
}
