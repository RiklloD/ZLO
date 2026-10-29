//! What a coding-agent CLI turn needs from IronClaw while it runs, shared by
//! every driver:
//!
//! - **Approvals.** A tool call the CLI wants to make waits here as a pending
//!   gate until the thread's owner answers it (`resolve_gate`), the turn ends,
//!   or [`CliBridge::approval_timeout`] passes. Anything but an explicit
//!   approval is a denial.
//! - **Live turn state.** Streamed reply text, thinking, tool calls and open
//!   gates per thread, with a broadcast of every change, so the product
//!   surface can stream a CLI turn the way it streams IronClaw's own. Text
//!   and thinking restart in a new segment at each tool call, so the thread
//!   shows them in the order they happened, between the calls.
//! - **Session tokens.** One bearer token per CLI process, bound to its
//!   thread and run, for the loopback MCP route that serves the orchestrator
//!   tools ([`handle_mcp`]). Revoked when the turn ends; expired ones fail.
//!
//! The live gates are not durable: the CLI child dies with the backend, and
//! so do its pending calls (they are denied by the process exit). Once
//! [`CliBridge::record_to`] has a store, every call is also written to
//! `orch_cli_approvals` when asked and when answered, and a call that cannot
//! be recorded is denied.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use ironclaw_host_api::ids::ThreadId;
use ironclaw_orchestrator::cli_approvals::{CliApprovalDecision, CliApprovalRecord};
use ironclaw_orchestrator::engine::{CallContext, Engine};
use ironclaw_orchestrator::model::now_ms;
use ironclaw_orchestrator::store::Store;
use ironclaw_orchestrator::tools::TOOLS;
use serde_json::{Value, json};
use tokio::sync::{broadcast, oneshot};

/// How long a tool call waits for a person before it is denied.
pub const DEFAULT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Upper bound on a session token's life; turn end revokes it sooner.
pub const SESSION_TOKEN_TTL: Duration = Duration::from_secs(12 * 60 * 60);
/// Name the orchestrator MCP server has inside the CLI (`mcp__ironclaw__*`).
pub const MCP_SERVER_NAME: &str = "ironclaw";
/// The live text is cumulative; broadcasting it at most this often keeps a
/// long reply from being copied once per token.
const TEXT_EMIT_EVERY: Duration = Duration::from_millis(50);
/// Bound on the tool-input preview shown with an approval.
const GATE_BODY_MAX_CHARS: usize = 2_000;
/// Bound on a tool row's subject (the wire allows 2 KiB).
const TOOL_SUBJECT_MAX_CHARS: usize = 400;
const EVENT_BUFFER: usize = 256;

/// The answer a CLI gets for one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolDecision {
    Allow,
    Deny(String),
}

/// One pending tool call as the person sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateView {
    pub gate_ref: String,
    pub run_id: String,
    pub tool_name: String,
    pub headline: String,
    pub body: Option<String>,
    /// Why the agent wants the call (see [`crate::approval_reason`]).
    pub reason: Option<String>,
}

/// Where one tool call the CLI ran stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolState {
    Running,
    Completed,
    Failed,
}

/// One tool call the CLI ran, as its row shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolView {
    /// A uuid minted by the bridge, stable for the call.
    pub invocation: String,
    pub run_id: String,
    pub tool_name: String,
    /// The command, path or host the call is about.
    pub subject: Option<String>,
    pub state: ToolState,
}

/// A change to a thread's live CLI turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveEvent {
    Started {
        thread: ThreadId,
        run_id: String,
    },
    /// The reply text of `segment` so far (cumulative, not a delta).
    Text {
        thread: ThreadId,
        run_id: String,
        segment: u32,
        body: String,
    },
    /// The model's thinking in `segment` so far (cumulative, not a delta).
    Thinking {
        thread: ThreadId,
        run_id: String,
        segment: u32,
        body: String,
    },
    /// A tool call started or finished.
    Tool {
        thread: ThreadId,
        tool: ToolView,
    },
    GateOpened {
        thread: ThreadId,
        gate: GateView,
    },
    GateClosed {
        thread: ThreadId,
        run_id: String,
        gate_ref: String,
    },
    /// `status` is a `TurnEndStatus` wire string (`completed`, `failed`, ...).
    Ended {
        thread: ThreadId,
        run_id: String,
        status: &'static str,
    },
}

impl LiveEvent {
    pub fn thread(&self) -> &ThreadId {
        match self {
            Self::Started { thread, .. }
            | Self::Text { thread, .. }
            | Self::Thinking { thread, .. }
            | Self::Tool { thread, .. }
            | Self::GateOpened { thread, .. }
            | Self::GateClosed { thread, .. }
            | Self::Ended { thread, .. } => thread,
        }
    }
}

/// A thread's running CLI turn, for a stream that connects mid-turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSnapshot {
    pub run_id: String,
    /// Every segment so far, oldest first.
    pub segments: Vec<SegmentView>,
    pub tools: Vec<ToolView>,
    pub gates: Vec<GateView>,
}

