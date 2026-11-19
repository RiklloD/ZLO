//! What a Muse turn shows while it runs, from the MSP view events the
//! outcome fold ([`super::fold`]) also reads: reply and reasoning text,
//! one tool row per tool item with its streamed output, and the todo list
//! as the thread's plan. Pure: it turns one notification into the same
//! [`LiveStep`]s the Codex driver hands the bridge.
//!
//! Muse sends whole item snapshots (`item/started|updated|completed`) and
//! appends (`item/delta`); both update the item's text here, and only the
//! part not shown yet becomes a step. A revision that rewrites shown text
//! is not retracted (the durable reply replaces the live bubble).

use std::collections::HashMap;

use ironclaw_host_api::agent_plan::{AgentPlan, PlanEntry, PlanEntryStatus};
use serde_json::Value;

use super::fold::{HIDDEN_KINDS, HOUSEKEEPING_TOOLS};
use crate::codex::live::LiveStep;

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Message,
    Reasoning,
    Tool,
}

#[derive(Debug)]
struct Item {
    kind: Kind,
    /// Reply text, or reasoning text when Muse sends it whole.
    text: String,
    /// Reasoning summary parts (`summary.N` deltas).
    parts: Vec<String>,
    /// Tool output so far (`visibleOutput`).
    output: String,
    /// How much of the rendered body / output is already shown.
    shown: usize,
    finished: bool,
}

impl Item {
    fn body(&self) -> String {
        if self.kind == Kind::Reasoning && self.text.is_empty() {
            self.parts.join("\n\n")
        } else if self.kind == Kind::Tool {
            self.output.clone()
        } else {
            self.text.clone()
        }
    }

    /// The part of the body not shown yet (`None`: nothing new, or a
    /// rewrite of what was shown).
    fn unsent(&mut self) -> Option<String> {
        let body = self.body();
        if body.len() <= self.shown || !body.is_char_boundary(self.shown) {
            return None;
        }
        let fresh = body[self.shown..].to_string();
        self.shown = body.len();
        Some(fresh)
    }
}

#[derive(Debug, Default)]
pub(crate) struct MuseLive {
    items: HashMap<String, Item>,
    /// The message / reasoning item the last text step belonged to, so a new
    /// one starts its own paragraph.
    text_item: Option<String>,
    thinking_item: Option<String>,
}

impl MuseLive {
    /// The live steps of one notification. `owned` says whether an item
    /// event belongs to this turn (the outcome fold decides); deltas only
    /// reach items already accepted.
    pub(crate) fn push(&mut self, method: &str, params: &Value, owned: bool) -> Vec<LiveStep> {
        match method {
            "item/started" | "item/updated" | "item/completed" if owned => {
                self.item(&params["item"])
            }
            "item/delta" => self.delta(params),
            _ => Vec::new(),
        }
    }

    fn item(&mut self, raw: &Value) -> Vec<LiveStep> {
        let Some(id) = s(raw, "itemId") else {
            return Vec::new();
        };
        let kind_name = s(raw, "kind").unwrap_or_default();
        let tool = s(raw, "tool");
        let status = s(raw, "status").unwrap_or("inProgress");
        let kind = match kind_name {
            "agentMessage" => Kind::Message,
            "reasoning" => Kind::Reasoning,
            _ if HIDDEN_KINDS.contains(&kind_name) => return Vec::new(),
            // Muse draws its todo list and questions itself; the call is noise.
            "toolCall"
                if tool.is_some_and(|t| HOUSEKEEPING_TOOLS.contains(&t)) && status != "failed" =>
            {
                return Vec::new();
            }
            _ => Kind::Tool,
        };
        let mut steps = Vec::new();
        let item = match self.items.get_mut(id) {
            Some(item) => item,
            None => {
                if kind == Kind::Tool {
                    let (name, subject) = tool_row(raw);
                    steps.push(LiveStep::ToolStarted {
                        call_id: id.to_string(),
                        tool: name,
                        subject,
                    });
                }
                self.items.entry(id.to_string()).or_insert(Item {
                    kind,
                    text: String::new(),
                    parts: Vec::new(),
                    output: String::new(),
                    shown: 0,
                    finished: false,
                })
            }
        };
        match kind {
            Kind::Message | Kind::Reasoning => {
                if let Some(text) = s(raw, "text") {
                    item.text = text.to_string();
                }
                if let Some(parts) = raw.get("summary").and_then(Value::as_array) {
                    item.parts = parts
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect();
                }
            }
            Kind::Tool => {
                if let Some(output) = s(raw, "visibleOutput") {
                    item.output = output.to_string();
                }
            }
        }
        steps.extend(self.flush(id));
        if kind == Kind::Tool
            && status != "inProgress"
            && let Some(item) = self.items.get_mut(id).filter(|i| !i.finished)
        {
            item.finished = true;
            let failed = status != "completed"
                || raw
                    .get("exitCode")
                    .and_then(Value::as_i64)
                    .is_some_and(|code| code != 0);
            steps.push(LiveStep::ToolFinished {
                call_id: id.to_string(),
                failed,
            });
        }
        steps
    }

