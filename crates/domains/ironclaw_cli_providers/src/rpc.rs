//! Newline-delimited JSON-RPC 2.0 over a child's stdio, and the process-group
//! handling the long-lived agent hosts share (Antigravity's ACP server,
//! `muse serve`).
//!
//! Client side only: requests we send are matched to their responses here;
//! everything the agent sends on its own (notifications and server requests)
//! comes out of one ordered [`Inbound`] channel for the driver to fold.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot, watch};

/// A failed request.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum RpcError {
    /// The peer answered with a JSON-RPC error object.
    Remote {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    /// The peer did not answer in time.
    Timeout,
    /// The process went away before answering.
    Closed,
}

impl RpcError {
    /// A short, path-free label for logs and sanitized turn errors.
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Remote { code, .. } => format!("error {code}"),
            Self::Timeout => "timeout".into(),
            Self::Closed => "connection closed".into(),
        }
    }

    pub(crate) fn message(&self) -> &str {
        match self {
            Self::Remote { message, .. } => message,
            _ => "",
        }
    }
}

/// Something the agent sent without being asked.
#[derive(Debug, Clone)]
pub(crate) enum Inbound {
    Notification {
        method: String,
        params: Value,
    },
    Request {
        id: Value,
        method: String,
        params: Value,
    },
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, RpcError>>>>>;

/// One frame for the writer, and where to report whether it reached the
/// child's stdin.
type Outgoing = (String, Option<watch::Sender<Option<bool>>>);

/// How long one frame may take to go onto the child's stdin. A child that
/// stops reading its input fails the write; the writer then closes the
/// pipe for good and reports it ([`RpcClient::writer_failed`]), so the
/// driver tears the turn down in bounded time.
pub(crate) const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether one frame reached the child's stdin (written and flushed).
/// Cloneable: the requester and the turn's ledger can both follow it. It
/// always ends: the writer reports every frame, and a frame it never got to
/// (pipe broken, writer gone) is `false`.
#[derive(Clone)]
pub(crate) struct Written(pub(crate) watch::Receiver<Option<bool>>);

impl Written {
    fn pending() -> (watch::Sender<Option<bool>>, Self) {
        let (tx, rx) = watch::channel(None);
        (tx, Self(rx))
    }

    /// Already settled, for a frame that was never queued.
    pub(crate) fn failed() -> Self {
        let (tx, rx) = Self::pending();
        tx.send_replace(Some(false));
        rx
    }

    /// `Some(written)` once settled, `None` while the write is pending.
    pub(crate) fn now(&self) -> Option<bool> {
        *self.0.borrow()
    }

    /// Wait until the write settled.
    pub(crate) async fn outcome(&mut self) -> bool {
        let settled = self.0.wait_for(Option::is_some).await.map(|state| *state);
        match settled {
            Ok(state) => state.unwrap_or(false), // silent-ok: `wait_for` returned a settled value
            // The writer dropped the report without settling it: not written.
            Err(_) => self.now().unwrap_or(false), // silent-ok: no report means never written
        }
    }
}

fn ack(written: Option<watch::Sender<Option<bool>>>, ok: bool) {
    if let Some(tx) = written {
        tx.send_replace(Some(ok));
    }
}

/// Request handle. Cloneable; dropping every clone closes the child's stdin.
#[derive(Clone)]
pub(crate) struct RpcClient {
    out: mpsc::UnboundedSender<Outgoing>,
    /// Flips to `true` once a write failed or timed out and the writer
    /// closed the child's stdin for good.
    failed: watch::Receiver<bool>,
    pending: Pending,
    next_id: Arc<AtomicI64>,
}

impl RpcClient {
    /// Wire up `stdin`/`stdout`. The returned receiver ends when stdout does.
    pub(crate) fn start(
        stdin: ChildStdin,
        stdout: ChildStdout,
        name: &'static str,
    ) -> (Self, mpsc::UnboundedReceiver<Inbound>) {
        Self::start_bounded(stdin, stdout, name, WRITE_TIMEOUT)
    }

