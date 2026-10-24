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

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot};

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

/// Request handle. Cloneable; dropping every clone closes the child's stdin.
#[derive(Clone)]
pub(crate) struct RpcClient {
    out: mpsc::UnboundedSender<String>,
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
        let (out, mut out_rx) = mpsc::unbounded_channel::<String>();
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

        let mut stdin = stdin;
        tokio::spawn(async move {
            while let Some(frame) = out_rx.recv().await {
                if stdin.write_all(frame.as_bytes()).await.is_err() {
                    break;
                }
                if stdin.flush().await.is_err() {
                    break;
                }
            }
            let _ = stdin.shutdown().await; // silent-ok: the child may already be gone
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
                    let _ = waiter.send(Err(RpcError::Closed)); // silent-ok: requester may have timed out
                }
            }
        });

        (
            Self {
                out,
                pending,
                next_id: Arc::new(AtomicI64::new(1)),
            },
            in_rx,
        )
    }

    fn send(&self, frame: Value) -> Result<(), RpcError> {
        self.out
            .send(format!("{frame}\n"))
            .map_err(|_closed| RpcError::Closed)
    }

    pub(crate) async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        if let Ok(mut map) = self.pending.lock() {
            map.insert(id, tx);
        }
        let sent =
            self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        if let Err(error) = sent {
            self.forget(id);
            return Err(error);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_dropped)) => Err(RpcError::Closed),
            Err(_elapsed) => {
                self.forget(id);
                Err(RpcError::Timeout)
            }
        }
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

    pub(crate) fn respond_error(
        &self,
        id: &Value,
        code: i64,
        message: &str,
    ) -> Result<(), RpcError> {
        self.send(json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}))
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
            let _ = inbound.send(Inbound::Request {
                id: id.clone(),
                method: method.to_string(),
                params,
            }); // silent-ok: the driver stopped listening, the turn is over
        }
        (Some(method), None) => {
            let _ = inbound.send(Inbound::Notification {
                method: method.to_string(),
                params,
            }); // silent-ok: as above
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
            let _ = waiter.send(result); // silent-ok: requester timed out
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
        let _ = child.wait().await; // silent-ok: reaping a killed child
    }
    // Tool subprocesses can outlive the leader; the group goes with it.
    let _ = killpg(group, Signal::SIGKILL); // silent-ok: usually nothing is left
}

/// The binary `name`: `PATH` first, then the installers' usual homes (a
/// desktop-launched backend often runs with a minimal `PATH`).
pub(crate) fn find_on_path(home: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    let from_path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default();
    from_path
        .into_iter()
        .chain([
            home.join(".local/bin"),
            std::path::PathBuf::from("/opt/homebrew/bin"),
            std::path::PathBuf::from("/usr/local/bin"),
        ])
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}
