//! Meta's Muse Code (`muse serve`, the Muse Session Protocol) as an
//! orchestrator runtime, driven like the Hermes t3code driver: a minted
//! UUIDv7 session id, `session/start` on the first turn and
//! `session/resume` after, `turn/start`, `turn/steer`, `turn/interrupt`.
//!
//! One host process per turn. Muse persists the session itself; transcripts
//! for `history` are IronClaw's own journal
//! (`<state_dir>/transcripts/<session>.jsonl`), which also marks a session as
//! started. Subscription windows (`usage/changed`) go to `usage_file`.
//!
//! Tools fail closed: the host runs with `--disable-shell --disable-write
//! --disable-request-user-input`, without `--trust-workspace` (no workspace
//! rules can allow anything), in `denyUnmatched` approval mode, and any
//! approval Muse still asks for goes through [`crate::permissions::decide`],
//! which denies until the approval bridge lands.

mod fold;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use ironclaw_host_api::ids::ThreadId;
use ironclaw_orchestrator::model::{RuntimeId, ThreadRecord, ThreadScope, now_ms};
use ironclaw_orchestrator::runtime::*;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, watch};

use crate::journal;
use crate::permissions::{self, PermissionDecision, PermissionRequest};
use crate::rpc::{self, Inbound, RpcClient, RpcError};
use fold::TurnFold;

/// The Muse release the wire handling was checked against.
pub const EXPECTED_HOST_VERSION: &str = "1.4.12";

const DEFAULT_MODEL: &str = "default";
const PROVIDER: &str = "meta";
/// The default-deny approval mode Muse documents for automation.
const APPROVAL_MODE: &str = "denyUnmatched";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a host may drain after stdin closes before its group is killed.
const DRAIN: Duration = Duration::from_secs(10);
/// How long a `model/list` answer is reused.
const CATALOG_TTL: Duration = Duration::from_secs(600);

#[derive(Debug, Clone)]
pub struct MuseConfig {
    /// Runtime and provider id (`muse-code`).
    pub runtime_id: String,
    pub display_name: String,
    pub binary: PathBuf,
    /// Working directory and workspace root of every session.
    pub work_dir: PathBuf,
    /// Transcripts.
    pub state_dir: PathBuf,
    /// Where each turn's subscription windows are written for the Usage page
    /// (`~/.ironclaw/usage/muse-code.json`); `None` writes nothing.
    pub usage_file: Option<PathBuf>,
    /// Reasoning tier sent with every turn; `None` keeps the model default.
    pub reasoning_effort: Option<String>,
}

impl MuseConfig {
    fn transcript(&self, session: &str) -> PathBuf {
        self.state_dir
            .join("transcripts")
            .join(format!("{session}.jsonl"))
    }
}

/// What a steer or Stop needs from the running turn.
#[derive(Clone)]
struct LiveTurn {
    client: RpcClient,
    session_id: String,
    turn_id: String,
}

struct ActiveTurn {
    pgid: i32,
    cancelled: Arc<AtomicBool>,
    live: Arc<Mutex<Option<LiveTurn>>>,
    done: watch::Receiver<bool>,
}

#[derive(Clone, Default)]
struct Catalog {
    at: Option<Instant>,
    models: Vec<String>,
    windows: HashMap<String, u64>,
    default: Option<String>,
}

pub struct MuseRuntime {
    id: RuntimeId,
    config: Arc<MuseConfig>,
    sink: Weak<dyn TurnEventSink>,
    active: Arc<Mutex<HashMap<ThreadId, ActiveTurn>>>,
    catalog: Arc<tokio::sync::Mutex<Catalog>>,
}

impl MuseRuntime {
    pub fn new(
        config: MuseConfig,
        sink: Weak<dyn TurnEventSink>,
    ) -> Result<Self, ironclaw_orchestrator::error::OrchestratorError> {
        Ok(Self {
            id: RuntimeId::new(config.runtime_id.clone())?,
            config: Arc::new(config),
            sink,
            active: Arc::new(Mutex::new(HashMap::new())),
            catalog: Arc::new(tokio::sync::Mutex::new(Catalog::default())),
        })
    }

