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
//! read-only and nothing is asked. Codex's own subagents (`multi_agent`)
//! are disabled: subagents are orchestrator threads (`t3_delegate`).
//!
//! While a turn runs, reply and reasoning-summary deltas, tool rows with
//! their streaming output and the turn's diff go to the bridge's live
//! state (`live.rs`). Review and compaction are `thread_action`s
//! (`actions.rs`); idle threads compact like Claude CLI threads do
//! (`compact.rs`).

mod actions;
mod admission;
mod compact;
mod fold;
pub(crate) mod import;
pub(crate) mod live;
mod plan;
mod session;
mod turn_end;

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
use crate::steer_door::{Knock, Settled, SteerDoor, SteerPipe, SteerRequest, report_settlement};
use fold::TurnFold;
use ironclaw_product_contracts::orchestration::{ThreadAction, ThreadActionOutcome};

/// The Codex release the wire handling was checked against.
pub const EXPECTED_CLI_VERSION: &str = "0.166.0";

const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// How many already-received messages a steer folds before its cut, at
/// most, before Stop is looked at again.
const DRAIN_BATCH: usize = 64;
/// How long a `turn/steer` may take before it is judged without its
/// answer (see `steer_door`): short, since the person's send waits on it.
const STEER_TIMEOUT: Duration = Duration::from_secs(15);
/// Codex features that let the agent spawn its own subagents.
const CODEX_SUBAGENT_FEATURES: [&str; 2] = ["multi_agent", "multi_agent_v2"];

/// How long an app-server may drain after stdin closes before its group is killed.
const DRAIN: Duration = Duration::from_secs(10);
/// How long a `model/list` answer is reused.
const CATALOG_TTL: Duration = Duration::from_secs(600);
/// Session holders a new turn or action stops instead of waiting for.
const PREEMPTIBLE: [&str; 2] = ["idle compaction", "compact"];
/// How often throttled live detail (output tails, the diff) is flushed.
const LIVE_FLUSH_EVERY: Duration = Duration::from_millis(400);

/// What a turn sends Codex: a prompt (`turn/start`) or a review
/// (`review/start`, findings as the answer).
#[derive(Debug, Clone)]
enum TurnKind {
    Prompt,
    Review(ironclaw_product_contracts::orchestration::ReviewTarget),
}

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
        transcript_in(&self.state_dir, session)
    }

    fn thread_file(&self, session: &str) -> PathBuf {
        thread_file_in(&self.state_dir, session)
    }
}

/// IronClaw's transcript of a session (`HistoryMessage` per line).
pub(crate) fn transcript_in(state_dir: &Path, session: &str) -> PathBuf {
    state_dir
        .join("transcripts")
        .join(format!("{session}.jsonl"))
}

/// The Codex thread id a session resumes (`thread/resume`).
pub(crate) fn thread_file_in(state_dir: &Path, session: &str) -> PathBuf {
    state_dir.join("threads").join(format!("{session}.id"))
}

/// Where a session runs when it is not the shared work dir: an imported
/// terminal session's folder and its identity (`crate::workspace`).
pub(crate) fn cwd_file_in(state_dir: &Path, session: &str) -> PathBuf {
    state_dir.join("threads").join(format!("{session}.cwd"))
}

/// The driver's state folder (`~/.ironclaw/codex`).
pub fn state_dir(home: &Path) -> PathBuf {
    home.join(".ironclaw").join("codex")
}

/// What a steer or Stop needs from the running turn.
#[derive(Clone)]
struct LiveTurn {
    client: RpcClient,
    codex_thread: String,
    turn_id: String,
    /// IronClaw's run id, which gates are matched under.
    run_id: String,
}

type Door = Arc<SteerDoor<LiveTurn>>;

struct ActiveTurn {
    pgid: i32,
    cancelled: Arc<AtomicBool>,
    door: Door,
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
    /// How long a `turn/steer` waits for its answer ([`STEER_TIMEOUT`]).
    steer_timeout: Duration,
    /// How long one frame may take to go onto the server's stdin.
    write_timeout: Duration,
    /// Idle compaction timers, one per thread (see `compact.rs`).
    compactor: Arc<compact::IdleCompactor>,
    /// One holder per session: turns, thread actions, idle compaction.
    admission: Arc<admission::Admission>,
    /// How the profile's skills are listed and their reads counted.
    skills: Arc<crate::skill_listing::CliSkills>,
}

