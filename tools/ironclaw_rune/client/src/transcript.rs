//! Reconciles the durable timeline with the live projection into the rows a
//! chat view renders. UI-free so it can be unit-tested.
//!
//! The model follows `CONTRACT.md` "Streaming model":
//! - projection frames are partial: every item is upserted by identity
//!   (`text:{run}:{call}`, run id, gate ref, invocation id);
//! - live assistant text is cumulative per model call; the run's trailing
//!   unflagged call is the streaming bubble, earlier calls and `narration`
//!   items are activity, like thinking and tool cards;
//! - once a run reaches a terminal status the durable reply converges into the
//!   bubble: the app reloads the timeline and the durable message replaces it;
//! - thinking and narration are live-only state; tool cards are live too,
//!   but rebuilt from the stored tool-preview rows when the timeline loads,
//!   so a reopened chat still shows what each run did;
//! - a message sent while a run works (a steer) sits where it was sent: the
//!   run's activity before it renders above it, the rest below.

use std::collections::{HashMap, HashSet};

use ironclaw_host_api::agent_plan::{
    AGENT_PLAN_OUTPUT_KIND, AgentPlan, QUESTION_GATE_REF_PREFIX, QuestionForm,
};
use ironclaw_product_contracts::outbound::{
    CapabilityActivityStatusView, CapabilityActivityView, CapabilityDisplayPreviewView,
    CliDetailKind, ProductGateKind, ProductProjectionItem,
};

use crate::api::GateDecision;
use crate::orchestration::MergedFork;
use crate::wire::{
    ATTACHMENTS_ONLY_CONTENT, CommandField, CommandOutcome, MessageKind, MessageStatus,
    StreamEvent, TimelineMessage, TimelinePage,
};

mod attached;
mod reason;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateState {
    Open,
    /// The person answered; waiting for the projection to confirm.
    Resolving(GateDecision),
    /// The run moved on. The decision is known only when it was made in this
    /// view: the projection does not carry it, so a reloaded gate has `None`.
    Closed(Option<GateDecision>),
}

/// Where a question form stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuestionState {
    Open,
    /// Sent from this view (`true`: answered, `false`: dismissed); waiting
    /// for the stream to confirm.
    Sending(bool),
    /// The run moved on. How it ended is known only when it was sent here.
    Closed(Option<bool>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    User {
        key: String,
        /// The person's words, with `{{attachment:N}}` where they placed
        /// `attachments[N]` (see [`readable`]).
        text: String,
        pending: bool,
        attachments: Vec<AttachmentChip>,
        /// Set when the message steered a turn already running (t3code's
        /// "Steer" marker), rather than starting one.
        steer: Option<SteerMark>,
    },
    Assistant {
        key: String,
        text: String,
        streaming: bool,
    },
    System {
        key: String,
        text: String,
    },
    Thinking {
        key: String,
        text: String,
    },
    Narration {
        key: String,
        text: String,
    },
    Tool {
        key: String,
        title: String,
        detail: Option<String>,
        status: ToolStatus,
        /// Why it ran: what the model said before the call (see `reason`).
        reason: Option<String>,
    },
    Skills {
        key: String,
        names: Vec<String>,
    },
    WorkSummary {
        key: String,
        text: String,
    },
    /// Finished delegated tasks the engine handed to this thread. Stored as
    /// a user-kind message, but the person never typed it.
    Delegated {
        key: String,
        title: String,
        body: String,
    },
    Gate {
        key: String,
        run_id: String,
        gate_ref: String,
        kind: ProductGateKind,
        headline: String,
        body: Option<String>,
        allow_always: bool,
        state: GateState,
        /// Why the agent asked (see `reason`).
        reason: Option<String>,
    },
    RunNotice {
        key: String,
        run_id: String,
        text: String,
        failed: bool,
        retryable: bool,
    },
    /// A steer the agent did not confirm reading before Stop: not requeued,
    /// so the person decides (t3code's "Steer delivery not confirmed").
    SteerNotice {
        key: String,
        title: String,
        /// The notice sentence, with the steer's words.
        text: String,
        /// The steer's own words, to put back in the composer.
        words: String,
    },
    /// A slash command's answer. Local to this view: the gateway does not
    /// store command results in the timeline.
    Command {
        key: String,
        input: String,
        title: String,
        fields: Vec<CommandField>,
        lines: Vec<String>,
        failed: bool,
    },
    /// The agent updated its plan here (one compact line; the live
    /// checklist is [`Transcript::plan`]).
    Plan {
        key: String,
        plan: AgentPlan,
    },
    /// A question the run waits on, drawn as a form.
    Question {
        key: String,
        run_id: String,
        gate_ref: String,
        form: QuestionForm,
        state: QuestionState,
    },
}

/// A steer's marker on its row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SteerMark {
    /// The agent has not read it yet ("Steer · waiting to be read").
    pub waiting: bool,
    /// The row above is a steer too: the label shows once for the run.
    pub continues: bool,
    /// How long the turn kept working after it, once the turn has ended;
    /// `None` while it runs ("Continued").
    pub continued_ms: Option<i64>,
}

/// A file on a user message. `message_id` is set once the gateway stored the
/// bytes, which is when the file can be opened; `local_path` is the backend's
/// copy of a file named in the prompt, opened straight from disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentChip {
    pub message_id: Option<String>,
    pub id: String,
    pub filename: String,
    pub size: Option<u64>,
    pub local_path: Option<String>,
}

impl Row {
    pub fn key(&self) -> &str {
        match self {
            Row::User { key, .. }
            | Row::Assistant { key, .. }
            | Row::System { key, .. }
            | Row::Thinking { key, .. }
            | Row::Narration { key, .. }
            | Row::Tool { key, .. }
            | Row::Skills { key, .. }
            | Row::WorkSummary { key, .. }
            | Row::Delegated { key, .. }
            | Row::Gate { key, .. }
            | Row::RunNotice { key, .. }
            | Row::SteerNotice { key, .. }
            | Row::Command { key, .. }
            | Row::Plan { key, .. }
            | Row::Question { key, .. } => key,
        }
    }
}

#[derive(Debug, Clone)]
enum Live {
    Text {
        body: String,
        narration: bool,
    },
    Thinking(String),
    Tool {
        title: String,
        detail: Option<String>,
        status: ToolStatus,
    },
    Skills(Vec<String>),
    WorkSummary(String),
    Gate {
        gate_ref: String,
        kind: ProductGateKind,
        headline: String,
        body: Option<String>,
        allow_always: bool,
        reason: Option<String>,
    },
    Plan(AgentPlan),
    Question {
        gate_ref: String,
        form: QuestionForm,
        /// The stream closed this one form.
        closed: bool,
    },
}

#[derive(Debug, Clone)]
struct LiveEntry {
    run: Option<String>,
    /// Tick of the last upsert; orders gates against later run statuses.
    tick: u64,
    /// Wall-clock ms the entry first appeared; places it around steers.
    at: i64,
    item: Live,
}

#[derive(Debug, Clone)]
struct RunStatus {
    status: String,
    tick: u64,
    failure: Option<String>,
    retryable: bool,
}

/// What the app should do after applying a frame.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Applied {
    pub changed: bool,
    /// A run settled: refetch the timeline so the durable reply lands.
    pub reload_timeline: bool,
    /// Set once per run, on the frame that first settled it.
    pub run_ended: Option<RunEnd>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunEnd {
    Completed,
    Failed,
    Cancelled,
}

impl RunEnd {
    fn from_status(status: &str) -> Self {
        match status {
            "completed" | "succeeded" => Self::Completed,
            "cancelled" => Self::Cancelled,
            _ => Self::Failed,
        }
    }
}

#[derive(Debug, Default)]
pub struct Transcript {
    durable: Vec<TimelineMessage>,
    order: Vec<String>,
    live: HashMap<String, LiveEntry>,
    runs: HashMap<String, RunStatus>,
    settled: HashSet<String>,
    /// Answers given in this view, kept after the gate closes for its label.
    decisions: HashMap<String, GateDecision>,
    /// Question forms sent from this view: answered (`true`) or dismissed.
    question_sends: HashMap<String, bool>,
    pending_user: Vec<PendingUser>,
    /// Row keys of server messages that replaced an optimistic bubble
    /// (`message_id` → `pending:N`): the copy takes the bubble's key, so the
    /// view keeps one element instead of fading one out and another in.
    sent_keys: HashMap<String, String>,
    /// Command results, each placed after the last durable message it saw.
    notes: Vec<(u64, Row)>,
    latest_run: Option<String>,
    /// The run whose end was reported last (`Applied::run_ended`).
    last_settled: Option<String>,
    /// Stop clicked in this view; shows "Stopping" before the server confirms.
    stop_requested: Option<String>,
    tick: u64,
    /// A fork's source conversation, shown read-only above its own messages.
    prefix: Vec<TimelineMessage>,
    /// Forks merged back into this thread, each shown where it arrived.
    merges: Vec<MergedFork>,
    /// Live detail a CLI agent reported (Codex): output tails by tool
    /// invocation, the latest turn diff and review. Live-only, kept while
    /// the view is open.
    cli: CliDetails,
}

/// A bounded live body; `truncated` when the agent's text was cut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliBody {
    pub run: Option<String>,
    pub body: String,
    pub truncated: bool,
}

/// Output tails a view keeps: the newest rows' only.
const CLI_OUTPUTS_MAX: usize = 64;
/// Bytes of one output tail a view keeps (the newest end).
const CLI_OUTPUT_MAX_BYTES: usize = 8 * 1024;

#[derive(Debug, Default)]
struct CliDetails {
    /// By tool row key (`tool:{invocation}`). Shared with the renderer:
    /// cloning is a pointer copy, and a frame copies the map only when the
    /// renderer still holds the previous one.
    outputs: std::sync::Arc<HashMap<String, CliBody>>,
    /// Row keys oldest first, for eviction past [`CLI_OUTPUTS_MAX`].
    order: std::collections::VecDeque<String>,
    diff: Option<CliBody>,
    review: Option<CliBody>,
    /// Bumped whenever `diff` or `review` changes, so a caller can tell
    /// "moved" without keeping a copy of either body.
    rev: u64,
}

impl CliDetails {
    fn set_report(&mut self, slot: fn(&mut Self) -> &mut Option<CliBody>, body: CliBody) {
        let slot = slot(self);
        if slot.as_ref() != Some(&body) {
            *slot = Some(body);
            self.rev += 1;
        }
    }

    fn put_output(&mut self, key: String, mut body: CliBody) {
        if body.body.len() > CLI_OUTPUT_MAX_BYTES {
            let mut cut = body.body.len() - CLI_OUTPUT_MAX_BYTES;
            while !body.body.is_char_boundary(cut) {
                cut += 1;
            }
            body.body.replace_range(..cut, "");
            body.truncated = true;
        }
        let outputs = std::sync::Arc::make_mut(&mut self.outputs);
        if outputs.insert(key.clone(), body).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > CLI_OUTPUTS_MAX {
            if let Some(old) = self.order.pop_front() {
                outputs.remove(&old);
            }
        }
    }
}

/// A message sent from this view that the timeline does not show yet.
#[derive(Debug)]
struct PendingUser {
    id: u64,
    /// The engine's id for it once the ack named one (a steer).
    steer_id: Option<String>,
    text: String,
    /// The gateway accepted the send.
    acked: bool,
    chips: Vec<AttachmentChip>,
    /// Newest durable sequence when it was sent; its server copy is newer.
    after: Option<u64>,
    /// Sent into the running turn: `Some(waiting)` until the timeline has it.
    steer: Option<bool>,
}

/// Back-to-back steers show their label once (t3code's `continuesSteer`).
fn mark_steer_runs(rows: &mut [Row]) {
    let mut after_steer = false;
    for row in rows.iter_mut() {
        let steer = match row {
            Row::User { steer, .. } => steer.as_mut(),
            _ => None,
        };
        match steer {
            Some(mark) => {
                mark.continues = after_steer;
                after_steer = true;
            }
            None => after_steer = false,
        }
    }
}

fn pending_key(id: u64) -> String {
    format!("pending:{id}")
}

/// The native agent's checklist capability: its result is a plan, drawn as
/// the plan strip and a compact line rather than a tool row.
pub const PLAN_UPDATE_CAPABILITY: &str = "builtin.plan_update";

/// The plan a `builtin.plan_update` preview carries, if it is one.
fn preview_plan(capability: &str, kind: Option<&str>, preview: Option<&str>) -> Option<AgentPlan> {
    (capability == PLAN_UPDATE_CAPABILITY && kind == Some(AGENT_PLAN_OUTPUT_KIND))
        .then(|| serde_json::from_str(preview?).ok()) // silent-ok: a torn preview falls back to a tool row
        .flatten()
}

/// Key prefix of the rows a fork shows from its source thread.
pub const PREFIX_KEY: &str = "src:";

/// A user row's text with each attachment anchor replaced by the file's
/// name: for places that show plain text (the outline, ↑ to edit).
pub fn readable(text: &str, chips: &[AttachmentChip]) -> String {
    use crate::inbound_requests::{AnchoredPart, anchored_parts};
    anchored_parts(text)
        .into_iter()
        .map(|part| match part {
            AnchoredPart::Text(run) => run,
            AnchoredPart::Attachment(ix) => chips.get(ix).map_or("", |c| c.filename.as_str()),
        })
        .collect()
}

