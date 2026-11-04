//! What a Codex turn shows while it runs, from the app-server notifications
//! the outcome fold ([`super::fold`]) does not need: reply and reasoning
//! summary deltas, tool rows with their streaming output, the turn's diff
//! (and file-change previews before it), and a review's findings. Pure: it
//! turns one notification into [`LiveStep`]s; [`apply`] hands them to the
//! bridge.
//!
//! Plan items (`turn/plan/updated`, `item/plan/delta`) are not handled here.

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
    /// The turn's whole diff so far.
    Diff(String),
    Review(String),
}

/// Which blocks the live turn is in, so a new message or summary part
/// starts its own paragraph.
#[derive(Debug, Default)]
pub(crate) struct LiveFold {
    turn_id: Option<String>,
    text_item: Option<String>,
    /// `(itemId, summaryIndex)` of the summary part being written.
    thinking_part: Option<(String, i64)>,
    /// Codex's own turn diff arrived: previews no longer stand in for it.
    has_turn_diff: bool,
}

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

impl LiveFold {
    pub(crate) fn for_turn(turn_id: Option<String>) -> Self {
        Self {
            turn_id,
            ..Self::default()
        }
    }

    fn owns(&self, params: &Value) -> bool {
        match (s(params, "turnId"), self.turn_id.as_deref()) {
            (Some(id), Some(ours)) => id == ours,
            _ => true,
        }
    }

    /// The live steps one notification makes.
    pub(crate) fn push(&mut self, method: &str, params: &Value) -> Vec<LiveStep> {
        if !self.owns(params) {
            return Vec::new();
        }
        match method {
            "item/agentMessage/delta" => {
                let (Some(item), Some(delta)) = (s(params, "itemId"), s(params, "delta")) else {
                    return Vec::new();
                };
                let new_block = self.text_item.as_deref() != Some(item);
                self.text_item = Some(item.to_string());
                vec![LiveStep::Text {
                    delta: delta.to_string(),
                    new_block,
                }]
            }
            "item/reasoning/summaryTextDelta" => {
                let (Some(item), Some(delta)) = (s(params, "itemId"), s(params, "delta")) else {
                    return Vec::new();
                };
                let part = (
                    item.to_string(),
                    params
                        .get("summaryIndex")
                        .and_then(Value::as_i64)
                        .unwrap_or(0),
                );
                let new_block = self.thinking_part.as_ref() != Some(&part);
                self.thinking_part = Some(part);
                vec![LiveStep::Thinking {
                    delta: delta.to_string(),
                    new_block,
                }]
            }
            "item/commandExecution/outputDelta" => {
                match (s(params, "itemId"), s(params, "delta")) {
                    (Some(item), Some(delta)) if !delta.is_empty() => vec![LiveStep::ToolOutput {
                        call_id: item.to_string(),
                        delta: delta.to_string(),
                    }],
                    _ => Vec::new(),
                }
            }
            "item/started" => tool_row(&params["item"]).into_iter().collect(),
            "item/completed" => self.completed(&params["item"]),
            "turn/diff/updated" => match s(params, "diff") {
                Some(diff) => {
                    self.has_turn_diff = true;
                    vec![LiveStep::Diff(diff.to_string())]
                }
                None => Vec::new(),
            },
            "item/fileChange/patchUpdated" if !self.has_turn_diff => {
                let preview = preview_diff(params.get("changes").unwrap_or(&Value::Null));
                if preview.is_empty() {
                    Vec::new()
                } else {
                    vec![LiveStep::Diff(preview)]
                }
            }
            _ => Vec::new(),
        }
    }