    fn delta(&mut self, params: &Value) -> Vec<LiveStep> {
        let (Some(id), Some(delta)) = (s(params, "itemId"), s(params, "delta")) else {
            return Vec::new();
        };
        let field = s(params, "field").unwrap_or("text");
        let Some(item) = self.items.get_mut(id) else {
            return Vec::new();
        };
        match (item.kind, field) {
            (Kind::Tool, "output" | "visibleOutput") => item.output.push_str(delta),
            (Kind::Message | Kind::Reasoning, "text") => item.text.push_str(delta),
            (Kind::Reasoning, part) => {
                let Some(index) = part
                    .strip_prefix("summary.")
                    .and_then(|n| n.parse::<usize>().ok())
                    .filter(|n| *n < 100)
                else {
                    return Vec::new();
                };
                if item.parts.len() <= index {
                    item.parts.resize(index + 1, String::new());
                }
                item.parts[index].push_str(delta);
            }
            _ => return Vec::new(),
        }
        self.flush(id)
    }

    /// The unshown part of item `id` as one step.
    fn flush(&mut self, id: &str) -> Vec<LiveStep> {
        let Some(item) = self.items.get_mut(id) else {
            return Vec::new();
        };
        let Some(delta) = item.unsent() else {
            return Vec::new();
        };
        let step = match item.kind {
            Kind::Message => {
                let new_block = self.text_item.as_deref() != Some(id);
                self.text_item = Some(id.to_string());
                LiveStep::Text { delta, new_block }
            }
            Kind::Reasoning => {
                let new_block = self.thinking_item.as_deref() != Some(id);
                self.thinking_item = Some(id.to_string());
                LiveStep::Thinking { delta, new_block }
            }
            Kind::Tool => LiveStep::ToolOutput {
                call_id: id.to_string(),
                delta,
            },
        };
        vec![step]
    }
}

/// A tool item's row name and subject: the command, the path, the query or
/// URL, or a subagent's objective.
fn tool_row(item: &Value) -> (String, Option<String>) {
    let args: Option<Value> = s(item, "args").and_then(|a| serde_json::from_str(a).ok()); // silent-ok: streamed model arguments may not be JSON yet
    let arg = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| args.as_ref().and_then(|a| s(a, k)))
            .map(str::to_string)
    };
    match s(item, "kind").unwrap_or_default() {
        "userShell" => ("shell".into(), s(item, "commandText").map(str::to_string)),
        "subagent" => (
            "subagent".into(),
            s(item, "objective")
                .or_else(|| s(item, "role"))
                .map(str::to_string),
        ),
        "workflow" => ("workflow".into(), s(item, "entryId").map(str::to_string)),
        kind => {
            let name = s(item, "tool").unwrap_or(kind).to_string();
            let subject = s(item, "commandText").map(str::to_string).or_else(|| {
                arg(&[
                    "command",
                    "cmd",
                    "file_path",
                    "path",
                    "filePath",
                    "query",
                    "url",
                    "pattern",
                ])
            });
            (name, subject)
        }
    }
}