    fn is_active(&self, thread: &ThreadId) -> bool {
        self.active
            .lock()
            .map(|a| a.contains_key(thread))
            .unwrap_or(false) // silent-ok: a poisoned map only happens after a panic elsewhere
    }

    fn spawn_host(&self, read_only: bool) -> Result<(Child, i32), RuntimeError> {
        let mut cmd = Command::new(&self.config.binary);
        cmd.args([
            "serve",
            // Fail closed: no shell, no writes, nobody to answer questions.
            "--disable-shell",
            "--disable-write",
            "--disable-request-user-input",
        ]);
        if read_only {
            cmd.arg("--no-session-log");
        }
        // Keep the CLI login; never an API-key override.
        cmd.env_remove("META_API_KEY")
            .env("MUSE_NO_AUTO_UPDATE", "1")
            .current_dir(&self.config.work_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let child = cmd
            .spawn()
            .map_err(|e| RuntimeError::Unavailable(format!("spawn muse: {}", e.kind())))?;
        let pgid = rpc::group_of(&child)
            .ok_or_else(|| RuntimeError::Unavailable("muse exited at spawn".into()))?;
        Ok((child, pgid))
    }

    async fn spawn_turn(&self, thread: &ThreadRecord, text: &str) -> Result<String, RuntimeError> {
        let session_id = thread
            .native_session_id
            .clone()
            .ok_or_else(|| RuntimeError::Rejected("thread has no muse session".into()))?;
        tokio::fs::create_dir_all(&self.config.work_dir)
            .await
            .map_err(|e| RuntimeError::Unavailable(format!("work dir: {}", e.kind())))?;
        let (mut child, pgid) = self.spawn_host(false)?;
        let (client, inbound) = connect(&mut child, pgid)?;

        let run_id = uuid::Uuid::new_v4().to_string();
        let cancelled = Arc::new(AtomicBool::new(false));
        let live = Arc::new(Mutex::new(None));
        let (done_tx, done_rx) = watch::channel(false);
        if let Ok(mut active) = self.active.lock() {
            active.insert(
                thread.thread_id.clone(),
                ActiveTurn {
                    pgid,
                    cancelled: Arc::clone(&cancelled),
                    live: Arc::clone(&live),
                    done: done_rx,
                },
            );
        }

        // Never wait on a catalog probe in flight; the window is a fallback.
        let window = self
            .catalog
            .try_lock()
            .map(|c| c.windows.clone())
            .unwrap_or_default(); // silent-ok: a busy catalog only drops the context-max fallback
        let turn = Turn {
            config: Arc::clone(&self.config),
            session_id,
            model: thread.model.clone(),
            text: text.to_string(),
            cancelled,
            live,
            windows: window,
        };
        let sink = self.sink.clone();
        let active = Arc::clone(&self.active);
        let thread_id = thread.thread_id.clone();
        let run = run_id.clone();
        tokio::spawn(async move {
            if let Some(sink) = sink.upgrade() {
                sink.turn_started(&thread_id, Some(run.clone()), now_ms())
                    .await;
            }
            let (outcome, telemetry) = turn.run(client, inbound).await;
            close_host(&mut child, pgid).await;
            if let Ok(mut map) = active.lock() {
                map.remove(&thread_id);
            }
            if let Some(sink) = sink.upgrade() {
                if let Some(telemetry) = telemetry {
                    sink.turn_telemetry(&thread_id, telemetry, now_ms()).await;
                }
                sink.turn_finished(&thread_id, Some(run), outcome, now_ms())
                    .await;
            }
            let _ = done_tx.send(true); // silent-ok: nobody waiting on this turn
        });
        Ok(run_id)
    }

    fn live(&self, thread: &ThreadId) -> Option<LiveTurn> {
        self.active
            .lock()
            .ok()
            .and_then(|a| a.get(thread).and_then(|t| t.live.lock().ok()?.clone()))
    }

    /// `turn/interrupt`, then the process group if the turn does not settle
    /// within the grace period. Waits until the turn is reported.
    async fn stop(&self, thread: &ThreadId) {
        let entry = self.active.lock().ok().and_then(|a| {
            a.get(thread)
                .map(|t| (t.pgid, Arc::clone(&t.cancelled), t.done.clone()))
        });
        let Some((pgid, cancelled, mut done)) = entry else {
            return;
        };
        cancelled.store(true, Ordering::SeqCst);
        if let Some(live) = self.live(thread) {
            let params = json!({
                "sessionId": live.session_id,
                "commandId": command_id(),
                "turnId": live.turn_id,
            });
            let interrupt = live
                .client
                .request("turn/interrupt", params, rpc::KILL_GRACE);
            if let Err(error) = interrupt.await {
                tracing::debug!(error = %error.label(), "muse: interrupt not admitted");
            }
        }
        if tokio::time::timeout(rpc::KILL_GRACE, done.wait_for(|d| *d))
            .await
            .is_ok()
        {
            return;
        }
        let group = Pid::from_raw(pgid);
        if let Err(error) = killpg(group, Signal::SIGTERM) {
            tracing::debug!(%error, "muse: SIGTERM failed");
        }
        if tokio::time::timeout(rpc::KILL_GRACE, done.wait_for(|d| *d))
            .await
            .is_err()
        {
            if let Err(error) = killpg(group, Signal::SIGKILL) {
                tracing::debug!(%error, "muse: SIGKILL failed");
            }
            let _ = done.wait_for(|d| *d).await; // silent-ok: sender dropped means the turn task ended
        }
    }

    /// `model/list` from a read-only host, cached for [`CATALOG_TTL`].
    async fn models(&self) -> Catalog {
        let mut cached = self.catalog.lock().await;
        if cached.at.is_some_and(|at| at.elapsed() < CATALOG_TTL) {
            return cached.clone();
        }
        match self.probe().await {
            Ok(rows) => {
                cached.at = Some(Instant::now());
                cached.models = rows.iter().map(|r| r.0.clone()).collect();
                cached.windows = rows
                    .iter()
                    .filter_map(|(id, window, _)| Some((id.clone(), (*window)?)))
                    .collect();
                cached.default = rows.iter().find(|r| r.2).map(|r| r.0.clone());
            }
            Err(error) => tracing::debug!(%error, "muse: model list unavailable"),
        }
        cached.clone()
    }

    async fn probe(&self) -> Result<Vec<(String, Option<u64>, bool)>, String> {
        tokio::fs::create_dir_all(&self.config.work_dir)
            .await
            .map_err(|e| format!("work dir: {}", e.kind()))?;
        let (mut child, pgid) = self.spawn_host(true).map_err(|e| e.to_string())?;
        let rows = async {
            let (client, _inbound) = connect(&mut child, pgid).map_err(|e| e.to_string())?;
            handshake(&client).await.map_err(|e| e.label())?;
            let catalog = client
                .request("model/list", json!({}), Duration::from_secs(15))
                .await
                .map_err(|e| e.label())?;
            Ok::<_, String>(fold::model_rows(&catalog))
        }
        .await;
        rpc::end_group(&mut child, pgid, rpc::KILL_GRACE).await;
        rows
    }
}

fn connect(
    child: &mut Child,
    pgid: i32,
) -> Result<(RpcClient, mpsc::UnboundedReceiver<Inbound>), RuntimeError> {
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        let _ = killpg(Pid::from_raw(pgid), Signal::SIGKILL); // silent-ok: cleanup of a half-spawned child
        return Err(RuntimeError::Unavailable("muse stdio unavailable".into()));
    };
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::debug!(target: "muse", "stderr: {line}");
        }
    });
    Ok(RpcClient::start(stdin, stdout, "muse"))
}

