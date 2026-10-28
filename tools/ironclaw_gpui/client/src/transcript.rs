//! Reconciles the durable timeline with the live projection into the rows a
//! chat view renders. GPUI-free so it can be unit-tested.
//!
//! The model follows `CONTRACT.md` "Streaming model":
//! - projection frames are partial: every item is upserted by identity
//!   (`text:{run}:{call}`, run id, gate ref, invocation id);
//! - live assistant text is cumulative per model call; the run's trailing
//!   unflagged call is the streaming bubble, earlier calls and `narration`
//!   items are activity, like thinking and tool cards;
//! - once a run reaches a terminal status the durable reply converges into the
//!   bubble: the app reloads the timeline and the durable message replaces it;
//! - thinking, narration and tool cards are live-only state, never durable.

use std::collections::{HashMap, HashSet};

use ironclaw_product_contracts::outbound::{
    CapabilityActivityStatusView, CapabilityActivityView, CapabilityDisplayPreviewView,
    ProductGateKind, ProductProjectionItem,
};

use crate::api::GateDecision;
use crate::orchestration::MergedFork;
use crate::wire::{
    ATTACHMENTS_ONLY_CONTENT, CommandField, CommandOutcome, MessageKind, StreamEvent,
    TimelineMessage, TimelinePage,
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

#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    User {
        key: String,
        /// The person's words, with `{{attachment:N}}` where they placed
        /// `attachments[N]` (see [`readable`]).
        text: String,
        pending: bool,
        attachments: Vec<AttachmentChip>,
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
            | Row::Gate { key, .. }
            | Row::RunNotice { key, .. }
            | Row::Command { key, .. } => key,
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
}

#[derive(Debug, Clone)]
struct LiveEntry {
    run: Option<String>,
    /// Tick of the last upsert; orders gates against later run statuses.
    tick: u64,
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
    pending_user: Vec<PendingUser>,
    /// Command results, each placed after the last durable message it saw.
    notes: Vec<(u64, Row)>,
    latest_run: Option<String>,
    /// Stop clicked in this view; shows "Stopping" before the server confirms.
    stop_requested: Option<String>,
    tick: u64,
    /// A fork's source conversation, shown read-only above its own messages.
    prefix: Vec<TimelineMessage>,
    /// Forks merged back into this thread, each shown where it arrived.
    merges: Vec<MergedFork>,
}

