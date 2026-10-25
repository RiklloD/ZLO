//! OpenAI's Codex CLI (`codex app-server`, JSON-RPC over stdio) as an
//! orchestrator runtime, on the user's own `codex login` (their ChatGPT
//! plan): `thread/start` on the first turn and `thread/resume` after,
//! `turn/start`, `turn/steer`, `turn/interrupt`.
//!
//! One app-server process per turn. Codex persists the thread itself; its
//! id is kept at `<state_dir>/threads/<session>.id`, which also marks the
//! session as started. Transcripts for `history` are IronClaw's journal.
//!
//! Tools fail closed. With a bridge, threads run in the `workspace-write`
//! sandbox with the `untrusted` approval policy: anything outside Codex's
//! known-safe read-only commands asks first, and every request becomes an
//! IronClaw approval through [`crate::permissions::decide`] (allow once or
//! decline; never "for session"). Without a bridge the sandbox is
//! read-only and nothing is asked.

mod fold;

use std::collections::HashMap;
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

use crate::bridge::CliBridge;
use crate::journal;
use crate::permissions::{self, PermissionDecision, PermissionRequest};
use crate::rpc::{self, Inbound, RpcClient, RpcError};
use fold::TurnFold;

/// The Codex release the wire handling was checked against.
pub const EXPECTED_CLI_VERSION: &str = "0.163.0";

const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// How long an app-server may drain after stdin closes before its group is killed.
const DRAIN: Duration = Duration::from_secs(10);
/// How long a `model/list` answer is reused.
const CATALOG_TTL: Duration = Duration::from_secs(600);

#[derive(Debug, Clone)]
pub struct CodexConfig {
    /// Runtime and provider id (`codex`).
    pub runtime_id: String,
    pub display_name: String,
    pub binary: PathBuf,
    /// Working directory of every thread.
    pub work_dir: PathBuf,
    /// Thread-id files and transcripts.
    pub state_dir: PathBuf,
    /// Present once `codex login` ran.
    pub login_file: PathBuf,
    /// Reasoning effort sent with every turn; `None` keeps the model default.
    pub reasoning_effort: Option<String>,
}

impl CodexConfig {
    fn transcript(&self, session: &str) -> PathBuf {
        self.state_dir
            .join("transcripts")
            .join(format!("{session}.jsonl"))
    }

    fn thread_file(&self, session: &str) -> PathBuf {
        self.state_dir.join("threads").join(format!("{session}.id"))
    }
}

/// What a steer or Stop needs from the running turn.
#[derive(Clone)]
struct LiveTurn {
    client: RpcClient,
    codex_thread: String,
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
}

pub struct CodexRuntime {
    id: RuntimeId,
    config: Arc<CodexConfig>,
    sink: Weak<dyn TurnEventSink>,
    // arch-exempt: optional_arc, a host without the gateway keeps the sandbox read-only, plan #v3-cli-bridge
    bridge: Option<Arc<CliBridge>>,
    active: Arc<Mutex<HashMap<ThreadId, ActiveTurn>>>,
    catalog: Arc<tokio::sync::Mutex<Catalog>>,
    primed: crate::Primed,
}

impl CodexRuntime {
    pub fn new(
        config: CodexConfig,
        sink: Weak<dyn TurnEventSink>,
        bridge: Option<Arc<CliBridge>>,
    ) -> Result<Self, ironclaw_orchestrator::error::OrchestratorError> {
        Ok(Self {
            id: RuntimeId::new(config.runtime_id.clone())?,
            config: Arc::new(config),
            sink,
            bridge,
            active: Arc::new(Mutex::new(HashMap::new())),
            catalog: Arc::new(tokio::sync::Mutex::new(Catalog::default())),
            primed: crate::Primed::default(),
        })
    }

    fn is_active(&self, thread: &ThreadId) -> bool {
        self.active
            .lock()
            .map(|a| a.contains_key(thread))
            .unwrap_or(false) // silent-ok: a poisoned map only happens after a panic elsewhere
    }