impl CodexRuntime {
    pub fn new(
        config: CodexConfig,
        sink: Weak<dyn TurnEventSink>,
        bridge: Option<Arc<CliBridge>>,
    ) -> Result<Self, ironclaw_orchestrator::error::OrchestratorError> {
        let config_runtime_id = config.runtime_id.clone();
        Ok(Self {
            id: RuntimeId::new(config.runtime_id.clone())?,
            config: Arc::new(config),
            sink,
            bridge,
            active: Arc::new(Mutex::new(HashMap::new())),
            catalog: Arc::new(tokio::sync::Mutex::new(Catalog::default())),
            primed: crate::Primed::default(),
            steer_timeout: STEER_TIMEOUT,
            write_timeout: rpc::WRITE_TIMEOUT,
            compactor: Arc::default(),
            admission: Arc::default(),
            skills: Arc::new(crate::skill_listing::CliSkills::new(
                crate::skill_listing::CliSkillUsage::default(),
                &config_runtime_id,
            )),
        })
    }

    /// List the profile's skills from the shared skill usage ledger (the
    /// default lists every skill in full).
    pub fn with_skill_usage(mut self, usage: crate::skill_listing::CliSkillUsage) -> Self {
        self.skills = Arc::new(crate::skill_listing::CliSkills::new(
            usage,
            &self.config.runtime_id,
        ));
        self
    }

    /// Bound one write to the server's stdin (default 30 s); a write past it
    /// closes the server's input and ends the turn. Tests shorten it.
    pub fn with_write_timeout(mut self, timeout: Duration) -> Self {
        self.write_timeout = timeout;
        self
    }

    /// Bound how long a steer waits for `turn/steer`'s answer (default
    /// [`STEER_TIMEOUT`]); tests shorten it.
    pub fn with_steer_timeout(mut self, timeout: Duration) -> Self {
        self.steer_timeout = timeout;
        self
    }

    /// Reserve `thread`'s session for `what`: a running compaction is
    /// stopped first (and waited for), then the claim is taken or `None`
    /// while a turn or another action holds it.
    async fn admit(&self, thread: &ThreadId, what: &'static str) -> Option<admission::Claim> {
        // Only a compaction yields; a turn or a review keeps the session.
        if self
            .admission
            .holder(thread)
            .is_some_and(|h| !PREEMPTIBLE.contains(&h))
        {
            return None;
        }
        self.compactor.preempt(thread).await;
        self.admission.try_claim(thread, what)
    }

    fn spawn_server(&self) -> Result<(Child, i32), RuntimeError> {
        spawn_app_server(&self.config)
    }