/// What the model wrote between two tool calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentView {
    pub segment: u32,
    pub thinking: String,
    pub text: String,
}

/// A tool call matched to its turn by [`CliBridge::claim_call`].
#[derive(Debug, Clone, Default)]
pub struct CallClaim {
    live: bool,
    /// What the model said between the previous call and this one.
    said: Option<String>,
    /// The CLI's own id for the call, when its protocol has one: its
    /// result finishes the call's row.
    call_id: Option<String>,
}

impl CallClaim {
    pub fn with_call_id(mut self, call_id: Option<String>) -> Self {
        self.call_id = call_id;
        self
    }
}

/// What a session token stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionGrant {
    pub thread_id: ThreadId,
    pub run_id: String,
}

struct TokenEntry {
    secret: String,
    grant: SessionGrant,
    expires_at: Instant,
}

struct PendingGate {
    view: GateView,
    answer: oneshot::Sender<(ToolDecision, CliApprovalDecision)>,
}

struct LiveTurn {
    run_id: String,
    /// The segment being written: text and thinking since the last call.
    segment: u32,
    text: LiveBuffer,
    thinking: LiveBuffer,
    /// Earlier segments, for a stream that connects mid-turn.
    past: Vec<SegmentView>,
    /// Calls by the CLI's own id, when it has one.
    tools: Vec<(Option<String>, ToolView)>,
    gates: Vec<PendingGate>,
}

/// Unsent bodies of one segment, thinking before text.
type Unsent = (u32, Option<String>, Option<String>);

impl LiveTurn {
    fn new(run_id: &str) -> Self {
        Self {
            run_id: run_id.to_string(),
            segment: 0,
            text: LiveBuffer::default(),
            thinking: LiveBuffer::default(),
            past: Vec::new(),
            tools: Vec::new(),
            gates: Vec::new(),
        }
    }

    /// The reason for a call: what the model told the person since the
    /// previous one. Thinking already shows as its own row, so it is never
    /// borrowed (the thread drew the same paragraph twice).
    fn said_reason(&self) -> Option<String> {
        crate::approval_reason::from_said(&self.text.body)
    }

    /// Everything not broadcast yet, thinking before text.
    fn flush(&mut self) -> Unsent {
        (self.segment, self.thinking.flush(), self.text.flush())
    }

    /// A tool call: what was written so far closes as a segment, and what
    /// follows starts the next one below the call.
    fn cut_segment(&mut self) -> Unsent {
        let unsent = self.flush();
        if !self.text.body.is_empty() || !self.thinking.body.is_empty() {
            self.past.push(SegmentView {
                segment: self.segment,
                thinking: std::mem::take(&mut self.thinking.body),
                text: std::mem::take(&mut self.text.body),
            });
            self.thinking = LiveBuffer::default();
            self.text = LiveBuffer::default();
            self.segment += 1;
        }
        unsent
    }

    /// The model went on writing, so the calls it waited on are done.
    fn settle_tools(&mut self, state: ToolState) -> Vec<ToolView> {
        self.tools
            .iter_mut()
            .filter(|(_, t)| t.state == ToolState::Running)
            .map(|(_, t)| {
                t.state = state;
                t.clone()
            })
            .collect()
    }
}

/// One cumulative streamed body, broadcast at most every `TEXT_EMIT_EVERY`.
#[derive(Default)]
struct LiveBuffer {
    body: String,
    /// When the body was last broadcast; `None` while nothing is unsent.
    sent_at: Option<Instant>,
    unsent: bool,
}

impl LiveBuffer {
    /// Append a delta; `new_block` separates blocks with a blank line, the
    /// same join the durable timeline uses. Returns the body when it is due.
    fn push(&mut self, delta: &str, new_block: bool) -> Option<String> {
        if new_block && !self.body.is_empty() {
            self.body.push_str("\n\n");
        }
        self.body.push_str(delta);
        self.unsent = true;
        if self
            .sent_at
            .is_some_and(|at| at.elapsed() < TEXT_EMIT_EVERY)
        {
            return None;
        }
        self.flush()
    }

    /// The body to broadcast now, if it changed since the last one.
    fn flush(&mut self) -> Option<String> {
        if !self.unsent {
            return None;
        }
        self.unsent = false;
        self.sent_at = Some(Instant::now());
        Some(self.body.clone())
    }
}

pub struct CliBridge {
    approval_timeout: Duration,
    tokens: Mutex<HashMap<String, TokenEntry>>,
    live: Mutex<HashMap<ThreadId, LiveTurn>>,
    events: broadcast::Sender<LiveEvent>,
    /// The loopback MCP route, known once the gateway listener is bound.
    mcp_url: OnceLock<String>,
    /// Where approvals are recorded, set once by the host.
    ledger: OnceLock<Store>,
}

impl std::fmt::Debug for CliBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CliBridge").finish_non_exhaustive()
    }
}