    fn spawn_server(&self) -> Result<(Child, i32), RuntimeError> {
        let mut cmd = Command::new(&self.config.binary);
        cmd.args(["app-server", "--listen", "stdio://"])
            // Keep the CLI's ChatGPT login; never an API-key override.
            .env_remove("OPENAI_API_KEY")
            .env_remove("CODEX_API_KEY")
            .current_dir(&self.config.work_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        let child = cmd
            .spawn()
            .map_err(|e| RuntimeError::Unavailable(format!("spawn codex: {}", e.kind())))?;
        let pgid = rpc::group_of(&child)
            .ok_or_else(|| RuntimeError::Unavailable("codex exited at spawn".into()))?;
        Ok((child, pgid))
    }

    async fn spawn_turn(
        &self,
        thread: &ThreadRecord,
        text: &str,
        context: Option<&str>,
        effort: Option<&str>,
    ) -> Result<String, RuntimeError> {
        let session = thread
            .native_session_id
            .clone()
            .ok_or_else(|| RuntimeError::Rejected("thread has no codex session".into()))?;
        tokio::fs::create_dir_all(&self.config.work_dir)
            .await
            .map_err(|e| RuntimeError::Unavailable(format!("work dir: {}", e.kind())))?;
        let (mut child, pgid) = self.spawn_server()?;
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
        let turn = Turn {
            config: Arc::clone(&self.config),
            session,
            model: thread.model.clone(),
            text: text.to_string(),
            context: context.map(str::to_string),
            effort: effort.map(str::to_string),
            cancelled,
            live,
            bridge: self.bridge.clone(),
            thread_id: thread.thread_id.clone(),
            run_id: run_id.clone(),
        };
        let bridge = self.bridge.clone();
        let sink = self.sink.clone();
        let active = Arc::clone(&self.active);
        let thread_id = thread.thread_id.clone();
        let run = run_id.clone();
        tokio::spawn(async move {
            if let Some(sink) = sink.upgrade() {
                sink.turn_started(&thread_id, Some(run.clone()), now_ms())
                    .await;
            }
            if let Some(bridge) = &bridge {
                bridge.turn_started(&thread_id, &run);
            }
            let (outcome, telemetry) = turn.run(client, inbound).await;
            close_server(&mut child, pgid).await;
            if let Some(bridge) = &bridge {
                bridge.turn_ended(&thread_id, &run, outcome.status.as_str());
            }
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
            let params = json!({"threadId": live.codex_thread, "turnId": live.turn_id});
            if let Err(error) = live
                .client
                .request("turn/interrupt", params, rpc::KILL_GRACE)
                .await
            {
                tracing::debug!(error = %error.label(), "codex: interrupt not admitted");
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
            tracing::debug!(%error, "codex: SIGTERM failed");
        }
        if tokio::time::timeout(rpc::KILL_GRACE, done.wait_for(|d| *d))
            .await
            .is_err()
        {
            if let Err(error) = killpg(group, Signal::SIGKILL) {
                tracing::debug!(%error, "codex: SIGKILL failed");
            }
            let _ = done.wait_for(|d| *d).await; // silent-ok: sender dropped means the turn task ended
        }
    }

    /// `model/list` from a short-lived app-server, cached for [`CATALOG_TTL`].
    async fn models(&self) -> Catalog {
        let mut cached = self.catalog.lock().await;
        if cached.at.is_some_and(|at| at.elapsed() < CATALOG_TTL) {
            return cached.clone();
        }
        match self.probe().await {
            Ok(models) => {
                cached.at = Some(Instant::now());
                cached.models = models;
            }
            Err(error) => tracing::debug!(%error, "codex: model list unavailable"),
        }
        cached.clone()
    }

    async fn probe(&self) -> Result<Vec<String>, String> {
        tokio::fs::create_dir_all(&self.config.work_dir)
            .await
            .map_err(|e| format!("work dir: {}", e.kind()))?;
        let (mut child, pgid) = self.spawn_server().map_err(|e| e.to_string())?;
        let rows = async {
            let (client, _inbound) = connect(&mut child, pgid).map_err(|e| e.to_string())?;
            handshake(&client).await.map_err(|e| e.label())?;
            let list = client
                .request("model/list", json!({}), Duration::from_secs(15))
                .await
                .map_err(|e| e.label())?;
            Ok::<_, String>(fold::model_rows(&list).0)
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
        return Err(RuntimeError::Unavailable("codex stdio unavailable".into()));
    };
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::debug!(target: "codex", "stderr: {line}");
        }
    });
    Ok(RpcClient::start(stdin, stdout, "codex"))
}

/// `initialize` + `initialized`.
async fn handshake(client: &RpcClient) -> Result<Value, RpcError> {
    let init = client
        .request(
            "initialize",
            json!({
                "clientInfo": {"name": "ironclaw", "title": "IronClaw", "version": env!("CARGO_PKG_VERSION")},
                "capabilities": null,
            }),
            Duration::from_secs(20),
        )
        .await?;
    client.notify("initialized", None)?;
    Ok(init)
}

struct Turn {
    config: Arc<CodexConfig>,
    /// IronClaw's session key (`native_session_id`).
    session: String,
    model: Option<String>,
    text: String,
    /// Profile preamble and thread goal, sent ahead of `text` but never journaled.
    context: Option<String>,
    /// The thread's effort pick; over the configured default.
    effort: Option<String>,
    cancelled: Arc<AtomicBool>,
    live: Arc<Mutex<Option<LiveTurn>>>,
    // arch-exempt: optional_arc, a host without the gateway asks nothing, plan #v3-cli-bridge
    bridge: Option<Arc<CliBridge>>,
    thread_id: ThreadId,
    run_id: String,
}

type Telemetry = Option<ironclaw_orchestrator::telemetry::TurnTelemetry>;

impl Turn {
    async fn run(
        self,
        client: RpcClient,
        inbound: mpsc::UnboundedReceiver<Inbound>,
    ) -> (TurnOutcome, Telemetry) {
        let result = self.drive(client, inbound).await;
        // Drop every handle so the server sees stdin close.
        if let Ok(mut slot) = self.live.lock() {
            *slot = None;
        }
        result
    }

