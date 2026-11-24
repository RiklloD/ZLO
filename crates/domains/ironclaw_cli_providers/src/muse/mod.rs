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
//! Tools fail closed: the host runs without `--trust-workspace` (no
//! workspace rules can allow anything), with `--disable-request-user-input`,
//! in `promptUnmatched` approval mode (`denyUnmatched` without a bridge) and
//! the `:ask-me` permission profile (Muse's default may let its own reviewer
//! decide), and every approval Muse asks for goes
//! through [`crate::permissions::decide`]: an IronClaw approval in the
//! thread when the host has a bridge. Without one, shell and writes are off
//! too (`--disable-shell --disable-write`) and approvals are denied.
//!
//! With a bridge the turn also streams live (`live.rs`: reply, reasoning,
//! tool rows, the todo list as the plan) and, when the gateway's MCP route is
//! known and the host grants `sessionMcp`, the orchestrator tools ride along
//! as an optional session MCP server. Compaction is a `thread_action`
//! (`session/compact`, `compact.rs`).

mod compact;
mod fold;
mod live;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
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
use crate::slot::GroupGuard;
use crate::steer_door::{Knock, Settled, SteerDoor, SteerPipe, SteerRequest, report_settlement};
use fold::{ModelRow, TurnFold};
use ironclaw_product_contracts::orchestration::{ThreadAction, ThreadActionOutcome};

/// The Muse release the wire handling was checked against.
pub const EXPECTED_HOST_VERSION: &str = "1.4.24";

const DEFAULT_MODEL: &str = "default";
const PROVIDER: &str = "meta";
/// Without a bridge: Muse's default-deny mode for automation.
const DENY_UNMATCHED: &str = "denyUnmatched";
/// With a bridge: unmatched calls become approval requests, which the bridge
/// answers ("Full access") or holds as a gate ("Ask first"). In
/// `denyUnmatched` Muse denies them itself and the bridge never sees them.
const PROMPT_UNMATCHED: &str = "promptUnmatched";
/// Ask the client about every call Muse's rules leave open, never Muse's own
/// AI reviewer (its default profile may let one decide).
const PERMISSION_PROFILE: &str = ":ask-me";
/// Tiers offered for a model the catalog reports none for (custom models).
const FALLBACK_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
/// The session MCP server carrying the orchestrator tools.
const MCP_SERVER: &str = "ironclaw";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
/// How many already-received messages a steer folds before its cut, at
/// most, before Stop is looked at again.
const DRAIN_BATCH: usize = 64;
/// How long a `turn/steer` may take before it is judged without its
/// answer (see `steer_door`): short, since the person's send waits on it.
const STEER_TIMEOUT: Duration = Duration::from_secs(15);
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
    /// Offer IronClaw's orchestrator tools as a session MCP server. Off by
    /// default: the MCP route does not gate calls itself, and that Muse asks
    /// the client before each MCP call under `:ask-me` is not yet verified
    /// against a live host, so the tools stay away until it is.
    pub orchestrator_tools: bool,
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
    /// Turns Muse started on its own inside this one; Stop ends them too.
    joined: Vec<String>,
    /// IronClaw's run id, which steers are journaled and gates matched under.
    run_id: String,
}

type Door = Arc<SteerDoor<LiveTurn>>;

struct ActiveTurn {
    pgid: i32,
    door: Door,
    done: watch::Receiver<bool>,
}

type Slots = crate::slot::Slots<ActiveTurn>;
type Claim = crate::slot::Claim<ActiveTurn>;

#[derive(Clone, Default)]
struct Catalog {
    at: Option<Instant>,
    rows: Vec<ModelRow>,
}

pub struct MuseRuntime {
    id: RuntimeId,
    config: Arc<MuseConfig>,
    sink: Weak<dyn TurnEventSink>,
    // arch-exempt: optional_arc, a host without the gateway keeps shell and writes off, plan #v3-cli-bridge
    bridge: Option<Arc<CliBridge>>,
    active: Slots,
    catalog: Arc<tokio::sync::Mutex<Catalog>>,
    primed: crate::Primed,
    /// What each thread's agent was sent, for the context meter.
    sent: Arc<crate::context_meter::SentContexts>,
    /// How long a `turn/steer` waits for its answer ([`STEER_TIMEOUT`]).
    steer_timeout: Duration,
    /// How the profile's skills are listed and their reads counted.
    skills: Arc<crate::skill_listing::CliSkills>,
    #[cfg(test)]
    prep_gate: Option<Arc<crate::slot::PrepGate>>,
}