    async fn spawn_turn(
        &self,
        thread: &ThreadRecord,
        text: &str,
        context: Option<&str>,
        effort: Option<&str>,
        kind: TurnKind,
        claim: admission::Claim,
    ) -> Result<String, RuntimeError> {
        let session = thread
            .native_session_id
            .clone()
            .ok_or_else(|| RuntimeError::Rejected("thread has no codex session".into()))?;
        tokio::fs::create_dir_all(&self.config.work_dir)
            .await
            .map_err(|e| RuntimeError::Unavailable(format!("work dir: {}", e.kind())))?;
        // An imported session resumes in its own folder, or not at all.
        let cwd = crate::workspace::resolve(&cwd_file_in(&self.config.state_dir, &session))
            .await?
            .unwrap_or_else(|| self.config.work_dir.clone());
        let (mut child, pgid) = self.spawn_server()?;
        let (client, inbound) = connect_bounded(&mut child, pgid, self.write_timeout)?;

        let run_id = uuid::Uuid::new_v4().to_string();
        let cancelled = Arc::new(AtomicBool::new(false));
        let (door, steer_rx) = SteerDoor::new();
        let door: Door = Arc::new(door);
        let (done_tx, done_rx) = watch::channel(false);
        if let Ok(mut active) = self.active.lock() {
            active.insert(
                thread.thread_id.clone(),
                ActiveTurn {
                    pgid,
                    cancelled: Arc::clone(&cancelled),
                    door: Arc::clone(&door),
                    done: done_rx,
                },
            );
        }
        let turn = Turn {
            config: Arc::clone(&self.config),
            session,
            cwd,
            model: thread.model.clone(),
            text: text.to_string(),
            context: context.map(str::to_string),
            effort: effort.map(str::to_string),
            cancelled,
            door,
            steer_rx: Some(steer_rx),
            steer_timeout: self.steer_timeout,
            bridge: self.bridge.clone(),
            thread_id: thread.thread_id.clone(),
            run_id: run_id.clone(),
            kind,
            skills: Arc::clone(&self.skills),
            skill_reads: std::sync::Mutex::new(self.skills.watch(&thread.thread_id)),
        };
        let idle = compact::IdleArm::new(self, thread);
        let bridge = self.bridge.clone();
        let sink = self.sink.clone();
        let thread_id = thread.thread_id.clone();
        let run = run_id.clone();
        // Owns the teardown: a panic or a dropped task still clears the
        // live state, frees the session and reports the turn.
        let mut end = turn_end::TurnEnd::new(turn_end::TurnEndParts {
            thread_id: thread_id.clone(),
            run: run.clone(),
            active: Arc::clone(&self.active),
            bridge: bridge.clone(),
            sink: sink.clone(),
            claim,
            done: done_tx,
        });
        tokio::spawn(async move {
            if let Some(sink) = sink.upgrade() {
                sink.turn_started(&thread_id, Some(run.clone()), now_ms())
                    .await;
            }
            if let Some(bridge) = &bridge {
                bridge.turn_started(&thread_id, &run);
            }
            let (outcome, telemetry, steers) = turn.run(client, inbound).await;
            close_server(&mut child, pgid).await;
            // Steers held for a start that never came go back to the engine
            // before the turn is reported (and before the session frees).
            let outcome = report_settlement(
                &sink,
                &thread_id,
                &run,
                steers.unread,
                steers.unconfirmed,
                outcome,
            )
            .await;
            end.settle(outcome.status.as_str());
            let context_used = telemetry.as_ref().and_then(|t| t.context_used);
            let status = outcome.status;
            if let Some(sink) = sink.upgrade() {
                if let Some(telemetry) = telemetry {
                    sink.turn_telemetry(&thread_id, telemetry, now_ms()).await;
                }
                end.reporting();
                sink.turn_finished(&thread_id, Some(run), outcome, now_ms())
                    .await;
            }
            end.finish();
            idle.arm(status, context_used);
        });
        Ok(run_id)
    }

    fn door(&self, thread: &ThreadId) -> Option<Door> {
        self.active
            .lock()
            .ok() // silent-ok: a poisoned map only follows a panic elsewhere
            .and_then(|a| a.get(thread).map(|t| Arc::clone(&t.door)))
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
        // Queued under the door's lock: no steer is queued after it.
        let interrupt = self.door(thread).and_then(|door| {
            door.interrupt(|live| {
                let params = json!({"threadId": live.codex_thread, "turnId": live.turn_id});
                live.client.start_request("turn/interrupt", params)
            })
        });
        match interrupt {
            Some(Ok(reply)) => {
                if let Err(error) = reply.wait(rpc::KILL_GRACE).await {
                    tracing::debug!(error = %error.label(), "codex: interrupt not admitted");
                }
            }
            Some(Err(error)) => {
                tracing::debug!(error = %error.label(), "codex: interrupt not sent");
            }
            None => {}
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
            #[expect(
                clippy::let_underscore_must_use,
                reason = "sender dropped means the turn task ended"
            )]
            let _ = done.wait_for(|d| *d).await;
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

/// One `codex app-server` over stdio in its own process group.
fn spawn_app_server(config: &CodexConfig) -> Result<(Child, i32), RuntimeError> {
    let mut cmd = Command::new(&config.binary);
    cmd.arg("app-server");
    // Subagents are orchestrator threads (`t3_delegate`), never Codex's
    // own hidden ones.
    for feature in CODEX_SUBAGENT_FEATURES {
        cmd.args(["--disable", feature]);
    }
    cmd.args(["--listen", "stdio://"])
        // Keep the CLI's ChatGPT login; never an API-key override.
        .env_remove("OPENAI_API_KEY")
        .env_remove("CODEX_API_KEY")
        .current_dir(&config.work_dir)
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

fn connect(
    child: &mut Child,
    pgid: i32,
) -> Result<(RpcClient, mpsc::UnboundedReceiver<Inbound>), RuntimeError> {
    connect_bounded(child, pgid, rpc::WRITE_TIMEOUT)
}

/// [`connect`] with another per-frame write bound.
fn connect_bounded(
    child: &mut Child,
    pgid: i32,
    write_timeout: Duration,
) -> Result<(RpcClient, mpsc::UnboundedReceiver<Inbound>), RuntimeError> {
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        crate::rpc::kill_group(pgid, "codex: half-spawned child");
        return Err(RuntimeError::Unavailable("codex stdio unavailable".into()));
    };
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::debug!(target: "codex", "stderr: {line}");
        }
    });
    Ok(RpcClient::start_bounded(
        stdin,
        stdout,
        "codex",
        write_timeout,
    ))
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
    /// The thread's working directory: the shared work dir, or an imported
    /// session's own folder (checked before the turn started).
    cwd: PathBuf,
    model: Option<String>,
    text: String,
    /// Profile preamble and thread goal, sent ahead of `text` but never journaled.
    context: Option<String>,
    /// The thread's effort pick; over the configured default.
    effort: Option<String>,
    cancelled: Arc<AtomicBool>,
    door: Door,
    /// Where the door's steers arrive; taken by `run`.
    steer_rx: Option<mpsc::UnboundedReceiver<SteerRequest>>,
    steer_timeout: Duration,
    // arch-exempt: optional_arc, a host without the gateway asks nothing, plan #v3-cli-bridge
    bridge: Option<Arc<CliBridge>>,
    thread_id: ThreadId,
    run_id: String,
    kind: TurnKind,
    skills: Arc<crate::skill_listing::CliSkills>,
    /// Which shown skills' SKILL.md this turn read.
    skill_reads: std::sync::Mutex<crate::skill_listing::SkillReadWatch>,
}