    /// [`Self::start`] with another per-frame write bound (tests).
    pub(crate) fn start_bounded(
        stdin: ChildStdin,
        stdout: ChildStdout,
        name: &'static str,
        write_timeout: Duration,
    ) -> (Self, mpsc::UnboundedReceiver<Inbound>) {
        let (out, mut out_rx) = mpsc::unbounded_channel::<Outgoing>();
        let (failed_tx, failed) = watch::channel(false);
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

        let mut stdin = stdin;
        tokio::spawn(async move {
            while let Some((frame, written)) = out_rx.recv().await {
                let write = async {
                    stdin.write_all(frame.as_bytes()).await?;
                    stdin.flush().await
                };
                let ok = matches!(tokio::time::timeout(write_timeout, write).await, Ok(Ok(())));
                ack(written, ok);
                if !ok {
                    tracing::debug!("{name}: write to the child failed; its input is closed");
                    break;
                }
            }
            // Frames left behind never reach the child: each is reported
            // unwritten before anyone hears that input failed, so a turn
            // tearing down on the failure finds every frame settled.
            out_rx.close();
            while let Ok((_, written)) = out_rx.try_recv() {
                ack(written, false);
            }
            failed_tx.send_replace(true);
            #[expect(
                clippy::let_underscore_must_use,
                reason = "the child may already be gone"
            )]
            let _ = stdin.shutdown().await;
        });

        let reader_pending = Arc::clone(&pending);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => route(&line, &reader_pending, &in_tx, name),
                    Ok(None) => break,
                    Err(error) => {
                        tracing::debug!(%error, "{name}: stdout read failed");
                        break;
                    }
                }
            }
            if let Ok(mut map) = reader_pending.lock() {
                for (_, waiter) in map.drain() {
                    #[expect(
                        clippy::let_underscore_must_use,
                        reason = "requester may have timed out"
                    )]
                    let _ = waiter.send(Err(RpcError::Closed));
                }
            }
        });

        (
            Self {
                out,
                failed,
                pending,
                next_id: Arc::new(AtomicI64::new(1)),
            },
            in_rx,
        )
    }

    fn send(&self, frame: Value) -> Result<(), RpcError> {
        self.out
            .send((format!("{frame}\n"), None))
            .map_err(|_closed| RpcError::Closed)
    }

    /// [`Self::send`], with a receiver that resolves once the frame is on
    /// the child's stdin (or never will be).
    fn send_acked(&self, frame: Value) -> Result<Written, RpcError> {
        let (tx, rx) = Written::pending();
        self.out
            .send((format!("{frame}\n"), Some(tx)))
            .map_err(|_closed| RpcError::Closed)?;
        Ok(rx)
    }

    /// Resolves once a write failed and the child's input is closed: the
    /// turn cannot reach the agent any more and must end.
    pub(crate) async fn writer_failed(&self) {
        let mut failed = self.failed.clone();
        if failed.wait_for(|f| *f).await.is_err() {
            // The writer ended cleanly (every client dropped): never fails.
            std::future::pending::<()>().await;
        }
    }

    pub(crate) async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RpcError> {
        self.start_request(method, params)?.wait(timeout).await
    }

    /// Queue the request frame now, before anything awaits, and hand back
    /// the handle its answer arrives on. Frames go out in the order they
    /// were started, so a caller can report "sent" as soon as this returns.
    pub(crate) fn start_request(
        &self,
        method: &str,
        params: Value,
    ) -> Result<PendingReply, RpcError> {
        Ok(self.start_request_acked(method, params)?.0)
    }

    /// [`Self::start_request`], plus a receiver for when the frame is
    /// actually on the child's stdin: "queued" is not "sent" when the child
    /// has closed its input.
    pub(crate) fn start_request_acked(
        &self,
        method: &str,
        params: Value,
    ) -> Result<(PendingReply, Written), RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        if let Ok(mut map) = self.pending.lock() {
            map.insert(id, tx);
        }
        let sent = self
            .send_acked(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let written = match sent {
            Ok(written) => written,
            Err(error) => {
                self.forget(id);
                return Err(error);
            }
        };
        Ok((
            PendingReply {
                client: self.clone(),
                id,
                rx: Some(rx),
            },
            written,
        ))
    }

    fn forget(&self, id: i64) {
        if let Ok(mut map) = self.pending.lock() {
            map.remove(&id);
        }
    }

    pub(crate) fn notify(&self, method: &str, params: Option<Value>) -> Result<(), RpcError> {
        let mut frame = json!({"jsonrpc": "2.0", "method": method});
        if let Some(params) = params {
            frame["params"] = params;
        }
        self.send(frame)
    }

    pub(crate) fn respond(&self, id: &Value, result: Value) -> Result<(), RpcError> {
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}))
    }

    /// [`Self::respond`], resolving once the answer is on the child's stdin.
    pub(crate) fn respond_acked(&self, id: &Value, result: Value) -> Result<Written, RpcError> {
        self.send_acked(json!({"jsonrpc": "2.0", "id": id, "result": result}))
    }

    pub(crate) fn respond_error(
        &self,
        id: &Value,
        code: i64,
        message: &str,
    ) -> Result<(), RpcError> {
        self.send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}))
    }
}