    fn policy(&self) -> (&'static str, &'static str) {
        if self.bridge.is_some() {
            ("untrusted", "workspace-write")
        } else {
            ("never", "read-only")
        }
    }

    async fn drive(
        &self,
        client: RpcClient,
        mut inbound: mpsc::UnboundedReceiver<Inbound>,
    ) -> (TurnOutcome, Telemetry) {
        let mut fold = TurnFold::default();
        let codex_thread = match self.open(&client, &mut fold).await {
            Ok(id) => id,
            Err(error) => return (open_failed(&error), None),
        };
        if self.cancelled.load(Ordering::SeqCst) {
            return (TurnOutcome::ended(TurnEndStatus::Cancelled, None), None);
        }
        let transcript = self.config.transcript(&self.session);
        journal::append(
            &transcript,
            &[journal::message(
                HistoryRole::User,
                self.text.clone(),
                Vec::new(),
            )],
        )
        .await;

        let text = crate::with_context(&self.text, self.context.as_deref());
        let mut params = json!({
            "threadId": codex_thread,
            "input": [{"type": "text", "text": text, "text_elements": []}],
        });
        if let Some(model) = &self.model {
            params["model"] = json!(model);
        }
        if let Some(effort) = self
            .effort
            .as_ref()
            .or(self.config.reasoning_effort.as_ref())
        {
            params["effort"] = json!(effort);
        }
        let admitted = client.request("turn/start", params, COMMAND_TIMEOUT).await;
        let turn_id = match admitted
            .as_ref()
            .ok()
            .and_then(|r| r["turn"].get("id"))
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
                        Some(format!("codex refused the turn ({label})")),
                    ),
                    None,
                );
            }
        };
        fold.turn_id = Some(turn_id.clone());
        if let Ok(mut slot) = self.live.lock() {
            *slot = Some(LiveTurn {
                client: client.clone(),
                codex_thread: codex_thread.clone(),
                turn_id,
            });
        }

        while let Some(message) = inbound.recv().await {
            match message {
                Inbound::Notification { method, params } => {
                    if params
                        .get("threadId")
                        .and_then(Value::as_str)
                        .is_some_and(|t| t != codex_thread)
                    {
                        continue;
                    }
                    if fold.push(&method, &params) {
                        break;
                    }
                }
                Inbound::Request { id, method, params } => {
                    self.answer_request(&client, id, &method, &params);
                }
            }
        }

        let outcome = fold.outcome(self.cancelled.load(Ordering::SeqCst));
        if let Some(entry) = fold.assistant_message() {
            journal::append(&transcript, &[entry]).await;
        }
        (outcome, fold.telemetry(&self.config.runtime_id))
    }

    /// `initialize`, then resume the saved Codex thread or start one, in
    /// the sandbox and approval policy above. Returns the Codex thread id.
    async fn open(&self, client: &RpcClient, fold: &mut TurnFold) -> Result<String, RpcError> {
        handshake(client).await?;
        let (approval, sandbox) = self.policy();
        let cwd = canonical(&self.config.work_dir);
        let file = self.config.thread_file(&self.session);
        let saved = tokio::fs::read_to_string(&file)
            .await
            .ok() // silent-ok: no file means the session has not started yet
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let mut params = json!({
            "cwd": cwd,
            "approvalPolicy": approval,
            "sandbox": sandbox,
        });
        if let Some(model) = &self.model {
            params["model"] = json!(model);
        }
        let result = match &saved {
            Some(id) => {
                params["threadId"] = json!(id);
                params["excludeTurns"] = json!(true);
                client
                    .request("thread/resume", params, COMMAND_TIMEOUT)
                    .await?
            }
            None => {
                params["serviceName"] = json!("ironclaw");
                client
                    .request("thread/start", params, COMMAND_TIMEOUT)
                    .await?
            }
        };
        let id = result["thread"]
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| RpcError::Remote {
                code: 0,
                message: "codex returned no thread id".into(),
                data: None,
            })?;
        if saved.as_deref() != Some(id.as_str()) {
            if let Some(dir) = file.parent()
                && let Err(error) = tokio::fs::create_dir_all(dir).await
            {
                tracing::debug!(error = %error.kind(), "codex: thread dir not created");
            }
            if let Err(error) = tokio::fs::write(&file, &id).await {
                tracing::debug!(error = %error.kind(), "codex: thread id not saved");
            }
        }
        fold.model = result
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok(id)
    }

    /// Server requests: approvals go through the shared seam; everything a
    /// person would have to answer mid-turn is declined.
    fn answer_request(&self, client: &RpcClient, id: Value, method: &str, params: &Value) {
        let approval = match method {
            "item/commandExecution/requestApproval" => Some((
                "shell",
                params
                    .get("command")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                true,
            )),
            "item/fileChange/requestApproval" => Some((
                "apply_patch",
                params
                    .get("grantRoot")
                    .or_else(|| params.get("reason"))
                    .and_then(Value::as_str)
                    .map(str::to_string),
                true,
            )),
            "execCommandApproval" | "applyPatchApproval" => Some((
                if method == "execCommandApproval" {
                    "shell"
                } else {
                    "apply_patch"
                },
                params
                    .get("command")
                    .and_then(Value::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(" ")
                    }),
                false,
            )),
            _ => None,
        };
        let Some((tool, subject, v2)) = approval else {
            let sent = client.respond_error(&id, -32601, "not supported by this client");
            if let Err(error) = sent {
                tracing::debug!(error = %error.label(), "codex: reply not sent");
            }
            return;
        };
        let request = PermissionRequest {
            runtime: self.config.runtime_id.clone(),
            session_id: self.session.clone(),
            tool: tool.to_string(),
            subject,
        };
        let client = client.clone();
        let bridge = self.bridge.clone();
        let (thread, run) = (self.thread_id.clone(), self.run_id.clone());
        // The decision waits for a person; the turn's events keep flowing.
        tokio::spawn(async move {
            let decision = permissions::decide(bridge.as_deref(), &thread, &run, &request).await;
            let answer = match (decision, v2) {
                (PermissionDecision::AllowOnce, true) => json!({"decision": "accept"}),
                (PermissionDecision::Deny, true) => json!({"decision": "decline"}),
                (PermissionDecision::AllowOnce, false) => json!({"decision": "approved"}),
                (PermissionDecision::Deny, false) => json!({"decision": "denied"}),
            };
            if let Err(error) = client.respond(&id, answer) {
                tracing::debug!(error = %error.label(), "codex: approval reply not sent");
            }
        });
    }
}