impl CliBridge {
    pub fn new(approval_timeout: Duration) -> Arc<Self> {
        Arc::new(Self {
            approval_timeout,
            tokens: Mutex::new(HashMap::new()),
            live: Mutex::new(HashMap::new()),
            events: broadcast::channel(EVENT_BUFFER).0,
            mcp_url: OnceLock::new(),
            ledger: OnceLock::new(),
        })
    }

    pub fn approval_timeout(&self) -> Duration {
        self.approval_timeout
    }

    /// Record the loopback MCP endpoint (first call wins).
    pub fn set_mcp_url(&self, url: String) {
        let _ = self.mcp_url.set(url); // silent-ok: the gateway binds once per process
    }

    /// Record every approval in `store` from now on (first call wins).
    pub fn record_to(&self, store: Store) {
        let _ = self.ledger.set(store); // silent-ok: the host wires one store per process
    }

    /// Whether `thread`'s profile is on "Full access", so its CLI tool calls
    /// run without a gate. An unset mode keeps asking: CLI calls never
    /// followed the global auto-approve switch. Read per call, so a switch
    /// reaches running turns on their next tool call.
    async fn profile_full_access(&self, thread: &ThreadId) -> bool {
        let Some(ledger) = self.ledger.get() else {
            return false;
        };
        match ledger.thread_profile(thread).await {
            Ok(Some(profile)) => profile.settings.auto_approve(false),
            Ok(None) => false,
            Err(error) => {
                tracing::debug!(%error, "cli bridge: profile approval mode unreadable; asking");
                false
            }
        }
    }

    pub fn mcp_url(&self) -> Option<&str> {
        self.mcp_url.get().map(String::as_str)
    }

    // ---- session tokens -------------------------------------------------

    /// Mint `<id>.<secret>` for one CLI process.
    pub fn mint_token(&self, grant: SessionGrant, ttl: Duration) -> String {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let secret = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let token = format!("{id}.{secret}");
        if let Ok(mut tokens) = self.tokens.lock() {
            let now = Instant::now();
            tokens.retain(|_, e| e.expires_at > now);
            tokens.insert(
                id,
                TokenEntry {
                    secret,
                    grant,
                    expires_at: now + ttl,
                },
            );
        }
        token
    }

    /// The grant behind a live token; `None` for unknown, wrong or expired.
    pub fn resolve_token(&self, token: &str) -> Option<SessionGrant> {
        let (id, secret) = token.split_once('.')?;
        let tokens = self.tokens.lock().ok()?;
        let entry = tokens.get(id)?;
        (constant_time_eq(entry.secret.as_bytes(), secret.as_bytes())
            && entry.expires_at > Instant::now())
        .then(|| entry.grant.clone())
    }

    pub fn revoke_token(&self, token: &str) {
        if let (Some((id, _)), Ok(mut tokens)) = (token.split_once('.'), self.tokens.lock()) {
            tokens.remove(id);
        }
    }

    // ---- live turn state ------------------------------------------------

    pub fn subscribe(&self) -> broadcast::Receiver<LiveEvent> {
        self.events.subscribe()
    }

    pub fn snapshot(&self, thread: &ThreadId) -> Option<LiveSnapshot> {
        let live = self.live.lock().ok()?;
        let turn = live.get(thread)?;
        let mut segments = turn.past.clone();
        segments.push(SegmentView {
            segment: turn.segment,
            thinking: turn.thinking.body.clone(),
            text: turn.text.body.clone(),
        });
        Some(LiveSnapshot {
            run_id: turn.run_id.clone(),
            segments,
            tools: turn.tools.iter().map(|(_, t)| t.clone()).collect(),
            gates: turn.gates.iter().map(|g| g.view.clone()).collect(),
        })
    }

    fn emit(&self, event: LiveEvent) {
        let _ = self.events.send(event); // silent-ok: no stream is open for this thread
    }

    pub fn turn_started(&self, thread: &ThreadId, run_id: &str) {
        if let Ok(mut live) = self.live.lock() {
            live.insert(thread.clone(), LiveTurn::new(run_id));
        }
        self.emit(LiveEvent::Started {
            thread: thread.clone(),
            run_id: run_id.to_string(),
        });
    }

    /// Append streamed reply text. `new_block` separates text blocks with a
    /// blank line, the same join the durable timeline uses.
    pub fn text_delta(&self, thread: &ThreadId, run_id: &str, delta: &str, new_block: bool) {
        let (done, bodies) = {
            let Ok(mut live) = self.live.lock() else {
                return;
            };
            let Some(turn) = live.get_mut(thread).filter(|t| t.run_id == run_id) else {
                return;
            };
            // The answer started: unsent thinking shows first.
            let done = turn.settle_tools(ToolState::Completed);
            let thinking = turn.thinking.flush();
            (
                done,
                (turn.segment, thinking, turn.text.push(delta, new_block)),
            )
        };
        self.emit_tools(thread, done);
        self.emit_text(thread, run_id, bodies);
    }