/// A message sent from this view that the timeline does not show yet.
#[derive(Debug)]
struct PendingUser {
    id: u64,
    text: String,
    /// The gateway accepted the send.
    acked: bool,
    chips: Vec<AttachmentChip>,
    /// Newest durable sequence when it was sent; its server copy is newer.
    after: Option<u64>,
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

/// A message's text as a person reads it: without the attachments-only
/// marker, and without the context handoff the engine prepends to the first
/// message sent after a fork or provider switch (`{context}\n\n---\n\n{typed}`).
fn shown_text(message: &TimelineMessage) -> String {
    let text = message
        .content
        .as_deref()
        .filter(|c| *c != ATTACHMENTS_ONLY_CONTENT)
        .unwrap_or_default();
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
            },
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
        messages.sort_by_key(|m| m.sequence);
        self.durable = messages;
        self.reconcile_pending();
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
            let found = durable
                .iter()
                .filter(newer)
                .find(same_text)
                .or_else(|| p.acked.then(|| durable.iter().find(newer)).flatten());
            match found {
                Some(m) => {
                    claimed.insert(m.message_id.as_str());
                    false
                }
                None => true,
            }
        });
    }

    /// Show a message immediately while the send is in flight.
    pub fn push_pending_user(&mut self, text: &str, files: &[String]) -> u64 {
        self.tick += 1;
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
            id: self.tick,
            text: text.to_string(),
            acked: false,
            chips,
            after: self.durable.iter().map(|m| m.sequence).max(),
        });
        self.tick
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
                self.upsert(
                    format!("final:{run}"),
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
                self.upsert(
                    id.clone(),
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

    fn upsert_activity(&mut self, a: &CapabilityActivityView) {
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
        // A preview carries a better title than a later bare activity frame.
        let (title, detail) = match self.live.get(&key).map(|e| &e.item) {
            Some(Live::Tool {
                title: old_title,
                detail: old_detail,
                ..
            }) => (
                if title.contains('.') && !old_title.contains('.') {
                    old_title.clone()
                } else {
                    title
                },
                detail.or_else(|| old_detail.clone()),
            ),
            _ => (title, detail),
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
                self.live.insert(key, LiveEntry { run, tick, item });
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
            out.reload_timeline = true;
            out.run_ended = Some(end);
        }
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
        // A run that fails before producing any output has no live group and
        // no reply; its notice goes right after the user message that started it.
        let quiet_run = |run: &str| {
            !durable_reply_runs.contains(run)
                && !groups.iter().any(|(r, _)| r.as_deref() == Some(run))
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
        for message in self.durable.iter().filter(|m| m.is_visible()) {
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
            let key = format!("msg:{}", message.message_id);
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
                    rows.push(Row::User {
                        key,
                        text,
                        pending: false,
                        attachments,
                    });
                    if let Some(run) = message.turn_run_id.as_deref()
                        && quiet_run(run)
                        && emitted.insert(Some(run.to_string()))
                    {
                        self.notice_row(run, &mut rows);
                    }
                }
                MessageKind::Assistant => {
                    let run = message.turn_run_id.clone();
                    if run.is_some()
                        && !emitted.contains(&run)
                        && let Some((_, keys)) = groups.iter().find(|(r, _)| *r == run)
                    {
                        self.activity_rows(keys, Some(&text), &mut rows);
                        emitted.insert(run.clone());
                    }
                    rows.push(Row::Assistant {
                        key,
                        text,
                        streaming: false,
                    });
                    if let Some(run) = &run {
                        self.notice_row(run, &mut rows);
                    }
                }
                _ => rows.push(Row::System { key, text }),
            }
        }

        for merge in merges {
            merge_rows(merge, &mut rows);
        }
        rows.extend(notes.map(|(_, note)| note.clone()));
        for p in &self.pending_user {
            rows.push(Row::User {
                key: format!("pending:{}", p.id),
                text: p.text.clone(),
                pending: !p.acked,
                attachments: p.chips.clone(),
            });
        }

        for (run, keys) in &groups {
            if emitted.contains(run) {
                continue;
            }
            let durable = run.as_deref().and_then(|r| durable_replies.get(r).copied());
            self.activity_rows(keys, durable, &mut rows);
            if let Some(run) = run {
                self.notice_row(run, &mut rows);
            }
        }
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
        let mut bubble_row = None;
        // What the model said last before each call: the reason it shows.
        let mut said: Option<&str> = None;
        for key in keys {
            let entry = &self.live[*key];
            if let Live::Text { body, .. } | Live::Thinking(body) = &entry.item {
                said = Some(body.as_str());
            }
            let said_reason = || said.and_then(reason::from_said);
            let row = match &entry.item {
                Live::Text { body, .. } if Some(*key) == bubble => {
                    let streaming = entry
                        .run
                        .as_deref()
                        .is_none_or(|r| !self.settled.contains(r));
                    bubble_row = Some(Row::Assistant {
                        key: key.to_string(),
                        text: body.clone(),
                        streaming,
                    });
                    continue;
                }
                // A final reply frame duplicates the durable reply once it lands.
                Live::Text { .. } if key.starts_with("final:") => continue,
                Live::Text {
                    body,
                    narration: false,
                } if Some(body.trim()) == reply
                    || self.is_durable_reply(entry.run.as_deref(), body) =>
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
        rows.extend(bubble_row);
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
        assert!(matches!(rows.last(), Some(Row::User { key, .. }) if key == "msg:m3"));
    }
}