    fn completed(&mut self, item: &Value) -> Vec<LiveStep> {
        if s(item, "type") == Some("exitedReviewMode") {
            return s(item, "review")
                .filter(|r| !r.trim().is_empty())
                .map(|r| LiveStep::Review(r.to_string()))
                .into_iter()
                .collect();
        }
        let Some(row) = tool_row(item) else {
            return Vec::new();
        };
        let LiveStep::ToolStarted { call_id, .. } = &row else {
            return Vec::new();
        };
        let failed = matches!(s(item, "status"), Some("failed" | "declined"))
            || item
                .get("exitCode")
                .and_then(Value::as_i64)
                .is_some_and(|code| code != 0);
        let done = LiveStep::ToolFinished {
            call_id: call_id.clone(),
            failed,
        };
        // A call that never started (declined first) still gets its row.
        vec![row, done]
    }
}

/// A tool row for one item, named like its approval (`shell`,
/// `apply_patch`) or `mcp__server__tool`.
fn tool_row(item: &Value) -> Option<LiveStep> {
    let call_id = s(item, "id")?.to_string();
    let (tool, subject) = match s(item, "type")? {
        "commandExecution" => ("shell".to_string(), s(item, "command").map(str::to_string)),
        "fileChange" => {
            let paths: Vec<&str> = item["changes"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| s(c, "path"))
                .collect();
            (
                "apply_patch".to_string(),
                (!paths.is_empty()).then(|| paths.join(", ")),
            )
        }
        "mcpToolCall" => (
            format!(
                "mcp__{}__{}",
                s(item, "server").unwrap_or("mcp"),
                s(item, "tool").unwrap_or("tool")
            ),
            None,
        ),
        "dynamicToolCall" => (s(item, "tool").unwrap_or("tool").to_string(), None),
        "webSearch" => (
            "web_search".to_string(),
            s(item, "query").map(str::to_string),
        ),
        _ => return None,
    };
    Some(LiveStep::ToolStarted {
        call_id,
        tool,
        subject,
    })
}