/// `initialize` + `initialized`, refusing an envelope version we do not speak.
async fn handshake(client: &RpcClient) -> Result<Value, RpcError> {
    let init = client
        .request(
            "initialize",
            json!({
                "clientInfo": {"name": "ironclaw", "title": "IronClaw", "version": env!("CARGO_PKG_VERSION")},
                "capabilities": {"requestedCapabilities": []},
            }),
            Duration::from_secs(20),
        )
        .await?;
    if init["schema"].get("version").and_then(Value::as_u64) != Some(1) {
        return Err(RpcError::Remote {
            code: 0,
            message: "unsupported MSP envelope version".into(),
            data: None,
        });
    }
    client.notify("initialized", None)?;
    Ok(init)
}

/// MSP command ids are UUIDv7.
fn command_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

struct Turn {
    config: Arc<MuseConfig>,
    session_id: String,
    model: Option<String>,
    text: String,
    cancelled: Arc<AtomicBool>,
    live: Arc<Mutex<Option<LiveTurn>>>,
    /// Context limits by model, from the catalog.
    windows: HashMap<String, u64>,
}

type Telemetry = Option<ironclaw_orchestrator::telemetry::TurnTelemetry>;

impl Turn {
    async fn run(
        self,
        client: RpcClient,
        inbound: mpsc::UnboundedReceiver<Inbound>,
    ) -> (TurnOutcome, Telemetry) {
        let result = self.drive(client, inbound).await;
        // Drop every handle so the host sees stdin close.
        if let Ok(mut slot) = self.live.lock() {
            *slot = None;
        }
        result
    }

