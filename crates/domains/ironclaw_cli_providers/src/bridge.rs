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

use ironclaw_host_api::agent_plan::{AgentPlan, QuestionForm};
use ironclaw_host_api::ids::ThreadId;
use ironclaw_host_api::turn::DenialFeedback;
use ironclaw_orchestrator::cli_approvals::{CliApprovalDecision, CliApprovalRecord};
use ironclaw_orchestrator::engine::{CallContext, Engine};
use ironclaw_orchestrator::model::{Author, now_ms};
use ironclaw_orchestrator::store::Store;
use ironclaw_orchestrator::tools::TOOLS;
use serde_json::{Value, json};
use tokio::sync::{broadcast, oneshot};

mod detail;

/// What the agent reads when it reaches for its own subagent tool.
const BUILTIN_SUBAGENT_DENIAL: &str = "Built-in subagents, agent teams and their tools are switched off \
     here. Start a subagent with the T3 orchestrator instead: t3_delegate (mcp__ironclaw__t3_delegate on Claude Code), which opens a thread \
     the user can see and steer.";

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
pub(crate) const GATE_BODY_MAX_CHARS: usize = 2_000;
/// Bound on a tool row's subject (the wire allows 2 KiB).
const TOOL_SUBJECT_MAX_CHARS: usize = 400;
const EVENT_BUFFER: usize = 256;

/// The answer a CLI gets for one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolDecision {
    Allow,
    /// Not run, for the host's reason (no turn, timeout, plain decline, …).
    Deny(String),
    /// Not run: the person rejected it and said what to do instead. Still a
    /// denial in every respect; the feedback only explains it to the agent.
    Rejected(DenialFeedback),
}

impl ToolDecision {
    /// What the agent reads (and the ledger records) for a denial; `None`
    /// for an approval.
    pub fn denial_message(&self) -> Option<String> {
        match self {
            Self::Allow => None,
            Self::Deny(reason) => Some(reason.clone()),
            Self::Rejected(feedback) => Some(rejection_message(feedback)),
        }
    }
}

/// The text an agent gets for a call the person rejected with feedback.
pub fn rejection_message(feedback: &DenialFeedback) -> String {
    format!("The user rejected this call: {}", feedback.as_str())
}

/// How the thread's owner answered one pending tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateAnswer {
    Approve,
    /// Decline, optionally saying what to do instead ("no, do X instead").
    Decline {
        feedback: Option<DenialFeedback>,
    },
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

mod questions;

/// Read back by whoever shows a thread's past questions.
pub use questions::{QuestionOutcome, QuestionRecordBody, question_record};

/// A structured question the turn waits on, as the person sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionView {
    pub gate_ref: String,
    pub run_id: String,
    pub form: QuestionForm,
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
    /// The newest tail of tool row `invocation`'s output (whole, redacted).
    ToolOutput {
        thread: ThreadId,
        run_id: String,
        invocation: String,
        tail: String,
        /// Older output was dropped from the front.
        truncated: bool,
    },
    /// The turn's aggregated unified diff so far (whole, redacted).
    TurnDiff {
        thread: ThreadId,
        run_id: String,
        diff: String,
        truncated: bool,
    },
    /// A finished code review's findings text (redacted).
    Review {
        thread: ThreadId,
        run_id: String,
        text: String,
    },
    /// The agent replaced its plan. `seq` numbers the thread's updates.
    Plan {
        thread: ThreadId,
        run_id: String,
        seq: u64,
        plan: AgentPlan,
    },
    QuestionOpened {
        thread: ThreadId,
        question: QuestionView,
    },
    /// One question closed (answered, dismissed, or cut short): the same
    /// view it opened with, so a client closes exactly that form.
    QuestionClosed {
        thread: ThreadId,
        question: QuestionView,
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
            | Self::Ended { thread, .. }
            | Self::ToolOutput { thread, .. }
            | Self::TurnDiff { thread, .. }
            | Self::Review { thread, .. }
            | Self::Plan { thread, .. }
            | Self::QuestionOpened { thread, .. }
            | Self::QuestionClosed { thread, .. } => thread,
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
    /// Output tails by tool invocation (redacted).
    pub outputs: Vec<OutputView>,
    /// The turn's diff so far (redacted), and whether it was cut.
    pub diff: Option<(String, bool)>,
    /// A review's findings, once it finished.
    pub review: Option<String>,
    /// The turn's latest plan and its update number.
    pub plan: Option<(u64, AgentPlan)>,
    pub questions: Vec<QuestionView>,
}

