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
use crate::wire::{MessageKind, StreamEvent, TimelineMessage, TimelinePage};

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
        text: String,
        pending: bool,
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
    },
    RunNotice {
        key: String,
        run_id: String,
        text: String,
        failed: bool,
        retryable: bool,
    },
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
            | Row::RunNotice { key, .. } => key,
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
    pending_user: Vec<(u64, String, bool)>,
    latest_run: Option<String>,
    tick: u64,
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
        // The server accepted these sends; the durable copy replaces them.
        self.pending_user.retain(|(_, _, acked)| !acked);
    }

    /// Show a message immediately while the send is in flight.
    pub fn push_pending_user(&mut self, text: &str) -> u64 {
        self.tick += 1;
        self.pending_user.push((self.tick, text.to_string(), false));
        self.tick
    }

    pub fn ack_pending_user(&mut self, id: u64) {
        if let Some(entry) = self.pending_user.iter_mut().find(|(i, _, _)| *i == id) {
            entry.2 = true;
        }
    }

    pub fn drop_pending_user(&mut self, id: u64) {
        self.pending_user.retain(|(i, _, _)| *i != id);
    }

    pub fn mark_gate_resolving(&mut self, gate_ref: &str, decision: GateDecision) {
        self.decisions.insert(gate_ref.to_string(), decision);
    }

    pub fn clear_gate_resolving(&mut self, gate_ref: &str) {
        self.decisions.remove(gate_ref);
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

    /// Stop was accepted but the run has not wound down yet.
    pub fn is_stopping(&self) -> bool {
        self.active_run()
            .and_then(|run| self.runs.get(run))
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

        for message in self.durable.iter().filter(|m| m.is_visible()) {
            let text = message.content.clone().unwrap_or_default();
            let key = format!("msg:{}", message.message_id);
            match message.kind {
                MessageKind::User => {
                    rows.push(Row::User {
                        key,
                        text,
                        pending: false,
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

        for (id, text, _) in &self.pending_user {
            rows.push(Row::User {
                key: format!("pending:{id}"),
                text: text.clone(),
                pending: true,
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
        for key in keys {
            let entry = &self.live[*key];
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
                } if Some(body.trim()) == reply => continue,
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
                } => Row::Gate {
                    key: key.to_string(),
                    run_id: entry.run.clone().unwrap_or_default(),
                    gate_ref: gate_ref.clone(),
                    kind: *kind,
                    headline: headline.clone(),
                    body: body.clone(),
                    allow_always: *allow_always,
                    state: self.gate_state(gate_ref, entry),
                },
            };
            rows.push(row);
        }
        rows.extend(bubble_row);
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
}