    /// Append streamed thinking, like [`Self::text_delta`].
    pub fn thinking_delta(&self, thread: &ThreadId, run_id: &str, delta: &str, new_block: bool) {
        let (done, body) = {
            let Ok(mut live) = self.live.lock() else {
                return;
            };
            let Some(turn) = live.get_mut(thread).filter(|t| t.run_id == run_id) else {
                return;
            };
            (
                turn.settle_tools(ToolState::Completed),
                (turn.segment, turn.thinking.push(delta, new_block), None),
            )
        };
        self.emit_tools(thread, done);
        self.emit_text(thread, run_id, body);
    }

    fn emit_text(&self, thread: &ThreadId, run_id: &str, (segment, thinking, text): Unsent) {
        if let Some(body) = thinking {
            self.emit(LiveEvent::Thinking {
                thread: thread.clone(),
                run_id: run_id.to_string(),
                segment,
                body,
            });
        }
        if let Some(body) = text {
            self.emit(LiveEvent::Text {
                thread: thread.clone(),
                run_id: run_id.to_string(),
                segment,
                body,
            });
        }
    }

    fn emit_tools(&self, thread: &ThreadId, tools: Vec<ToolView>) {
        for tool in tools {
            self.emit(LiveEvent::Tool {
                thread: thread.clone(),
                tool,
            });
        }
    }

    /// An allowed call starts running: its row shows under the segment
    /// that led to it.
    fn tool_started(
        &self,
        thread: &ThreadId,
        run_id: &str,
        tool_name: &str,
        input: &Value,
        call_id: Option<String>,
    ) {
        let tool = ToolView {
            invocation: uuid::Uuid::new_v4().to_string(),
            run_id: run_id.to_string(),
            tool_name: tool_name.to_string(),
            subject: gate_body(tool_name, input)
                .map(|s| truncate_chars(&s, TOOL_SUBJECT_MAX_CHARS)),
            state: ToolState::Running,
        };
        let registered = self.live.lock().ok().is_some_and(|mut live| {
            live.get_mut(thread)
                .filter(|t| t.run_id == run_id)
                .map(|t| t.tools.push((call_id, tool.clone())))
                .is_some()
        });
        if registered {
            self.emit_tools(thread, vec![tool]);
        }
    }

    /// The CLI reported the result of call `call_id`.
    pub fn tool_finished(&self, thread: &ThreadId, run_id: &str, call_id: &str, failed: bool) {
        let done = self.live.lock().ok().and_then(|mut live| {
            let turn = live.get_mut(thread).filter(|t| t.run_id == run_id)?;
            let (_, tool) = turn
                .tools
                .iter_mut()
                .find(|(id, _)| id.as_deref() == Some(call_id))?;
            tool.state = if failed {
                ToolState::Failed
            } else {
                ToolState::Completed
            };
            Some(tool.clone())
        });
        self.emit_tools(thread, done.into_iter().collect());
    }

    /// The turn is over: deny whatever still waits and forget its state.
    pub fn turn_ended(&self, thread: &ThreadId, run_id: &str, status: &'static str) {
        let mut turn = self.live.lock().ok().and_then(|mut live| {
            if live.get(thread).is_some_and(|t| t.run_id == run_id) {
                live.remove(thread)
            } else {
                None
            }
        });
        let done = turn
            .as_mut()
            .map(|t| t.settle_tools(ToolState::Completed))
            .unwrap_or_default();
        let unsent = turn.as_mut().map(LiveTurn::flush).unwrap_or_default();
        self.emit_text(thread, run_id, unsent);
        self.emit_tools(thread, done);
        for gate in turn.map(|t| t.gates).unwrap_or_default() {
            let _ = gate.answer.send((
                ToolDecision::Deny("The turn ended.".into()),
                CliApprovalDecision::TurnEnded,
            )); // silent-ok: the waiter already gave up
        }
        self.emit(LiveEvent::Ended {
            thread: thread.clone(),
            run_id: run_id.to_string(),
            status,
        });
    }

    // ---- approvals ------------------------------------------------------

    /// Hold one tool call until its owner answers. Denies on timeout, on a
    /// thread with no live turn, and when the turn ends first.
    pub async fn request_approval(
        &self,
        thread: &ThreadId,
        run_id: &str,
        tool_name: &str,
        input: &Value,
    ) -> ToolDecision {
        self.request_approval_with_reason(thread, run_id, tool_name, input, None)
            .await
    }

    /// [`Self::request_approval`] with the engine's own justification, if it
    /// gave one; without it the gate carries what the model said before.
    pub async fn request_approval_with_reason(
        &self,
        thread: &ThreadId,
        run_id: &str,
        tool_name: &str,
        input: &Value,
        reason: Option<String>,
    ) -> ToolDecision {
        let claim = self.claim_call(thread, run_id);
        self.decide_call(thread, run_id, tool_name, input, reason, claim)
            .await
    }