/// The output tail of one tool row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputView {
    pub invocation: String,
    pub tail: String,
    pub truncated: bool,
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
    answer: oneshot::Sender<(ToolDecision, CliApprovalDecision, Receipt)>,
}

/// Handed to a driver with a decision or answer from the bridge: the driver
/// reports with [`Receipt::delivered`] whether what it was told reached the
/// agent (its reply written, the agent's RPC answered). Dropped unreported,
/// it reports `false`. Whoever answered the gate on a steer's behalf waits
/// on it, so the steer is sequenced after the reply and never claims words
/// were delivered that were not.
pub struct Receipt {
    tx: Option<oneshot::Sender<bool>>,
    /// Shared with the steer that answered the gate: whoever moves it off
    /// `Waiting` first decides whether the reply still carries the text.
    ticket: Option<Arc<Mutex<Ticket>>>,
}

/// Where a steer and the driver replying for it agree, once, on what the
/// reply carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ticket {
    Waiting,
    /// The driver is handing the reply to its transport as it was given.
    Taken,
    /// The steer stopped waiting: the reply goes in its plain form and the
    /// steer sends the text itself.
    Withdrawn,
}

fn settle_ticket(ticket: &Mutex<Ticket>, to: Ticket) -> Result<(), Ticket> {
    // A poisoned ticket only follows a panic elsewhere; its state is intact.
    let mut state = ticket
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match *state {
        Ticket::Waiting => {
            *state = to;
            Ok(())
        }
        other => Err(other),
    }
}

impl Receipt {
    /// A receipt nobody waits on.
    pub fn none() -> Self {
        Self {
            tx: None,
            ticket: None,
        }
    }

    fn pair() -> (Self, Answered) {
        let (tx, rx) = oneshot::channel();
        let ticket = Arc::new(Mutex::new(Ticket::Waiting));
        (
            Self {
                tx: Some(tx),
                ticket: Some(Arc::clone(&ticket)),
            },
            Answered { rx, ticket },
        )
    }

    /// Call right before handing the reply to the transport. `true`: send it
    /// as given. `false`: the steer that answered this gate gave up waiting
    /// and sends its words itself, so the reply must go in its plain form
    /// ([`PLAIN_DECLINE`], or a dismissed question) or they would arrive
    /// twice.
    pub fn claim(&mut self) -> bool {
        self.ticket
            .as_ref()
            .is_none_or(|t| settle_ticket(t, Ticket::Taken) != Err(Ticket::Withdrawn))
    }

    /// Report whether the reply reached the agent (written to its input;
    /// for an RPC request, also answered).
    pub fn delivered(mut self, ok: bool) {
        self.send(ok);
    }

    fn send(&mut self, ok: bool) {
        if let Some(tx) = self.tx.take()
            && tx.send(ok).is_err()
        {
            tracing::debug!("cli bridge: nobody waits on this receipt any more");
        }
    }
}

/// A gate's reply as the steer that answered it follows it.
#[derive(Debug)]
struct Answered {
    rx: oneshot::Receiver<bool>,
    ticket: Arc<Mutex<Ticket>>,
}