/// The answer to a request whose frame is already queued.
pub(crate) struct PendingReply {
    client: RpcClient,
    id: i64,
    rx: Option<oneshot::Receiver<Result<Value, RpcError>>>,
}

impl PendingReply {
    pub(crate) async fn wait(mut self, timeout: Duration) -> Result<Value, RpcError> {
        let Some(rx) = self.rx.take() else {
            return Err(RpcError::Closed);
        };
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_dropped)) => Err(RpcError::Closed),
            Err(_elapsed) => Err(RpcError::Timeout),
        }
    }
}

/// A wait given up (timed out, or dropped unawaited) leaves no registration
/// behind: a late answer then has nobody to reach and is dropped.
impl Drop for PendingReply {
    fn drop(&mut self) {
        self.client.forget(self.id);
    }
}

fn route(
    line: &str,
    pending: &Pending,
    inbound: &mpsc::UnboundedSender<Inbound>,
    name: &'static str,
) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    let Ok(mut frame) = serde_json::from_str::<Value>(line) else {
        tracing::debug!("{name}: unparsed stdout line");
        return;
    };
    let params = frame
        .get_mut("params")
        .map(Value::take)
        .unwrap_or(Value::Null);
    match (frame.get("method").and_then(Value::as_str), frame.get("id")) {
        (Some(method), Some(id)) => {
            #[expect(
                clippy::let_underscore_must_use,
                reason = "the driver stopped listening, the turn is over"
            )]
            let _ = inbound.send(Inbound::Request {
                id: id.clone(),
                method: method.to_string(),
                params,
            });
        }
        (Some(method), None) => {
            #[expect(clippy::let_underscore_must_use, reason = "as above")]
            let _ = inbound.send(Inbound::Notification {
                method: method.to_string(),
                params,
            });
        }
        (None, Some(id)) => {
            let Some(id) = id.as_i64() else { return };
            let waiter = pending.lock().ok().and_then(|mut map| map.remove(&id));
            let Some(waiter) = waiter else { return };
            let result = match frame.get_mut("error") {
                Some(error) => Err(RpcError::Remote {
                    code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    data: error.get_mut("data").map(Value::take),
                }),
                None => Ok(frame
                    .get_mut("result")
                    .map(Value::take)
                    .unwrap_or(Value::Null)),
            };
            #[expect(clippy::let_underscore_must_use, reason = "requester timed out")]
            let _ = waiter.send(result);
        }
        (None, None) => tracing::debug!("{name}: frame without method or id"),
    }
}

/// How long Stop waits after SIGTERM before SIGKILL.
pub(crate) const KILL_GRACE: Duration = Duration::from_secs(2);

/// The child's process group id (it was spawned with `process_group(0)`).
pub(crate) fn group_of(child: &Child) -> Option<i32> {
    child.id().and_then(|id| i32::try_from(id).ok())
}

/// SIGTERM the whole group, then SIGKILL after `grace` if `child` is still
/// running. Reaps `child`.
pub(crate) async fn end_group(child: &mut Child, pgid: i32, grace: Duration) {
    let group = Pid::from_raw(pgid);
    if let Err(error) = killpg(group, Signal::SIGTERM) {
        tracing::debug!(%error, "agent host: SIGTERM failed");
    }
    if tokio::time::timeout(grace, child.wait()).await.is_err() {
        if let Err(error) = killpg(group, Signal::SIGKILL) {
            tracing::debug!(%error, "agent host: SIGKILL failed");
        }
        if let Err(error) = child.wait().await {
            tracing::debug!(%error, "agent host: reaping the killed child failed");
        }
    }
    // Tool subprocesses can outlive the leader; the group goes with it.
    kill_group(pgid, "agent host");
}