/// The engine's result-delivery turn (`result_turn_text` in the
/// orchestrator): a title naming each task, and the results below it.
fn delegated_result(text: &str) -> Option<(String, String)> {
    let body = text.strip_prefix("Delegated work finished.")?;
    let body = body
        .trim_start()
        .strip_prefix("Use these results to continue.")
        .unwrap_or(body)
        .trim()
        .to_string();
    let titles: Vec<&str> = body
        .lines()
        .filter(|l| l.starts_with("[T3 delegated task "))
        .filter_map(|l| l.split('"').nth(1))
        .collect();
    let title = match titles.as_slice() {
        [] => "Delegated work finished".to_string(),
        list => format!("Delegated work finished: {}", list.join(", ")),
    };
    Some((title, body))
}

/// A message's text as a person reads it: without the attachments-only
/// marker, the expanded @-mention context, and the context handoff the engine prepends to the first
/// message sent after a fork or provider switch (`{context}\n\n---\n\n{typed}`).
fn shown_text(message: &TimelineMessage) -> String {
    let text = message
        .content
        .as_deref()
        .filter(|c| *c != ATTACHMENTS_ONLY_CONTENT)
        .unwrap_or_default();
    // The context the backend expanded from @-mentions is for the agent.
    let text = if message.kind == MessageKind::User {
        crate::mentions::strip_mention_context(text)
    } else {
        text
    };
    if message.kind != MessageKind::User
        || !(text.starts_with("Context handoff (") || text.starts_with("Merged context from fork"))
    {
        return text.to_string();
    }
    ["[End of context handoff]", "[End of merged fork context]"]
        .iter()
        .filter_map(|end| text.rfind(end).map(|i| i + end.len()))
        .max()
        .map_or(text, |i| {
            text[i..]
                .trim_start_matches('\n')
                .trim_start_matches("---")
                .trim_start()
        })
        .to_string()
}

/// Rows for messages shown from another thread (a fork's source, a merged
/// fork), keyed under `prefix` so they carry no fork action.
fn borrowed_rows(messages: &[TimelineMessage], prefix: &str, rows: &mut Vec<Row>) {
    for message in messages {
        let key = format!("{prefix}{}", message.message_id);
        let text = shown_text(message);
        if text.trim().is_empty() {
            continue;
        }
        rows.push(match message.kind {
            MessageKind::User => Row::User {
                key,
                text,
                pending: false,
                attachments: Vec::new(),
                steer: None,
            },
            MessageKind::Assistant if message.commentary => Row::Narration { key, text },
            MessageKind::Assistant => Row::Assistant {
                key,
                text,
                streaming: false,
            },
            _ => Row::System { key, text },
        });
    }
}

/// A thread as Markdown, for handing it to another thread to read:
/// prompts and answers in order, oldest first.
pub fn thread_markdown(title: &str, messages: &[TimelineMessage]) -> String {
    let mut out = format!("# Thread: {title}\n");
    for m in messages.iter().filter(|m| m.is_visible()) {
        let who = match m.kind {
            MessageKind::User => "User",
            MessageKind::Assistant => "Assistant",
            _ => continue,
        };
        let text = shown_text(m);
        if !text.trim().is_empty() {
            out.push_str(&format!("\n## {who}\n\n{}\n", text.trim()));
        }
    }
    out
}

const TERMINAL: [&str; 5] = [
    "completed",
    "succeeded",
    "failed",
    "cancelled",
    "recovery_required",
];

fn is_terminal(status: &str) -> bool {
    TERMINAL.contains(&status)
}

impl Transcript {
    pub fn set_timeline(&mut self, page: TimelinePage) {
        let mut messages = page.messages;
        // The typed marks win over the rows' own fields.
        for mark in page.steer_marks {
            if let Some(m) = messages
                .iter_mut()
                .find(|m| m.message_id == mark.message_id)
            {
                if mark.steer.is_some() {
                    m.steer = mark.steer;
                }
                if !mark.notices.is_empty() {
                    m.steer_notices = mark.notices;
                }
            }
        }
        messages.sort_by_key(|m| m.sequence);
        // The backend renumbers a steer when the model reads it; show it
        // when it was sent instead, if every row carries its send time.
        if messages.iter().all(|m| m.created_at.is_some()) {
            messages.sort_by_key(|m| m.created_at);
        }
        for m in &messages {
            if m.kind == MessageKind::CapabilityDisplayPreview {
                self.seed_tool(m);
            }
        }
        self.durable = messages;
        self.reconcile_pending();
    }

    /// A stored tool preview becomes a tool card, unless the live stream
    /// already drew it.
    fn seed_tool(&mut self, m: &TimelineMessage) {
        let Some(v) = m
            .content
            .as_deref()
            .and_then(|c| serde_json::from_str::<serde_json::Value>(c).ok())
        else {
            return;
        };
        let field = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
        let Some(invocation) = field("invocation_id") else {
            return;
        };
        if let Some(plan) = preview_plan(
            field("capability_id").as_deref().unwrap_or_default(),
            field("output_kind").as_deref(),
            field("output_preview").as_deref(),
        ) {
            self.upsert(
                format!("plan:{invocation}"),
                m.turn_run_id.clone(),
                Live::Plan(plan),
            );
            if let (Some(entry), Some(at)) = (
                self.live.get_mut(&format!("plan:{invocation}")),
                m.created_at,
            ) {
                entry.at = at.timestamp_millis();
            }
            return;
        }
        let key = format!("tool:{invocation}");
        let status = match field("status").as_deref() {
            Some("completed") => ToolStatus::Completed,
            Some("failed" | "killed") => ToolStatus::Failed,
            _ => ToolStatus::Running,
        };
        let detail = field("output_summary").or_else(|| field("input_summary"));
        // A card the stream drew keeps its place; the stored row settles it
        // when the stream missed the completion.
        if let Some(entry) = self.live.get_mut(&key) {
            if let Live::Tool {
                detail: old_detail,
                status: old_status,
                ..
            } = &mut entry.item
                && status != ToolStatus::Running
            {
                *old_status = status;
                if detail.is_some() {
                    *old_detail = detail;
                }
            }
            return;
        }
        let title = field("title")
            .or_else(|| field("capability_id"))
            .unwrap_or_else(|| "tool".into());
        self.upsert_tool(invocation, m.turn_run_id.clone(), title, detail, status);
        if let (Some(entry), Some(at)) = (self.live.get_mut(&key), m.created_at) {
            entry.at = at.timestamp_millis();
        }
    }

    pub fn set_prefix(&mut self, mut messages: Vec<TimelineMessage>) {
        messages.retain(TimelineMessage::is_visible);
        self.prefix = messages;
    }

    pub fn set_merges(&mut self, mut merges: Vec<MergedFork>) {
        for m in &mut merges {
            m.messages.retain(TimelineMessage::is_visible);
        }
        self.merges = merges;
    }

    /// Drop each optimistic message once its server copy is in the timeline.
    /// Accepting a send is not enough: a CLI runtime writes the person's
    /// message into history later, and the bubble must not vanish meanwhile.
    fn reconcile_pending(&mut self) {
        let mut claimed: HashSet<&str> = HashSet::new();
        let durable = &self.durable;
        let sent_keys = &mut self.sent_keys;
        self.pending_user.retain(|p| {
            let newer = |m: &&TimelineMessage| {
                m.kind == MessageKind::User
                    && p.after.is_none_or(|after| m.sequence > after)
                    && !claimed.contains(m.message_id.as_str())
            };
            let same_text = |m: &&TimelineMessage| {
                let content = m.content.as_deref().unwrap_or_default();
                shown_text(m).trim() == p.text.trim()
                    || (p.text.trim().is_empty() && content == ATTACHMENTS_ONLY_CONTENT)
            };
            // Prefer the copy with the same text; once accepted, any newer
            // message of the person stands for it (the gateway may reword).
            // The engine's steer id names its row outright.
            let same_steer = |m: &&TimelineMessage| {
                p.steer_id.as_deref().is_some_and(|id| {
                    m.steer_id.as_deref() == Some(id)
                        || m.steer.as_ref().is_some_and(|s| s.id == id)
                })
            };
            let found = durable
                .iter()
                .filter(|m| !claimed.contains(m.message_id.as_str()))
                .find(same_steer)
                .or_else(|| durable.iter().filter(newer).find(same_text))
                .or_else(|| p.acked.then(|| durable.iter().find(newer)).flatten());
            match found {
                Some(m) => {
                    claimed.insert(m.message_id.as_str());
                    sent_keys.insert(m.message_id.clone(), pending_key(p.id));
                    false
                }
                None => true,
            }
        });
    }

    /// Show a message immediately while the send is in flight.
    pub fn push_pending_user(&mut self, text: &str, files: &[String]) -> u64 {
        // Unique across every transcript (main and side chats): the row key
        // `pending:N` outlives the send once the server copy takes it, and
        // scroll requests resolve keys window-wide.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let chips = files
            .iter()
            .enumerate()
            .map(|(i, name)| AttachmentChip {
                message_id: None,
                id: i.to_string(),
                filename: name.clone(),
                size: None,
                local_path: None,
            })
            .collect();
        self.pending_user.push(PendingUser {
            id,
            steer_id: None,
            text: text.to_string(),
            acked: false,
            chips,
            after: self.durable.iter().map(|m| m.sequence).max(),
            steer: None,
        });
        id
    }

    /// Note the engine's id for pending message `id` (the ack's `steer_id`).
    pub fn set_pending_steer_id(&mut self, id: u64, steer_id: Option<String>) {
        if let Some(entry) = self.pending_user.iter_mut().find(|p| p.id == id) {
            entry.steer_id = steer_id;
        }
    }

    /// Mark pending message `id` as a steer of the running turn
    /// (`Some(waiting)`), or as the start of its own turn (`None`).
    pub fn set_pending_steer(&mut self, id: u64, steer: Option<bool>) {
        if let Some(entry) = self.pending_user.iter_mut().find(|p| p.id == id) {
            entry.steer = steer;
        }
    }

    /// The row key of durable message `message_id`.
    pub fn key_of_message(&self, message_id: &str) -> String {
        self.sent_keys
            .get(message_id)
            .cloned()
            .unwrap_or_else(|| format!("msg:{message_id}"))
    }

    pub fn ack_pending_user(&mut self, id: u64) {
        if let Some(entry) = self.pending_user.iter_mut().find(|p| p.id == id) {
            entry.acked = true;
        }
    }

    pub fn drop_pending_user(&mut self, id: u64) {
        self.pending_user.retain(|p| p.id != id);
    }

    /// Show a slash command and its answer (or rejection) in the transcript.
    pub fn push_command(&mut self, input: &str, outcome: Result<CommandOutcome, String>) {
        self.tick += 1;
        let (title, fields, lines, failed) = match outcome {
            Ok(CommandOutcome {
                result: Some(r), ..
            }) => (r.title, r.fields, r.lines, false),
            Ok(CommandOutcome {
                rejection: Some(r), ..
            }) => (String::new(), Vec::new(), vec![r.message], true),
            Ok(_) => (String::new(), Vec::new(), Vec::new(), false),
            Err(message) => (String::new(), Vec::new(), vec![message], true),
        };
        let anchor = self
            .durable
            .iter()
            .filter(|m| m.is_visible())
            .map(|m| m.sequence)
            .max()
            .unwrap_or(0);
        let row = Row::Command {
            key: format!("cmd:{}", self.tick),
            input: input.to_string(),
            title,
            fields,
            lines,
            failed,
        };
        self.notes.push((anchor, row));
    }

    pub fn mark_gate_resolving(&mut self, gate_ref: &str, decision: GateDecision) {
        self.decisions.insert(gate_ref.to_string(), decision);
    }

    pub fn clear_gate_resolving(&mut self, gate_ref: &str) {
        self.decisions.remove(gate_ref);
    }

    /// A question form was answered (`true`) or dismissed from this view.
    pub fn mark_question_sending(&mut self, gate_ref: &str, answered: bool) {
        self.question_sends.insert(gate_ref.to_string(), answered);
    }

    /// The send failed: the form opens again.
    pub fn clear_question_sending(&mut self, gate_ref: &str) {
        self.question_sends.remove(gate_ref);
    }

    /// The thread's current plan: the newest one any run reported.
    pub fn plan(&self) -> Option<&AgentPlan> {
        self.live
            .values()
            .filter_map(|e| match &e.item {
                Live::Plan(plan) => Some(plan),
                _ => None,
            })
            .max_by_key(|plan| plan.updated_at)
    }

    /// Unix ms of the newest durable prompt the person sent.
    pub fn last_prompt_sent_ms(&self) -> Option<i64> {
        self.durable
            .iter()
            .rev()
            .find(|m| m.kind == MessageKind::User)
            .and_then(|m| m.created_at)
            .map(|t| t.timestamp_millis())
    }

    /// The run the Stop button targets: the latest one not yet terminal.
    pub fn active_run(&self) -> Option<&str> {
        let run = self.latest_run.as_deref()?;
        match self.runs.get(run) {
            Some(status) if is_terminal(&status.status) => None,
            _ if self.settled.contains(run) => None,
            _ => Some(run),
        }
    }

    pub fn mark_stop_requested(&mut self, run: &str) {
        self.stop_requested = Some(run.to_string());
    }

    /// The cancel POST failed: fall back to the server's view of the run.
    pub fn clear_stop_requested(&mut self) {
        self.stop_requested = None;
    }

    /// Stop was clicked or accepted, but the run has not wound down yet.
    pub fn is_stopping(&self) -> bool {
        let Some(run) = self.active_run() else {
            return false;
        };
        self.stop_requested.as_deref() == Some(run)
            || self
                .runs
                .get(run)
                .is_some_and(|s| s.status == "cancel_requested")
    }