impl Answered {
    /// Wait for the reply. Past `deadline` the steer withdraws it (if the
    /// driver has not taken it yet: it then goes plain, `false`); once
    /// taken, its own end is waited for, which the driver's write bound
    /// limits. Never "maybe": a reply that may still land is waited for.
    async fn reached(mut self, deadline: tokio::time::Instant) -> bool {
        if let Ok(reached) = tokio::time::timeout_at(deadline, &mut self.rx).await {
            return reached.unwrap_or(false); // silent-ok: a dropped receipt means the reply never went out
        }
        match settle_ticket(&self.ticket, Ticket::Withdrawn) {
            Ok(()) => false,
            Err(_) => self.rx.await.unwrap_or(false), // silent-ok: as above
        }
    }
}

impl Drop for Receipt {
    fn drop(&mut self) {
        self.send(false);
    }
}

impl std::fmt::Debug for Receipt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.tx.is_some() {
            "Receipt(awaited)"
        } else {
            "Receipt(none)"
        })
    }
}

/// How long a steer waits for the replies to the gates it answered before
/// it withdraws them (a reply already being written is still waited for).
const GATE_RECEIPT_TIMEOUT: Duration = Duration::from_secs(30);

/// The deny reason of an approval a steer declined, in its plain form.
pub const PLAIN_DECLINE: &str = "The user declined this call and sent a new message instead.";

/// The deny reason an approval carries when a steer declines it and the
/// agent takes the reason as the call's result (Claude's PreToolUse hook).
pub fn steer_denial(text: &str) -> String {
    format!("The user declined this call and said instead: {text}")
}

/// What a steer did at its run's open gates ([`CliBridge::steer_at_gates`]).
#[derive(Debug, Default)]
pub struct GateSteer {
    /// How long replies are waited for before the untaken are withdrawn.
    wait: Duration,
    /// The question the person's text answered (its reply may or may not
    /// hand the text to the agent: only some protocols take free text).
    question: Option<Answered>,
    /// The approval declined with the text as its feedback.
    carrier: Option<Answered>,
    /// Approvals declined without the text.
    declined: Vec<Answered>,
}

impl GateSteer {
    /// Nothing was waiting.
    pub fn is_empty(&self) -> bool {
        self.question.is_none() && self.carrier.is_none() && self.declined.is_empty()
    }