    async fn drive(
        &self,
        client: RpcClient,
        mut inbound: mpsc::UnboundedReceiver<Inbound>,
    ) -> (TurnOutcome, Telemetry) {
        let mut fold = TurnFold::default();
        if let Err(error) = self.open(&client, &mut fold).await {
            return (open_failed(&error), None);
        }
        if self.cancelled.load(Ordering::SeqCst) {
            return (TurnOutcome::ended(TurnEndStatus::Cancelled, None), None);
        }
        let transcript = self.config.transcript(&self.session_id);
        journal::append(
            &transcript,
            &[journal::message(
                HistoryRole::User,
                self.text.clone(),
                Vec::new(),
            )],
        )
        .await;

        let mut params = json!({
            "sessionId": self.session_id,
            "commandId": command_id(),
            "input": [{"type": "text", "text": self.text}],
            "displayText": self.text,
            "workspaceRoots": [workspace_root(&self.config.work_dir)],
            "ifBusy": "queue",
        });
        if let Some(effort) = &self.config.reasoning_effort {
            params["reasoningEffort"] = json!(effort);
        }
        let admitted = client.request("turn/start", params, COMMAND_TIMEOUT).await;
        let turn_id = match admitted
            .as_ref()
            .ok()
            .and_then(|r| r.get("turnId"))
            .and_then(Value::as_str)
        {
            Some(id) => id.to_string(),
            None => {
                let label = admitted
                    .err()
                    .map_or_else(|| "no turn id".into(), |e| e.label());
                return (
                    TurnOutcome::ended(
                        TurnEndStatus::Failed,
                        Some(format!("muse refused the turn ({label})")),
                    ),
                    None,
                );
            }
        };
        fold.turn_id = Some(turn_id.clone());
        if let Ok(mut slot) = self.live.lock() {
            *slot = Some(LiveTurn {
                client: client.clone(),
                session_id: self.session_id.clone(),
                turn_id,
            });
        }

        let mut asked = HashSet::new();
        while let Some(message) = inbound.recv().await {
            match message {
                Inbound::Notification { method, params } => {
                    if params
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .is_some_and(|s| s != self.session_id)
                    {
                        continue;
                    }
                    if method == "approval/requested" || method == "approval/updated" {
                        self.approval(&client, &params, &mut asked);
                    }
                    if fold.push(&method, &params) {
                        break;
                    }
                }
                // Presentation receipts only; decisions travel as commands.
                Inbound::Request { id, method, .. } => {
                    let sent = if method == "approval/request" || method == "userInput/request" {
                        client.respond(&id, json!({}))
                    } else {
                        client.respond_error(&id, -32601, "method not supported by this client")
                    };
                    if let Err(error) = sent {
                        tracing::debug!(error = %error.label(), "muse: reply not sent");
                    }
                }
            }
        }

        let outcome = fold.outcome(self.cancelled.load(Ordering::SeqCst));
        if let Some(entry) = fold.assistant_message() {
            journal::append(&transcript, &[entry]).await;
        }
        if let (Some(path), Some(reading)) = (&self.config.usage_file, &fold.limit_reading) {
            journal::write_limit_reading(path, "usage", reading).await;
        }
        let window = fold
            .model
            .as_ref()
            .and_then(|m| self.windows.get(m).copied());
        (outcome, fold.telemetry(&self.config.runtime_id, window))
    }

