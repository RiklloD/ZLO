//! Subagents inline, where they were launched (t3code's subagent rows):
//! each delegated task of the thread becomes a [`Row::Subagent`] in the
//! turn that started it, instead of a box above the composer.
//!
//! A task names the parent run that launched it (`parentRunId`). Within a
//! run, its tasks pair up in order with the run's `t3_delegate` calls (one
//! call starts one task), and each paired call's tool row gives way to the
//! subagent row, so the launch shows once. A call that failed started
//! nothing and keeps its row. A task with no call left to take goes at the
//! end of its turn's work, above the turn's answer; a task from before the
//! anchor existed goes to the turn whose prompt came last before it.
//!
//! Tasks come from the orchestration poll ([`Transcript::set_tasks`]), so
//! the rows are rebuilt from stored data after a restart and follow the
//! child's status and newest words as the poll brings them.

use std::collections::HashSet;

use super::{Live, Row, ToolStatus, Transcript};
use crate::orchestration::{TaskStatus, TaskView};
use crate::wire::MessageKind;

/// What a CLI or the loop names the delegate call: `t3_delegate`,
/// `builtin.t3_delegate`, `mcp__ironclaw__t3_delegate`, ….
fn is_delegate_call(title: &str) -> bool {
    title
        .trim()
        .rsplit(['.', ' '])
        .next()
        .is_some_and(|name| name.ends_with("t3_delegate"))
}

/// The row key of a task's subagent row.
pub fn subagent_key(task_id: &str) -> String {
    format!("subagent:{task_id}")
}

impl Transcript {
    /// The thread's delegated tasks, as the orchestration poll read them.
    /// Returns whether anything a row shows changed.
    pub fn set_tasks(&mut self, mut tasks: Vec<TaskView>) -> bool {
        tasks.sort_by(|a, b| {
            (a.created_at, a.task_id.as_str()).cmp(&(b.created_at, b.task_id.as_str()))
        });
        if tasks == self.tasks {
            return false;
        }
        self.tasks = tasks;
        true
    }

    /// Delegate calls this view saw finish. When the count grows while a
    /// run works, a task was just started: the app reads the tasks then,
    /// not at its next poll, so the subagent row takes the call's place
    /// at once.
    pub fn settled_delegate_calls(&self) -> usize {
        self.live
            .values()
            .filter(|e| {
                matches!(&e.item, Live::Tool { title, status, .. }
                    if *status == ToolStatus::Completed && is_delegate_call(title))
            })
            .count()
    }

    /// The run a task belongs to: its anchor, or for a task from before
    /// anchors, the run of the last prompt sent before it.
    fn task_run(&self, task: &TaskView) -> Option<String> {
        if task.parent_run_id.is_some() {
            return task.parent_run_id.clone();
        }
        self.durable
            .iter()
            .filter(|m| m.kind == MessageKind::User && m.is_visible())
            .filter(|m| {
                m.created_at
                    .is_some_and(|at| at.timestamp_millis() <= task.created_at)
            })
            .filter_map(|m| m.turn_run_id.clone())
            .next_back()
    }

    /// `rows` with every task drawn as its subagent row.
    pub(super) fn place_subagents(&self, mut rows: Vec<Row>) -> Vec<Row> {
        if self.tasks.is_empty() {
            return rows;
        }
        let drawn: HashSet<&str> = rows.iter().map(Row::key).collect();
        // Each run's delegate calls that started a task, in call order.
        let calls: Vec<(String, Option<&str>)> = self
            .order
            .iter()
            .filter(|key| drawn.contains(key.as_str()))
            .filter_map(|key| {
                let entry = &self.live[key];
                match &entry.item {
                    Live::Tool { title, status, .. }
                        if *status != ToolStatus::Failed && is_delegate_call(title) =>
                    {
                        Some((key.clone(), entry.run.as_deref()))
                    }
                    _ => None,
                }
            })
            .collect();
        let mut taken: HashSet<usize> = HashSet::new();
        let mut unplaced: Vec<(Option<String>, Row)> = Vec::new();
        for task in &self.tasks {
            let row = Row::Subagent {
                key: subagent_key(&task.task_id),
                task: Box::new(task.clone()),
            };
            let run = self.task_run(task);
            let call = calls
                .iter()
                .enumerate()
                .find(|(ix, (_, r))| !taken.contains(ix) && run.is_some() && *r == run.as_deref());
            match call {
                Some((ix, (key, _))) => {
                    taken.insert(ix);
                    if let Some(slot) = rows.iter_mut().find(|r| r.key() == key) {
                        *slot = row;
                    }
                }
                None => unplaced.push((run, row)),
            }
        }
        for (run, row) in unplaced {
            let at = self.turn_end(&rows, run.as_deref());
            rows.insert(at, row);
        }
        rows
    }