type Telemetry = Option<ironclaw_orchestrator::telemetry::TurnTelemetry>;

impl Turn {
    /// Drive the turn, then close its door (dropping every handle, so the
    /// server sees stdin close). Also returns the steers it never
    /// delivered: held for a start that never came, or refused.
    async fn run(
        mut self,
        client: RpcClient,
        inbound: mpsc::UnboundedReceiver<Inbound>,
    ) -> (TurnOutcome, Telemetry, Settled) {
        let mut steers = self
            .steer_rx
            .take()
            .unwrap_or_else(|| mpsc::unbounded_channel().1);
        let mut unread = Settled::default();
        let (outcome, telemetry) = self.drive(client, inbound, &mut steers, &mut unread).await;
        // A turn that never reached its loop: what it held goes back.
        let mut pipe = SteerPipe::new(&self.run_id, self.steer_timeout, Box::new(|_| true));
        if self.cancelled.load(Ordering::SeqCst) || self.door.stopping() {
            pipe.stop();
        }
        let mut settled = pipe.finish(&self.door, &mut steers, true).await;
        settled.entries.clear();
        unread.extend(settled);
        let reads = self
            .skill_reads
            .lock()
            .map(|mut w| std::mem::take(&mut *w))
            .unwrap_or_default(); // silent-ok: a poisoned watch only follows a panic elsewhere
        self.skills.record(reads).await;
        (outcome, telemetry, unread)
    }

    /// Settled steers: their entries into the transcript, in order; the held
    /// ones that never reached Codex back to the engine.
    async fn record(&self, transcript: &Path, mut settled: Settled, steers: &mut Settled) {
        if !settled.entries.is_empty() {
            journal::append(transcript, &settled.entries).await;
        }
        settled.entries.clear();
        steers.extend(settled);
    }

    fn policy(&self) -> (&'static str, &'static str) {
        session_policy(self.bridge.is_some())
    }

    /// One message from Codex, folded and shown. `true` once the turn has
    /// completed.
    fn on_inbound(
        &self,
        client: &RpcClient,
        codex_thread: &str,
        root: &str,
        fold: &mut TurnFold,
        live: &mut live::LiveFold,
        message: Inbound,
    ) -> bool {
        match message {
            Inbound::Notification { method, params } => {
                if params
                    .get("threadId")
                    .and_then(Value::as_str)
                    .is_some_and(|t| t != codex_thread)
                {
                    return false;
                }
                let steps = live.push(&method, &params);
                live::apply(
                    self.bridge.as_deref(),
                    &self.thread_id,
                    &self.run_id,
                    root,
                    steps,
                );
                plan::on_notification(
                    self.bridge.as_deref(),
                    &self.thread_id,
                    &self.run_id,
                    fold.turn_id.as_deref(),
                    &method,
                    &params,
                );
                if let Ok(mut reads) = self.skill_reads.lock() {
                    reads.push_codex(&method, &params);
                }
                fold.push(&method, &params)
            }
            Inbound::Request { id, method, params } => {
                self.answer_request(client, id, &method, &params);
                false
            }
        }
    }