    /// The retained output tails, by tool row key (`tool:{invocation}`):
    /// a shared handle, not a copy.
    pub fn tool_outputs(&self) -> std::sync::Arc<HashMap<String, CliBody>> {
        std::sync::Arc::clone(&self.cli.outputs)
    }

    /// The latest turn diff a CLI agent computed (Codex `turn/diff/updated`).
    pub fn cli_turn_diff(&self) -> Option<&CliBody> {
        self.cli.diff.as_ref()
    }

    /// Changes whenever [`Self::cli_turn_diff`] or [`Self::cli_review`] does.
    pub fn cli_report_rev(&self) -> u64 {
        self.cli.rev
    }

    /// The latest finished review's findings text (Codex `review/start`).
    pub fn cli_review(&self) -> Option<&CliBody> {
        self.cli.review.as_ref()
    }

    pub fn apply(&mut self, event: &StreamEvent) -> Applied {
        let mut out = Applied::default();
        match event {
            StreamEvent::ProjectionSnapshot { state } | StreamEvent::ProjectionUpdate { state } => {
                for item in &state.items {
                    self.apply_item(item, &mut out);
                }
            }
            StreamEvent::Accepted { ack } => {
                if let Some(run) = &ack.run_id {
                    self.note_run(run);
                    out.changed = true;
                }
            }
            StreamEvent::Running { progress } | StreamEvent::CapabilityProgress { progress } => {
                self.note_run(&progress.turn_run_id.to_string());
                out.changed = true;
            }
            StreamEvent::CapabilityActivity { activity } => {
                self.upsert_activity(activity);
                out.changed = true;
            }
            StreamEvent::CapabilityDisplayPreview { preview } => {
                self.upsert_preview(preview);
                out.changed = true;
            }
            StreamEvent::Gate { prompt } => {
                let run = prompt.turn_run_id.to_string();
                self.upsert(
                    format!("gate:{}", prompt.gate_ref),
                    Some(run),
                    Live::Gate {
                        gate_ref: prompt.gate_ref.clone(),
                        kind: ProductGateKind::Approval,
                        headline: prompt.headline.clone(),
                        body: Some(prompt.body.clone()).filter(|b| !b.trim().is_empty()),
                        allow_always: prompt.allow_always,
                        reason: None,
                    },
                );
                out.changed = true;
            }
            StreamEvent::FinalReply { reply } => {
                let run = reply.turn_run_id.to_string();
                // The reply streamed in already: finish that bubble in place,
                // so the answer keeps its element and never reflows.
                let key = self
                    .streamed_reply(&run, &reply.text)
                    .unwrap_or_else(|| format!("final:{run}"));
                self.upsert(
                    key,
                    Some(run.clone()),
                    Live::Text {
                        body: reply.text.clone(),
                        narration: false,
                    },
                );
                self.settle(&run, RunEnd::Completed, &mut out);
                out.changed = true;
            }
            StreamEvent::Cancelled { .. } | StreamEvent::Failed { .. } => {
                let end = if matches!(event, StreamEvent::Cancelled { .. }) {
                    RunEnd::Cancelled
                } else {
                    RunEnd::Failed
                };
                if let Some(run) = self.latest_run.clone() {
                    self.settle(&run, end, &mut out);
                }
                out.reload_timeline = true;
                out.changed = true;
            }
            StreamEvent::AuthRequired { .. } | StreamEvent::KeepAlive | StreamEvent::Unknown => {}
        }
        out
    }

    fn apply_item(&mut self, item: &ProductProjectionItem, out: &mut Applied) {
        out.changed = true;
        match item {
            ProductProjectionItem::Text {
                id,
                run_id,
                body,
                narration,
                ..
            } => {
                let run = run_id.map(|r| r.to_string());
                if let Some(run) = &run {
                    self.note_run(run);
                }
                // A finalized copy keyed by the stored message id lands in
                // the bubble it streamed into rather than opening a new one.
                let key = match &run {
                    Some(run)
                        if !narration
                            && !id.starts_with("text:")
                            && !self.live.contains_key(id) =>
                    {
                        self.streamed_reply(run, body).unwrap_or_else(|| id.clone())
                    }
                    _ => id.clone(),
                };
                self.upsert(
                    key,
                    run,
                    Live::Text {
                        body: body.clone(),
                        narration: *narration,
                    },
                );
            }
            ProductProjectionItem::Thinking { id, run_id, body } => {
                self.upsert(
                    id.clone(),
                    run_id.map(|r| r.to_string()),
                    Live::Thinking(body.clone()),
                );
            }
            ProductProjectionItem::CapabilityActivity(activity) => self.upsert_activity(activity),
            ProductProjectionItem::WorkSummary {
                id, run_id, body, ..
            } => {
                self.upsert(
                    id.clone(),
                    Some(run_id.to_string()),
                    Live::WorkSummary(body.clone()),
                );
            }
            ProductProjectionItem::CliDetail {
                run_id,
                kind,
                invocation_id,
                body,
                truncated,
                ..
            } => {
                let detail = CliBody {
                    run: run_id.map(|r| r.to_string()),
                    body: body.clone(),
                    truncated: *truncated,
                };
                match kind {
                    CliDetailKind::ToolOutput => {
                        if let Some(invocation) = invocation_id {
                            self.cli.put_output(format!("tool:{invocation}"), detail);
                        }
                    }
                    CliDetailKind::TurnDiff => self.cli.set_report(|c| &mut c.diff, detail),
                    CliDetailKind::Review => self.cli.set_report(|c| &mut c.review, detail),
                }
            }
            ProductProjectionItem::SkillActivation {
                id,
                run_id,
                skill_names,
                ..
            } => {
                self.upsert(
                    id.clone(),
                    Some(run_id.to_string()),
                    Live::Skills(skill_names.clone()),
                );
            }
            ProductProjectionItem::Gate {
                run_id,
                gate_kind,
                gate_ref,
                headline,
                body,
                allow_always,
                reason,
                ..
            } => {
                self.upsert(
                    format!("gate:{gate_ref}"),
                    Some(run_id.to_string()),
                    Live::Gate {
                        gate_ref: gate_ref.clone(),
                        kind: *gate_kind,
                        headline: headline.clone(),
                        body: body.clone(),
                        allow_always: *allow_always,
                        reason: reason.as_deref().and_then(reason::bounded),
                    },
                );
            }
            ProductProjectionItem::Plan { id, run_id, plan } => {
                self.upsert(
                    id.clone(),
                    run_id.map(|r| r.to_string()),
                    Live::Plan(plan.clone()),
                );
            }
            ProductProjectionItem::Question {
                run_id,
                gate_ref,
                form,
                closed,
            } => {
                let key = format!("question:{gate_ref}");
                // A close never reopens: a late open frame keeps it closed.
                let was_closed = matches!(
                    self.live.get(&key).map(|e| &e.item),
                    Some(Live::Question { closed: true, .. })
                );
                self.upsert(
                    key,
                    Some(run_id.to_string()),
                    Live::Question {
                        gate_ref: gate_ref.clone(),
                        form: form.clone(),
                        closed: *closed || was_closed,
                    },
                );
            }
            ProductProjectionItem::RunStatus {
                run_id,
                status,
                failure_category,
                failure_summary,
                retryable,
            } => {
                let run = run_id.to_string();
                self.note_run(&run);
                self.tick += 1;
                let failure = failure_summary.clone().or_else(|| {
                    failure_category
                        .as_ref()
                        .map(|f| f.category().replace('_', " "))
                });
                self.runs.insert(
                    run.clone(),
                    RunStatus {
                        status: status.clone(),
                        tick: self.tick,
                        failure,
                        retryable: retryable.unwrap_or(false),
                    },
                );
                if is_terminal(status) {
                    self.settle(&run, RunEnd::from_status(status), out);
                }
            }
        }
    }

    /// The live, unflagged text of `run` that `reply` finishes: the same
    /// words, or the start of them. Its key is the bubble the reader watched.
    fn streamed_reply(&self, run: &str, reply: &str) -> Option<String> {
        let reply = reply.trim();
        self.order
            .iter()
            .rev()
            .find_map(|k| {
                let e = &self.live[k];
                match &e.item {
                    Live::Text {
                        body,
                        narration: false,
                    } if e.run.as_deref() == Some(run) => Some((k, body.trim())),
                    _ => None,
                }
            })
            .filter(|(_, body)| !body.is_empty() && reply.starts_with(body))
            .map(|(k, _)| k.clone())
    }

    fn upsert_activity(&mut self, a: &CapabilityActivityView) {
        // A plan update shows as its plan once the preview lands; only a
        // failed one keeps a tool row.
        if a.capability_id.as_str() == PLAN_UPDATE_CAPABILITY
            && a.status != CapabilityActivityStatusView::Failed
        {
            return;
        }
        let title = a
            .subtitle
            .clone()
            .filter(|s| !s.trim().is_empty())
            .map(|s| format!("{} {s}", a.capability_id))
            .unwrap_or_else(|| a.capability_id.to_string());
        let detail = a.input_summary.clone().or_else(|| a.error_detail.clone());
        self.upsert_tool(
            a.invocation_id.to_string(),
            a.turn_run_id.map(|r| r.to_string()),
            title,
            detail,
            tool_status(a.status),
        );
    }

    fn upsert_preview(&mut self, p: &CapabilityDisplayPreviewView) {
        if let Some(plan) = preview_plan(
            p.capability_id.as_str(),
            p.output_kind.as_deref(),
            p.output_preview.as_deref(),
        ) {
            self.upsert(
                format!("plan:{}", p.invocation_id),
                p.turn_run_id.map(|r| r.to_string()),
                Live::Plan(plan),
            );
            return;
        }
        let detail = p.output_summary.clone().or_else(|| p.input_summary.clone());
        self.upsert_tool(
            p.invocation_id.to_string(),
            p.turn_run_id.map(|r| r.to_string()),
            p.title.clone(),
            detail,
            tool_status(p.status),
        );
    }

    fn upsert_tool(
        &mut self,
        invocation: String,
        run: Option<String>,
        title: String,
        detail: Option<String>,
        status: ToolStatus,
    ) {
        let key = format!("tool:{invocation}");
        // A row keeps the first title and detail it showed, so its label
        // never flips mid-run; a late frame never reopens a finished call.
        let (title, detail, status) = match self.live.get(&key).map(|e| &e.item) {
            Some(Live::Tool {
                title: old_title,
                detail: old_detail,
                status: old_status,
            }) => (
                // A preview's title beats a bare capability id, once.
                if old_title.contains('.') && !title.contains('.') {
                    title
                } else {
                    old_title.clone()
                },
                // Input while it runs, the output once it is done.
                if status == ToolStatus::Running {
                    old_detail.clone().or(detail)
                } else {
                    detail.or_else(|| old_detail.clone())
                },
                if *old_status != ToolStatus::Running && status == ToolStatus::Running {
                    *old_status
                } else {
                    status
                },
            ),
            _ => (title, detail, status),
        };
        self.upsert(
            key,
            run,
            Live::Tool {
                title,
                detail,
                status,
            },
        );
    }

    fn upsert(&mut self, key: String, run: Option<String>, item: Live) {
        self.tick += 1;
        let tick = self.tick;
        // A bare frame (some activity arrives without a run id) belongs to
        // the run in flight, so it never lands in another run's group.
        let run = run.or_else(|| {
            (!self.live.contains_key(&key))
                .then(|| self.active_run().map(str::to_string))
                .flatten()
        });
        match self.live.get_mut(&key) {
            Some(entry) => {
                entry.item = item;
                entry.tick = tick;
                if run.is_some() {
                    entry.run = run;
                }
            }
            None => {
                self.order.push(key.clone());
                let at = chrono::Utc::now().timestamp_millis();
                self.live.insert(
                    key,
                    LiveEntry {
                        run,
                        tick,
                        at,
                        item,
                    },
                );
            }
        }
    }

    fn note_run(&mut self, run: &str) {
        if self.latest_run.as_deref() != Some(run) && !self.settled.contains(run) {
            self.latest_run = Some(run.to_string());
        }
    }

    fn settle(&mut self, run: &str, end: RunEnd, out: &mut Applied) {
        if self.settled.insert(run.to_string()) {
            self.last_settled = Some(run.to_string());
            out.reload_timeline = true;
            out.run_ended = Some(end);
        }
    }

    /// The run whose end `apply` reported last, so a caller can tell which
    /// run a `run_ended` belongs to.
    pub fn last_settled_run(&self) -> Option<&str> {
        self.last_settled.as_deref()
    }