    /// `initialize`, the subscription reading, then start or resume the
    /// session in the default-deny approval mode and on the thread's model.
    async fn open(&self, client: &RpcClient, fold: &mut TurnFold) -> Result<(), RpcError> {
        handshake(client).await?;
        if let Ok(read) = client
            .request("usage/read", json!({}), Duration::from_secs(10))
            .await
            && let Some(usage) = read.get("usage").filter(|u| u.is_object())
        {
            fold.limit_reading = Some(usage.clone());
        }
        let model = self.model.as_deref().filter(|m| *m != DEFAULT_MODEL);
        let started = tokio::fs::try_exists(self.config.transcript(&self.session_id))
            .await
            .unwrap_or(false); // silent-ok: an unreadable marker reads as a new session, which Muse then refuses loudly
        let result = if started {
            let resumed = client
                .request(
                    "session/resume",
                    json!({"sessionId": self.session_id, "commandId": command_id(), "excludeItems": true}),
                    COMMAND_TIMEOUT,
                )
                .await?;
            client
                .request(
                    "session/setApprovalMode",
                    json!({"sessionId": self.session_id, "commandId": command_id(), "mode": APPROVAL_MODE}),
                    COMMAND_TIMEOUT,
                )
                .await?;
            resumed
        } else {
            let mut params = json!({
                "sessionId": self.session_id,
                "commandId": command_id(),
                "workspaceRoot": workspace_root(&self.config.work_dir),
                "providerId": PROVIDER,
                "approvalMode": APPROVAL_MODE,
            });
            if let Some(model) = model {
                params["modelId"] = json!(model);
            }
            client
                .request("session/start", params, COMMAND_TIMEOUT)
                .await?
        };
        let session = &result["session"];
        if session.get("sessionId").and_then(Value::as_str) != Some(self.session_id.as_str()) {
            return Err(RpcError::Remote {
                code: 0,
                message: "muse returned a different session identity".into(),
                data: None,
            });
        }
        fold.model = session
            .get("modelId")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(model) = model
            && fold.model.as_deref() != Some(model)
        {
            client
                .request(
                    "session/setModel",
                    json!({"sessionId": self.session_id, "commandId": command_id(),
                           "model": {"modelId": model, "providerId": PROVIDER}}),
                    COMMAND_TIMEOUT,
                )
                .await?;
            fold.model = Some(model.to_string());
        }
        Ok(())
    }

    /// Answer one approval stage through the shared seam. Muse advances
    /// requirements under one approval id, so each stage is answered once.
    fn approval(&self, client: &RpcClient, approval: &Value, asked: &mut HashSet<String>) {
        let Some(approval_id) = approval.get("approvalId").and_then(Value::as_str) else {
            return;
        };
        let status = approval
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending");
        if matches!(
            status,
            "approved" | "denied" | "aborted" | "cancelled" | "completed"
        ) {
            return;
        }
        let requirement = approval
            .get("currentRequirementId")
            .cloned()
            .unwrap_or(Value::Null);
        if !asked.insert(format!("{approval_id}:{requirement}")) {
            return;
        }
        let subject = &approval["subject"];
        let request = PermissionRequest {
            runtime: self.config.runtime_id.clone(),
            session_id: self.session_id.clone(),
            tool: approval
                .get("toolName")
                .or_else(|| subject.get("kind"))
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string(),
            subject: ["command", "path", "host", "target"]
                .iter()
                .find_map(|k| subject.get(*k).and_then(Value::as_str))
                .map(str::to_string),
        };
        let Some(choice) = choice_for(approval, permissions::decide(&request)) else {
            return;
        };
        let params = json!({
            "sessionId": self.session_id,
            "commandId": command_id(),
            "approvalId": approval_id,
            "requirementId": requirement,
            "choiceId": choice,
        });
        let client = client.clone();
        tokio::spawn(async move {
            // Muse may already have settled it (its own rules decide many).
            if let Err(error) = client
                .request("approval/decide", params, COMMAND_TIMEOUT)
                .await
            {
                tracing::debug!(error = %error.label(), "muse: approval decision not admitted");
            }
        });
    }
}