/// File-change previews (`FileUpdateChange[]`) as one unified diff.
fn preview_diff(changes: &Value) -> String {
    let mut out = String::new();
    for change in changes.as_array().into_iter().flatten() {
        let (Some(path), Some(diff)) = (s(change, "path"), s(change, "diff")) else {
            continue;
        };
        if diff.trim().is_empty() {
            continue;
        }
        let headed = diff.starts_with("diff --git") || diff.starts_with("--- ");
        if !headed {
            let kind = change["kind"]["type"].as_str().unwrap_or("update");
            let (old, new) = match kind {
                "add" => ("/dev/null".to_string(), format!("b/{path}")),
                "delete" => (format!("a/{path}"), "/dev/null".to_string()),
                _ => (format!("a/{path}"), format!("b/{path}")),
            };
            out.push_str(&format!(
                "diff --git a/{path} b/{path}\n--- {old}\n+++ {new}\n"
            ));
        }
        out.push_str(diff);
        if !diff.ends_with('\n') {
            out.push('\n');
        }
    }
    out
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
            LiveStep::Diff(diff) => bridge.turn_diff(thread, run, &diff),
            LiveStep::Review(text) => bridge.review(thread, run, &text),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fold() -> LiveFold {
        LiveFold::for_turn(Some("t1".into()))
    }

    #[test]
    fn reasoning_summary_parts_and_messages_start_new_blocks() {
        let mut f = fold();
        let think = |f: &mut LiveFold, idx: i64, d: &str| {
            f.push(
                "item/reasoning/summaryTextDelta",
                &json!({"turnId": "t1", "itemId": "r1", "summaryIndex": idx, "delta": d}),
            )
        };
        assert_eq!(
            think(&mut f, 0, "**Plan**"),
            vec![LiveStep::Thinking {
                delta: "**Plan**".into(),
                new_block: true
            }]
        );
        assert!(matches!(
            think(&mut f, 0, " more")[0],
            LiveStep::Thinking {
                new_block: false,
                ..
            }
        ));
        assert!(matches!(
            think(&mut f, 1, "next")[0],
            LiveStep::Thinking {
                new_block: true,
                ..
            }
        ));
        let text = |f: &mut LiveFold, item: &str| {
            f.push(
                "item/agentMessage/delta",
                &json!({"turnId": "t1", "itemId": item, "delta": "x"}),
            )
        };
        assert!(matches!(
            text(&mut f, "m1")[0],
            LiveStep::Text {
                new_block: true,
                ..
            }
        ));
        assert!(matches!(
            text(&mut f, "m1")[0],
            LiveStep::Text {
                new_block: false,
                ..
            }
        ));
        assert!(matches!(
            text(&mut f, "m2")[0],
            LiveStep::Text {
                new_block: true,
                ..
            }
        ));
        // Another turn's deltas are not this turn's.
        assert!(
            f.push(
                "item/agentMessage/delta",
                &json!({"turnId": "t0", "itemId": "m9", "delta": "x"})
            )
            .is_empty()
        );
    }

    #[test]
    fn command_rows_stream_output_and_finish_with_their_exit_code() {
        let mut f = fold();
        let item = json!({"type": "commandExecution", "id": "c1", "command": "cargo test",
            "status": "inProgress", "exitCode": null});
        assert_eq!(
            f.push("item/started", &json!({"turnId": "t1", "item": item})),
            vec![LiveStep::ToolStarted {
                call_id: "c1".into(),
                tool: "shell".into(),
                subject: Some("cargo test".into())
            }]
        );
        assert_eq!(
            f.push(
                "item/commandExecution/outputDelta",
                &json!({"turnId": "t1", "itemId": "c1", "delta": "running 3 tests\n"})
            ),
            vec![LiveStep::ToolOutput {
                call_id: "c1".into(),
                delta: "running 3 tests\n".into()
            }]
        );
        let done = json!({"type": "commandExecution", "id": "c1", "command": "cargo test",
            "status": "completed", "exitCode": 101});
        let steps = f.push("item/completed", &json!({"turnId": "t1", "item": done}));
        assert_eq!(
            steps[1],
            LiveStep::ToolFinished {
                call_id: "c1".into(),
                failed: true
            }
        );
        // Conversation items are not tool rows.
        let reasoning = json!({"type": "reasoning", "id": "r1", "summary": [], "content": []});
        assert!(
            f.push("item/started", &json!({"turnId": "t1", "item": reasoning}))
                .is_empty()
        );
    }

    #[test]
    fn patch_previews_stand_in_until_the_turn_diff_arrives() {
        let mut f = fold();
        let changes = json!([{"path": "src/a.rs", "kind": {"type": "update", "move_path": null},
            "diff": "@@ -1 +1 @@\n-a\n+b\n"}]);
        let steps = f.push(
            "item/fileChange/patchUpdated",
            &json!({"turnId": "t1", "itemId": "p1", "changes": changes}),
        );
        let LiveStep::Diff(preview) = &steps[0] else {
            panic!("{steps:?}")
        };
        assert!(
            preview.starts_with(
                "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@"
            )
        );
        let turn_diff = "diff --git a/src/a.rs b/src/a.rs\n";
        assert_eq!(
            f.push(
                "turn/diff/updated",
                &json!({"threadId": "x", "turnId": "t1", "diff": turn_diff})
            ),
            vec![LiveStep::Diff(turn_diff.into())]
        );
        assert!(
            f.push(
                "item/fileChange/patchUpdated",
                &json!({"turnId": "t1", "itemId": "p2", "changes": changes})
            )
            .is_empty()
        );
    }

    #[test]
    fn exited_review_mode_carries_the_findings() {
        let mut f = fold();
        let item = json!({"type": "exitedReviewMode", "id": "rv", "review": "Looks fine.\n\nReview comment:\n\n- [P2] Leak — /w/a.rs:3-4\n  body"});
        assert!(matches!(
            &f.push("item/completed", &json!({"turnId": "t1", "item": item}))[0],
            LiveStep::Review(text) if text.contains("[P2] Leak")
        ));
    }
}