/// [`end_group`], and every process group the leader's descendants moved
/// into: a CLI starts its shell commands detached (their own group), so a
/// hung background command outlives a signal to the leader's group. The
/// descendants are listed before any signal, while their parent links
/// still lead back to the leader.
pub(crate) async fn end_tree(child: &mut Child, pgid: i32, grace: Duration) {
    let detached = descendant_groups(pgid).await;
    end_group(child, pgid, grace).await;
    for group in detached {
        kill_group(group, "agent host: detached descendant");
    }
}

/// The process groups of every descendant of `root`, other than its own.
/// Empty when the process table cannot be read (traced).
async fn descendant_groups(root: i32) -> Vec<i32> {
    let listed = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::process::Command::new("ps")
            .args(["-axo", "pid=,ppid=,pgid="])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await;
    let output = match listed {
        Ok(Ok(output)) if output.status.success() => output.stdout,
        Ok(Ok(output)) => {
            tracing::debug!(status = %output.status, "agent host: process table unreadable");
            return Vec::new();
        }
        Ok(Err(error)) => {
            tracing::debug!(%error, "agent host: process table unreadable");
            return Vec::new();
        }
        Err(_) => {
            tracing::debug!("agent host: process table listing timed out");
            return Vec::new();
        }
    };
    let rows: Vec<(i32, i32, i32)> = String::from_utf8_lossy(&output)
        .lines()
        .filter_map(|line| {
            let mut cols = line.split_whitespace().map(str::parse::<i32>);
            match (cols.next(), cols.next(), cols.next()) {
                (Some(Ok(pid)), Some(Ok(ppid)), Some(Ok(pgid))) => Some((pid, ppid, pgid)),
                _ => None,
            }
        })
        .collect();
    let mut tree = vec![root];
    let mut i = 0;
    while let Some(&parent) = tree.get(i) {
        tree.extend(
            rows.iter()
                .filter(|(pid, ppid, _)| *ppid == parent && !tree.contains(pid))
                .map(|(pid, _, _)| *pid)
                .collect::<Vec<_>>(),
        );
        i += 1;
    }
    let mut groups: Vec<i32> = rows
        .iter()
        .filter(|(pid, _, pgid)| *pid != root && tree.contains(pid) && *pgid != root && *pgid > 1)
        .map(|(_, _, pgid)| *pgid)
        .collect();
    groups.sort_unstable();
    groups.dedup();
    groups
}

/// SIGKILL the process group `pgid`. An already-empty group (`ESRCH`) is the
/// usual outcome at cleanup and stays quiet; any other failure is traced.
pub(crate) fn kill_group(pgid: i32, context: &'static str) {
    match killpg(Pid::from_raw(pgid), Signal::SIGKILL) {
        Ok(()) | Err(Errno::ESRCH) => {}
        Err(error) => tracing::debug!(%error, context, "process group SIGKILL failed"),
    }
}

/// The binary `name`: `PATH` first, then the installers' usual homes (a
/// desktop-launched backend often runs with a minimal `PATH`).
pub(crate) fn find_on_path(home: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    crate::binaries::find(home, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sol 8: a wait dropped unawaited (or given up) leaves no
    /// registration: the pending map does not grow with abandoned requests.
    #[tokio::test]
    async fn an_abandoned_request_leaves_no_registration() {
        let mut child = tokio::process::Command::new("cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let (client, _inbound) = RpcClient::start(
            child.stdin.take().unwrap(),
            child.stdout.take().unwrap(),
            "t",
        );
        let reply = client.start_request("x", json!({})).unwrap();
        assert_eq!(client.pending.lock().unwrap().len(), 1);
        drop(reply);
        assert_eq!(client.pending.lock().unwrap().len(), 0);
        let reply = client.start_request("y", json!({})).unwrap();
        assert!(reply.wait(Duration::from_millis(50)).await.is_err());
        assert_eq!(client.pending.lock().unwrap().len(), 0);
    }
}