    /// Match a tool call to its turn where the driver reads the request,
    /// before anything awaits: later text or a parallel call cannot change
    /// which words explain it, and a call from an ended run is caught.
    pub fn claim_call(&self, thread: &ThreadId, run_id: &str) -> CallClaim {
        let (claim, unsent) = {
            let Ok(mut live) = self.live.lock() else {
                return CallClaim::default(); // silent-ok: a poisoned map only follows a panic elsewhere; the call is denied
            };
            match live.get_mut(thread).filter(|t| t.run_id == run_id) {
                Some(turn) => {
                    let said = turn.said_reason();
                    let claim = CallClaim {
                        live: true,
                        said,
                        call_id: None,
                    };
                    (claim, Some(turn.cut_segment()))
                }
                None => return CallClaim::default(),
            }
        };
        // What came before the call shows first, closed as its segment.
        if let Some(unsent) = unsent {
            self.emit_text(thread, run_id, unsent);
        }
        claim
    }

    fn is_live(&self, thread: &ThreadId, run_id: &str) -> bool {
        self.live
            .lock()
            .ok()
            .is_some_and(|live| live.get(thread).is_some_and(|t| t.run_id == run_id))
    }

    /// Decide a call claimed with [`Self::claim_call`]: deny it when its turn
    /// is gone, allow it on a "Full access" profile, otherwise hold it as a
    /// gate until its owner answers.
    pub async fn decide_call(
        &self,
        thread: &ThreadId,
        run_id: &str,
        tool_name: &str,
        input: &Value,
        reason: Option<String>,
        claim: CallClaim,
    ) -> ToolDecision {
        if !claim.live {
            return ToolDecision::Deny("No live turn to approve this call.".into());
        }
        let call_id = claim.call_id.clone();
        if self.profile_full_access(thread).await {
            // The profile lookup awaited: the turn may have ended meanwhile.
            return if self.is_live(thread, run_id) {
                self.tool_started(thread, run_id, tool_name, input, call_id);
                ToolDecision::Allow
            } else {
                ToolDecision::Deny("The turn ended.".into())
            };
        }
        let gate = GateView {
            gate_ref: format!("gate:cli-{}", uuid::Uuid::new_v4().simple()),
            run_id: run_id.to_string(),
            tool_name: tool_name.to_string(),
            headline: format!("Allow {tool_name}?"),
            body: gate_body(tool_name, input),
            reason: reason.or(claim.said),
        };
        if let Some(ledger) = self.ledger.get() {
            let record = CliApprovalRecord {
                gate_ref: gate.gate_ref.clone(),
                thread_id: thread.clone(),
                run_id: run_id.to_string(),
                tool_name: tool_name.to_string(),
                body: gate.body.clone(),
                requested_at: now_ms(),
                decision: None,
                reason: None,
                decided_at: None,
            };
            if let Err(error) = ledger.cli_approval_opened(&record).await {
                tracing::debug!(%error, "cli bridge: approval not recorded; denied");
                return ToolDecision::Deny("This call could not be recorded.".into());
            }
        }
        let (tx, rx) = oneshot::channel();
        // `Some(unsent bodies)` once registered: what came before the call shows first.
        let registered = self.live.lock().ok().and_then(|mut live| {
            let turn = live.get_mut(thread).filter(|t| t.run_id == run_id)?;
            turn.gates.push(PendingGate {
                view: gate.clone(),
                answer: tx,
            });
            Some(turn.flush())
        });
        let Some(unsent) = registered else {
            self.record_close(&gate.gate_ref, CliApprovalDecision::TurnEnded, None)
                .await;
            return ToolDecision::Deny("No live turn to approve this call.".into());
        };
        self.emit_text(thread, run_id, unsent);
        self.emit(LiveEvent::GateOpened {
            thread: thread.clone(),
            gate: gate.clone(),
        });
        let (decision, recorded) = match tokio::time::timeout(self.approval_timeout, rx).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => (
                ToolDecision::Deny("The approval was dropped.".into()),
                CliApprovalDecision::TurnEnded,
            ),
            Err(_) => (
                ToolDecision::Deny("Nobody approved this call in time.".into()),
                CliApprovalDecision::TimedOut,
            ),
        };
        let reason = match &decision {
            ToolDecision::Deny(reason) => Some(reason.as_str()),
            ToolDecision::Allow => None,
        };
        self.record_close(&gate.gate_ref, recorded, reason).await;
        // A timeout leaves the gate registered; close it either way.
        if let Ok(mut live) = self.live.lock()
            && let Some(turn) = live.get_mut(thread)
        {
            turn.gates.retain(|g| g.view.gate_ref != gate.gate_ref);
        }
        self.emit(LiveEvent::GateClosed {
            thread: thread.clone(),
            run_id: run_id.to_string(),
            gate_ref: gate.gate_ref,
        });
        if decision == ToolDecision::Allow {
            self.tool_started(thread, run_id, tool_name, input, call_id);
        }
        decision
    }

    /// Decline every pending gate on `thread` with `reason`: a steer that
    /// arrives while a call waits answers it with the person's words, so the
    /// turn moves on to the new instruction. Returns how many were declined.
    /// Only `run_id`'s gates: a late steer never answers the next turn's.
    pub fn decline_pending(&self, thread: &ThreadId, run_id: &str, reason: &str) -> usize {
        let reason: String = reason.chars().take(GATE_BODY_MAX_CHARS).collect();
        let pending = self
            .live
            .lock()
            .ok()
            .and_then(|mut live| {
                live.get_mut(thread)
                    .filter(|t| t.run_id == run_id)
                    .map(|t| std::mem::take(&mut t.gates))
            })
            .unwrap_or_default(); // silent-ok: a poisoned map only follows a panic elsewhere
        let declined = pending.len();
        for gate in pending {
            let _ = gate.answer.send((
                ToolDecision::Deny(reason.clone()),
                CliApprovalDecision::Declined,
            )); // silent-ok: the waiter already gave up
        }
        declined
    }

    /// Answer a pending gate on `thread`. `false` when there is none.
    pub fn resolve_gate(&self, thread: &ThreadId, gate_ref: &str, approved: bool) -> bool {
        let pending = self.live.lock().ok().and_then(|mut live| {
            let turn = live.get_mut(thread)?;
            let at = turn
                .gates
                .iter()
                .position(|g| g.view.gate_ref == gate_ref)?;
            Some(turn.gates.remove(at))
        });
        let Some(pending) = pending else {
            return false;
        };
        let decision = if approved {
            (ToolDecision::Allow, CliApprovalDecision::Allowed)
        } else {
            (
                ToolDecision::Deny("The user declined this call.".into()),
                CliApprovalDecision::Declined,
            )
        };
        pending.answer.send(decision).is_ok()
    }

    async fn record_close(
        &self,
        gate_ref: &str,
        decision: CliApprovalDecision,
        reason: Option<&str>,
    ) {
        if let Some(ledger) = self.ledger.get()
            && let Err(error) = ledger
                .cli_approval_closed(gate_ref, decision, reason, now_ms())
                .await
        {
            // The open row stays; the next engine open closes it as interrupted.
            tracing::debug!(%error, "cli bridge: approval decision not recorded");
        }
    }
}

