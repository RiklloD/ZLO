//! What an Antigravity turn shows while it runs, and which of its tools are
//! still working. Pure: one ACP `session/update` becomes [`LiveStep`]s;
//! [`apply`] hands them to the bridge.
//!
//! Tool rows are keyed by `toolCallId`. A `tool_call_update` without a title
//! keeps the row's name; a row first seen through a location-only update is
//! labelled with that path. Antigravity's own subagents (`invoke_subagent`,
//! `start_subagent`) show as `subagent` rows: they are the agent's internal
//! work, displayed read-only, never started by IronClaw.
//!
//! `agy` 1.5 ends a prompt while a command it backgrounded still runs, then
//! reports the command and replies on its own when it finishes; the driver
//! keeps the turn open meanwhile ([`LiveFold::background_running`]). A
//! daemon (`IsDaemon`) may never finish, so it never holds a turn. agy's
//! own `schedule` call stays in progress until its wake fires, then the
//! agent replies by itself: it holds the turn too, without the silent-hold
//! bound a command gets ([`LiveFold::schedule_pending`]).
//!
//! agy reports its `update_steps` checklist only as tool calls: they become
//! the thread's plan, never tool rows ([`LiveFold::take_steps`]). A call
//! through `call_mcp_tool` (titled `MCP Tool <tool>`, its input naming the
//! server and tool) shows as that MCP tool ([`proxied_mcp`]).

use std::collections::{HashMap, HashSet};

use ironclaw_host_api::ids::ThreadId;
use serde_json::Value;

use crate::bridge::CliBridge;

/// One change to the live turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LiveStep {
    Text {
        delta: String,
        new_block: bool,
    },
    Thinking {
        delta: String,
        new_block: bool,
    },
    ToolStarted {
        call_id: String,
        tool: String,
        subject: Option<String>,
    },
    ToolOutput {
        call_id: String,
        delta: String,
    },
    ToolFinished {
        call_id: String,
        failed: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Pending,
    Running,
    Done,
}

#[derive(Debug)]
struct Tool {
    execute: bool,
    daemon: bool,
    /// agy's own `schedule` call: running until its wake fires.
    schedule: bool,
    status: Status,
    /// Output already sent, to send only what an update adds.
    output: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    Text,
    Thinking,
    Tool,
}

#[derive(Debug, Default)]
pub(crate) struct LiveFold {
    tools: HashMap<String, Tool>,
    last: Option<Block>,
    /// `update_steps` calls seen, by call id (their later updates name
    /// them by id only).
    steps_calls: HashSet<String>,
}

/// The names Antigravity gives its own subagent calls.
const SUBAGENT_NAMES: [&str; 2] = ["invoke_subagent", "start_subagent"];
const SUBAGENT_TITLES: [&str; 5] = [
    "Running start_subagent",
    "Run start_subagent?",
    "Running invoke_subagent",
    "Run invoke_subagent?",
    "Run invoke_subagent",
];
/// Input fields that name what a subagent is for, best first.
const SUBAGENT_SUBJECT_FIELDS: [&str; 9] = [
    "Role",
    "role",
    "TaskName",
    "Name",
    "name",
    "Prompt",
    "prompt",
    "Task",
    "toolSummary",
];
/// The titles agy gives its own `schedule` call.
const SCHEDULE_TITLES: [&str; 2] = ["Run schedule", "Running schedule"];
/// The title of agy's `update_steps` checklist call.
const STEPS_TITLE: &str = "Run update_steps";
/// Input fields that hold a command line.
const COMMAND_FIELDS: [&str; 4] = ["CommandLine", "command_line", "commandLine", "command"];

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// `subagent` for Antigravity's own subagent calls (MCP tools that happen
/// to share a title excluded).
fn is_subagent(update: &Value) -> bool {
    if update["_meta"]["is_mcp_tool_call"].as_bool() == Some(true) {
        return false;
    }
    let by_name = s(update, "name").is_some_and(|n| SUBAGENT_NAMES.contains(&n));
    let by_title = matches!(s(update, "kind"), None | Some("other"))
        && s(update, "title").is_some_and(|t| SUBAGENT_TITLES.contains(&t));
    by_name || by_title
}