    async fn drive(
        &self,
        client: RpcClient,
        mut inbound: mpsc::UnboundedReceiver<Inbound>,
        steers: &mut mpsc::UnboundedReceiver<SteerRequest>,
        unread: &mut Settled,
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
        let root = session::workspace_root(&self.cwd).await;
        let user_row = HistoryMessage {
            run_id: Some(self.run_id.clone()),
            ..journal::message(HistoryRole::User, self.text.clone(), Vec::new())
        };
        // What the person typed is kept even when Codex refuses it; a
        // review's request only once Codex admitted it.
        if matches!(self.kind, TurnKind::Prompt) {
            journal::append(&transcript, std::slice::from_ref(&user_row)).await;
        }

        let Some(admitted) = self.start(&client, &codex_thread).await else {
            // Stop reached the turn first: its prompt is never sent.
            return (TurnOutcome::ended(TurnEndStatus::Cancelled, None), None);
        };
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
        // Which native turn this run is, and where it works: Review's
        // workspace check reads the root back.
        if let Err(error) = session::append(
            &self.config.state_dir,
            &self.session,
            &session::turn_record(&self.run_id, &turn_id, &root),
        )
        .await
        {
            tracing::debug!(%error, "codex: turn not recorded");
        }
        if !matches!(self.kind, TurnKind::Prompt) {
            journal::append(&transcript, std::slice::from_ref(&user_row)).await;
        }
        // Stop can interrupt the turn from here on, and steers (held ones
        // first, in order) go to it from the loop below.
        let live_turn = LiveTurn {
            client: client.clone(),
            codex_thread: codex_thread.clone(),
            turn_id: turn_id.clone(),
            run_id: self.run_id.clone(),
        };
        if !self.door.open(live_turn) {
            // Stop came while `turn/start` was in flight and found no turn
            // to interrupt: interrupt it now that it has an id.
            let params = json!({"threadId": codex_thread, "turnId": turn_id});
            if let Err(error) = client
                .request("turn/interrupt", params, rpc::KILL_GRACE)
                .await
            {
                tracing::debug!(error = %error.label(), "codex: late interrupt not admitted");
            }
        }
        // Codex answers `turn/steer` with the turn it joined; any answer
        // to an `expectedTurnId` request is for this turn.
        let mut pipe = SteerPipe::new(&self.run_id, self.steer_timeout, Box::new(|_| true));

        let mut live = live::LiveFold::for_turn(fold.turn_id.clone());
        let mut flush = tokio::time::interval(LIVE_FLUSH_EVERY);
        let mut done = false;
        let mut lost_input = false;
        while !done {
            tokio::select! {
                message = inbound.recv() => match message {
                    Some(message) => {
                        done = self.on_inbound(&client, &codex_thread, &root, &mut fold, &mut live, message);
                    }
                    None => done = true,
                },
                // The next steer. Stop's flag is looked at again where the
                // frame is queued (under the door's lock).
                Some(request) = steers.recv(), if !self.cancelled.load(Ordering::SeqCst) => {
                    let input = codex_input(&request.steer.text).await;
                    // Fold what Codex already sent, so the cut is where its
                    // reply really stands (a bounded batch: Stop is looked
                    // at again right after).
                    for _ in 0..DRAIN_BATCH {
                        if done {
                            break;
                        }
                        match inbound.try_recv() {
                            Ok(message) => {
                                done = self.on_inbound(&client, &codex_thread, &root, &mut fold, &mut live, message);
                            }
                            Err(_) => break,
                        }
                    }
                    if self.door.stopping() {
                        pipe.stop();
                    }
                    if done || self.door.stopping() {
                        let settled = pipe.refuse(request);
                        self.record(&transcript, settled, unread).await;
                        continue;
                    }
                    let segment = fold.take_segment();
                    let params = json!({
                        "threadId": codex_thread,
                        "expectedTurnId": turn_id,
                        "input": input,
                    });
                    let started = self
                        .door
                        .submit(|live| live.client.start_request_acked("turn/steer", params));
                    let settled = pipe.sent(request, segment, started);
                    self.record(&transcript, settled, unread).await;
                }
                event = pipe.event(), if pipe.busy() => {
                    // Past Stop, nothing not delivered is queued again.
                    if self.door.stopping() {
                        pipe.stop();
                    }
                    let settled = pipe.on_event(event);
                    self.record(&transcript, settled, unread).await;
                }
                _ = flush.tick() => {
                    if let Some(bridge) = &self.bridge {
                        bridge.flush_details(&self.thread_id, &self.run_id);
                    }
                }
                // Codex can no longer be reached: end the turn now.
                () = client.writer_failed() => {
                    lost_input = true;
                    done = true;
                }
            }
        }
        if lost_input {
            tracing::debug!("codex: input to the server failed; turn ended");
        }

        // The turn is over: the door closes, steers still in flight settle
        // (their rows before the final reply), unsent ones are refused.
        let stopped = self.cancelled.load(Ordering::SeqCst) || self.door.stopping();
        if stopped {
            pipe.stop();
        }
        let settled = pipe.finish(&self.door, steers, stopped).await;
        self.record(&transcript, settled, unread).await;
        let mut outcome = fold.outcome(self.cancelled.load(Ordering::SeqCst));
        if lost_input && outcome.status == TurnEndStatus::Completed {
            outcome = TurnOutcome::ended(
                TurnEndStatus::Failed,
                Some("codex: the server stopped taking input".into()),
            );
        }
        let entries: Vec<HistoryMessage> = fold
            .final_entries()
            .into_iter()
            .map(|entry| HistoryMessage {
                run_id: Some(self.run_id.clone()),
                ..entry
            })
            .collect();
        if !entries.is_empty() {
            journal::append(&transcript, &entries).await;
        }
        (outcome, fold.telemetry(&self.config.runtime_id))
    }