/// The native choice for a decision. A denial takes the once-scoped
/// `denied` choice, else `abort` (shell approvals offer no `denied`).
fn choice_for(approval: &Value, decision: PermissionDecision) -> Option<String> {
    let choices: Vec<&Value> = approval["availableChoices"]
        .as_array()
        .into_iter()
        .flatten()
        .collect();
    let pick = |decisions: &[&str]| {
        decisions.iter().find_map(|wanted| {
            let mut matching: Vec<&&Value> = choices
                .iter()
                .filter(|c| c.get("decision").and_then(Value::as_str) == Some(wanted))
                .collect();
            // Prefer the narrowest scope.
            matching.sort_by_key(|c| c.get("scope").and_then(Value::as_str) != Some("once"));
            matching
                .first()
                .and_then(|c| c.get("choiceId").and_then(Value::as_str))
                .map(str::to_string)
        })
    };
    match decision {
        PermissionDecision::AllowOnce => pick(&["approved"]),
        PermissionDecision::Deny => pick(&["denied", "abort"]),
    }
}

fn open_failed(error: &RpcError) -> TurnOutcome {
    let lower = error.message().to_ascii_lowercase();
    let message = if lower.contains("auth") || lower.contains("login") {
        "muse: sign-in required (run `muse login`)".to_string()
    } else {
        format!("muse session could not open ({})", error.label())
    };
    TurnOutcome::ended(TurnEndStatus::Failed, Some(message))
}

/// `turn/start` and `session/start` take the canonical path (macOS `/tmp`
/// is `/private/tmp`).
fn workspace_root(dir: &Path) -> String {
    std::fs::canonicalize(dir)
        .unwrap_or_else(|_| dir.to_path_buf()) // silent-ok: Muse rejects a bad root itself
        .display()
        .to_string()
}

/// After the turn: let the host drain on stdin EOF (it ends the session
/// durably), then make sure the whole group is gone.
async fn close_host(child: &mut Child, pgid: i32) {
    if tokio::time::timeout(DRAIN, child.wait()).await.is_err() {
        rpc::end_group(child, pgid, rpc::KILL_GRACE).await;
    } else {
        let _ = killpg(Pid::from_raw(pgid), Signal::SIGKILL); // silent-ok: usually nothing is left
    }
}