    /// How long each settled turn worked, by its prompt row's key: from the
    /// prompt to the run's last row, stored or live (t3code's "Worked for").
    /// The running turn has no length yet and is left out.
    pub fn turn_spans(&self) -> HashMap<String, i64> {
        // Stored times are the runtime's own; a live entry's is when this
        // view first saw it (late for a replayed frame), so it only stands
        // in for a run with no stored time past its prompt.
        fn latest<'a>(pairs: impl Iterator<Item = (&'a str, i64)>) -> HashMap<&'a str, i64> {
            let mut last: HashMap<&str, i64> = HashMap::new();
            for (run, at) in pairs {
                last.entry(run)
                    .and_modify(|t| *t = (*t).max(at))
                    .or_insert(at);
            }
            last
        }
        let stored =
            latest(self.durable.iter().filter_map(|m| {
                Some((m.turn_run_id.as_deref()?, m.created_at?.timestamp_millis()))
            }));
        let seen = latest(
            self.live
                .values()
                .filter_map(|e| Some((e.run.as_deref()?, e.at))),
        );
        let active = self.active_run();
        let mut started: HashSet<&str> = HashSet::new();
        let mut spans = HashMap::new();
        for m in self
            .durable
            .iter()
            .filter(|m| m.kind == MessageKind::User && m.is_visible())
        {
            let Some(run) = m.turn_run_id.as_deref() else {
                continue;
            };
            // A run's later messages of the person's are steers: the turn
            // is timed from the prompt that started it.
            if !started.insert(run) || active == Some(run) {
                continue;
            }
            let Some(sent) = m.created_at.map(|t| t.timestamp_millis()) else {
                continue;
            };
            let end = stored
                .get(run)
                .filter(|end| **end > sent)
                .or_else(|| seen.get(run));
            if let Some(ms) = end.map(|end| end - sent).filter(|ms| *ms > 0) {
                spans.insert(self.key_of_message(&m.message_id), ms);
            }
        }
        spans
    }

    /// Whether the running turn's prompt is among the rows: one sent from
    /// this view, or the newest stored message of the person's belongs to
    /// the run (or has no run yet). False while a run started elsewhere
    /// waits for its prompt to load, so the turn before it is not taken
    /// for the live one.
    pub fn shows_running_prompt(&self) -> bool {
        let Some(run) = self.active_run() else {
            return true;
        };
        self.pending_user.iter().any(|p| p.steer.is_none())
            || self
                .durable
                .iter()
                .rev()
                .find(|m| m.kind == MessageKind::User && m.is_visible())
                .is_none_or(|m| m.turn_run_id.as_deref().is_none_or(|r| r == run))
    }

    /// The text of stored message `message_id`, for finding the row it
    /// draws as when that row kept the key of the live text it replaced.
    pub fn message_text(&self, message_id: &str) -> Option<&str> {
        self.durable
            .iter()
            .find(|m| m.message_id == message_id)
            .and_then(|m| m.content.as_deref())
    }

    pub fn rows(&self) -> Vec<Row> {
        // Live entries grouped by run, in first-seen order.
        let mut groups: Vec<(Option<String>, Vec<&str>)> = Vec::new();
        for key in &self.order {
            let run = self.live[key].run.clone();
            // A frame without a run id (some tool activity arrives bare)
            // belongs to the run it arrived during, not to a trailing
            // group drawn under the final answer.
            if run.is_none()
                && let Some((_, keys)) = groups.last_mut()
            {
                keys.push(key);
                continue;
            }
            match groups.iter_mut().find(|(r, _)| *r == run) {
                Some((_, keys)) => keys.push(key),
                None => groups.push((run, vec![key])),
            }
        }

        let durable_replies: HashMap<&str, &str> = self
            .durable
            .iter()
            .filter(|m| m.kind == MessageKind::Assistant && m.is_visible())
            .filter_map(|m| Some((m.turn_run_id.as_deref()?, m.content.as_deref()?)))
            .collect();
        let durable_reply_runs: HashSet<&str> = durable_replies.keys().copied().collect();

        let mut rows = Vec::new();
        let mut emitted: HashSet<Option<String>> = HashSet::new();
        // Runs drawn up to one of their replies, with live rows still to come.
        let mut split: HashSet<Option<String>> = HashSet::new();
        // Live keys a stored reply took over.
        let mut reply_keys: HashSet<&str> = HashSet::new();
        // Each run's last stored reply: where its notice goes.
        let last_reply: HashMap<&str, &str> = self
            .durable
            .iter()
            .filter(|m| m.kind == MessageKind::Assistant && m.is_visible())
            .filter_map(|m| Some((m.turn_run_id.as_deref()?, m.message_id.as_str())))
            .collect();
        // Activity already drawn above a steer; never drawn twice.
        let mut shown: HashSet<&str> = HashSet::new();
        // A run that fails before producing any output has no live group and
        // no reply; its notice goes right after the user message that started it.
        let quiet_run = |run: &str| {
            !durable_reply_runs.contains(run)
                && !groups.iter().any(|(r, _)| r.as_deref() == Some(run))
        };

        // Steers: a person's message in a run that a message of theirs
        // already started (or one the steer ledger marked). A run's last
        // stored time measures the work after a steer once it ended.
        let mut started_runs: HashSet<&str> = HashSet::new();
        let mut run_last_at: HashMap<&str, i64> = HashMap::new();
        for m in &self.durable {
            if let (Some(run), Some(at)) = (m.turn_run_id.as_deref(), m.created_at) {
                let at = at.timestamp_millis();
                run_last_at
                    .entry(run)
                    .and_modify(|t| *t = (*t).max(at))
                    .or_insert(at);
            }
        }
        // Where each steer's segment ends: the person's next message in the
        // same run, else the run's last stored row.
        let mut segment_end: HashMap<&str, i64> = HashMap::new();
        {
            let mut last_user: HashMap<&str, &str> = HashMap::new();
            for m in self.durable.iter().filter(|m| m.kind == MessageKind::User) {
                if let (Some(run), Some(at)) = (m.turn_run_id.as_deref(), m.created_at)
                    && let Some(prev) = last_user.insert(run, m.message_id.as_str())
                {
                    segment_end.insert(prev, at.timestamp_millis());
                }
            }
        }
        let active = self.active_run();
        // Notices a Stop left on a run's last row (stored or not) show
        // after the last row drawn at or before it.
        let mut owed: Vec<Row> = Vec::new();
        let notice_rows = |m: &TimelineMessage| -> Vec<Row> {
            m.steer_notices
                .iter()
                .map(|n| Row::SteerNotice {
                    key: format!("steer-notice:{}", n.steer_id),
                    title: if n.title.trim().is_empty() {
                        "Steer delivery not confirmed".into()
                    } else {
                        n.title.clone()
                    },
                    text: n.content.clone(),
                    words: n.text.clone(),
                })
                .collect()
        };

        // A fork's source, keyed apart so its rows carry no fork action.
        borrowed_rows(&self.prefix, PREFIX_KEY, &mut rows);
        if !self.prefix.is_empty() {
            rows.push(Row::System {
                key: format!("{PREFIX_KEY}divider"),
                text: "Forked here".into(),
            });
        }

        let mut notes = self.notes.iter().peekable();
        let mut merges = self.merges.iter().peekable();
        let merge_rows = |merge: &MergedFork, rows: &mut Vec<Row>| {
            let key = format!("{PREFIX_KEY}merge:{}:", merge.at);
            rows.push(Row::System {
                key: format!("{key}head"),
                text: format!("Merged back from {}", merge.title),
            });
            borrowed_rows(&merge.messages, &key, rows);
            if merge.pending {
                rows.push(Row::System {
                    key: format!("{key}tail"),
                    text: "Goes to the model with your next message".into(),
                });
            }
        };
        for message in &self.durable {
            if !message.is_visible() {
                owed.extend(notice_rows(message));
                continue;
            }
            rows.append(&mut owed);
            owed = notice_rows(message);
            while let Some((_, note)) = notes.next_if(|(anchor, _)| *anchor < message.sequence) {
                rows.push(note.clone());
            }
            let sent = message.created_at.map(|t| t.timestamp_millis());
            while let Some(merge) = merges.next_if(|m| sent.is_some_and(|at| m.at < at)) {
                merge_rows(merge, &mut rows);
            }
            let text = shown_text(message);
            // A context turn the engine sent on its own: nothing to read.
            if message.kind == MessageKind::User
                && text.is_empty()
                && message.attachments.is_empty()
            {
                continue;
            }
            let key = self.key_of_message(&message.message_id);
            if message.kind == MessageKind::User
                && let Some((title, body)) = delegated_result(&text)
            {
                rows.push(Row::Delegated { key, title, body });
                continue;
            }
            match message.kind {
                MessageKind::User => {
                    let uploads = message
                        .attachments
                        .iter()
                        .map(|a| AttachmentChip {
                            message_id: a.storage_key.as_ref().map(|_| message.message_id.clone()),
                            id: a.id.clone(),
                            filename: a.filename.clone().unwrap_or_else(|| "attachment".into()),
                            size: a.size_bytes,
                            local_path: None,
                        })
                        .collect();
                    let (text, attachments) = attached::split(&text, uploads);
                    // A run already working when this was sent: what it did
                    // before goes above the message, the rest below.
                    if let Some(sent) = sent {
                        for (run, keys) in &groups {
                            let Some(r) = run.as_deref() else { continue };
                            if emitted.contains(run) {
                                continue;
                            }
                            let before: Vec<&str> = keys
                                .iter()
                                .copied()
                                .filter(|k| !shown.contains(k) && self.live[*k].at < sent)
                                .collect();
                            if before.is_empty() {
                                continue;
                            }
                            let reply = durable_replies.get(r).copied().or(Some(""));
                            self.activity_rows(&before, reply, &mut rows);
                            shown.extend(before);
                        }
                    }
                    let queued = message.status == MessageStatus::Queued;
                    let rejected = message.status == MessageStatus::RejectedBusy;
                    let run = message.turn_run_id.as_deref();
                    // A run's later message of the person's is a steer; the
                    // native loop marks only unread ones (`queued`), so a
                    // read one is known by its run. Not one that the turn
                    // never took (`rejected_busy`).
                    let later = !rejected && run.is_some_and(|r| !started_runs.insert(r));
                    let steer = match (&message.steer, later) {
                        (Some(ann), _) => Some(ann.waiting_to_be_read),
                        (None, true) => Some(queued),
                        (None, false) => None,
                    }
                    .map(|waiting| SteerMark {
                        waiting,
                        continues: false,
                        continued_ms: run
                            .filter(|r| active != Some(*r))
                            .and_then(|r| {
                                segment_end
                                    .get(message.message_id.as_str())
                                    .or_else(|| run_last_at.get(r))
                            })
                            .zip(sent)
                            .map(|(end, at)| end - at)
                            .filter(|ms| *ms > 0),
                    });
                    let words = text.clone();
                    rows.push(Row::User {
                        key: key.clone(),
                        text,
                        // Queued until the model reads it.
                        pending: queued,
                        attachments,
                        steer,
                    });
                    if rejected {
                        rows.push(Row::SteerNotice {
                            key: format!("{key}:not-delivered"),
                            title: "Not delivered".into(),
                            text: "This message wasn't delivered to the running task. Send it again if you still need it.".into(),
                            words,
                        });
                    }
                    if let Some(run) = message.turn_run_id.as_deref()
                        && quiet_run(run)
                        && emitted.insert(Some(run.to_string()))
                    {
                        self.notice_row(run, &mut rows);
                    }
                }
                MessageKind::Assistant => {
                    let run = message.turn_run_id.clone();
                    let group = groups.iter().find(|(r, _)| run.is_some() && *r == run);
                    // The live text this stored message streamed as, if held.
                    let copy = |k: &&str| {
                        matches!(&self.live[*k].item,
                            Live::Text { body, .. } if body.trim() == text.trim())
                    };
                    let mut own: Option<&str> = None;
                    if !emitted.contains(&run)
                        && let Some((_, keys)) = group
                    {
                        let rest: Vec<&str> = keys
                            .iter()
                            .copied()
                            .filter(|k| !shown.contains(k))
                            .collect();
                        // A run that replies more than once (a steer, or a
                        // stored narration) is drawn in reply order: only
                        // what came before this reply goes above it, the
                        // rest waits for the next reply or the run's tail.
                        // Dumping all of it here drew newer text above older
                        // text, then reflowed when the timeline caught up.
                        // A CLI's live text is cumulative: an entry that
                        // already runs past this reply is still growing, so
                        // it stays below as the bubble, not above the reply.
                        let grown = |k: &&str| {
                            matches!(&self.live[*k].item, Live::Text { body, .. }
                                if body.trim().len() > text.trim().len()
                                    && body.trim().starts_with(text.trim()))
                        };
                        let take: Vec<&str> = match rest.iter().position(|k| copy(k) || grown(k)) {
                            Some(i) if copy(&rest[i]) => {
                                own = Some(rest[i]);
                                rest[..=i].to_vec()
                            }
                            Some(i) => rest[..i].to_vec(),
                            None => match sent {
                                Some(sent) => rest
                                    .iter()
                                    .copied()
                                    .filter(|k| self.live[*k].at <= sent)
                                    .collect(),
                                None => rest.clone(),
                            },
                        };
                        self.activity_rows(&take, Some(&text), &mut rows);
                        if take.len() == rest.len() {
                            emitted.insert(run.clone());
                        } else {
                            split.insert(run.clone());
                        }
                        shown.extend(take);
                    }
                    // The bubble the reply streamed into keeps its key, so
                    // the stored copy replaces it in place.
                    // Each live copy lends its key once: a run that says
                    // the same words twice must not draw two rows one key.
                    let own = own.or_else(|| {
                        group.and_then(|(_, keys)| {
                            keys.iter()
                                .copied()
                                .find(|k| copy(k) && !reply_keys.contains(k))
                        })
                    });
                    let key = match own {
                        Some(k) => {
                            reply_keys.insert(k);
                            k.to_string()
                        }
                        None => key,
                    };
                    // Text the run moved on from is commentary: subdued,
                    // folded with the work, never the answer.
                    rows.push(if message.commentary {
                        Row::Narration { key, text }
                    } else {
                        Row::Assistant {
                            key,
                            text,
                            streaming: false,
                        }
                    });
                    // The run's notice goes once, under its last row.
                    if let Some(r) = run.as_deref()
                        && last_reply.get(r) == Some(&message.message_id.as_str())
                        && (group.is_none() || emitted.contains(&run))
                    {
                        self.notice_row(r, &mut rows);
                    }
                }
                _ => rows.push(Row::System { key, text }),
            }
        }

        rows.append(&mut owed);
        for merge in merges {
            merge_rows(merge, &mut rows);
        }
        rows.extend(notes.map(|(_, note)| note.clone()));
        for p in &self.pending_user {
            rows.push(Row::User {
                key: pending_key(p.id),
                text: p.text.clone(),
                pending: !p.acked,
                attachments: p.chips.clone(),
                steer: p.steer.map(|waiting| SteerMark {
                    waiting,
                    ..SteerMark::default()
                }),
            });
        }

        for (run, keys) in &groups {
            if emitted.contains(run) {
                continue;
            }
            // A split run's stored replies were matched in place; what is
            // left is newer, so its last text is the live bubble.
            let durable = run
                .as_deref()
                .and_then(|r| durable_replies.get(r).copied())
                .filter(|_| !split.contains(run));
            // A split run's stored replies are drawn already: a leftover
            // live copy of one (a final frame, a re-sent item) is not news.
            let stored: HashSet<&str> = if split.contains(run) {
                self.durable
                    .iter()
                    .filter(|m| m.kind == MessageKind::Assistant && m.turn_run_id == *run)
                    .filter_map(|m| m.content.as_deref().map(str::trim))
                    .collect()
            } else {
                HashSet::new()
            };
            let rest: Vec<&str> = keys
                .iter()
                .copied()
                .filter(|k| !shown.contains(k))
                .filter(|k| {
                    !matches!(&self.live[*k].item,
                        Live::Text { body, .. } if stored.contains(body.trim()))
                })
                .collect();
            self.activity_rows(&rest, durable, &mut rows);
            if let Some(run) = run {
                self.notice_row(run, &mut rows);
            }
        }
        mark_steer_runs(&mut rows);
        rows
    }

    /// Activity for one run. Unless the durable reply already exists, the
    /// trailing unflagged text becomes the (possibly still streaming) bubble.
    /// The gateway sends the reply several times (streamed draft, finalized
    /// item, final-reply frame); live copies of the reply text are dropped.
    fn activity_rows(&self, keys: &[&str], durable_reply: Option<&str>, rows: &mut Vec<Row>) {
        let bubble = if durable_reply.is_some() {
            None
        } else {
            keys.iter()
                .rev()
                .find(|k| {
                    matches!(
                        self.live[**k].item,
                        Live::Text {
                            narration: false,
                            ..
                        }
                    )
                })
                .copied()
        };
        let reply = durable_reply
            .or_else(|| {
                bubble.and_then(|k| match &self.live[k].item {
                    Live::Text { body, .. } => Some(body.as_str()),
                    _ => None,
                })
            })
            .map(str::trim);
        // What the model said to the person last before each call: the
        // reason it shows. A thought already has its own row, so it is never
        // borrowed as a reason (that drew the same paragraph twice).
        let mut said: Option<&str> = None;
        for key in keys {
            let entry = &self.live[*key];
            if let Live::Text { body, .. } = &entry.item {
                said = Some(body.as_str());
            }
            let said_reason = || said.and_then(reason::from_said);
            let row = match &entry.item {
                // The bubble stays where it arrived: tools that follow it
                // draw below, as they happened, and it never jumps.
                Live::Text { body, .. } if Some(*key) == bubble => {
                    let streaming = entry
                        .run
                        .as_deref()
                        .is_none_or(|r| !self.settled.contains(r));
                    Row::Assistant {
                        key: key.to_string(),
                        text: body.clone(),
                        streaming,
                    }
                }
                // A final reply frame duplicates the durable reply once it lands.
                Live::Text { .. } if key.starts_with("final:") => continue,
                // Any live copy of the reply, flagged as narration or not.
                // A CLI run's stored reply joins its segments.
                Live::Text { body, .. }
                    if Some(body.trim()) == reply
                        || self.is_durable_reply(entry.run.as_deref(), body)
                        || (key.contains(":cli:")
                            && durable_reply.is_some_and(|r| r.contains(body.trim()))) =>
                {
                    continue;
                }
                Live::Text { body, .. } => Row::Narration {
                    key: key.to_string(),
                    text: body.clone(),
                },
                Live::Thinking(text) => Row::Thinking {
                    key: key.to_string(),
                    text: text.clone(),
                },
                Live::Tool {
                    title,
                    detail,
                    status,
                } => Row::Tool {
                    key: key.to_string(),
                    title: title.clone(),
                    detail: detail.clone(),
                    status: *status,
                    reason: said_reason(),
                },
                Live::Skills(names) => Row::Skills {
                    key: key.to_string(),
                    names: names.clone(),
                },
                Live::WorkSummary(text) => Row::WorkSummary {
                    key: key.to_string(),
                    text: text.clone(),
                },
                Live::Plan(plan) => Row::Plan {
                    key: key.to_string(),
                    plan: plan.clone(),
                },
                Live::Question {
                    gate_ref,
                    form,
                    closed,
                } => Row::Question {
                    key: key.to_string(),
                    run_id: entry.run.clone().unwrap_or_default(),
                    gate_ref: gate_ref.clone(),
                    form: form.clone(),
                    state: self.question_state(gate_ref, *closed, entry),
                },
                Live::Gate {
                    gate_ref,
                    kind,
                    headline,
                    body,
                    allow_always,
                    reason,
                } => Row::Gate {
                    key: key.to_string(),
                    run_id: entry.run.clone().unwrap_or_default(),
                    gate_ref: gate_ref.clone(),
                    kind: *kind,
                    headline: headline.clone(),
                    body: body.clone(),
                    allow_always: *allow_always,
                    state: self.gate_state(gate_ref, entry),
                    // The CLI bridge already tied each call to what was said
                    // before it; its `None` means "nothing new", not "guess".
                    reason: if gate_ref.starts_with("gate:cli-") {
                        reason.clone()
                    } else {
                        reason.clone().or_else(said_reason)
                    },
                },
            };
            rows.push(row);
        }
    }

    /// A live copy of a durable reply of `run`. A steered run replies more
    /// than once, so the live text may match any one reply, or all of them
    /// joined (a CLI's live text is cumulative over the run).
    fn is_durable_reply(&self, run: Option<&str>, body: &str) -> bool {
        let Some(run) = run else {
            return false;
        };
        let replies: Vec<&str> = self
            .durable
            .iter()
            .filter(|m| m.kind == MessageKind::Assistant && m.turn_run_id.as_deref() == Some(run))
            .filter_map(|m| m.content.as_deref().map(str::trim))
            .collect();
        let body = body.trim();
        replies.len() > 1 && (replies.contains(&body) || replies.join("\n\n") == body)
    }

    /// Closed by its own close frame, or by the run's end. A CLI run asks
    /// several questions at once (`gate:question-*`), so only that form's
    /// close counts there; a native run parks on one gate at a time, so a
    /// later non-blocked run status means its gate moved on.
    fn question_state(&self, gate_ref: &str, closed: bool, entry: &LiveEntry) -> QuestionState {
        let sent = self.question_sends.get(gate_ref).copied();
        let superseded = closed
            || (!gate_ref.starts_with(QUESTION_GATE_REF_PREFIX)
                && entry
                    .run
                    .as_deref()
                    .and_then(|r| self.runs.get(r))
                    .is_some_and(|s| s.tick > entry.tick && !s.status.starts_with("blocked")));
        let settled = entry
            .run
            .as_deref()
            .is_some_and(|r| self.settled.contains(r));
        match sent {
            _ if superseded || settled => QuestionState::Closed(sent),
            Some(answered) => QuestionState::Sending(answered),
            None => QuestionState::Open,
        }
    }

    fn gate_state(&self, gate_ref: &str, entry: &LiveEntry) -> GateState {
        let decision = self.decisions.get(gate_ref).copied();
        let superseded = entry
            .run
            .as_deref()
            .and_then(|r| self.runs.get(r))
            .is_some_and(|s| s.tick > entry.tick && !s.status.starts_with("blocked"));
        if superseded
            || entry
                .run
                .as_deref()
                .is_some_and(|r| self.settled.contains(r))
        {
            GateState::Closed(decision)
        } else if let Some(decision) = decision {
            GateState::Resolving(decision)
        } else {
            GateState::Open
        }
    }

    fn notice_row(&self, run: &str, rows: &mut Vec<Row>) {
        let Some(status) = self.runs.get(run) else {
            return;
        };
        let (text, failed) = match status.status.as_str() {
            "failed" => (
                status
                    .failure
                    .clone()
                    .map(|f| format!("Run failed: {f}"))
                    .unwrap_or_else(|| "Run failed".into()),
                true,
            ),
            "cancelled" => ("Stopped".to_string(), false),
            "recovery_required" => ("Run interrupted; it needs recovery".to_string(), true),
            _ => return,
        };
        rows.push(Row::RunNotice {
            key: format!("notice:{run}"),
            run_id: run.to_string(),
            text,
            failed,
            retryable: status.retryable,
        });
    }
}