    /// `turn/start` with the prompt, or `review/start` for a review.
    /// `None` when Stop reached the turn first: the frame is queued under the
    /// door's lock, so it goes out before Stop or never.
    async fn start(
        &self,
        client: &RpcClient,
        codex_thread: &str,
    ) -> Option<Result<Value, RpcError>> {
        let (method, params) = match &self.kind {
            TurnKind::Review(target) => {
                ("review/start", actions::review_params(codex_thread, target))
            }
            _ => ("turn/start", self.start_params(codex_thread).await),
        };
        let queued = self.door.start(|| client.start_request(method, params))?;
        Some(match queued {
            Ok(reply) => reply.wait(COMMAND_TIMEOUT).await,
            Err(error) => Err(error),
        })
    }

    async fn start_params(&self, codex_thread: &str) -> Value {
        let text = crate::with_context(&self.text, self.context.as_deref());
        let mut params = json!({
            "threadId": codex_thread,
            "input": codex_input(&text).await,
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
        params
    }

    /// `initialize`, then resume the saved Codex thread or start one, in
    /// the sandbox and approval policy above. Returns the Codex thread id.
    async fn open(&self, client: &RpcClient, fold: &mut TurnFold) -> Result<String, RpcError> {
        let (id, model) = open_session(
            client,
            &self.config,
            &self.session,
            &self.cwd,
            self.model.as_deref(),
            self.policy(),
        )
        .await?;
        fold.model = model;
        Ok(id)
    }

    /// Server requests: approvals go through the shared seam; everything a
    /// person would have to answer mid-turn is declined.
    fn answer_request(&self, client: &RpcClient, id: Value, method: &str, params: &Value) {
        if plan::answer_question(
            self.bridge.as_ref(),
            client,
            &self.thread_id,
            &self.run_id,
            &id,
            method,
            params,
        ) {
            return;
        }
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
            // Codex approval requests carry an optional model-written `reason`.
            reason: crate::approval_reason::explicit(params, &["reason"]),
        };
        let client = client.clone();
        let bridge = self.bridge.clone();
        let door = Arc::clone(&self.door);
        let (thread, run) = (self.thread_id.clone(), self.run_id.clone());
        let claim = permissions::claim(bridge.as_deref(), &thread, &run);
        // The decision waits for a person; the turn's events keep flowing.
        tokio::spawn(async move {
            let (decision, mut receipt) =
                permissions::decide_receipted(bridge.as_deref(), &thread, &run, &request, claim)
                    .await;
            // Codex's answer carries no message: plain either way.
            receipt.claim();
            let feedback = match &decision {
                PermissionDecision::Deny { feedback } => feedback.clone(),
                PermissionDecision::AllowOnce => None,
            };
            let answer = match (decision, v2) {
                (PermissionDecision::AllowOnce, true) => json!({"decision": "accept"}),
                (PermissionDecision::Deny { .. }, true) => json!({"decision": "decline"}),
                (PermissionDecision::AllowOnce, false) => json!({"decision": "approved"}),
                (PermissionDecision::Deny { .. }, false) => json!({"decision": "denied"}),
            };
            // The receipt follows the reply onto Codex's stdin: a steer that
            // declined this call goes out only after it.
            match client.respond_acked(&id, answer) {
                Ok(mut written) => receipt.delivered(written.outcome().await),
                Err(error) => {
                    tracing::debug!(error = %error.label(), "codex: approval reply not sent");
                    receipt.delivered(false);
                    return;
                }
            }
            if let Some(feedback) = feedback
                && !steer_rejection_feedback(&door, &feedback).await
            {
                deliver_later(bridge.as_deref(), &thread, &feedback).await;
            }
        });
    }
}

/// `initialize`, then resume `session`'s saved Codex thread or start one
/// in the given `(approval policy, sandbox)`. Returns the Codex thread id
/// and the model Codex picked.
async fn open_session(
    client: &RpcClient,
    config: &CodexConfig,
    session: &str,
    cwd: &Path,
    model: Option<&str>,
    (approval, sandbox): (&str, &str),
) -> Result<(String, Option<String>), RpcError> {
    handshake(client).await?;
    let cwd = canonical(cwd);
    let file = config.thread_file(session);
    let saved = saved_thread(config, session).await;
    let mut params = json!({
        "cwd": cwd,
        "approvalPolicy": approval,
        "sandbox": sandbox,
    });
    if let Some(model) = model {
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
    let model = result
        .get("model")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok((id, model))
}

/// The Codex thread id saved for `session`; `None` before its first turn.
async fn saved_thread(config: &CodexConfig, session: &str) -> Option<String> {
    tokio::fs::read_to_string(config.thread_file(session))
        .await
        .ok() // silent-ok: no file means the session has not started yet
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// `(approval policy, sandbox)`: with a bridge to ask, the workspace
/// sandbox and `untrusted`; without, read-only and nothing asked.
fn session_policy(bridged: bool) -> (&'static str, &'static str) {
    if bridged {
        ("untrusted", "workspace-write")
    } else {
        ("never", "read-only")
    }
}

/// Codex's approval reply has no message field, so a rejection's feedback
/// ("no, do X instead") follows the decline as a steer into the same turn,
/// through the turn's door like any steer: in order, after Stop never, and
/// judged on evidence. `true` once it is in the turn (delivered, or written
/// with no word either way: queueing it then could deliver it twice);
/// `false` when it definitely is not, and the caller queues it.
async fn steer_rejection_feedback(
    door: &SteerDoor<LiveTurn>,
    feedback: &ironclaw_host_api::turn::DenialFeedback,
) -> bool {
    match door.knock_feedback(&crate::bridge::rejection_message(feedback)) {
        Knock::Sent(verdict) => verdict.await == Ok(SteerOutcome::Steered),
        Knock::Held => true,
        Knock::Closed => false,
    }
}

/// Feedback the turn could not take waits for the next turn instead of
/// vanishing (see `CliBridge::queue_undelivered_feedback`).
async fn deliver_later(
    bridge: Option<&CliBridge>,
    thread: &ThreadId,
    feedback: &ironclaw_host_api::turn::DenialFeedback,
) {
    let queued = match bridge {
        Some(bridge) => bridge.queue_undelivered_feedback(thread, feedback).await,
        None => false,
    };
    if !queued {
        tracing::debug!("codex: rejection feedback could not be delivered or queued");
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
/// A turn's `input`: the text, then each attached picture as a data URL
/// (T3 Code's Codex shape). The text keeps the file paths either way.
async fn codex_input(text: &str) -> Value {
    let mut input = vec![json!({"type": "text", "text": text, "text_elements": []})];
    for image in crate::attachments::inline_images(text).await {
        input.push(json!({"type": "image", "url": image.data_url()}));
    }
    Value::Array(input)
}

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
        crate::rpc::kill_group(pgid, "codex: server group");
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
            host_files: true,
        }
    }

    /// `turn/steer` joins the running turn without cancelling its tool
    /// calls (t3code: no `activeSteeringInterruptsTools`).
    fn steer_interrupts_tools(&self) -> bool {
        false
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
        // A new turn ends the idle period: a compaction is stopped first.
        let Some(claim) = self.admit(&thread.thread_id, "turn").await else {
            return Ok(TurnAdmission::Busy);
        };
        let mut thread = thread.clone();
        if input.model.is_some() {
            thread.model = input.model;
        }
        let listed = self
            .skills
            .listed_profile(&thread.thread_id, input.profile.as_ref(), &input.text)
            .await;
        let context = crate::prompt_context(
            crate::join_notes([
                self.primed.take(&thread.thread_id, listed.profile.as_ref()),
                listed.requested,
            ]),
            input.goal.as_deref(),
        );
        let run_id = self
            .spawn_turn(
                &thread,
                &input.text,
                context.as_deref(),
                input.effort.as_deref(),
                TurnKind::Prompt,
                claim,
            )
            .await?;
        Ok(TurnAdmission::Started {
            run_id: Some(run_id),
        })
    }

    /// A steer joins the running turn (T3 Code's Codex steer), through the
    /// turn's door (see `steer_door`):
    /// - pending approvals are declined first and the steer waits until
    ///   those replies are on Codex's stdin (its decline carries no
    ///   message); a pending question is answered with the text, which
    ///   Codex's reply cannot carry, so the text still goes as the steer;
    /// - while the turn starts the steer is held and sent once it has an
    ///   id; a turn that never starts reports it unread;
    /// - otherwise this waits for the loop's verdict on its `turn/steer`.
    async fn steer(
        &self,
        thread: &ThreadRecord,
        text: &str,
        how: SteerHow,
    ) -> Result<SteerOutcome, RuntimeError> {
        let request = ironclaw_orchestrator::runtime::SteerRequest {
            id: ironclaw_orchestrator::runtime::SteerId::generate(),
            text: text.to_string(),
            how,
        };
        self.steer_message(thread, &request).await
    }

    /// The engine's steer, under its id (the transcript row and every report
    /// about it carry it). `TurnEnded` once the turn is gone or past its
    /// end: the engine then starts the next turn with the text at once.
    async fn steer_message(
        &self,
        thread: &ThreadRecord,
        request: &ironclaw_orchestrator::runtime::SteerRequest,
    ) -> Result<SteerOutcome, RuntimeError> {
        let (text, how, id) = (request.text.as_str(), request.how, request.id.as_str());
        if how == SteerHow::Steer {
            let Some(door) = self.door(&thread.thread_id) else {
                return Ok(SteerOutcome::TurnEnded);
            };
            // One steer at a time from its declines to its knock: a second
            // one cannot overtake this one's declines.
            let admitted = door.admit().await;
            if let (Some(bridge), Some(live)) = (&self.bridge, door.live()) {
                let gates = bridge.steer_at_gates(&thread.thread_id, &live.run_id, text, false);
                if !gates.is_empty() && gates.settle().await {
                    return Ok(SteerOutcome::Steered);
                }
            }
            let knock = door.knock(id, text);
            drop(admitted);
            return Ok(match knock {
                Knock::Held => SteerOutcome::Steered,
                Knock::Closed => SteerOutcome::TurnEnded,
                // Dropped unanswered only with the turn's task.
                Knock::Sent(verdict) => verdict.await.unwrap_or(SteerOutcome::Rejected), // silent-ok: a lost verdict means the turn ended before sending it
            });
        }
        self.stop(&thread.thread_id).await;
        let Some(claim) = self.admit(&thread.thread_id, "turn").await else {
            return Ok(SteerOutcome::Rejected);
        };
        self.spawn_turn(thread, text, None, None, TurnKind::Prompt, claim)
            .await?;
        Ok(SteerOutcome::Restarted)
    }

    async fn cancel(&self, thread: &ThreadRecord, _reason: &str) -> Result<(), RuntimeError> {
        self.stop(&thread.thread_id).await;
        self.compactor.preempt(&thread.thread_id).await;
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

    async fn thread_action(
        &self,
        thread: &ThreadRecord,
        action: ThreadAction,
    ) -> Result<ThreadActionOutcome, RuntimeError> {
        let Some(claim) = self.admit(&thread.thread_id, action.label()).await else {
            return Ok(ThreadActionOutcome::Busy);
        };
        match action {
            ThreadAction::Review { target, repo } => {
                if let Some(refused) = self.workspace_refusal(thread, &repo).await? {
                    return Ok(refused);
                }
                let label = actions::review_label(&target);
                let run_id = self
                    .spawn_turn(thread, &label, None, None, TurnKind::Review(target), claim)
                    .await?;
                Ok(ThreadActionOutcome::Started {
                    run_id: Some(run_id),
                })
            }
            ThreadAction::Compact => self.compact_now(thread, claim).await,
        }
    }
}

/// The `codex` binary.
pub fn find_binary(home: &Path) -> Option<PathBuf> {
    rpc::find_on_path(home, "codex")
}

pub fn default_config(home: &Path, binary: &Path) -> CodexConfig {
    let state = state_dir(home);
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