/// The server and tool of a call through agy's `call_mcp_tool`: titled
/// `MCP Tool <tool>`, its input naming that same tool and its server. Any
/// other call's input is the tool's own arguments, which may hold fields of
/// the same names.
pub(crate) fn proxied_mcp(call: &Value) -> Option<(String, String)> {
    let titled = s(call, "title")?
        .strip_prefix("MCP Tool ")?
        .trim_end_matches('?')
        .to_string();
    let input = &call["rawInput"];
    let (server, tool) = (s(input, "ServerName")?, s(input, "ToolName")?);
    (tool == titled).then(|| (server.to_string(), tool.to_string()))
}

/// The row's label and subject at its first sighting.
fn describe(update: &Value) -> (String, Option<String>) {
    let input = &update["rawInput"];
    if let Some((server, tool)) = proxied_mcp(update) {
        return ("mcp".into(), Some(format!("{server}: {tool}")));
    }
    if is_subagent(update) {
        let subject = SUBAGENT_SUBJECT_FIELDS
            .iter()
            .find_map(|k| s(input, k))
            .map(str::to_string);
        return ("subagent".into(), subject);
    }
    let tool = s(update, "name")
        .or_else(|| s(update, "kind"))
        .unwrap_or("tool")
        .to_string();
    let subject = COMMAND_FIELDS
        .iter()
        .find_map(|k| s(input, k))
        .or_else(|| s(update, "title"))
        .or_else(|| {
            update["locations"]
                .as_array()
                .and_then(|l| l.first())
                .and_then(|l| s(l, "path"))
        })
        .map(str::to_string);
    (tool, subject)
}

/// The text a tool update carries: `rawOutput.output` (1.5 command output)
/// or its text content blocks.
fn output_text(update: &Value) -> Option<String> {
    if let Some(text) = update["rawOutput"]["output"].as_str() {
        return Some(text.to_string());
    }
    if let Some(text) = update["rawOutput"].as_str() {
        return Some(text.to_string());
    }
    let blocks: Vec<&str> = update["content"]
        .as_array()?
        .iter()
        .filter_map(|c| c["content"]["text"].as_str())
        .filter(|t| !t.is_empty())
        .collect();
    (!blocks.is_empty()).then(|| blocks.join("\n"))
}

impl LiveFold {
    /// The live steps of one `session/update`'s `update`.
    pub(crate) fn push(&mut self, update: &Value) -> Vec<LiveStep> {
        match update.get("sessionUpdate").and_then(Value::as_str) {
            Some("agent_message_chunk") => self.chunk(update, Block::Text),
            Some("agent_thought_chunk") => self.chunk(update, Block::Thinking),
            Some("tool_call" | "tool_call_update") => self.tool(update),
            _ => Vec::new(),
        }
    }

    /// A steer starts a new block of reply text.
    pub(crate) fn steered(&mut self) {
        self.last = None;
    }

    fn chunk(&mut self, update: &Value, block: Block) -> Vec<LiveStep> {
        let Some(delta) = update["content"]["text"].as_str().filter(|t| !t.is_empty()) else {
            return Vec::new();
        };
        let new_block = self.last != Some(block);
        self.last = Some(block);
        let delta = delta.to_string();
        vec![match block {
            Block::Thinking => LiveStep::Thinking { delta, new_block },
            _ => LiveStep::Text { delta, new_block },
        }]
    }

    fn tool(&mut self, update: &Value) -> Vec<LiveStep> {
        let Some(id) = s(update, "toolCallId") else {
            return Vec::new();
        };
        let mut steps = Vec::new();
        let tool = match self.tools.get_mut(id) {
            Some(tool) => tool,
            None => {
                let (label, subject) = describe(update);
                steps.push(LiveStep::ToolStarted {
                    call_id: id.to_string(),
                    tool: label,
                    subject,
                });
                self.last = Some(Block::Tool);
                self.tools.entry(id.to_string()).or_insert(Tool {
                    execute: s(update, "kind") == Some("execute") && proxied_mcp(update).is_none(),
                    schedule: false,
                    daemon: update["rawInput"]["IsDaemon"].as_bool() == Some(true),
                    status: Status::Pending,
                    output: String::new(),
                })
            }
        };
        if tool.status == Status::Done {
            return steps;
        }
        if s(update, "title").is_some_and(|t| SCHEDULE_TITLES.contains(&t)) {
            tool.schedule = true;
        }
        if let Some(text) = output_text(update)
            && text != tool.output
        {
            let delta = match text.strip_prefix(tool.output.as_str()) {
                Some(rest) => rest.to_string(),
                // Replaced, not extended: show the new text on its own line.
                None if tool.output.is_empty() => text.clone(),
                None => format!("\n{text}"),
            };
            tool.output = text;
            steps.push(LiveStep::ToolOutput {
                call_id: id.to_string(),
                delta,
            });
        }
        match update.get("status").and_then(Value::as_str) {
            Some("pending") => tool.status = Status::Pending,
            Some("in_progress") => tool.status = Status::Running,
            Some(end @ ("completed" | "failed")) => {
                tool.status = Status::Done;
                steps.push(LiveStep::ToolFinished {
                    call_id: id.to_string(),
                    failed: end == "failed",
                });
            }
            _ => {}
        }
        steps
    }