    /// Where a task with no call row goes: after its turn's work, above the
    /// turn's answer; the end when the turn's prompt is not among the rows.
    fn turn_end(&self, rows: &[Row], run: Option<&str>) -> usize {
        let prompt = run.and_then(|run| {
            self.durable
                .iter()
                .find(|m| m.kind == MessageKind::User && m.turn_run_id.as_deref() == Some(run))
                .map(|m| self.key_of_message(&m.message_id))
        });
        let Some(start) = prompt.and_then(|key| rows.iter().position(|r| r.key() == key)) else {
            return rows.len();
        };
        let end = rows[start + 1..]
            .iter()
            .position(|r| matches!(r, Row::User { steer: None, .. }))
            .map_or(rows.len(), |n| start + 1 + n);
        // Above the answer, and below any subagent already placed there.
        rows[start + 1..end]
            .iter()
            .rposition(|r| matches!(r, Row::Assistant { .. }))
            .map_or(end, |n| start + 1 + n)
    }
}

/// The server's placeholder when a child ends without output; the status
/// already says it.
const GENERIC_END: &str = "child task ended with status";

/// A subagent row's second line (t3code's subagent detail): why it failed,
/// what it found once finished, else what it is saying or doing now.
pub fn preview(task: &TaskView) -> Option<String> {
    let raw = match task.status {
        TaskStatus::Failed | TaskStatus::Interrupted => {
            task.error.clone().or_else(|| task.summary.clone())
        }
        status if !status.is_live() => task.summary.clone(),
        _ => task.progress.as_ref().and_then(|p| {
            p.latest_output
                .clone()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| p.last_tool.clone())
        }),
    }?;
    let line = plain_line(&raw);
    (!line.is_empty() && !line.to_ascii_lowercase().starts_with(GENERIC_END)).then_some(line)
}