/// What the approval shows of the call: the command or path for the common
/// tools, else the bounded JSON input.
fn gate_body(tool_name: &str, input: &Value) -> Option<String> {
    let field = match tool_name {
        "Bash" => input.get("command"),
        "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" => input
            .get("file_path")
            .or_else(|| input.get("notebook_path")),
        "WebFetch" => input.get("url"),
        _ => None,
    };
    let text = match field.or(Some(input)).and_then(Value::as_str) {
        Some(s) => s.to_string(),
        None if input.as_object().is_some_and(|o| o.is_empty()) || input.is_null() => {
            return None;
        }
        None => serde_json::to_string_pretty(input).ok()?,
    };
    // Commands and inputs can carry tokens; the card shows them redacted.
    let text = ironclaw_safety::sanitize_display_text(&text);
    Some(truncate_chars(&text, GATE_BODY_MAX_CHARS))
}

fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_string(),
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `--mcp-config` for one CLI session: the loopback MCP route with its token.
pub fn mcp_config_json(url: &str, token: &str) -> String {
    json!({
        "mcpServers": {
            MCP_SERVER_NAME: {
                "type": "http",
                "url": url,
                "headers": {"Authorization": format!("Bearer {token}")}
            }
        }
    })
    .to_string()
}

/// One request on the loopback MCP route: loopback peer, a live session
/// token (`Authorization` header value), one JSON-RPC object. `Err` is the
/// HTTP status to refuse with (403, 401, 400); `Ok(None)` answers 202.
pub async fn serve_mcp(
    bridge: &CliBridge,
    engine: &Engine,
    loopback: bool,
    authorization: Option<&str>,
    body: &[u8],
) -> Result<Option<Value>, u16> {
    if !loopback {
        return Err(403);
    }
    let grant = authorization
        .and_then(|v| v.strip_prefix("Bearer "))
        .and_then(|token| bridge.resolve_token(token.trim()))
        .ok_or(401u16)?;
    match serde_json::from_slice::<Value>(body) {
        Ok(message @ Value::Object(_)) => Ok(handle_mcp(engine, &grant, &message).await),
        _ => Err(400),
    }
}

/// One JSON-RPC message on the loopback MCP route, as the session's thread.
/// `None` for notifications (the route answers 202 with no body).
pub async fn handle_mcp(engine: &Engine, grant: &SessionGrant, message: &Value) -> Option<Value> {
    let id = message.get("id").cloned();
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let Some(id) = id else {
        return None; // notifications/initialized and friends
    };
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": message["params"]["protocolVersion"]
                .as_str()
                .unwrap_or("2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": MCP_SERVER_NAME, "version": env!("CARGO_PKG_VERSION")},
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({
            "tools": TOOLS.iter().map(|t| json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": (t.schema)(),
            })).collect::<Vec<_>>()
        })),
        "tools/call" => call_tool(engine, grant, &message["params"]).await,
        _ => Err((-32601, "method not found")),
    };
    Some(match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, text)) => json!({
            "jsonrpc": "2.0", "id": id, "error": {"code": code, "message": text}
        }),
    })
}