fn tool_status(status: CapabilityActivityStatusView) -> ToolStatus {
    match status {
        CapabilityActivityStatusView::Started | CapabilityActivityStatusView::Running => {
            ToolStatus::Running
        }
        CapabilityActivityStatusView::Completed => ToolStatus::Completed,
        CapabilityActivityStatusView::Failed | CapabilityActivityStatusView::Killed => {
            ToolStatus::Failed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::StreamFrame;

    const RUN: &str = "6f1c1c0e-0000-4000-8000-000000000001";

    fn frame(json: serde_json::Value) -> StreamEvent {
        serde_json::from_value::<StreamFrame>(json)
            .expect("frame decodes")
            .event
    }

    fn text_update(body: &str, narration: bool, call: u32) -> StreamEvent {
        frame(serde_json::json!({
            "cursor": "c1", "type": "projection_update",
            "state": { "thread_id": "t1", "items": [
                { "text": { "id": format!("text:{RUN}:{call}"), "run_id": RUN,
                            "body": body, "narration": narration } }
            ]}
        }))
    }

    /// A long-open view keeps the newest output tails only, each bounded,
    /// and handing them to the renderer copies nothing.
    #[test]
    fn output_tails_are_bounded_and_shared_not_copied() {
        let mut t = Transcript::default();
        let output = |n: usize, body: String| {
            frame(serde_json::json!({
                "cursor": "c", "type": "projection_update",
                "state": { "thread_id": "t1", "items": [
                    { "cli_detail": { "id": format!("output:{n}"), "run_id": RUN,
                        "kind": "tool_output",
                        "invocation_id": format!("6f1c1c0e-0000-4000-8000-{n:012}"),
                        "body": body } }
                ]}
            }))
        };
        for n in 0..(CLI_OUTPUTS_MAX + 6) {
            t.apply(&output(n, format!("line {n}\n")));
        }
        let first = t.tool_outputs();
        assert_eq!(first.len(), CLI_OUTPUTS_MAX);
        assert!(!first.contains_key("tool:6f1c1c0e-0000-4000-8000-000000000000"));
        assert!(first.contains_key(&format!(
            "tool:6f1c1c0e-0000-4000-8000-{:012}",
            CLI_OUTPUTS_MAX + 5
        )));
        assert!(std::sync::Arc::ptr_eq(&first, &t.tool_outputs()));
        // One oversized tail keeps its newest bytes.
        t.apply(&output(
            1000,
            format!("{}tail-end", "x".repeat(3 * CLI_OUTPUT_MAX_BYTES)),
        ));
        let outputs = t.tool_outputs();
        let big = outputs
            .get("tool:6f1c1c0e-0000-4000-8000-000000001000")
            .unwrap();
        assert!(big.body.len() <= CLI_OUTPUT_MAX_BYTES && big.truncated);
        assert!(big.body.ends_with("tail-end"));
        // The renderer's old handle is untouched by the new frame.
        assert_eq!(first.len(), CLI_OUTPUTS_MAX);
        assert!(!first.contains_key("tool:6f1c1c0e-0000-4000-8000-000000001000"));
    }

    #[test]
    fn cli_detail_frames_feed_tool_output_turn_diff_and_review() {
        let inv = "6f1c1c0e-0000-4000-8000-0000000000aa";
        let mut t = Transcript::default();
        let applied = t.apply(&frame(serde_json::json!({
            "cursor": "c9", "type": "projection_update",
            "state": { "thread_id": "t1", "items": [
                { "capability_activity": { "invocation_id": inv, "turn_run_id": RUN,
                    "thread_id": "t1", "capability_id": "cli.shell", "status": "running",
                    "input_summary": "cargo test", "updated_at": "2026-11-04T00:00:00Z" } },
                { "cli_detail": { "id": format!("output:{inv}"), "run_id": RUN,
                    "kind": "tool_output", "invocation_id": inv, "body": "running 3 tests\n" } },
                { "cli_detail": { "id": format!("diff:{RUN}"), "run_id": RUN,
                    "kind": "turn_diff", "body": "diff --git a/x b/x\n", "truncated": true } },
            ]}
        })));
        assert!(applied.changed);
        let row = t
            .rows()
            .into_iter()
            .find(|r| matches!(r, Row::Tool { .. }))
            .expect("tool row");
        assert_eq!(
            t.tool_outputs().get(row.key()).map(|o| o.body.as_str()),
            Some("running 3 tests\n")
        );
        let diff = t.cli_turn_diff().unwrap();
        assert!(diff.truncated && diff.run.as_deref() == Some(RUN));
        assert!(t.cli_review().is_none());
        t.apply(&frame(serde_json::json!({
            "cursor": "c10", "type": "projection_update",
            "state": { "thread_id": "t1", "items": [
                { "cli_detail": { "id": format!("review:{RUN}"), "run_id": RUN,
                    "kind": "review", "body": "No issues found." } }
            ]}
        })));
        assert_eq!(t.cli_review().unwrap().body, "No issues found.");
        // The revision moves with the diff or review only, not with a
        // repeat of the same body.
        let rev = t.cli_report_rev();
        let again = frame(serde_json::json!({
            "cursor": "c11", "type": "projection_update",
            "state": { "thread_id": "t1", "items": [
                { "cli_detail": { "id": format!("review:{RUN}"), "run_id": RUN,
                    "kind": "review", "body": "No issues found." } }
            ]}
        }));
        t.apply(&again);
        assert_eq!(t.cli_report_rev(), rev);
        t.apply(&frame(serde_json::json!({
            "cursor": "c12", "type": "projection_update",
            "state": { "thread_id": "t1", "items": [
                { "cli_detail": { "id": format!("diff:{RUN}"), "run_id": RUN,
                    "kind": "turn_diff", "body": "diff --git a/y b/y\n" } }
            ]}
        })));
        assert!(t.cli_report_rev() > rev);
    }

    fn status(s: &str) -> StreamEvent {
        frame(serde_json::json!({
            "cursor": "c2", "type": "projection_update",
            "state": { "thread_id": "t1", "items": [
                { "run_status": { "run_id": RUN, "status": s } }
            ]}
        }))
    }

    fn timeline(reply: Option<&str>) -> TimelinePage {
        let mut messages = vec![serde_json::json!({
            "message_id": "m1", "sequence": 1, "kind": "user", "status": "accepted",
            "turn_run_id": RUN, "content": "hi"
        })];
        if let Some(reply) = reply {
            messages.push(serde_json::json!({
                "message_id": "m2", "sequence": 2, "kind": "assistant", "status": "finalized",
                "turn_run_id": RUN, "content": reply
            }));
        }
        serde_json::from_value(serde_json::json!({
            "thread": { "thread_id": "t1" }, "messages": messages
        }))
        .expect("timeline decodes")
    }

    /// A CLI run whose stored timeline caught up mid-run (a reconnect, a
    /// compaction): each text block is its own stored row, as its live
    /// segment streamed it. Every line draws once, in arrival order, with
    /// the tools between them. (A joined stored reply used to stack every
    /// line over the live segments, so each drew twice.)
    #[test]
    fn a_cli_runs_stored_steps_and_live_segments_draw_each_line_once() {
        let seg = |n: u32, body: &str| {
            items(serde_json::json!([{ "text": {
                "id": format!("text:{RUN}:cli:t1:{n}"), "run_id": RUN,
                "body": body, "narration": false } }]))
        };
        let tool = |n: u32, input: &str, status: &str| {
            items(serde_json::json!([{ "capability_activity": {
                "invocation_id": format!("6f1c1c0e-0000-4000-8000-{n:012}"),
                "turn_run_id": RUN, "thread_id": "t1", "capability_id": "cli.bash",
                "status": status, "input_summary": input,
                "updated_at": "2026-11-22T21:00:00Z" } }]))
        };
        let mut t = Transcript::default();
        t.apply(&status("running"));
        t.apply(&tool(1, "ls", "completed"));
        t.apply(&seg(0, "First, the decision record."));
        t.apply(&tool(2, "cat roadmap.md", "completed"));
        t.apply(&seg(1, "Now the engine contract."));
        t.apply(&tool(3, "bun test", "running"));
        t.set_timeline(page(serde_json::json!([
            { "message_id": "m1", "sequence": 1, "kind": "user", "status": "accepted",
              "turn_run_id": RUN, "content": "build it" },
            { "message_id": "m2", "sequence": 2, "kind": "assistant", "status": "finalized",
              "turn_run_id": RUN, "content": "First, the decision record." },
            { "message_id": "m3", "sequence": 3, "kind": "assistant", "status": "finalized",
              "turn_run_id": RUN, "content": "Now the engine contract." },
        ])));
        let shape: Vec<String> = t
            .rows()
            .iter()
            .map(|r| match r {
                Row::User { text, .. } => format!("user:{text}"),
                Row::Assistant { text, .. } | Row::Narration { text, .. } => {
                    format!("text:{text}")
                }
                Row::Tool { detail, .. } => format!("tool:{}", detail.clone().unwrap_or_default()),
                other => format!("other:{}", other.key()),
            })
            .collect();
        assert_eq!(
            shape,
            [
                "user:build it",
                "tool:ls",
                "text:First, the decision record.",
                "tool:cat roadmap.md",
                "text:Now the engine contract.",
                "tool:bun test",
            ]
        );
    }

    /// A stored row the gateway flags `commentary` (text the run moved on
    /// from) is narration, never an answer bubble: it draws subdued and
    /// folds with the work. The run's last row stays the answer.
    #[test]
    fn stored_commentary_rows_are_narration_and_the_answer_stays_a_reply() {
        let mut t = Transcript::default();
        t.set_timeline(page(serde_json::json!([
            { "message_id": "m1", "sequence": 1, "kind": "user", "status": "accepted",
              "turn_run_id": RUN, "content": "spin up 2" },
            { "message_id": "m2", "sequence": 2, "kind": "assistant", "status": "finalized",
              "turn_run_id": RUN, "content": "I'll launch two subagents.", "commentary": true },
            { "message_id": "m3", "sequence": 3, "kind": "assistant", "status": "finalized",
              "turn_run_id": RUN, "content": "Both are running." },
        ])));
        let rows = t.rows();
        assert!(
            matches!(&rows[1], Row::Narration { text, .. } if text == "I'll launch two subagents."),
            "{rows:?}"
        );
        assert!(matches!(&rows[2], Row::Assistant { text, .. } if text == "Both are running."));
    }

    /// The running turn's prompt is shown once a message of the person's
    /// belongs to it; a run started elsewhere, its prompt not loaded yet,
    /// is not taken for the newest prompt's.
    #[test]
    fn a_running_prompt_is_shown_once_it_belongs_to_the_run() {
        let mut t = Transcript::default();
        t.set_timeline(page(serde_json::json!([
            { "message_id": "m1", "sequence": 1, "kind": "user", "status": "accepted",
              "turn_run_id": "6f1c1c0e-0000-4000-8000-0000000000ff", "content": "earlier" },
        ])));
        assert!(t.shows_running_prompt(), "nothing runs");
        t.apply(&status("running"));
        assert!(!t.shows_running_prompt(), "the run's prompt is not loaded");
        t.set_timeline(page(serde_json::json!([
            { "message_id": "m1", "sequence": 1, "kind": "user", "status": "accepted",
              "turn_run_id": "6f1c1c0e-0000-4000-8000-0000000000ff", "content": "earlier" },
            { "message_id": "m2", "sequence": 2, "kind": "user", "status": "accepted",
              "turn_run_id": RUN, "content": "now" },
        ])));
        assert!(t.shows_running_prompt());
    }

    /// A settled turn is timed from its prompt to its run's last row; the
    /// running turn has no length yet.
    #[test]
    fn turn_spans_time_each_settled_prompt() {
        let mut t = Transcript::default();
        t.set_timeline(page(serde_json::json!([
            { "message_id": "m1", "sequence": 1, "kind": "user", "status": "accepted",
              "turn_run_id": RUN, "content": "go", "created_at": "2026-11-22T21:00:00Z" },
            { "message_id": "m2", "sequence": 2, "kind": "assistant", "status": "finalized",
              "turn_run_id": RUN, "content": "Working on it.", "created_at": "2026-11-22T21:10:00Z" },
            { "message_id": "m3", "sequence": 3, "kind": "user", "status": "accepted",
              "turn_run_id": RUN, "content": "a steer", "created_at": "2026-11-22T21:20:00Z" },
            { "message_id": "m4", "sequence": 4, "kind": "assistant", "status": "finalized",
              "turn_run_id": RUN, "content": "Done.", "created_at": "2026-11-22T21:31:12Z" },
        ])));
        let spans = t.turn_spans();
        assert_eq!(
            spans.get("msg:m1"),
            Some(&1_872_000),
            "31m 12s, steers included"
        );
        assert!(!spans.contains_key("msg:m3"), "a steer is not a turn");
        // A live frame seen late does not stretch a turn with stored times.
        t.apply(&items(serde_json::json!([{ "text": {
            "id": format!("text:{RUN}:cli:t1:9"), "run_id": RUN,
            "body": "Done.", "narration": false } }])));
        t.apply(&status("completed"));
        assert_eq!(t.turn_spans().get("msg:m1"), Some(&1_872_000));
        // While a run is live, its turn has no length yet.
        let mut live = Transcript::default();
        live.set_timeline(page(serde_json::json!([
            { "message_id": "m1", "sequence": 1, "kind": "user", "status": "accepted",
              "turn_run_id": RUN, "content": "go", "created_at": "2026-11-22T21:00:00Z" },
            { "message_id": "m2", "sequence": 2, "kind": "assistant", "status": "finalized",
              "turn_run_id": RUN, "content": "Working on it.", "created_at": "2026-11-22T21:10:00Z" },
        ])));
        live.apply(&status("running"));
        assert!(live.turn_spans().is_empty());
    }

    fn items(items: serde_json::Value) -> StreamEvent {
        frame(serde_json::json!({
            "cursor": "c3", "type": "projection_update",
            "state": { "thread_id": "t1", "items": items }
        }))
    }

    fn plan_item(id: &str, at: &str, statuses: &[&str]) -> serde_json::Value {
        let entries: Vec<_> = statuses
            .iter()
            .enumerate()
            .map(|(i, s)| serde_json::json!({"content": format!("step {i}"), "status": s}))
            .collect();
        serde_json::json!({"plan": {"id": id, "run_id": RUN,
            "plan": {"entries": entries, "updated_at": at}}})
    }

    #[test]
    fn plan_updates_draw_a_line_each_and_the_newest_is_the_plan() {
        let mut t = Transcript::default();
        t.apply(&status("running"));
        t.apply(&items(serde_json::json!([plan_item(
            "plan:1",
            "2026-11-03T10:00:00Z",
            &["in_progress", "pending"]
        )])));
        t.apply(&items(serde_json::json!([plan_item(
            "plan:2",
            "2026-11-03T10:05:00Z",
            &["completed", "in_progress"]
        )])));
        let plan = t.plan().expect("a plan");
        assert_eq!(plan.progress(), (1, 2));
        assert_eq!(plan.current().map(|e| e.content.as_str()), Some("step 1"));
        let lines = t
            .rows()
            .iter()
            .filter(|r| matches!(r, Row::Plan { .. }))
            .count();
        assert_eq!(lines, 2, "one compact line per update");
    }

    #[test]
    fn a_reopened_native_thread_reads_its_plan_from_the_stored_preview() {
        let plan = serde_json::json!({"entries": [
            {"content": "Look", "status": "completed"},
            {"content": "Fix", "status": "in_progress"}
        ], "updated_at": "2026-11-03T10:00:00Z"});
        let preview = serde_json::json!({
            "invocation_id": "i9", "capability_id": "builtin.plan_update",
            "status": "completed", "title": "plan_update",
            "output_kind": "agent_plan", "output_preview": plan.to_string()
        })
        .to_string();
        let page = serde_json::json!({ "thread": { "thread_id": "t1" }, "messages": [
            { "message_id": "m1", "sequence": 1, "kind": "user", "status": "accepted",
              "turn_run_id": RUN, "content": "fix it" },
            { "message_id": "p1", "sequence": 2, "kind": "capability_display_preview",
              "status": "finalized", "turn_run_id": RUN, "content": preview },
        ]});
        let mut t = Transcript::default();
        t.set_timeline(serde_json::from_value(page).unwrap());
        assert_eq!(t.plan().map(AgentPlan::progress), Some((1, 2)));
        let rows = t.rows();
        assert!(rows.iter().any(|r| matches!(r, Row::Plan { .. })));
        assert!(
            !rows.iter().any(|r| matches!(r, Row::Tool { .. })),
            "no raw tool row for a plan update"
        );
    }

    fn question_item(gate: &str, closed: bool) -> serde_json::Value {
        serde_json::json!({"question": {
            "run_id": RUN, "gate_ref": gate, "closed": closed,
            "form": {"fields": [{"id": "q", "label": "Go?", "required": true,
                "kind": {"type": "choice", "options": [{"label": "Yes"}]}}]}
        }})
    }

    fn question_states(t: &Transcript) -> Vec<(String, QuestionState)> {
        t.rows()
            .iter()
            .filter_map(|r| match r {
                Row::Question {
                    gate_ref, state, ..
                } => Some((gate_ref.clone(), *state)),
                _ => None,
            })
            .collect()
    }

    // Review finding 3: closing one form closes that form only.
    #[test]
    fn closing_one_question_leaves_the_others_and_an_approval_open() {
        let mut t = Transcript::default();
        t.apply(&status("running"));
        t.apply(&items(serde_json::json!([
            question_item("gate:question-a", false),
            question_item("gate:question-b", false),
            {"gate": {"run_id": RUN, "gate_kind": "approval", "gate_ref": "gate:cli-1",
                      "headline": "Allow Bash?"}}
        ])));
        t.mark_question_sending("gate:question-a", true);
        t.apply(&items(serde_json::json!([question_item(
            "gate:question-a",
            true
        )])));
        assert_eq!(
            question_states(&t),
            vec![
                (
                    "gate:question-a".to_string(),
                    QuestionState::Closed(Some(true))
                ),
                ("gate:question-b".to_string(), QuestionState::Open),
            ]
        );
        let gate = t.rows().iter().find_map(|r| match r {
            Row::Gate { state, .. } => Some(*state),
            _ => None,
        });
        assert_eq!(gate, Some(GateState::Open), "the approval still asks");
    }

    #[test]
    fn a_question_is_a_form_until_answered_and_the_run_moves_on() {
        let mut t = Transcript::default();
        t.apply(&status("running"));
        t.apply(&items(serde_json::json!([{"question": {
            "run_id": RUN, "gate_ref": "gate:question-1",
            "form": {"fields": [{"id": "q", "label": "Go?", "required": true,
                "kind": {"type": "choice", "options": [{"label": "Yes"}]}}]}
        }}])));
        let state = |t: &Transcript| {
            t.rows().iter().find_map(|r| match r {
                Row::Question { state, form, .. } => {
                    assert_eq!(form.fields[0].label, "Go?");
                    Some(*state)
                }
                _ => None,
            })
        };
        assert_eq!(state(&t), Some(QuestionState::Open));
        t.mark_question_sending("gate:question-1", true);
        assert_eq!(state(&t), Some(QuestionState::Sending(true)));
        t.apply(&status("running"));
        assert_eq!(
            state(&t),
            Some(QuestionState::Sending(true)),
            "a run status names no form; only its own close does"
        );
        t.apply(&items(serde_json::json!([question_item(
            "gate:question-1",
            true
        )])));
        assert_eq!(state(&t), Some(QuestionState::Closed(Some(true))));
        t.apply(&status("completed"));
        assert_eq!(state(&t), Some(QuestionState::Closed(Some(true))));
    }

    #[test]
    fn reopened_chat_shows_stored_tools_around_a_steer_and_result_cards() {
        let preview = |id: &str, title: &str| {
            serde_json::json!({ "invocation_id": id, "capability_id": "builtin.read_file",
                                "status": "completed", "title": title })
            .to_string()
        };
        let at = |s: u32| format!("2026-10-29T18:{s:02}:00Z");
        let page = serde_json::json!({ "thread": { "thread_id": "t1" }, "messages": [
            { "message_id": "m1", "sequence": 1, "kind": "user", "status": "accepted",
              "turn_run_id": RUN, "content": "review it", "created_at": at(10) },
            { "message_id": "p1", "sequence": 2, "kind": "capability_display_preview",
              "status": "finalized", "turn_run_id": RUN, "content": preview("i1", "Read a"),
              "created_at": at(11) },
            { "message_id": "p2", "sequence": 3, "kind": "capability_display_preview",
              "status": "finalized", "turn_run_id": RUN, "content": preview("i2", "Read b"),
              "created_at": at(13) },
            // Sent at :12, renumbered when the model read it after "Read b".
            { "message_id": "m2", "sequence": 4, "kind": "user", "status": "submitted",
              "turn_run_id": RUN, "content": "give feedback now", "created_at": at(12) },
            { "message_id": "m3", "sequence": 5, "kind": "assistant", "status": "finalized",
              "turn_run_id": RUN, "content": "Here it is.", "created_at": at(14) },
            { "message_id": "m4", "sequence": 6, "kind": "user", "status": "submitted",
              "content": "Delegated work finished. Use these results to continue.\n\n[T3 delegated task completed] \"Review page\" — task t\n\nLooks good.",
              "created_at": at(15) },
        ]});
        let mut t = Transcript::default();
        t.set_timeline(serde_json::from_value(page).expect("timeline decodes"));
        let labels: Vec<String> = t
            .rows()
            .iter()
            .map(|r| match r {
                Row::User { text, .. } => format!("user:{text}"),
                Row::Tool { title, .. } => format!("tool:{title}"),
                Row::Assistant { text, .. } => format!("reply:{text}"),
                Row::Delegated { title, .. } => format!("result:{title}"),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            labels,
            [
                "user:review it",
                "tool:Read a",
                "user:give feedback now",
                "tool:Read b",
                "reply:Here it is.",
                "result:Delegated work finished: Review page",
            ]
        );
    }

    /// A steered run stores its earlier replies while it still streams.
    /// Newer live text must stay below them (and below the steer), not jump
    /// above the stored replies and reflow once the timeline catches up.
    #[test]
    fn a_steered_run_keeps_reply_order_while_it_streams() {
        let mut t = Transcript::default();
        let ms = |ts: i64| chrono::DateTime::from_timestamp_millis(ts).map(|d| d.to_rfc3339());
        let now = chrono::Utc::now().timestamp_millis();
        t.apply(&text_update("Starting a long run.", false, 1));
        t.apply(&tool("running"));
        t.apply(&text_update("Starting a long run.", true, 1));
        t.apply(&text_update("The profile folder holds one file.", false, 2));
        t.apply(&text_update("The profile folder holds one file.", true, 2));
        let page = |extra: Vec<serde_json::Value>| {
            let mut messages = vec![
                serde_json::json!({ "message_id": "m1", "sequence": 1, "kind": "user",
                    "status": "accepted", "turn_run_id": RUN, "content": "go",
                    "created_at": ms(now - 60_000) }),
                serde_json::json!({ "message_id": "a1", "sequence": 2, "kind": "assistant",
                    "status": "finalized", "turn_run_id": RUN,
                    "content": "Starting a long run.", "created_at": ms(now + 1) }),
                serde_json::json!({ "message_id": "a2", "sequence": 3, "kind": "assistant",
                    "status": "finalized", "turn_run_id": RUN,
                    "content": "The profile folder holds one file.", "created_at": ms(now + 2) }),
                serde_json::json!({ "message_id": "m2", "sequence": 4, "kind": "user",
                    "status": "submitted", "turn_run_id": RUN, "content": "say bananas",
                    "created_at": ms(now + 3) }),
            ];
            messages.extend(extra);
            serde_json::from_value::<TimelinePage>(serde_json::json!({
                "thread": { "thread_id": "t1" }, "messages": messages
            }))
            .expect("timeline decodes")
        };
        t.set_timeline(page(vec![]));
        // After the steer: the model answers it while the run goes on.
        std::thread::sleep(std::time::Duration::from_millis(10));
        t.apply(&text_update("Bananas.", false, 3));
        let labels = |t: &Transcript| -> Vec<String> {
            t.rows()
                .iter()
                .map(|r| match r {
                    Row::User { text, .. } => format!("user:{text}"),
                    Row::Tool { .. } => "tool".into(),
                    Row::Assistant { text, .. } => format!("reply:{text}"),
                    Row::Narration { text, .. } => format!("narration:{text}"),
                    other => format!("{other:?}"),
                })
                .collect()
        };
        let streaming = [
            "user:go",
            "reply:Starting a long run.",
            "tool",
            "reply:The profile folder holds one file.",
            "user:say bananas",
            "reply:Bananas.",
        ];
        assert_eq!(labels(&t), streaming);
        let keys = |t: &Transcript| {
            t.rows()
                .iter()
                .map(|r| r.key().to_string())
                .collect::<Vec<_>>()
        };
        let before = keys(&t);
        // The stop lands, then the timeline stores the answer: nothing moves.
        t.apply(&status("cancelled"));
        let stopped = keys(&t);
        assert_eq!(&stopped[..before.len()], &before[..]);
        assert_eq!(
            stopped.iter().filter(|k| k.starts_with("notice:")).count(),
            1
        );
        t.set_timeline(page(vec![
            serde_json::json!({ "message_id": "a3", "sequence": 5,
            "kind": "assistant", "status": "finalized", "turn_run_id": RUN,
            "content": "Bananas.", "created_at": ms(chrono::Utc::now().timestamp_millis()) }),
        ]));
        assert_eq!(keys(&t), stopped);
    }

    /// A run that says the same words twice keeps one row per reply, each
    /// under its own key, and a final frame's copy of a stored reply that
    /// is already drawn never adds a third.
    #[test]
    fn repeated_replies_keep_their_own_keys_and_no_final_copy() {
        let mut t = Transcript::default();
        t.apply(&text_update("Done.", true, 1));
        t.apply(&tool("completed"));
        t.apply(&text_update("Done.", true, 2));
        t.apply(&frame(serde_json::json!({
            "cursor": "c9", "type": "final_reply",
            "reply": { "turn_run_id": RUN, "text": "Done.",
                       "generated_at": "2026-10-28T00:00:00Z" }
        })));
        let at = |s: u32| format!("2000-01-01T00:00:{s:02}Z");
        t.set_timeline(
            serde_json::from_value(
                serde_json::json!({ "thread": { "thread_id": "t1" }, "messages": [
                    { "message_id": "m1", "sequence": 1, "kind": "user", "status": "accepted",
                      "turn_run_id": RUN, "content": "go", "created_at": at(1) },
                    { "message_id": "a1", "sequence": 2, "kind": "assistant", "status": "finalized",
                      "turn_run_id": RUN, "content": "Done.", "created_at": at(2) },
                    { "message_id": "a2", "sequence": 3, "kind": "assistant", "status": "finalized",
                      "turn_run_id": RUN, "content": "Done.", "created_at": at(3) },
                ]}),
            )
            .expect("timeline decodes"),
        );
        let rows = t.rows();
        let replies: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Assistant { key, .. } => Some(key.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            replies,
            [format!("text:{RUN}:1"), format!("text:{RUN}:2")],
            "{rows:?}"
        );
        let keys: HashSet<&str> = rows.iter().map(Row::key).collect();
        assert_eq!(keys.len(), rows.len(), "duplicate row keys: {rows:?}");
    }

    #[test]
    fn a_stored_result_settles_a_card_whose_completion_was_missed() {
        let mut t = Transcript::default();
        t.upsert_tool(
            "i1".into(),
            Some(RUN.into()),
            "Read a".into(),
            Some("path: a".into()),
            ToolStatus::Running,
        );
        let preview = serde_json::json!({ "invocation_id": "i1", "status": "completed",
            "title": "Read a", "output_summary": "12 lines" })
        .to_string();
        t.set_timeline(
            serde_json::from_value(serde_json::json!({ "thread": { "thread_id": "t1" },
                "messages": [{ "message_id": "p1", "sequence": 1,
                    "kind": "capability_display_preview", "status": "finalized",
                    "turn_run_id": RUN, "content": preview }] }))
            .expect("timeline decodes"),
        );
        assert!(t.rows().iter().any(|r| matches!(r,
            Row::Tool { status: ToolStatus::Completed, detail: Some(d), .. } if d == "12 lines")));
    }

    #[test]
    fn streaming_text_converges_into_durable_reply() {
        let mut t = Transcript::default();
        t.set_timeline(timeline(None));
        t.apply(&status("running"));
        t.apply(&text_update("Hel", false, 1));
        t.apply(&text_update("Hello", false, 1));

        let rows = t.rows();
        assert!(
            matches!(&rows[1], Row::Assistant { text, streaming: true, .. } if text == "Hello")
        );
        assert_eq!(t.active_run(), Some(RUN));

        // The live gateway re-sends the reply as a finalized item keyed by
        // the durable message id; it must not show up as a second copy.
        t.apply(&frame(serde_json::json!({
            "cursor": "c3", "type": "projection_update",
            "state": { "thread_id": "t1", "items": [
                { "text": { "id": "m2", "run_id": RUN, "body": "Hello", "finalized": true } }
            ]}
        })));
        let no_narration = |rows: &[Row]| !rows.iter().any(|r| matches!(r, Row::Narration { .. }));
        assert!(
            no_narration(&t.rows()),
            "reply copies must not render as narration"
        );

        let applied = t.apply(&status("completed"));
        assert!(applied.reload_timeline);
        assert_eq!(t.active_run(), None);

        t.set_timeline(timeline(Some("Hello")));
        let rows = t.rows();
        assert!(
            no_narration(&rows),
            "reply copies must not render as narration"
        );
        let replies: Vec<_> = rows
            .iter()
            .filter(|r| matches!(r, Row::Assistant { .. }))
            .collect();
        assert_eq!(
            replies.len(),
            1,
            "live bubble must yield to the durable reply"
        );
        assert!(matches!(
            replies[0],
            Row::Assistant {
                streaming: false,
                ..
            }
        ));
    }

    /// Captured from a live gateway with no LLM configured: the run fails
    /// before producing any output, so only run_status frames arrive.
    #[test]
    fn run_failing_before_output_still_shows_notice() {
        let mut t = Transcript::default();
        t.set_timeline(timeline(None));
        t.apply(&status("running"));
        t.apply(&frame(serde_json::json!({
            "cursor": "c4", "type": "projection_update",
            "state": { "thread_id": "t1", "items": [
                { "run_status": { "run_id": RUN, "status": "failed",
                                  "failure_category": "model_credentials_unavailable",
                                  "failure_summary": "Check the provider API key.",
                                  "retryable": true } }
            ]}
        })));
        let rows = t.rows();
        assert_eq!(rows.len(), 2);
        assert!(matches!(&rows[0], Row::User { .. }));
        assert!(matches!(
            &rows[1],
            Row::RunNotice { text, failed: true, retryable: true, .. }
                if text.contains("Check the provider API key.")
        ));
    }

    /// Captured from a live gateway: Stop pressed during the thinking phase.
    #[test]
    fn cancelled_run_releases_stop_button() {
        let mut t = Transcript::default();
        t.set_timeline(timeline(None));
        for s in ["queued", "running", "cancel_requested"] {
            t.apply(&status(s));
        }
        assert_eq!(t.active_run(), Some(RUN));
        assert!(t.is_stopping());
        t.apply(&status("cancelled"));
        assert_eq!(t.active_run(), None);
        assert!(!t.is_stopping());
    }

    /// The click alone shows "Stopping"; a failed POST reverts it.
    #[test]
    fn stop_click_shows_stopping_before_the_server_confirms() {
        let mut t = Transcript::default();
        t.set_timeline(timeline(None));
        t.apply(&status("running"));
        assert!(!t.is_stopping());
        t.mark_stop_requested(RUN);
        assert!(t.is_stopping());
        t.clear_stop_requested();
        assert!(!t.is_stopping());
        t.mark_stop_requested(RUN);
        t.apply(&status("cancelled"));
        assert!(!t.is_stopping());
    }

    #[test]
    fn earlier_model_calls_render_as_activity_not_bubbles() {
        let mut t = Transcript::default();
        t.apply(&text_update("Let me check.", false, 1));
        t.apply(&text_update("Done.", false, 2));
        let rows = t.rows();
        assert!(matches!(&rows[0], Row::Narration { text, .. } if text == "Let me check."));
        assert!(matches!(&rows[1], Row::Assistant { text, .. } if text == "Done."));
    }

    fn tool(status: &str) -> StreamEvent {
        frame(serde_json::json!({
            "cursor": "c4", "type": "projection_update",
            "state": { "thread_id": "t1", "items": [
                { "capability_activity": {
                    "invocation_id": "6f1c1c0e-0000-4000-8000-0000000000aa",
                    "turn_run_id": RUN, "capability_id": "ironclaw.shell",
                    "status": status, "input_summary": "ls",
                    "updated_at": "2026-10-28T00:00:00Z" } }
            ]}
        }))
    }

    /// Text, a tool, then the same text flagged as narration: the order
    /// and the text's key hold at every step, and no thought becomes a
    /// tool's reason.
    #[test]
    fn streamed_text_keeps_its_place_and_key_around_tools() {
        let mut t = Transcript::default();
        t.apply(&status("running"));
        t.apply(&frame(serde_json::json!({
            "cursor": "c0", "type": "projection_update",
            "state": { "thread_id": "t1", "items": [
                { "thinking": { "id": "th1", "run_id": RUN, "body": "Plan the listing." } }
            ]}
        })));
        t.apply(&text_update("Let me look.", false, 0));
        t.apply(&tool("running"));
        let order = |t: &Transcript| {
            t.rows()
                .iter()
                .map(|r| r.key().to_string())
                .collect::<Vec<_>>()
        };
        let tool_key = "tool:6f1c1c0e-0000-4000-8000-0000000000aa";
        let text_key = format!("text:{RUN}:0");
        let want = vec!["th1".to_string(), text_key.clone(), tool_key.to_string()];
        assert_eq!(order(&t), want);
        t.apply(&text_update("Let me look.", true, 0));
        assert_eq!(order(&t), want);
        assert!(
            t.rows()
                .iter()
                .all(|r| !matches!(r, Row::Tool { reason: Some(r), .. } if r.contains("Plan")))
        );

        // A late running frame never reopens the finished call.
        t.apply(&tool("completed"));
        t.apply(&tool("running"));
        assert!(t.rows().iter().any(|r| matches!(
            r,
            Row::Tool {
                status: ToolStatus::Completed,
                ..
            }
        )));
    }

    #[test]
    fn run_end_is_reported_once() {
        let mut t = Transcript::default();
        assert_eq!(t.apply(&status("running")).run_ended, None);
        assert_eq!(
            t.apply(&status("completed")).run_ended,
            Some(RunEnd::Completed)
        );
        assert_eq!(t.apply(&status("completed")).run_ended, None);
        let mut t = Transcript::default();
        assert_eq!(t.apply(&status("failed")).run_ended, Some(RunEnd::Failed));
    }

    #[test]
    fn the_settled_run_is_named() {
        let mut t = Transcript::default();
        assert_eq!(t.last_settled_run(), None);
        t.apply(&status("running"));
        assert_eq!(t.last_settled_run(), None, "running is not settled");
        t.apply(&status("completed"));
        assert_eq!(t.last_settled_run(), Some(RUN));
    }

    #[test]
    fn gate_closes_when_run_moves_on() {
        let mut t = Transcript::default();
        t.apply(&frame(serde_json::json!({
            "cursor": "c3", "type": "projection_update",
            "state": { "thread_id": "t1", "items": [
                { "gate": { "run_id": RUN, "gate_kind": "approval", "gate_ref": "g1",
                            "headline": "Run shell?", "allow_always": true } }
            ]}
        })));
        assert!(matches!(
            t.rows()[0],
            Row::Gate {
                state: GateState::Open,
                ..
            }
        ));
        let approve = GateDecision::Approve { always: false };
        t.mark_gate_resolving("g1", approve);
        assert!(matches!(
            t.rows()[0],
            Row::Gate {
                state: GateState::Resolving(d),
                ..
            } if d == approve
        ));
        t.apply(&status("running"));
        assert!(matches!(
            t.rows()[0],
            Row::Gate {
                state: GateState::Closed(Some(d)),
                ..
            } if d == approve
        ));
    }

    #[test]
    fn a_sent_message_stays_until_the_timeline_has_its_copy() {
        let user = |rows: &[Row]| {
            rows.iter()
                .filter(|r| matches!(r, Row::User { .. }))
                .count()
        };
        let mut t = Transcript::default();
        t.set_timeline(timeline(Some("Hello")));
        let id = t.push_pending_user("again", &[]);
        assert_eq!(user(&t.rows()), 2, "shown at once");

        // Accepted, but the runtime has not written it into history yet.
        t.ack_pending_user(id);
        t.set_timeline(timeline(Some("Hello")));
        assert_eq!(user(&t.rows()), 2, "an accepted send must not vanish");

        let mut page = timeline(Some("Hello"));
        page.messages.push(
            serde_json::from_value(serde_json::json!({
                "message_id": "m3", "sequence": 3, "kind": "user", "status": "accepted",
                "content": "again"
            }))
            .expect("message decodes"),
        );
        t.set_timeline(page);
        let rows = t.rows();
        assert_eq!(user(&rows), 2, "the server copy replaces it, no duplicate");
        // It takes the bubble's key: the view keeps the one element (no
        // second bubble fading in while the first fades out).
        assert!(
            matches!(rows.last(), Some(Row::User { key, pending: false, .. }) if *key == format!("pending:{id}"))
        );
        assert_eq!(t.key_of_message("m3"), format!("pending:{id}"));
        assert_eq!(t.key_of_message("m1"), "msg:m1");
    }

    fn page(messages: serde_json::Value) -> TimelinePage {
        serde_json::from_value(serde_json::json!({
            "thread": { "thread_id": "t1" }, "messages": messages
        }))
        .expect("timeline decodes")
    }

    fn steers(rows: &[Row]) -> Vec<(String, Option<SteerMark>)> {
        rows.iter()
            .filter_map(|r| match r {
                Row::User { text, steer, .. } => Some((text.clone(), steer.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_later_message_of_the_same_run_is_a_steer_and_back_to_back_ones_label_once() {
        let mut t = Transcript::default();
        t.set_timeline(page(serde_json::json!([
            {"message_id": "m1", "sequence": 1, "kind": "user", "status": "accepted",
             "turn_run_id": RUN, "content": "port it", "created_at": "2026-11-19T10:00:00Z"},
            {"message_id": "m2", "sequence": 2, "kind": "user", "status": "accepted",
             "turn_run_id": RUN, "content": "use rune", "created_at": "2026-11-19T10:00:10Z",
             "steer": {"id": "s1", "state": "read", "waiting_to_be_read": false}},
            {"message_id": "m3", "sequence": 3, "kind": "user", "status": "queued",
             "turn_run_id": RUN, "content": "and test it", "created_at": "2026-11-19T10:00:20Z"},
            {"message_id": "m4", "sequence": 4, "kind": "assistant", "status": "finalized",
             "turn_run_id": RUN, "content": "Done.", "created_at": "2026-11-19T10:02:10Z"},
            {"message_id": "m5", "sequence": 5, "kind": "user", "status": "accepted",
             "turn_run_id": "run-2", "content": "next", "created_at": "2026-11-19T10:03:00Z"},
        ])));
        let marks = steers(&t.rows());
        assert_eq!(
            marks[0],
            ("port it".into(), None),
            "the run's first message starts it"
        );
        let first = marks[1].1.clone().expect("a steer");
        assert!(!first.waiting && !first.continues);
        assert_eq!(
            first.continued_ms,
            Some(10_000),
            "its segment ends where the next steer starts"
        );
        let second = marks[2].1.clone().expect("a steer");
        assert!(second.waiting, "queued until the model reads it");
        assert!(second.continues, "the label shows once");
        assert_eq!(
            second.continued_ms,
            Some(110_000),
            "the last steer's segment runs to the run's end"
        );
        assert_eq!(marks[3], ("next".into(), None));
    }

    #[test]
    fn the_ledger_waiting_flag_wins_and_a_pending_steer_shows_waiting() {
        let mut t = Transcript::default();
        t.set_timeline(page(serde_json::json!([
            {"message_id": "m1", "sequence": 1, "kind": "user", "status": "accepted",
             "turn_run_id": RUN, "content": "hi"},
            {"message_id": "m2", "sequence": 2, "kind": "user", "status": "accepted",
             "turn_run_id": RUN, "content": "steer",
             "steer": {"id": "s1", "state": "pending", "waiting_to_be_read": true}},
        ])));
        let id = t.push_pending_user("more", &[]);
        t.set_pending_steer(id, Some(true));
        let marks = steers(&t.rows());
        assert!(marks[1].1.as_ref().is_some_and(|m| m.waiting));
        let pending = marks[2].1.clone().expect("the optimistic row is a steer");
        assert!(pending.waiting && pending.continues);
        t.set_pending_steer(id, None);
        assert_eq!(steers(&t.rows())[2].1, None, "a send that started a turn");
    }

    #[test]
    fn unconfirmed_steer_notices_follow_their_run_even_from_a_hidden_row() {
        let mut t = Transcript::default();
        t.set_timeline(page(serde_json::json!([
            {"message_id": "m1", "sequence": 1, "kind": "user", "status": "accepted",
             "turn_run_id": RUN, "content": "hi"},
            {"message_id": "m2", "sequence": 2, "kind": "assistant", "status": "finalized",
             "turn_run_id": RUN, "content": "Stopped."},
            {"message_id": "m3", "sequence": 3, "kind": "tool_result_reference", "status": "finalized",
             "turn_run_id": RUN, "content": "",
             "steer_notices": [{"steer_id": "s9", "title": "Steer delivery not confirmed",
                "text": "use rune", "content": "The agent did not confirm reading your steer before Stop."}]},
            {"message_id": "m4", "sequence": 4, "kind": "user", "status": "accepted",
             "turn_run_id": "run-2", "content": "again"},
        ])));
        let rows = t.rows();
        let at = rows
            .iter()
            .position(|r| matches!(r, Row::SteerNotice { .. }))
            .expect("the notice shows");
        assert!(matches!(&rows[at], Row::SteerNotice { title, words, .. }
            if title == "Steer delivery not confirmed" && words == "use rune"));
        assert!(matches!(&rows[at - 1], Row::Assistant { text, .. } if text == "Stopped."));
        assert!(matches!(&rows[at + 1], Row::User { text, .. } if text == "again"));
    }

    /// A's final wire: the page's typed `steer_marks` mark rows by message
    /// id (and win over row fields); a native `rejected_busy` row is never
    /// a steer and says it was not delivered; the ack's `steer_id` claims
    /// its own row even when the text was reworded.
    #[test]
    fn steer_marks_rejected_rows_and_steer_ids_from_the_final_wire() {
        let mut t = Transcript::default();
        let page: TimelinePage = serde_json::from_value(serde_json::json!({
            "thread": {"thread_id": "t1"},
            "messages": [
                {"message_id": "m1", "sequence": 1, "kind": "user", "status": "accepted",
                 "turn_run_id": RUN, "content": "go"},
                {"message_id": "m2", "sequence": 2, "kind": "user", "status": "accepted",
                 "turn_run_id": "other-run", "content": "marked by id", "steer_id": "s7"},
                {"message_id": "m3", "sequence": 3, "kind": "user", "status": "rejected_busy",
                 "turn_run_id": RUN, "content": "too late"},
            ],
            "steer_marks": [
                {"message_id": "m2", "steer": {"id": "s7", "state": "pending",
                  "waiting_to_be_read": true},
                 "notices": [{"steer_id": "s8", "title": "Steer delivery not confirmed",
                  "text": "lost words", "content": "Not confirmed."}]},
            ],
        }))
        .expect("page decodes");
        t.set_timeline(page);
        let rows = t.rows();
        let marks = steers(&rows);
        assert!(
            marks[1].1.as_ref().is_some_and(|m| m.waiting),
            "a mark by message id, though the row's run is its own"
        );
        assert_eq!(
            marks[2].1, None,
            "a message the turn never took is no steer"
        );
        assert!(
            rows.iter()
                .any(|r| matches!(r, Row::SteerNotice { words, .. } if words == "lost words"))
        );
        assert!(
            rows.iter()
                .any(|r| matches!(r, Row::SteerNotice { title, words, .. }
            if title == "Not delivered" && words == "too late"))
        );

        // The ack's steer id claims the stored row, whatever its text.
        let mut t = Transcript::default();
        let id = t.push_pending_user("my words", &[]);
        t.set_pending_steer_id(id, Some("s9".into()));
        t.ack_pending_user(id);
        t.set_timeline(
            serde_json::from_value(serde_json::json!({
                "thread": {"thread_id": "t1"},
                "messages": [
                    {"message_id": "m9", "sequence": 9, "kind": "user", "status": "accepted",
                     "turn_run_id": RUN, "content": "reworded by the runtime", "steer_id": "s9"},
                ],
            }))
            .expect("page decodes"),
        );
        assert_eq!(t.key_of_message("m9"), format!("pending:{id}"));
    }
}