    /// Wait until every answered gate's reply has reached the agent or
    /// definitely failed (withdrawing, past the bound, any the driver has not
    /// taken yet). `true` when the text itself reached the agent (through
    /// the question's reply or the carrying denial): the caller must not
    /// send it again. `false` only when it definitely did not: the caller
    /// steers it, after every decline.
    pub async fn settle(self) -> bool {
        let deadline = tokio::time::Instant::now() + self.wait;
        let mut carried = false;
        if let Some(question) = self.question {
            carried |= question.reached(deadline).await;
        }
        if let Some(carrier) = self.carrier {
            carried |= carrier.reached(deadline).await;
        }
        for declined in self.declined {
            declined.reached(deadline).await;
        }
        carried
    }
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
    /// Output tails by tool invocation.
    outputs: HashMap<String, detail::Detail>,
    diff: detail::Detail,
    review: Option<String>,
    questions: Vec<questions::PendingQuestion>,
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
            outputs: HashMap::new(),
            diff: detail::Detail::default(),
            review: None,
            questions: Vec::new(),
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
    /// How long a steer waits for the replies of the gates it answered
    /// before it withdraws the ones not taken yet (milliseconds).
    gate_receipt_ms: std::sync::atomic::AtomicU64,
    tokens: Mutex<HashMap<String, TokenEntry>>,
    live: Mutex<HashMap<ThreadId, LiveTurn>>,
    events: broadcast::Sender<LiveEvent>,
    /// The loopback MCP route, known once the gateway listener is bound.
    mcp_url: OnceLock<String>,
    /// Where approvals are recorded, set once by the host.
    ledger: OnceLock<Store>,
    /// Each thread's latest plan with its update number, kept past the
    /// turn so a stream that opens later still shows it.
    plans: Mutex<HashMap<ThreadId, (u64, String, AgentPlan)>>,
    /// Host tools beside the orchestrator's on the MCP route.
    pub(crate) tool_providers: Mutex<Vec<Arc<dyn crate::bridge_tools::BridgeToolProvider>>>,
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
            gate_receipt_ms: std::sync::atomic::AtomicU64::new(
                u64::try_from(GATE_RECEIPT_TIMEOUT.as_millis()).unwrap_or(u64::MAX),
            ),
            tokens: Mutex::new(HashMap::new()),
            live: Mutex::new(HashMap::new()),
            events: broadcast::channel(EVENT_BUFFER).0,
            mcp_url: OnceLock::new(),
            ledger: OnceLock::new(),
            plans: Mutex::new(HashMap::new()),
            tool_providers: Mutex::new(Vec::new()),
        })
    }

    /// Bound how long a steer waits for its gate replies before it
    /// withdraws those not taken yet (default 30 s); tests shorten it.
    pub fn set_gate_receipt_timeout(&self, timeout: Duration) {
        self.gate_receipt_ms.store(
            u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    pub fn approval_timeout(&self) -> Duration {
        self.approval_timeout
    }

    /// Record the loopback MCP endpoint (first call wins).
    pub fn set_mcp_url(&self, url: String) {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "first call wins: the gateway binds once per process"
        )]
        let _ = self.mcp_url.set(url);
    }

    /// Record every approval in `store` from now on (first call wins).
    pub fn record_to(&self, store: Store) {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "first call wins: the host wires one store per process"
        )]
        let _ = self.ledger.set(store);
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
        let plan = self.plan_of(thread).map(|(seq, _, plan)| (seq, plan));
        Some(LiveSnapshot {
            run_id: turn.run_id.clone(),
            segments,
            tools: turn.tools.iter().map(|(_, t)| t.clone()).collect(),
            gates: turn.gates.iter().map(|g| g.view.clone()).collect(),
            outputs: turn
                .outputs
                .iter()
                .map(|(invocation, d)| OutputView {
                    invocation: invocation.clone(),
                    tail: ironclaw_safety::sanitize_display_text(&d.body),
                    truncated: d.truncated,
                })
                .collect(),
            diff: (!turn.diff.body.is_empty()).then(|| {
                (
                    ironclaw_safety::sanitize_display_text(&turn.diff.body),
                    turn.diff.truncated,
                )
            }),
            review: turn.review.clone(),
            plan,
            questions: turn.questions.iter().map(|q| q.view.clone()).collect(),
        })
    }

    fn emit(&self, event: LiveEvent) {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "a broadcast send fails only when no live stream is subscribed to this thread, the normal idle case"
        )]
        let _ = self.events.send(event);
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
                // A CLI that reports a call before asking about it (ACP's
                // `tool_call`) already drew its row: never draw it twice.
                .filter(|t| call_id.is_none() || !t.tools.iter().any(|(id, _)| *id == call_id))
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
        if let Some(tool) = &done {
            self.flush_output(thread, run_id, &tool.invocation);
        }
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
        let details = turn
            .as_mut()
            .map(|t| t.take_details(thread, true))
            .unwrap_or_default();
        self.emit_text(thread, run_id, unsent);
        for event in details {
            self.emit(event);
        }
        self.emit_tools(thread, done);
        let (gates, waiting) = turn.map(|t| (t.gates, t.questions)).unwrap_or_default();
        for gate in gates {
            if gate
                .answer
                .send((
                    ToolDecision::Deny("The turn ended.".into()),
                    CliApprovalDecision::TurnEnded,
                    Receipt::none(),
                ))
                .is_err()
            {
                tracing::debug!("turn ended: the approval waiter was already gone");
            }
        }
        for question in waiting {
            if question
                .answer
                .send((QuestionOutcome::TurnEnded, Receipt::none()))
                .is_err()
            {
                tracing::debug!("turn ended: the question waiter was already gone");
            }
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
    /// is gone or it would start a built-in subagent, allow it on a "Full
    /// access" profile, otherwise hold it as a gate until its owner answers.
    pub async fn decide_call(
        &self,
        thread: &ThreadId,
        run_id: &str,
        tool_name: &str,
        input: &Value,
        reason: Option<String>,
        claim: CallClaim,
    ) -> ToolDecision {
        self.decide_call_receipted(thread, run_id, tool_name, input, reason, claim)
            .await
            .0
    }

    /// [`Self::decide_call`], plus the [`Receipt`] the driver reports on once
    /// its reply reached the agent (a steer that declined the gate waits on
    /// it before it is sent).
    pub async fn decide_call_receipted(
        &self,
        thread: &ThreadId,
        run_id: &str,
        tool_name: &str,
        input: &Value,
        reason: Option<String>,
        claim: CallClaim,
    ) -> (ToolDecision, Receipt) {
        if !claim.live {
            return (
                ToolDecision::Deny("No live turn to approve this call.".into()),
                Receipt::none(),
            );
        }
        // Backstop for the launch flags: a hidden subagent never runs, even
        // on a "Full access" profile.
        if crate::is_builtin_subagent_tool(tool_name) {
            return (
                ToolDecision::Deny(BUILTIN_SUBAGENT_DENIAL.into()),
                Receipt::none(),
            );
        }
        let call_id = claim.call_id.clone();
        if self.profile_full_access(thread).await {
            // The profile lookup awaited: the turn may have ended meanwhile.
            let decision = if self.is_live(thread, run_id) {
                self.tool_started(thread, run_id, tool_name, input, call_id);
                ToolDecision::Allow
            } else {
                ToolDecision::Deny("The turn ended.".into())
            };
            return (decision, Receipt::none());
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
                return (
                    ToolDecision::Deny("This call could not be recorded.".into()),
                    Receipt::none(),
                );
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
            return (
                ToolDecision::Deny("No live turn to approve this call.".into()),
                Receipt::none(),
            );
        };
        self.emit_text(thread, run_id, unsent);
        self.emit(LiveEvent::GateOpened {
            thread: thread.clone(),
            gate: gate.clone(),
        });
        let (decision, recorded, receipt) =
            match tokio::time::timeout(self.approval_timeout, rx).await {
                Ok(Ok(answer)) => answer,
                Ok(Err(_)) => (
                    ToolDecision::Deny("The approval was dropped.".into()),
                    CliApprovalDecision::TurnEnded,
                    Receipt::none(),
                ),
                Err(_) => (
                    ToolDecision::Deny("Nobody approved this call in time.".into()),
                    CliApprovalDecision::TimedOut,
                    Receipt::none(),
                ),
            };
        let reason = decision.denial_message();
        self.record_close(&gate.gate_ref, recorded, reason.as_deref())
            .await;
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
        (decision, receipt)
    }

    /// A steer arrived while `run_id` waits on the person (T3 Code: a
    /// message sent while approvals are pending redirects the agent):
    /// - the oldest pending question is answered by the text, as typed;
    /// - every pending approval is declined. With `feedback` (the agent
    ///   reads a denial's reason, as Claude's hook does) and no question
    ///   answered, the oldest one carries the whole text as its reason
    ///   ([`steer_denial`]) and the rest are declined plainly, so the agent
    ///   reads the words once.
    ///
    /// A gate whose waiter is already gone (it timed out or its turn ended
    /// meanwhile) is skipped: it answers nothing. Wait on the result with
    /// [`GateSteer::settle`] before steering. Only `run_id`'s gates: a late
    /// steer never answers the next turn's.
    pub fn steer_at_gates(
        &self,
        thread: &ThreadId,
        run_id: &str,
        text: &str,
        feedback: bool,
    ) -> GateSteer {
        let (gates, mut questions) = self
            .live
            .lock()
            .ok()
            .and_then(|mut live| {
                live.get_mut(thread)
                    .filter(|t| t.run_id == run_id)
                    .map(|t| {
                        (
                            std::mem::take(&mut t.gates),
                            std::mem::take(&mut t.questions),
                        )
                    })
            })
            .unwrap_or_default(); // silent-ok: a poisoned map only follows a panic elsewhere
        let mut steer = GateSteer {
            wait: Duration::from_millis(
                self.gate_receipt_ms
                    .load(std::sync::atomic::Ordering::Relaxed),
            ),
            ..GateSteer::default()
        };
        // Oldest first; one that cannot take the answer hands it on.
        let mut rest = Vec::new();
        for question in questions.drain(..) {
            if steer.question.is_some() {
                rest.push(question);
                continue;
            }
            let (receipt, rx) = Receipt::pair();
            match question
                .answer
                .send((QuestionOutcome::Replied(text.to_string()), receipt))
            {
                Ok(()) => steer.question = Some(rx),
                Err(_) => tracing::debug!("steer: a question waiter was already gone"),
            }
        }
        // Questions the text did not answer keep waiting.
        if !rest.is_empty()
            && let Ok(mut live) = self.live.lock()
            && let Some(turn) = live.get_mut(thread).filter(|t| t.run_id == run_id)
        {
            turn.questions.splice(0..0, rest);
        }
        let carry = feedback && steer.question.is_none();
        for gate in gates {
            let (receipt, rx) = Receipt::pair();
            let reason = if carry && steer.carrier.is_none() {
                steer_denial(text)
            } else {
                PLAIN_DECLINE.to_string()
            };
            let carries = carry && steer.carrier.is_none();
            match gate.answer.send((
                ToolDecision::Deny(reason),
                CliApprovalDecision::Declined,
                receipt,
            )) {
                Ok(()) if carries => steer.carrier = Some(rx),
                Ok(()) => steer.declined.push(rx),
                Err(_) => tracing::debug!("steer: an approval waiter was already gone"),
            }
        }
        steer
    }

    /// Keep rejection feedback the running turn could not take — the decline
    /// ended it, or it refused the steer — as the thread's next turn input:
    /// it waits in the durable queue (visible in the thread's queue) and the
    /// engine runs it once the thread is idle. `false` when nothing could
    /// keep it (no store), so the caller can say so.
    pub async fn queue_undelivered_feedback(
        &self,
        thread: &ThreadId,
        feedback: &DenialFeedback,
    ) -> bool {
        let Some(store) = self.ledger.get() else {
            return false;
        };
        match store
            .queue_add(thread, &rejection_message(feedback), Author::User)
            .await
        {
            Ok(_) => true,
            Err(error) => {
                tracing::debug!(%error, "cli bridge: rejection feedback not queued");
                false
            }
        }
    }

    /// Answer a pending gate on `thread`. `false` when there is none.
    pub fn resolve_gate(&self, thread: &ThreadId, gate_ref: &str, answer: GateAnswer) -> bool {
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
        let decision = match answer {
            GateAnswer::Approve => (
                ToolDecision::Allow,
                CliApprovalDecision::Allowed,
                Receipt::none(),
            ),
            GateAnswer::Decline { feedback: None } => (
                ToolDecision::Deny("The user declined this call.".into()),
                CliApprovalDecision::Declined,
                Receipt::none(),
            ),
            GateAnswer::Decline {
                feedback: Some(feedback),
            } => (
                ToolDecision::Rejected(feedback),
                CliApprovalDecision::Declined,
                Receipt::none(),
            ),
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
        "Bash" | crate::hook_approval::WORKTREE_SETUP_TOOL => input.get("command"),
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
        Ok(message @ Value::Object(_)) => {
            Ok(crate::bridge_tools::handle_with_tools(bridge, engine, &grant, &message).await)
        }
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
        run_id: Some(grant.run_id.clone()),
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

    /// Review (Sol, 4): a gate whose waiter is gone (it timed out, or its
    /// task ended) answers nothing: a steer must not count it as delivered.
    #[tokio::test]
    async fn a_steer_skips_a_gate_whose_waiter_is_gone() {
        let bridge = CliBridge::new(Duration::from_secs(30));
        let thread = ThreadId::new("t-stale").unwrap();
        bridge.turn_started(&thread, "r1");
        let mut events = bridge.subscribe();
        let asker = Arc::clone(&bridge);
        let (t, waiting_thread) = (thread.clone(), thread.clone());
        let waiting = tokio::spawn(async move {
            asker
                .request_approval(&waiting_thread, "r1", "Bash", &json!({"command": "ls"}))
                .await
        });
        loop {
            if let LiveEvent::GateOpened { .. } = events.recv().await.unwrap() {
                break;
            }
        }
        // The waiter goes away while its gate is still listed.
        waiting.abort();
        assert!(waiting.await.unwrap_err().is_cancelled());
        let steer = bridge.steer_at_gates(&t, "r1", "do this instead", true);
        assert!(steer.is_empty(), "a stale gate was counted: {steer:?}");
        assert!(!steer.settle().await);
    }

    /// Review (Sol, 6): a steer is sequenced after the decline's reply, not
    /// after a yield: `settle` returns only once the driver reported its
    /// reply on the agent's wire (here, 300 ms later, after a durable write).
    #[tokio::test]
    async fn a_steer_waits_for_the_decline_to_reach_the_agent() {
        let bridge = CliBridge::new(Duration::from_secs(30));
        let thread = ThreadId::new("t-seq").unwrap();
        bridge.turn_started(&thread, "r1");
        let mut events = bridge.subscribe();
        let (asker, t) = (Arc::clone(&bridge), thread.clone());
        let waiter = tokio::spawn(async move {
            let claim = asker.claim_call(&t, "r1");
            let (decision, receipt) = asker
                .decide_call_receipted(&t, "r1", "Bash", &json!({"command": "rm"}), None, claim)
                .await;
            // The driver's reply takes a while to reach the agent.
            tokio::time::sleep(Duration::from_millis(300)).await;
            receipt.delivered(true);
            decision
        });
        loop {
            if let LiveEvent::GateOpened { .. } = events.recv().await.unwrap() {
                break;
            }
        }
        let steer = bridge.steer_at_gates(&thread, "r1", "stop", false);
        let started = Instant::now();
        assert!(
            !steer.settle().await,
            "a plain decline does not carry the text"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(250),
            "settled before the reply"
        );
        assert!(matches!(waiter.await.unwrap(), ToolDecision::Deny(_)));
    }

    /// Review (Sol, 4): the words go whole through one denial; the rest are
    /// declined without them, and every receipt reports.
    #[tokio::test]
    async fn a_steer_carries_its_text_through_one_gate_only() {
        let bridge = CliBridge::new(Duration::from_secs(30));
        let thread = ThreadId::new("t-two").unwrap();
        bridge.turn_started(&thread, "r1");
        let mut events = bridge.subscribe();
        let mut waiters = Vec::new();
        for command in ["a", "b"] {
            let (asker, t) = (Arc::clone(&bridge), thread.clone());
            waiters.push(tokio::spawn(async move {
                let claim = asker.claim_call(&t, "r1");
                asker
                    .decide_call_receipted(
                        &t,
                        "r1",
                        "Bash",
                        &json!({"command": command}),
                        None,
                        claim,
                    )
                    .await
            }));
            loop {
                if let LiveEvent::GateOpened { .. } = events.recv().await.unwrap() {
                    break;
                }
            }
        }
        let text = "z".repeat(GATE_BODY_MAX_CHARS * 2);
        let steer = bridge.steer_at_gates(&thread, "r1", &text, true);
        let mut reasons = Vec::new();
        for waiter in waiters {
            let (decision, receipt) = waiter.await.unwrap();
            reasons.push(decision.denial_message().unwrap());
            receipt.delivered(true);
        }
        assert!(steer.settle().await, "the carrying denial was delivered");
        assert_eq!(
            reasons.iter().filter(|r| r.contains(&text)).count(),
            1,
            "{reasons:?}"
        );
        assert!(reasons.iter().any(|r| !r.contains('z')));
    }
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
        assert!(bridge.resolve_gate(&thread, &gate.gate_ref, GateAnswer::Approve));
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

    /// The CLI's own subagent tools are denied outright, before any gate or
    /// "Full access" shortcut: subagents are orchestrator threads.
    #[tokio::test]
    async fn builtin_subagent_tools_are_denied_without_a_gate() {
        let bridge = CliBridge::new(Duration::from_millis(200));
        let thread = tid();
        bridge.turn_started(&thread, "r1");
        for tool in ["SendMessage", "ListAgents", "TaskStop"] {
            assert!(crate::is_builtin_subagent_tool(tool), "{tool}");
        }
        for tool in crate::BUILTIN_SUBAGENT_TOOLS {
            let claim = bridge.claim_call(&thread, "r1");
            let decision = bridge
                .decide_call(&thread, "r1", tool, &json!({"prompt": "go"}), None, claim)
                .await;
            assert_eq!(
                decision,
                ToolDecision::Deny(BUILTIN_SUBAGENT_DENIAL.into()),
                "{tool}"
            );
        }
        let snap = bridge.snapshot(&thread).unwrap();
        assert!(snap.gates.is_empty() && snap.tools.is_empty());
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
        assert!(!bridge.resolve_gate(
            &ThreadId::new("other").unwrap(),
            &gate.gate_ref,
            GateAnswer::Approve
        ));
        assert!(bridge.resolve_gate(&thread, &gate.gate_ref, GateAnswer::Approve));
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
        assert!(bridge.resolve_gate(&thread, &gate.gate_ref, GateAnswer::Approve));
        assert_eq!(allowed.await.unwrap(), ToolDecision::Allow);
        let declined = ask("Read");
        let gate = gate_for(0).await;
        assert!(bridge.resolve_gate(
            &thread,
            &gate.gate_ref,
            GateAnswer::Decline { feedback: None }
        ));
        assert!(matches!(declined.await.unwrap(), ToolDecision::Deny(_)));
        // "No, do X instead": still a decline, and the words are recorded.
        let rejected = ask("Edit");
        let gate = gate_for(0).await;
        let feedback = DenialFeedback::new("edit the copy in /tmp instead").unwrap();
        assert!(bridge.resolve_gate(
            &thread,
            &gate.gate_ref,
            GateAnswer::Decline {
                feedback: Some(feedback.clone())
            }
        ));
        let decision = rejected.await.unwrap();
        assert_eq!(decision, ToolDecision::Rejected(feedback));
        assert_eq!(
            decision.denial_message().as_deref(),
            Some("The user rejected this call: edit the copy in /tmp instead")
        );
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
                ("Edit", Some(D::Declined)),
                ("Write", None)
            ]
        );
        assert_eq!(rows[0].body.as_deref(), Some("ls"));
        assert_eq!(
            rows[2].reason.as_deref(),
            Some("The user rejected this call: edit the copy in /tmp instead"),
            "the feedback is recorded with the decision"
        );

        let reopened = open().await;
        let rows = reopened.store().cli_approvals(&thread).await.unwrap();
        assert_eq!(rows[3].decision, Some(D::Interrupted));
        assert_eq!(
            rows[0].decision,
            Some(D::Allowed),
            "closed rows keep their answer"
        );
    }
}