    /// A command the agent backgrounded, or a scheduled wake, is still
    /// running: the agent will report it and reply by itself, so the turn
    /// stays open.
    pub(crate) fn background_running(&self) -> bool {
        self.tools
            .values()
            .any(|t| t.status == Status::Running && (t.schedule || (t.execute && !t.daemon)))
    }

    /// A scheduled wake has not fired. Unlike a command it reports nothing
    /// while it waits, so silence does not mean it was lost.
    pub(crate) fn schedule_pending(&self) -> bool {
        self.tools
            .values()
            .any(|t| t.schedule && t.status == Status::Running)
    }

    /// An `update_steps` update: `Some(plan)` for one that carries its
    /// steps (an ACP `plan` update), `Some(None)` for one that only names
    /// the call; `None` for any other update, which folds as usual.
    pub(crate) fn take_steps(&mut self, update: &Value) -> Option<Option<Value>> {
        if !matches!(
            update.get("sessionUpdate").and_then(Value::as_str),
            Some("tool_call" | "tool_call_update")
        ) {
            return None;
        }
        let id = s(update, "toolCallId")?;
        if s(update, "title") == Some(STEPS_TITLE)
            && matches!(s(update, "kind"), None | Some("other"))
        {
            self.steps_calls.insert(id.to_string());
        }
        if !self.steps_calls.contains(id) {
            return None;
        }
        if matches!(s(update, "status"), Some("completed" | "failed")) {
            self.steps_calls.remove(id);
        }
        let Some(steps) = update["rawInput"]["steps"].as_array() else {
            return Some(None);
        };
        let entries: Vec<Value> = steps
            .iter()
            .filter_map(|step| {
                let title = step.get("title").and_then(Value::as_str)?;
                let status = match step.get("status").and_then(Value::as_str) {
                    Some("done") => "completed",
                    Some(other) => other,
                    None => "pending",
                };
                Some(serde_json::json!({"content": title, "status": status, "priority": "medium"}))
            })
            .collect();
        Some(Some(
            serde_json::json!({"sessionUpdate": "plan", "entries": entries}),
        ))
    }

    /// Some tool (a daemon aside) has not finished.
    pub(crate) fn busy(&self) -> bool {
        self.tools
            .values()
            .any(|t| !t.daemon && t.status != Status::Done)
    }
}