fn open_failed(error: &RpcError) -> TurnOutcome {
    let lower = error.message().to_ascii_lowercase();
    let message = if lower.contains("auth") || lower.contains("login") {
        "codex: sign-in required (run `codex login`)".to_string()
    } else {
        format!("codex thread could not open ({})", error.label())
    };
    TurnOutcome::ended(TurnEndStatus::Failed, Some(message))
}

/// The canonical path (macOS `/tmp` is `/private/tmp`).
fn canonical(dir: &Path) -> String {
    std::fs::canonicalize(dir)
        .unwrap_or_else(|_| dir.to_path_buf()) // silent-ok: Codex rejects a bad cwd itself
        .display()
        .to_string()
}

/// After the turn: let the server drain on stdin EOF, then make sure the
/// whole group is gone.
async fn close_server(child: &mut Child, pgid: i32) {
    if tokio::time::timeout(DRAIN, child.wait()).await.is_err() {
        rpc::end_group(child, pgid, rpc::KILL_GRACE).await;
    } else {
        let _ = killpg(Pid::from_raw(pgid), Signal::SIGKILL); // silent-ok: usually nothing is left
    }
}

#[async_trait]
impl TurnRuntime for CodexRuntime {
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
        let context = crate::prompt_context(
            self.primed.take(&thread.thread_id, input.profile.as_ref()),
            input.goal.as_deref(),
        );
        let run_id = self
            .spawn_turn(
                &thread,
                &input.text,
                context.as_deref(),
                input.effort.as_deref(),
            )
            .await?;
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
                "threadId": live.codex_thread,
                "expectedTurnId": live.turn_id,
                "input": [{"type": "text", "text": text, "text_elements": []}],
            });
            match live
                .client
                .request("turn/steer", params, COMMAND_TIMEOUT)
                .await
            {
                Ok(_) => {
                    if let Some(session) = thread.native_session_id.as_deref() {
                        journal::append(
                            &self.config.transcript(session),
                            &[journal::message(
                                HistoryRole::User,
                                text.to_string(),
                                Vec::new(),
                            )],
                        )
                        .await;
                    }
                    return Ok(SteerOutcome::Steered);
                }
                // The turn ended or moved on; fall back to a fresh turn.
                Err(error) => tracing::debug!(error = %error.label(), "codex: steer refused"),
            }
        }
        self.stop(&thread.thread_id).await;
        self.spawn_turn(thread, text, None, None).await?;
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

    async fn catalog(&self, _scope: &ThreadScope) -> Result<Vec<CatalogProvider>, RuntimeError> {
        let signed_in = self.config.login_file.is_file();
        let models = if signed_in {
            self.models().await.models
        } else {
            Vec::new()
        };
        Ok(vec![CatalogProvider {
            provider: self.config.runtime_id.clone(),
            name: self.config.display_name.clone(),
            authenticated: signed_in,
            current: false,
            models,
            efforts: ["low", "medium", "high", "xhigh"]
                .map(str::to_string)
                .to_vec(),
            model_efforts: Default::default(),
            brand: None,
        }])
    }
}

/// The `codex` binary.
pub fn find_binary(home: &Path) -> Option<PathBuf> {
    rpc::find_on_path(home, "codex")
}

pub fn default_config(home: &Path, binary: &Path) -> CodexConfig {
    let state = home.join(".ironclaw").join("codex");
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"));
    CodexConfig {
        runtime_id: "codex".into(),
        display_name: "Codex CLI".into(),
        binary: binary.to_path_buf(),
        work_dir: state.join("work"),
        state_dir: state,
        login_file: codex_home.join("auth.json"),
        reasoning_effort: None,
    }
}