async fn call_tool(
    engine: &Engine,
    grant: &SessionGrant,
    params: &Value,
) -> Result<Value, (i64, &'static str)> {
    let name = params["name"].as_str().unwrap_or("");
    if !TOOLS.iter().any(|t| t.name == name) {
        return Err((-32602, "unknown tool"));
    }
    // The call runs as the CLI thread: its owner's scope and current route.
    let thread = match engine.store().thread(&grant.thread_id).await {
        Ok(Some(thread)) => thread,
        Ok(None) => return Err((-32602, "thread not found")),
        Err(error) => {
            tracing::debug!(%error, "cli bridge: thread lookup failed");
            return Err((-32603, "thread unavailable"));
        }
    };
    let ctx = CallContext {
        thread_id: thread.thread_id.clone(),
        scope: thread.scope.clone(),
        provider: thread.provider.clone(),
        model: thread.model.clone(),
    };
    let arguments = match params.get("arguments") {
        Some(Value::Null) | None => json!({}),
        Some(args) => args.clone(),
    };
    let output = engine.call_tool(name, &ctx, &arguments).await;
    let failed = output.get("ok") == Some(&Value::Bool(false));
    Ok(json!({
        "content": [{"type": "text", "text": output.to_string()}],
        "isError": failed,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tid() -> ThreadId {
        ThreadId::new("t1").unwrap()
    }

    #[test]
    fn tokens_reject_wrong_secret_revoked_and_expired() {
        let bridge = CliBridge::new(DEFAULT_APPROVAL_TIMEOUT);
        let grant = SessionGrant {
            thread_id: tid(),
            run_id: "r".into(),
        };
        let token = bridge.mint_token(grant.clone(), SESSION_TOKEN_TTL);
        assert_eq!(bridge.resolve_token(&token), Some(grant.clone()));
        let (id, _) = token.split_once('.').unwrap();
        assert_eq!(bridge.resolve_token(&format!("{id}.nope")), None);
        assert_eq!(bridge.resolve_token("garbage"), None);
        bridge.revoke_token(&token);
        assert_eq!(bridge.resolve_token(&token), None);

        let short = bridge.mint_token(grant, Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(bridge.resolve_token(&short), None);
    }

    /// Text before a call and text after it are separate segments, the
    /// reason never borrows thinking, and an allowed call shows as a tool
    /// row that finishes when its result comes back.
    #[tokio::test]
    async fn a_call_cuts_the_segment_and_runs_as_a_tool_row() {
        let bridge = CliBridge::new(DEFAULT_APPROVAL_TIMEOUT);
        let thread = tid();
        bridge.turn_started(&thread, "r1");
        bridge.thinking_delta(&thread, "r1", "Plan the listing first.", true);
        bridge.text_delta(&thread, "r1", "Let me look.", true);
        let claim = bridge
            .claim_call(&thread, "r1")
            .with_call_id(Some("toolu_1".into()));
        let waiter = {
            let bridge = Arc::clone(&bridge);
            let thread = thread.clone();
            tokio::spawn(async move {
                bridge
                    .decide_call(
                        &thread,
                        "r1",
                        "Bash",
                        &json!({"command": "ls"}),
                        None,
                        claim,
                    )
                    .await
            })
        };
        let gate = loop {
            if let Some(g) = bridge
                .snapshot(&thread)
                .and_then(|s| s.gates.first().cloned())
            {
                break g;
            }
            tokio::task::yield_now().await;
        };
        assert_eq!(gate.reason.as_deref(), Some("Let me look."));
        assert!(bridge.resolve_gate(&thread, &gate.gate_ref, true));
        assert_eq!(waiter.await.unwrap(), ToolDecision::Allow);

        let snap = bridge.snapshot(&thread).unwrap();
        assert_eq!(snap.tools.len(), 1);
        assert_eq!(snap.tools[0].state, ToolState::Running);
        assert_eq!(snap.tools[0].subject.as_deref(), Some("ls"));
        bridge.tool_finished(&thread, "r1", "toolu_1", false);
        bridge.text_delta(&thread, "r1", "Found it.", true);
        let snap = bridge.snapshot(&thread).unwrap();
        assert_eq!(snap.tools[0].state, ToolState::Completed);
        let texts: Vec<(u32, &str)> = snap
            .segments
            .iter()
            .map(|s| (s.segment, s.text.as_str()))
            .collect();
        assert_eq!(texts, vec![(0, "Let me look."), (1, "Found it.")]);

        assert_eq!(
            bridge.claim_call(&thread, "r1").said.as_deref(),
            Some("Found it.")
        );
        // Only thinking since: the call borrows nothing.
        bridge.thinking_delta(&thread, "r1", "More thoughts.", true);
        assert!(bridge.claim_call(&thread, "r1").said.is_none());
    }

    #[tokio::test]
    async fn approval_waits_for_the_owner_and_turn_end_denies() {
        let bridge = CliBridge::new(DEFAULT_APPROVAL_TIMEOUT);
        let thread = tid();
        bridge.turn_started(&thread, "r1");
        let waiter = {
            let bridge = Arc::clone(&bridge);
            let thread = thread.clone();
            tokio::spawn(async move {
                bridge
                    .request_approval(&thread, "r1", "Bash", &json!({"command": "ls"}))
                    .await
            })
        };
        let gate = loop {
            if let Some(g) = bridge
                .snapshot(&thread)
                .and_then(|s| s.gates.first().cloned())
            {
                break g;
            }
            tokio::task::yield_now().await;
        };
        assert_eq!(gate.body.as_deref(), Some("ls"));
        assert!(!bridge.resolve_gate(&ThreadId::new("other").unwrap(), &gate.gate_ref, true));
        assert!(bridge.resolve_gate(&thread, &gate.gate_ref, true));
        assert_eq!(waiter.await.unwrap(), ToolDecision::Allow);

        let pending = {
            let bridge = Arc::clone(&bridge);
            let thread = thread.clone();
            tokio::spawn(async move {
                bridge
                    .request_approval(&thread, "r1", "Read", &json!({}))
                    .await
            })
        };
        while bridge.snapshot(&thread).is_some_and(|s| s.gates.is_empty()) {
            tokio::task::yield_now().await;
        }
        bridge.turn_ended(&thread, "r1", "cancelled");
        assert!(matches!(pending.await.unwrap(), ToolDecision::Deny(_)));
        // No live turn: denied without asking anyone.
        assert!(matches!(
            bridge
                .request_approval(&thread, "r1", "Read", &json!({}))
                .await,
            ToolDecision::Deny(_)
        ));
    }

    /// Every asked call is recorded with its answer, and a call still open
    /// when the engine reopens (a restart) is closed as interrupted.
    #[tokio::test]
    async fn approvals_are_recorded_and_a_restart_closes_open_ones() {
        use ironclaw_orchestrator::cli_approvals::CliApprovalDecision as D;

        let dir = tempfile::tempdir().unwrap();
        let open = || async {
            let db = Arc::new(
                libsql::Builder::new_local(dir.path().join("o.db"))
                    .build()
                    .await
                    .unwrap(),
            );
            let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap());
            Engine::open_libsql(runtime, "ironclaw").await.unwrap()
        };
        let engine = open().await;
        let bridge = CliBridge::new(DEFAULT_APPROVAL_TIMEOUT);
        bridge.record_to(engine.store().clone());
        let thread = tid();
        bridge.turn_started(&thread, "r1");

        let ask = |tool: &'static str| {
            let (bridge, thread) = (Arc::clone(&bridge), thread.clone());
            tokio::spawn(async move {
                bridge
                    .request_approval(&thread, "r1", tool, &json!({"command": "ls"}))
                    .await
            })
        };
        let gate_for = |n: usize| {
            let (bridge, thread) = (Arc::clone(&bridge), thread.clone());
            async move {
                loop {
                    if let Some(g) = bridge
                        .snapshot(&thread)
                        .and_then(|s| s.gates.get(n).cloned())
                    {
                        break g;
                    }
                    tokio::task::yield_now().await;
                }
            }
        };
        let allowed = ask("Bash");
        let gate = gate_for(0).await;
        assert!(bridge.resolve_gate(&thread, &gate.gate_ref, true));
        assert_eq!(allowed.await.unwrap(), ToolDecision::Allow);
        let declined = ask("Read");
        let gate = gate_for(0).await;
        assert!(bridge.resolve_gate(&thread, &gate.gate_ref, false));
        assert!(matches!(declined.await.unwrap(), ToolDecision::Deny(_)));
        // Left waiting when the backend "stops".
        let _pending = ask("Write");
        gate_for(0).await;

        let rows = engine.store().cli_approvals(&thread).await.unwrap();
        let seen: Vec<_> = rows
            .iter()
            .map(|r| (r.tool_name.as_str(), r.decision))
            .collect();
        assert_eq!(
            seen,
            [
                ("Bash", Some(D::Allowed)),
                ("Read", Some(D::Declined)),
                ("Write", None)
            ]
        );
        assert_eq!(rows[0].body.as_deref(), Some("ls"));

        let reopened = open().await;
        let rows = reopened.store().cli_approvals(&thread).await.unwrap();
        assert_eq!(rows[2].decision, Some(D::Interrupted));
        assert_eq!(
            rows[0].decision,
            Some(D::Allowed),
            "closed rows keep their answer"
        );
    }
}