impl MuseRuntime {
    pub fn new(
        config: MuseConfig,
        sink: Weak<dyn TurnEventSink>,
        bridge: Option<Arc<CliBridge>>,
    ) -> Result<Self, ironclaw_orchestrator::error::OrchestratorError> {
        let skills = Arc::new(crate::skill_listing::CliSkills::new(
            crate::skill_listing::CliSkillUsage::default(),
            &config.runtime_id,
        ));
        Ok(Self {
            id: RuntimeId::new(config.runtime_id.clone())?,
            config: Arc::new(config),
            sink,
            bridge,
            active: Slots::default(),
            catalog: Arc::new(tokio::sync::Mutex::new(Catalog::default())),
            primed: crate::Primed::default(),
            sent: Arc::default(),
            steer_timeout: STEER_TIMEOUT,
            skills,
            #[cfg(test)]
            prep_gate: None,
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

    /// Bound how long a steer waits for `turn/steer`'s answer (default
    /// [`STEER_TIMEOUT`]); tests shorten it.
    pub fn with_steer_timeout(mut self, timeout: Duration) -> Self {
        self.steer_timeout = timeout;
        self
    }

    fn claim(&self, thread: &ThreadId) -> Option<Claim> {
        self.active.claim(thread)
    }

    fn spawn_host(&self, read_only: bool) -> Result<(Child, i32), RuntimeError> {
        let mut cmd = Command::new(&self.config.binary);
        cmd.args([
            "serve",
            // Nobody answers the agent's questions mid-turn.
            "--disable-request-user-input",
        ]);
        // Without approvals there is nobody to ask: no shell, no writes.
        if self.bridge.is_none() {
            cmd.args(["--disable-shell", "--disable-write"]);
        }
        if read_only {
            cmd.arg("--no-session-log");
        }
        // Keep the CLI login: an inherited API key (any spelling) would
        // take priority over it and bill another account.
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().eq_ignore_ascii_case("META_API_KEY") {
                cmd.env_remove(&key);
            }
        }
        cmd.env("MUSE_NO_AUTO_UPDATE", "1")
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

    async fn spawn_turn(
        &self,
        claim: Claim,
        thread: &ThreadRecord,
        text: &str,
        context: Option<&str>,
        effort: Option<&str>,
        delivery: crate::context_meter::Delivery,
    ) -> Result<String, RuntimeError> {
        let session_id = thread
            .native_session_id
            .clone()
            .ok_or_else(|| RuntimeError::Rejected("thread has no muse session".into()))?;
        tokio::fs::create_dir_all(&self.config.work_dir)
            .await
            .map_err(|e| RuntimeError::Unavailable(format!("work dir: {}", e.kind())))?;
        #[cfg(test)]
        crate::slot::PrepGate::pass(self.prep_gate.as_ref()).await;
        let run_id = uuid::Uuid::new_v4().to_string();
        // A Stop while the turn prepared: the host is never launched.
        if claim.is_cancelled() {
            crate::slot::report_cancelled(
                claim,
                self.sink.clone(),
                thread.thread_id.clone(),
                run_id.clone(),
            );
            return Ok(run_id);
        }
        let (mut child, pgid) = self.spawn_host(false)?;
        let mut group = GroupGuard::new(pgid, "muse: dropped host group");
        let (client, inbound) = connect(&mut child, pgid)?;

        let cancelled = claim.cancel_flag();
        let (door, steer_rx) = SteerDoor::new();
        let door: Door = Arc::new(door);
        let (done_tx, done_rx) = watch::channel(false);
        let published = claim.fill(ActiveTurn {
            pgid,
            door: Arc::clone(&door),
            done: done_rx,
        });
        if !published {
            // Stopped while the host spawned: end it before it does anything.
            drop((client, inbound));
            close_host(&mut child, pgid).await;
            group.disarm();
            crate::slot::report_cancelled(
                claim,
                self.sink.clone(),
                thread.thread_id.clone(),
                run_id.clone(),
            );
            return Ok(run_id);
        }

        // Never wait on a catalog probe in flight; the rows are fallbacks.
        let models = self
            .catalog
            .try_lock()
            .map(|c| c.rows.iter().map(|r| (r.id.clone(), r.clone())).collect())
            .unwrap_or_default(); // silent-ok: a busy catalog only drops the context-max and effort fallbacks
        let mcp = self.mcp_grant(&thread.thread_id, &run_id);
        delivery.set_mcp_tools(mcp.is_some());
        let turn = Turn {
            config: Arc::clone(&self.config),
            session_id,
            model: thread.model.clone(),
            text: text.to_string(),
            context: context.map(str::to_string),
            effort: effort.map(str::to_string),
            cancelled,
            door,
            steer_rx: Some(steer_rx),
            steer_timeout: self.steer_timeout,
            models,
            mcp: mcp.clone(),
            delivery,
            bridge: self.bridge.clone(),
            thread_id: thread.thread_id.clone(),
            run_id: run_id.clone(),
            skills: Arc::clone(&self.skills),
            skill_reads: std::sync::Mutex::new(self.skills.watch(&thread.thread_id)),
        };
        let bridge = self.bridge.clone();
        let sink = self.sink.clone();
        let thread_id = thread.thread_id.clone();
        let run = run_id.clone();
        let mut report = crate::slot::ReportGuard::new(
            sink.clone(),
            bridge.clone(),
            thread_id.clone(),
            run.clone(),
            "muse",
        )
        .with_token(mcp.as_ref().map(|m| m.token.clone()));
        tokio::spawn(async move {
            if let Some(sink) = sink.upgrade() {
                sink.turn_started(&thread_id, Some(run.clone()), now_ms())
                    .await;
            }
            if let Some(bridge) = &bridge {
                bridge.turn_started(&thread_id, &run);
            }
            let (outcome, telemetry, steers) = turn.run(client, inbound).await;
            close_host(&mut child, pgid).await;
            // Steers held for a start that never came go back to the engine
            // before the turn is reported.
            let outcome = report_settlement(
                &sink,
                &thread_id,
                &run,
                steers.unread,
                steers.unconfirmed,
                outcome,
            )
            .await;
            group.disarm();
            report.disarm();
            if let Some(bridge) = &bridge {
                if let Some(mcp) = &mcp {
                    bridge.revoke_token(&mcp.token);
                }
                bridge.turn_ended(&thread_id, &run, outcome.status.as_str());
            }
            drop(claim);
            if let Some(sink) = sink.upgrade() {
                if let Some(telemetry) = telemetry {
                    sink.turn_telemetry(&thread_id, telemetry, now_ms()).await;
                }
                sink.turn_finished(&thread_id, Some(run), outcome, now_ms())
                    .await;
            }
            #[expect(
                clippy::let_underscore_must_use,
                reason = "nobody waiting on this turn"
            )]
            let _ = done_tx.send(true);
        });
        Ok(run_id)
    }

    /// A bearer token for the loopback MCP route, bound to this turn; `None`
    /// without a bridge or before the gateway reported its URL.
    fn mcp_grant(&self, thread: &ThreadId, run_id: &str) -> Option<McpGrant> {
        if !self.config.orchestrator_tools {
            return None;
        }
        let bridge = self.bridge.as_ref()?;
        let url = bridge.mcp_url()?.to_string();
        let token = bridge.mint_token(
            crate::bridge::SessionGrant {
                thread_id: thread.clone(),
                run_id: run_id.to_string(),
            },
            crate::bridge::SESSION_TOKEN_TTL,
        );
        Some(McpGrant { url, token })
    }

    fn door(&self, thread: &ThreadId) -> Option<Door> {
        self.active.with_turn(thread, |turn| Arc::clone(&turn.door))
    }

    /// `turn/interrupt`, then the process group if the turn does not settle
    /// within the grace period. Waits until the turn is reported.
    async fn stop(&self, thread: &ThreadId) {
        // Sets the turn's cancelled flag; a turn still preparing never runs.
        if !self.active.stop_admitted(thread).await {
            return;
        }
        let entry = self.active.with_turn(thread, |t| (t.pgid, t.done.clone()));
        let Some((pgid, mut done)) = entry else {
            return;
        };
        // Queued under the door's lock: no steer is queued after them.
        // Joined turns first: the admitted one ending ends the wait.
        let interrupts = self.door(thread).and_then(|door| {
            door.interrupt(|live| {
                live.joined
                    .iter()
                    .chain([&live.turn_id])
                    .map(|turn_id| {
                        let params = json!({
                            "sessionId": live.session_id,
                            "commandId": command_id(),
                            "turnId": turn_id,
                        });
                        live.client.start_request("turn/interrupt", params)
                    })
                    .collect::<Vec<_>>()
            })
        });
        for interrupt in interrupts.into_iter().flatten() {
            let answered = match interrupt {
                Ok(reply) => reply.wait(rpc::KILL_GRACE).await,
                Err(error) => Err(error),
            };
            if let Err(error) = answered {
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
            #[expect(
                clippy::let_underscore_must_use,
                reason = "sender dropped means the turn task ended"
            )]
            let _ = done.wait_for(|d| *d).await;
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
                cached.rows = rows;
            }
            Err(error) => tracing::debug!(%error, "muse: model list unavailable"),
        }
        cached.clone()
    }

    async fn probe(&self) -> Result<Vec<ModelRow>, String> {
        tokio::fs::create_dir_all(&self.config.work_dir)
            .await
            .map_err(|e| format!("work dir: {}", e.kind()))?;
        let (mut child, pgid) = self.spawn_host(true).map_err(|e| e.to_string())?;
        let rows = async {
            let (client, _inbound) = connect(&mut child, pgid).map_err(|e| e.to_string())?;
            handshake(&client, false).await.map_err(|e| e.label())?;
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
        crate::rpc::kill_group(pgid, "muse: half-spawned child");
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

/// `initialize` + `initialized`, refusing an envelope version we do not
/// speak. `session_mcp` asks for session MCP servers (a turn with tools).
async fn handshake(client: &RpcClient, session_mcp: bool) -> Result<Value, RpcError> {
    let requested: &[&str] = if session_mcp { &["sessionMcp"] } else { &[] };
    let init = client
        .request(
            "initialize",
            json!({
                "clientInfo": {"name": "ironclaw", "title": "IronClaw", "version": env!("CARGO_PKG_VERSION")},
                "capabilities": {"requestedCapabilities": requested},
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

/// The loopback MCP route and this turn's token for it.
#[derive(Clone)]
struct McpGrant {
    url: String,
    token: String,
}

impl McpGrant {
    /// Session config naming the route as an optional server: Muse fails a
    /// run whose required server is unreachable, so the turn goes on without
    /// the tools instead.
    fn session_config(&self) -> Value {
        json!({"mcpServers": {MCP_SERVER: {
            "transport": "streamableHttp",
            "mode": "optional",
            "url": self.url,
            "headers": {"Authorization": format!("Bearer {}", self.token)},
        }}})
    }
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
    /// Profile preamble and thread goal, sent ahead of `text` but never journaled.
    context: Option<String>,
    /// The thread's effort pick; over the configured default.
    effort: Option<String>,
    cancelled: Arc<AtomicBool>,
    door: Door,
    /// Where the door's steers arrive; taken by `run`.
    steer_rx: Option<mpsc::UnboundedReceiver<SteerRequest>>,
    steer_timeout: Duration,
    /// The catalog's rows by model id: context limits and reasoning tiers.
    models: HashMap<String, ModelRow>,
    /// The orchestrator tools' route, when the bridge serves one.
    mcp: Option<McpGrant>,
    /// What the prompt carries for the context meter; recorded once Muse
    /// admitted the turn.
    delivery: crate::context_meter::Delivery,
    // arch-exempt: optional_arc, a host without the gateway denies every approval, plan #v3-cli-bridge
    bridge: Option<Arc<CliBridge>>,
    thread_id: ThreadId,
    run_id: String,
    skills: Arc<crate::skill_listing::CliSkills>,
    /// Which shown skills' SKILL.md this turn read.
    skill_reads: std::sync::Mutex<crate::skill_listing::SkillReadWatch>,
}

type Telemetry = Option<ironclaw_orchestrator::telemetry::TurnTelemetry>;

impl Turn {
    /// Drive the turn, then close its door (dropping every handle, so the
    /// host sees stdin close). Also returns the steers it never delivered:
    /// held for a start that never came, or refused.
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
    /// ones that never reached Muse back to the engine.
    async fn record(&self, transcript: &Path, mut settled: Settled, steers: &mut Settled) {
        if !settled.entries.is_empty() {
            journal::append(transcript, &settled.entries).await;
        }
        settled.entries.clear();
        steers.extend(settled);
    }

    /// One of this turn's transcript rows, grouped under its run.
    fn row(&self, role: HistoryRole, text: String, tools: Vec<String>) -> HistoryMessage {
        HistoryMessage {
            run_id: Some(self.run_id.clone()),
            ..journal::message(role, text, tools)
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
        if let Err(error) = self.open(&client, &mut fold).await {
            return (open_failed(&error), None);
        }
        if self.cancelled.load(Ordering::SeqCst) {
            return (TurnOutcome::ended(TurnEndStatus::Cancelled, None), None);
        }
        let transcript = self.config.transcript(&self.session_id);
        journal::append(
            &transcript,
            &[self.row(HistoryRole::User, self.text.clone(), Vec::new())],
        )
        .await;

        let mut params = json!({
            "sessionId": self.session_id,
            "commandId": command_id(),
            "input": [{"type": "text", "text": crate::with_context(&self.text, self.context.as_deref())}],
            "displayText": self.text,
            "workspaceRoots": [workspace_root(&self.config.work_dir)],
            "ifBusy": "queue",
        });
        let picked = self
            .effort
            .as_deref()
            .or(self.config.reasoning_effort.as_deref());
        let effort = match fold.model.as_ref().and_then(|m| self.models.get(m)) {
            Some(row) => row.effort_for(picked),
            None => picked.map(str::to_string),
        };
        if let Some(effort) = effort {
            params["reasoningEffort"] = json!(effort);
        }
        // Queued under the door's lock Stop takes: before Stop, or never.
        let Some(queued) = self
            .door
            .start(|| client.start_request("turn/start", params))
        else {
            return (TurnOutcome::ended(TurnEndStatus::Cancelled, None), None);
        };
        let admitted = match queued {
            Ok(reply) => reply.wait(COMMAND_TIMEOUT).await,
            Err(error) => Err(error),
        };
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
        // Muse took the prompt: what it carried is what the meter reports.
        self.delivery.delivered();
        // Stop can interrupt the turn from here on, and steers (held ones
        // first, in order) go to it from the loop below.
        let live_turn = LiveTurn {
            client: client.clone(),
            session_id: self.session_id.clone(),
            turn_id: turn_id.clone(),
            joined: Vec::new(),
            run_id: self.run_id.clone(),
        };
        if !self.door.open(live_turn) {
            // Stop came while `turn/start` was in flight and found no turn
            // to interrupt: interrupt it now that it has an id.
            let params =
                json!({"sessionId": self.session_id, "commandId": command_id(), "turnId": turn_id});
            if let Err(error) = client
                .request("turn/interrupt", params, rpc::KILL_GRACE)
                .await
            {
                tracing::debug!(error = %error.label(), "muse: late interrupt not admitted");
            }
        }
        // Muse answers `turn/steer` with the turn it joined: only this one
        // counts (T3 Code checks the same).
        let expected = turn_id.clone();
        let mut pipe = SteerPipe::new(
            &self.run_id,
            self.steer_timeout,
            Box::new(move |answer| {
                answer.get("turnId").and_then(Value::as_str) == Some(expected.as_str())
            }),
        );

        let mut asked = HashSet::new();
        let mut shown = live::MuseLive::default();
        let root = workspace_root(&self.config.work_dir);
        let mut done = false;
        let mut lost_input = false;
        while !done {
            tokio::select! {
                message = inbound.recv() => match message {
                    Some(message) => {
                        done = self.on_inbound(&client, &root, &mut fold, &mut shown, &mut asked, message);
                    }
                    None => done = true,
                },
                // The next steer. Stop's flag is looked at again where the
                // frame is queued (under the door's lock).
                Some(request) = steers.recv(), if !self.cancelled.load(Ordering::SeqCst) => {
                    // Fold what Muse already sent, so the cut is where its
                    // reply really stands (a bounded batch: Stop is looked
                    // at again right after).
                    for _ in 0..DRAIN_BATCH {
                        if done {
                            break;
                        }
                        match inbound.try_recv() {
                            Ok(message) => {
                                done = self.on_inbound(&client, &root, &mut fold, &mut shown, &mut asked, message);
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
                        "sessionId": self.session_id,
                        "commandId": command_id(),
                        "expectedTurnId": turn_id,
                        "input": [{"type": "text", "text": request.steer.text}],
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
                // Muse can no longer be reached: end the turn now.
                () = client.writer_failed() => {
                    lost_input = true;
                    done = true;
                }
            }
        }

        // The turn is over: the door closes, steers still in flight settle
        // (their rows before the final reply), unsent ones are refused.
        let stopped = self.cancelled.load(Ordering::SeqCst) || self.door.stopping();
        if stopped {
            pipe.stop();
        }
        let settled = pipe.finish(&self.door, steers, stopped).await;
        self.record(&transcript, settled, unread).await;
        let mut outcome = fold.outcome(self.cancelled.load(Ordering::SeqCst), now_ms());
        if lost_input && outcome.status == TurnEndStatus::Completed {
            outcome = TurnOutcome::ended(
                TurnEndStatus::Failed,
                Some("muse: the host stopped taking input".into()),
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
        if let (Some(path), Some(reading)) = (&self.config.usage_file, &fold.limit_reading) {
            journal::write_limit_reading(path, "usage", reading).await;
        }
        let window = fold
            .model
            .as_ref()
            .and_then(|m| self.models.get(m)?.context_limit);
        (outcome, fold.telemetry(&self.config.runtime_id, window))
    }

    /// One message from Muse, folded and shown. `true` once the turn has
    /// completed.
    fn on_inbound(
        &self,
        client: &RpcClient,
        root: &str,
        fold: &mut TurnFold,
        shown: &mut live::MuseLive,
        asked: &mut HashSet<String>,
        message: Inbound,
    ) -> bool {
        match message {
            Inbound::Notification { method, params } => {
                if params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s != self.session_id)
                {
                    return false;
                }
                if method == "approval/requested" || method == "approval/updated" {
                    self.approval(client, &params, asked);
                }
                if let Ok(mut reads) = self.skill_reads.lock() {
                    reads.push_muse(&method, &params);
                }
                let done = fold.push(&method, &params);
                if method == "turn/started" {
                    let joined = fold.joined();
                    self.door.update(|live| live.joined = joined);
                }
                self.show(shown, fold, root, &method, &params);
                done
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
                false
            }
        }
    }

    /// `initialize`, the subscription reading, then start or resume the
    /// session in the default-deny approval mode and on the thread's model.
    async fn open(&self, client: &RpcClient, fold: &mut TurnFold) -> Result<(), RpcError> {
        let init = handshake(client, self.mcp.is_some()).await?;
        let granted = init["grantedCapabilities"]
            .as_array()
            .is_some_and(|c| c.iter().any(|c| c.as_str() == Some("sessionMcp")));
        let config = match &self.mcp {
            Some(mcp) if granted => Some(mcp.session_config()),
            Some(_) => {
                tracing::debug!("muse: host lacks sessionMcp; orchestrator tools off");
                self.delivery.set_mcp_tools(false);
                None
            }
            None => None,
        };
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
        let approval_mode = if self.bridge.is_some() {
            PROMPT_UNMATCHED
        } else {
            DENY_UNMATCHED
        };
        let result = if started {
            let mut params = json!({
                "sessionId": self.session_id,
                "commandId": command_id(),
                "excludeItems": true,
                "permissionProfile": PERMISSION_PROFILE,
            });
            if let Some(config) = &config {
                params["config"] = config.clone();
            }
            let resumed = client
                .request("session/resume", params, COMMAND_TIMEOUT)
                .await?;
            client
                .request(
                    "session/setApprovalMode",
                    json!({"sessionId": self.session_id, "commandId": command_id(), "mode": approval_mode}),
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
                "approvalMode": approval_mode,
                "permissionProfile": PERMISSION_PROFILE,
            });
            if let Some(model) = model {
                params["modelId"] = json!(model);
            }
            if let Some(config) = config {
                params["config"] = config;
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

    /// The live view of one notification, for whoever watches the thread.
    fn show(
        &self,
        shown: &mut live::MuseLive,
        fold: &TurnFold,
        root: &str,
        method: &str,
        params: &Value,
    ) {
        let Some(bridge) = self.bridge.as_deref() else {
            return;
        };
        if method == "session/todoListChanged" {
            if let Some(plan) = live::todo_plan(params) {
                bridge.plan_updated(&self.thread_id, &self.run_id, plan);
            }
            return;
        }
        let owned = fold.owns(params["item"].get("turnId").and_then(Value::as_str));
        let steps = shown.push(method, params, owned);
        crate::codex::live::apply(Some(bridge), &self.thread_id, &self.run_id, root, steps);
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
            // Used when Muse sends one; its approval schema does not promise it.
            reason: crate::approval_reason::explicit(approval, &["reason", "justification"])
                .or_else(|| crate::approval_reason::explicit(subject, &["reason", "description"])),
        };
        let client = client.clone();
        let approval = approval.clone();
        let approval_id = approval_id.to_string();
        let session_id = self.session_id.clone();
        let bridge = self.bridge.clone();
        let door = Arc::clone(&self.door);
        let (thread, run) = (self.thread_id.clone(), self.run_id.clone());
        let claim = permissions::claim(bridge.as_deref(), &thread, &run);
        // The decision waits for a person; the turn's events keep flowing.
        tokio::spawn(async move {
            let (decision, mut receipt) =
                permissions::decide_receipted(bridge.as_deref(), &thread, &run, &request, claim)
                    .await;
            // Muse's choice carries no message: plain either way.
            receipt.claim();
            let feedback = match &decision {
                PermissionDecision::Deny { feedback } => feedback.clone(),
                PermissionDecision::AllowOnce => None,
            };
            let Some(choice) = choice_for(&approval, &decision) else {
                receipt.delivered(false);
                return;
            };
            // `abort` ends Muse's turn by design: a steer could only reach a
            // finished turn, so the feedback goes straight to the next one.
            let ends_turn = choice_decision(&approval, &choice) == Some("abort");
            let params = json!({
                "sessionId": session_id,
                "commandId": command_id(),
                "approvalId": approval_id,
                "requirementId": requirement,
                "choiceId": choice,
            });
            // Muse may already have settled it (its own rules decide many).
            let admitted = match client
                .request("approval/decide", params, COMMAND_TIMEOUT)
                .await
            {
                Ok(_) => true,
                Err(error) => {
                    tracing::debug!(error = %error.label(), "muse: approval decision not admitted");
                    false
                }
            };
            // Muse answered the decision: a steer that declined this call
            // goes out only after it.
            receipt.delivered(admitted);
            let Some(feedback) = feedback else {
                return;
            };
            if !admitted || ends_turn || !steer_rejection_feedback(&door, &feedback).await {
                deliver_later(bridge.as_deref(), &thread, &feedback).await;
            }
        });
    }
}

/// Muse's approval choice carries no message, so a rejection's feedback
/// ("no, do X instead") follows the denial as a steer into the same turn,
/// through the turn's door like any steer (in order, never after Stop,
/// judged on evidence). `true` once it is in the turn or written with no
/// word either way; `false` when it definitely is not (the caller queues it).
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
        tracing::debug!("muse: rejection feedback could not be delivered or queued");
    }
}

/// The `decision` of `choice_id` among the approval's choices.
fn choice_decision<'a>(approval: &'a Value, choice_id: &str) -> Option<&'a str> {
    approval["availableChoices"]
        .as_array()?
        .iter()
        .find(|c| c.get("choiceId").and_then(Value::as_str) == Some(choice_id))?
        .get("decision")
        .and_then(Value::as_str)
}

/// The native choice for a decision. A denial takes the once-scoped
/// `denied` choice, else `abort` (shell approvals offer no `denied`).
fn choice_for(approval: &Value, decision: &PermissionDecision) -> Option<String> {
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
        PermissionDecision::Deny { .. } => pick(&["denied", "abort"]),
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
        crate::rpc::kill_group(pgid, "muse: host group");
    }
}

#[async_trait]
impl TurnRuntime for MuseRuntime {
    fn id(&self) -> RuntimeId {
        self.id.clone()
    }

    fn injected_context(
        &self,
        thread: &ThreadId,
        profile: Option<&TurnProfile>,
        goal: Option<&str>,
    ) -> ironclaw_orchestrator::context_report::ContextSample {
        self.sent.sample(thread, || {
            let notes = crate::session_context(profile).then(crate::goal_notes(goal));
            let mcp = self.config.orchestrator_tools
                && self.bridge.as_ref().is_some_and(|b| b.mcp_url().is_some());
            (notes, mcp)
        })
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
        let Some(claim) = self.claim(&thread.thread_id) else {
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
        // The preamble goes once per session (this process); the skills this
        // message asks for and the goal go with every turn that has them.
        let session = self.primed.take(&thread.thread_id, listed.profile.as_ref());
        let turn = crate::context_meter::Notes::default()
            .instructions(listed.requested)
            .then(crate::goal_notes(input.goal.as_deref()));
        let context = session
            .clone()
            .unwrap_or_default()
            .then(turn.clone())
            .text();
        // Recorded once Muse took the prompt: a failed launch keeps the
        // thread's earlier record.
        let delivery = self
            .sent
            .pending(&thread.thread_id, session.as_ref(), &turn, false);
        let run_id = self
            .spawn_turn(
                claim,
                &thread,
                &input.text,
                context.as_deref(),
                input.effort.as_deref(),
                delivery,
            )
            .await?;
        Ok(TurnAdmission::Started {
            run_id: Some(run_id),
        })
    }

    /// A steer joins the running turn (T3 Code's Muse steer), through the
    /// turn's door (see `steer_door`):
    /// - pending approvals are declined first and the steer waits until
    ///   Muse answered those decisions (its choice carries no message);
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
                Knock::Sent(verdict) => verdict.await.unwrap_or(SteerOutcome::Rejected), // silent-ok: a lost verdict means the turn ended before sending it
            });
        }
        self.stop(&thread.thread_id).await;
        // Something else took the slot meanwhile: the engine queues the text.
        let Some(claim) = self.claim(&thread.thread_id) else {
            return Ok(SteerOutcome::Rejected);
        };
        // The restarted host keeps the session's notes; this prompt has none.
        let delivery = self
            .sent
            .pending(&thread.thread_id, None, &Default::default(), false);
        self.spawn_turn(claim, thread, text, None, None, delivery)
            .await?;
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
        let mut models: Vec<String> = catalog.rows.iter().map(|r| r.id.clone()).collect();
        if let Some(default) = catalog.rows.iter().find(|r| r.is_default) {
            models.retain(|m| *m != default.id);
            models.insert(0, default.id.clone());
        }
        // Each model's own tiers; an unreported one takes the fallback.
        let model_efforts = catalog
            .rows
            .iter()
            .filter_map(|r| Some((r.id.clone(), r.efforts.clone()?)))
            .collect();
        Ok(vec![CatalogProvider {
            provider: self.config.runtime_id.clone(),
            name: self.config.display_name.clone(),
            authenticated: !models.is_empty(),
            current: false,
            models,
            efforts: FALLBACK_EFFORTS.map(str::to_string).to_vec(),
            model_efforts,
            brand: None,
        }])
    }

    /// Muse compacts near the window by itself; `Compact` asks it now.
    fn compaction(&self) -> RuntimeCompaction {
        RuntimeCompaction {
            manual_prompt: None,
            automatic: true,
        }
    }

    async fn thread_action(
        &self,
        thread: &ThreadRecord,
        action: ThreadAction,
    ) -> Result<ThreadActionOutcome, RuntimeError> {
        match action {
            ThreadAction::Compact => self.compact(thread).await,
            ThreadAction::Review { .. } => Err(RuntimeError::Unsupported("muse review")),
        }
    }
}

/// The `muse` launcher.
pub fn find_binary(home: &Path) -> Option<PathBuf> {
    rpc::find_on_path(home, "muse")
}

/// ZLO's Muse Code folder: its work folder and state.
pub fn state_dir(home: &Path) -> PathBuf {
    home.join(".ironclaw").join("muse-code")
}

pub fn default_config(home: &Path, binary: &Path) -> MuseConfig {
    let state = state_dir(home);
    MuseConfig {
        runtime_id: "muse-code".into(),
        display_name: "Muse Code".into(),
        binary: binary.to_path_buf(),
        work_dir: state.join("work"),
        state_dir: state,
        usage_file: Some(home.join(".ironclaw").join("usage").join("muse-code.json")),
        reasoning_effort: None,
        orchestrator_tools: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Review (2nd round, B): a Stop that lands while the turn prepares (no
    // host yet) ends it `Cancelled`, and `muse serve` is never launched.
    #[tokio::test]
    async fn stop_while_the_turn_prepares_never_launches_the_host() {
        use crate::slot::test_support::{Recorder, marker_host, stop_before_launch};
        let dir = tempfile::tempdir().unwrap();
        let (host, marker) = marker_host(dir.path());
        let recorder = Arc::new(Recorder::default());
        let mut rt = MuseRuntime::new(
            MuseConfig {
                runtime_id: "muse-code".into(),
                display_name: "Muse Code".into(),
                binary: host,
                work_dir: dir.path().join("work"),
                state_dir: dir.path().join("state"),
                usage_file: None,
                reasoning_effort: None,
                orchestrator_tools: false,
            },
            recorder.sink(),
            None,
        )
        .unwrap();
        let gate = Arc::new(crate::slot::PrepGate::default());
        rt.prep_gate = Some(Arc::clone(&gate));
        stop_before_launch(Arc::new(rt), gate, recorder, &marker).await;
    }

    #[test]
    fn denial_takes_the_once_denied_choice_else_abort() {
        let approval = json!({"availableChoices": [
            {"choiceId": "ok", "decision": "approved", "scope": "once"},
            {"choiceId": "no-session", "decision": "denied", "scope": "session"},
            {"choiceId": "no", "decision": "denied", "scope": "once"},
            {"choiceId": "stop", "decision": "abort"},
        ]});
        assert_eq!(
            choice_for(&approval, &PermissionDecision::DENY).as_deref(),
            Some("no")
        );
        let shell = json!({"availableChoices": [
            {"choiceId": "ok", "decision": "approved", "scope": "once"},
            {"choiceId": "stop", "decision": "abort"},
        ]});
        assert_eq!(
            choice_for(&shell, &PermissionDecision::DENY).as_deref(),
            Some("stop")
        );
    }
}