/// `session/todoListChanged` as the thread's plan; cancelled entries drop.
pub(crate) fn todo_plan(params: &Value) -> Option<AgentPlan> {
    let items = params.get("items")?.as_array()?;
    let entries = items.iter().filter_map(|item| {
        let status = match s(item, "status")? {
            "cancelled" => return None,
            "completed" => PlanEntryStatus::Completed,
            "inProgress" => PlanEntryStatus::InProgress,
            _ => PlanEntryStatus::Pending,
        };
        Some(PlanEntry::new(s(item, "text")?, status))
    });
    Some(AgentPlan::new(entries, None, chrono::Utc::now()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(id: &str, kind: &str, status: &str, extra: Value) -> Value {
        let mut item =
            json!({"itemId": id, "turnId": "t1", "kind": kind, "status": status, "revision": 1});
        if let (Some(item), Some(extra)) = (item.as_object_mut(), extra.as_object()) {
            item.extend(extra.clone());
        }
        json!({ "item": item })
    }

    #[test]
    fn snapshots_and_deltas_stream_each_part_once() {
        let mut live = MuseLive::default();
        let started = item("m1", "agentMessage", "inProgress", json!({}));
        assert!(live.push("item/started", &started, true).is_empty());
        let delta = json!({"itemId": "m1", "field": "text", "delta": "Hel"});
        assert_eq!(
            live.push("item/delta", &delta, true),
            vec![LiveStep::Text {
                delta: "Hel".into(),
                new_block: true
            }]
        );
        // The whole snapshot only adds what was not shown.
        let done = item("m1", "agentMessage", "completed", json!({"text": "Hello"}));
        assert_eq!(
            live.push("item/completed", &done, true),
            vec![LiveStep::Text {
                delta: "lo".into(),
                new_block: false
            }]
        );
        // Reasoning summary parts are thinking, each part its own paragraph.
        live.push(
            "item/started",
            &item("r1", "reasoning", "inProgress", json!({})),
            true,
        );
        let think =
            |n: u32, d: &str| json!({"itemId": "r1", "field": format!("summary.{n}"), "delta": d});
        assert_eq!(
            live.push("item/delta", &think(0, "Plan"), true),
            vec![LiveStep::Thinking {
                delta: "Plan".into(),
                new_block: true
            }]
        );
        assert_eq!(
            live.push("item/delta", &think(1, "Act"), true),
            vec![LiveStep::Thinking {
                delta: "\n\nAct".into(),
                new_block: false
            }]
        );
        // Another turn's item never shows; its deltas are dropped too.
        let foreign = item("m9", "agentMessage", "inProgress", json!({"text": "x"}));
        assert!(live.push("item/started", &foreign, false).is_empty());
        assert!(
            live.push("item/delta", &json!({"itemId": "m9", "delta": "y"}), true)
                .is_empty()
        );
    }

    #[test]
    fn tool_items_become_rows_with_output_and_an_end() {
        let mut live = MuseLive::default();
        let args = json!({"file_path": "src/a.rs"}).to_string();
        let started = item(
            "c1",
            "toolCall",
            "inProgress",
            json!({"tool": "read_file", "args": args}),
        );
        assert_eq!(
            live.push("item/started", &started, true),
            vec![LiveStep::ToolStarted {
                call_id: "c1".into(),
                tool: "read_file".into(),
                subject: Some("src/a.rs".into())
            }]
        );
        let out = json!({"itemId": "c1", "field": "visibleOutput", "delta": "fn a()"});
        assert_eq!(
            live.push("item/delta", &out, true),
            vec![LiveStep::ToolOutput {
                call_id: "c1".into(),
                delta: "fn a()".into()
            }]
        );
        let failed = item(
            "c1",
            "toolCall",
            "failed",
            json!({"tool": "read_file", "visibleOutput": "fn a()"}),
        );
        assert_eq!(
            live.push("item/completed", &failed, true),
            vec![LiveStep::ToolFinished {
                call_id: "c1".into(),
                failed: true
            }]
        );
        // A shell item that exits non-zero failed; housekeeping stays out.
        let shell = item(
            "s1",
            "userShell",
            "completed",
            json!({"commandText": "ls", "exitCode": 2}),
        );
        let steps = live.push("item/completed", &shell, true);
        assert_eq!(steps.len(), 2, "{steps:?}");
        assert_eq!(
            steps[1],
            LiveStep::ToolFinished {
                call_id: "s1".into(),
                failed: true
            }
        );
        let todos = item(
            "w1",
            "toolCall",
            "completed",
            json!({"tool": "write_todos"}),
        );
        assert!(live.push("item/completed", &todos, true).is_empty());
    }

    #[test]
    fn the_todo_list_is_the_plan_without_cancelled_entries() {
        let plan = todo_plan(&json!({"items": [
            {"text": "Read", "status": "completed"},
            {"text": "Edit", "status": "inProgress"},
            {"text": "Drop", "status": "cancelled"},
            {"text": "Test", "status": "pending"},
        ]}))
        .unwrap();
        let statuses: Vec<_> = plan
            .entries
            .iter()
            .map(|e| (e.content.as_str(), e.status))
            .collect();
        assert_eq!(
            statuses,
            [
                ("Read", PlanEntryStatus::Completed),
                ("Edit", PlanEntryStatus::InProgress),
                ("Test", PlanEntryStatus::Pending),
            ]
        );
    }
}