#[async_trait]
impl TurnRuntime for MuseRuntime {
    fn id(&self) -> RuntimeId {
        self.id.clone()
    }

    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities {
            native_fork: false,
            // `turn/steer` injects into the running turn.
            steer_in_turn: true,
            reports_usage: true,
            reports_restart_cuts: false,
        }
    }

    async fn create_thread(&self, request: CreateThread) -> Result<CreatedThread, RuntimeError> {
        let thread_id = ThreadId::new(uuid::Uuid::new_v4().to_string())
            .map_err(|e| RuntimeError::Rejected(e.to_string()))?;
        Ok(CreatedThread {
            thread_id,
            native_session_id: Some(uuid::Uuid::now_v7().to_string()),
            provider: Some(self.config.runtime_id.clone()),
            model: request.model,
        })
    }

    async fn start_turn(
        &self,
        thread: &ThreadRecord,
        input: TurnInput,
    ) -> Result<TurnAdmission, RuntimeError> {
        if self.is_active(&thread.thread_id) {
            return Ok(TurnAdmission::Busy);
        }
        let mut thread = thread.clone();
        if input.model.is_some() {
            thread.model = input.model;
        }
        let run_id = self.spawn_turn(&thread, &input.text).await?;
        Ok(TurnAdmission::Started {
            run_id: Some(run_id),
        })
    }

    async fn steer(
        &self,
        thread: &ThreadRecord,
        text: &str,
        how: SteerHow,
    ) -> Result<SteerOutcome, RuntimeError> {
        if how == SteerHow::Steer
            && let Some(live) = self.live(&thread.thread_id)
        {
            let params = json!({
                "sessionId": live.session_id,
                "commandId": command_id(),
                "expectedTurnId": live.turn_id,
                "input": [{"type": "text", "text": text}],
            });
            match live
                .client
                .request("turn/steer", params, COMMAND_TIMEOUT)
                .await
            {
                Ok(_) => {
                    journal::append(
                        &self.config.transcript(&live.session_id),
                        &[journal::message(
                            HistoryRole::User,
                            text.to_string(),
                            Vec::new(),
                        )],
                    )
                    .await;
                    return Ok(SteerOutcome::Steered);
                }
                // The turn ended or moved on; fall back to a fresh turn.
                Err(error) => tracing::debug!(error = %error.label(), "muse: steer refused"),
            }
        }
        self.stop(&thread.thread_id).await;
        self.spawn_turn(thread, text).await?;
        Ok(SteerOutcome::Restarted)
    }

    async fn cancel(&self, thread: &ThreadRecord, _reason: &str) -> Result<(), RuntimeError> {
        self.stop(&thread.thread_id).await;
        Ok(())
    }

    async fn history(
        &self,
        thread: &ThreadRecord,
        limit: usize,
    ) -> Result<Vec<HistoryMessage>, RuntimeError> {
        let Some(session) = thread.native_session_id.as_deref() else {
            return Ok(Vec::new());
        };
        journal::read(&self.config.transcript(session), limit).await
    }

    /// MSP has no sign-in signal; like the Hermes driver, a catalog answer
    /// counts as signed in (a logged-out account fails on its first turn).
    async fn catalog(&self, _scope: &ThreadScope) -> Result<Vec<CatalogProvider>, RuntimeError> {
        let catalog = self.models().await;
        let mut models = catalog.models.clone();
        if let Some(default) = &catalog.default {
            models.retain(|m| m != default);
            models.insert(0, default.clone());
        }
        Ok(vec![CatalogProvider {
            provider: self.config.runtime_id.clone(),
            name: self.config.display_name.clone(),
            authenticated: !models.is_empty(),
            current: false,
            models,
        }])
    }
}

/// The `muse` launcher.
pub fn find_binary(home: &Path) -> Option<PathBuf> {
    rpc::find_on_path(home, "muse")
}

pub fn default_config(home: &Path, binary: &Path) -> MuseConfig {
    let state = home.join(".ironclaw").join("muse-code");
    MuseConfig {
        runtime_id: "muse-code".into(),
        display_name: "Muse Code".into(),
        binary: binary.to_path_buf(),
        work_dir: state.join("work"),
        state_dir: state,
        usage_file: Some(home.join(".ironclaw").join("usage").join("muse-code.json")),
        reasoning_effort: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denial_takes_the_once_denied_choice_else_abort() {
        let approval = json!({"availableChoices": [
            {"choiceId": "ok", "decision": "approved", "scope": "once"},
            {"choiceId": "no-session", "decision": "denied", "scope": "session"},
            {"choiceId": "no", "decision": "denied", "scope": "once"},
            {"choiceId": "stop", "decision": "abort"},
        ]});
        assert_eq!(
            choice_for(&approval, PermissionDecision::Deny).as_deref(),
            Some("no")
        );
        let shell = json!({"availableChoices": [
            {"choiceId": "ok", "decision": "approved", "scope": "once"},
            {"choiceId": "stop", "decision": "abort"},
        ]});
        assert_eq!(
            choice_for(&shell, PermissionDecision::Deny).as_deref(),
            Some("stop")
        );
    }
}