/// Hand the steps to the bridge (`None`: nobody watches live turns).
pub(crate) fn apply(
    bridge: Option<&CliBridge>,
    thread: &ThreadId,
    run: &str,
    steps: Vec<LiveStep>,
) {
    let Some(bridge) = bridge else {
        return;
    };
    for step in steps {
        match step {
            LiveStep::Text { delta, new_block } => {
                bridge.text_delta(thread, run, &delta, new_block)
            }
            LiveStep::Thinking { delta, new_block } => {
                bridge.thinking_delta(thread, run, &delta, new_block)
            }
            LiveStep::ToolStarted {
                call_id,
                tool,
                subject,
            } => bridge.tool_observed(thread, run, &tool, subject.as_deref(), &call_id),
            LiveStep::ToolOutput { call_id, delta } => {
                bridge.tool_output(thread, run, &call_id, &delta)
            }
            LiveStep::ToolFinished { call_id, failed } => {
                bridge.tool_finished(thread, run, &call_id, failed)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_command_row_streams_its_output_and_holds_while_backgrounded() {
        let mut f = LiveFold::default();
        let call = |status: &str, output: Option<&str>| {
            let mut u = json!({
                "sessionUpdate": "tool_call_update", "toolCallId": "c1",
                "name": "run_command", "kind": "execute", "title": "sleep 9 && echo hi",
                "status": status,
                "rawInput": {"CommandLine": "sleep 9 && echo hi", "WaitMsBeforeAsync": 500},
            });
            if let Some(o) = output {
                u["rawOutput"] = json!({"output": o});
            }
            u
        };
        let mut first = call("in_progress", None);
        first["sessionUpdate"] = json!("tool_call");
        assert_eq!(
            f.push(&first),
            [LiveStep::ToolStarted {
                call_id: "c1".into(),
                tool: "run_command".into(),
                subject: Some("sleep 9 && echo hi".into()),
            }]
        );
        assert!(f.background_running() && f.busy());
        assert_eq!(
            f.push(&call("in_progress", Some("a"))),
            [LiveStep::ToolOutput {
                call_id: "c1".into(),
                delta: "a".into()
            }]
        );
        let done = f.push(&call("completed", Some("ab")));
        assert_eq!(
            done,
            [
                LiveStep::ToolOutput {
                    call_id: "c1".into(),
                    delta: "b".into()
                },
                LiveStep::ToolFinished {
                    call_id: "c1".into(),
                    failed: false
                }
            ]
        );
        assert!(!f.background_running() && !f.busy());
        // A finished row ignores late repeats.
        assert!(f.push(&call("completed", Some("ab"))).is_empty());
    }

    #[test]
    fn daemons_never_hold_a_turn() {
        let mut f = LiveFold::default();
        f.push(&json!({
            "sessionUpdate": "tool_call", "toolCallId": "d", "kind": "execute",
            "status": "in_progress", "rawInput": {"CommandLine": "npm run dev", "IsDaemon": true},
        }));
        assert!(!f.background_running() && !f.busy());
    }

    #[test]
    fn subagents_titleless_updates_and_location_only_rows() {
        let mut f = LiveFold::default();
        let steps = f.push(&json!({
            "sessionUpdate": "tool_call", "toolCallId": "s1", "name": "invoke_subagent",
            "kind": "other", "status": "in_progress",
            "rawInput": {"Role": "Reviewer", "Prompt": "check the diff"},
        }));
        assert_eq!(
            steps,
            [LiveStep::ToolStarted {
                call_id: "s1".into(),
                tool: "subagent".into(),
                subject: Some("Reviewer".into()),
            }]
        );
        // An MCP tool with a subagent-looking title is not a subagent.
        let steps = f.push(&json!({
            "sessionUpdate": "tool_call", "toolCallId": "m1", "title": "Run invoke_subagent",
            "_meta": {"is_mcp_tool_call": true},
        }));
        assert!(matches!(&steps[0], LiveStep::ToolStarted { tool, .. } if tool == "tool"));
        // Location-only first sighting names the file; a titleless update
        // afterwards keeps the row (no second start).
        let steps = f.push(&json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "e1", "kind": "edit",
            "locations": [{"path": "src/lib.rs"}],
        }));
        assert_eq!(
            steps,
            [LiveStep::ToolStarted {
                call_id: "e1".into(),
                tool: "edit".into(),
                subject: Some("src/lib.rs".into()),
            }]
        );
        assert!(
            f.push(&json!({"sessionUpdate": "tool_call_update", "toolCallId": "e1", "status": "in_progress"}))
                .is_empty()
        );
    }

    #[test]
    fn text_and_thinking_start_blocks_after_each_other_and_after_tools() {
        let mut f = LiveFold::default();
        let text = |t: &str| json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": t}});
        let think = |t: &str| json!({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": t}});
        let blocks: Vec<bool> = [think("a"), think("b"), text("c"), text("d")]
            .iter()
            .flat_map(|u| f.push(u))
            .map(|s| match s {
                LiveStep::Text { new_block, .. } | LiveStep::Thinking { new_block, .. } => {
                    new_block
                }
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(blocks, [true, false, true, false]);
        f.steered();
        assert!(matches!(
            f.push(&text("e"))[0],
            LiveStep::Text {
                new_block: true,
                ..
            }
        ));
    }
}