/// Markdown as one plain line: link targets, code ticks and list bullets
/// dropped, whitespace collapsed (t3code's `plainDetail`).
pub fn plain_line(text: &str) -> String {
    let mut plain = String::with_capacity(text.len());
    let mut rest = text;
    while let Some((before, after)) = rest.split_once('[') {
        plain.push_str(before);
        let link = after
            .split_once("](")
            .filter(|(label, _)| !label.contains('['))
            .and_then(|(label, tail)| tail.split_once(')').map(|(_, next)| (label, next)));
        match link {
            Some((label, next)) => {
                plain.push_str(label);
                rest = next;
            }
            None => {
                plain.push('[');
                rest = after;
            }
        }
    }
    plain.push_str(rest);
    plain
        .lines()
        .map(|line| {
            let line = line.trim_start();
            line.strip_prefix("- ")
                .or_else(|| line.strip_prefix("* "))
                .unwrap_or(line)
        })
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ")
        .replace('`', "")
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::wire::TimelinePage;

    fn task(id: &str, run: Option<&str>, at: i64, status: &str) -> TaskView {
        serde_json::from_value(json!({
            "taskId": id, "title": format!("Task {id}"), "status": status,
            "parentRunId": run, "createdAt": at, "childThreadId": format!("child-{id}"),
        }))
        .expect("task decodes")
    }

    fn message(id: &str, seq: u64, kind: &str, run: &str, at: i64, text: &str) -> Value {
        let at = chrono::DateTime::from_timestamp_millis(at).expect("a time");
        json!({
            "message_id": id, "sequence": seq, "kind": kind, "status": "completed",
            "created_at": at.to_rfc3339(), "turn_run_id": run, "content": text,
        })
    }

    fn page(messages: Vec<Value>) -> TimelinePage {
        serde_json::from_value(json!({"thread": {"thread_id": "t1"}, "messages": messages}))
            .expect("timeline decodes")
    }

    /// A transcript with one settled turn (`run-1`): a prompt, a read, three
    /// delegate calls (the second failed), and the answer.
    fn settled() -> Transcript {
        let mut t = Transcript::default();
        t.set_timeline(page(vec![
            message("u1", 1, "user", "run-1", 1_000, "Check both"),
            message("a1", 9, "assistant", "run-1", 9_000, "Started two."),
        ]));
        for (inv, title, status) in [
            ("1", "Read", ToolStatus::Completed),
            ("2", "t3_delegate", ToolStatus::Completed),
            ("3", "mcp__ironclaw__t3_delegate", ToolStatus::Failed),
            ("4", "builtin.t3_delegate", ToolStatus::Completed),
        ] {
            t.upsert_tool(inv.into(), Some("run-1".into()), title.into(), None, status);
            // Inside the turn, before its answer (a live entry is stamped
            // when this view first saw it).
            if let Some(entry) = t.live.get_mut(&format!("tool:{inv}")) {
                entry.at = 2_000;
            }
        }
        t
    }

    fn keys(rows: &[Row]) -> Vec<String> {
        rows.iter().map(|r| r.key().to_string()).collect()
    }

    #[test]
    fn a_task_takes_its_delegate_calls_place_and_a_failed_call_stays() {
        let mut t = settled();
        assert!(t.set_tasks(vec![
            task("b", Some("run-1"), 5_000, "running"),
            task("a", Some("run-1"), 4_000, "completed"),
        ]));
        let rows = t.rows();
        assert_eq!(
            keys(&rows),
            vec![
                "msg:u1",
                "tool:1",
                "subagent:a",
                "tool:3",
                "subagent:b",
                "msg:a1",
            ],
            "each task replaces its own call, in order; the failed call stays"
        );
        assert_eq!(
            t.settled_delegate_calls(),
            2,
            "the failed call started nothing"
        );
        assert!(!t.set_tasks(vec![
            task("a", Some("run-1"), 4_000, "completed"),
            task("b", Some("run-1"), 5_000, "running"),
        ]));
    }

    #[test]
    fn a_task_without_a_call_goes_above_its_turns_answer_and_old_tasks_find_their_turn() {
        let mut t = Transcript::default();
        t.set_timeline(page(vec![
            message("u1", 1, "user", "run-1", 1_000, "First"),
            message("a1", 2, "assistant", "run-1", 2_000, "One"),
            message("u2", 3, "user", "run-2", 3_000, "Second"),
            message("a2", 4, "assistant", "run-2", 4_000, "Two"),
        ]));
        t.set_tasks(vec![
            task("anchored", Some("run-1"), 1_500, "running"),
            // From before anchors: its turn is the last prompt before it.
            task("legacy", None, 3_500, "completed"),
            // A run whose prompt is not shown: the end.
            task("stray", Some("run-elsewhere"), 9_000, "running"),
        ]);
        assert_eq!(
            keys(&t.rows()),
            vec![
                "msg:u1",
                "subagent:anchored",
                "msg:a1",
                "msg:u2",
                "subagent:legacy",
                "msg:a2",
                "subagent:stray",
            ]
        );
    }

    #[test]
    fn the_preview_reads_failure_result_then_live_words() {
        let mut failed = task("f", None, 0, "failed");
        failed.error = Some("Usage limit\nreached".into());
        assert_eq!(preview(&failed).as_deref(), Some("Usage limit reached"));
        let mut done = task("d", None, 0, "completed");
        done.summary = Some("**Fixed** it in `pending.rs` — see [the PR](https://x/1)".into());
        assert_eq!(
            preview(&done).as_deref(),
            Some("**Fixed** it in pending.rs — see the PR")
        );
        done.summary = Some("Child task ended with status completed".into());
        assert_eq!(preview(&done), None, "the status already says it");
        let mut live: TaskView = serde_json::from_value(json!({
            "taskId": "l", "status": "running",
            "progress": {"lastTool": "Read(a.rs)", "latestOutput": "  "}
        }))
        .expect("task decodes");
        assert_eq!(preview(&live).as_deref(), Some("Read(a.rs)"));
        live.progress = None;
        assert_eq!(preview(&live), None);
        assert!(is_delegate_call("mcp_ironclaw_t3_delegate"));
        assert!(!is_delegate_call("t3_task"));
    }
}
