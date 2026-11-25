//! Subagents inline, where they were launched (t3code's subagent rows):
//! each delegated task of the thread becomes a [`Row::Subagent`] in the
//! turn that started it, instead of a box above the composer.
//!
//! A `t3_delegate` call's result names the task it started (`taskId`, in
//! the loop's stored preview or a CLI's tool output), and that call's tool
//! row gives way to the task's subagent row, so the launch shows once. A
//! call whose result this view never saw pairs with its run's remaining
//! tasks in order; a call whose result named no task (a refusal) keeps its
//! row. A task with no call left goes at the end of its turn's work, above
//! the answer and before any steer sent after it; a task from before runs
//! were recorded goes to the turn whose prompt came last before it; one
//! older than the loaded history waits until that history loads.
//!
//! Tasks come from the orchestration poll ([`Transcript::set_tasks`]), so
//! the rows are rebuilt from stored data after a restart and follow the
//! child's status and newest words as the poll brings them.

use std::collections::{HashMap, HashSet};

use super::{Live, Row, ToolStatus, Transcript};
use crate::orchestration::{TaskStatus, TaskView};
use crate::wire::{MessageKind, MessageStatus};

/// What a CLI or the loop names the delegate call: `t3_delegate`,
/// `builtin.t3_delegate`, `mcp__ironclaw__t3_delegate`, ….
fn is_delegate_call(title: &str) -> bool {
    title
        .trim()
        .rsplit(['.', ' '])
        .next()
        .is_some_and(|name| name.ends_with("t3_delegate"))
}

/// The task id a delegate call's result names, in a JSON body or a CLI's
/// escaped copy of one: `"taskId":"task_…"`.
fn task_id_in(text: &str) -> Option<String> {
    let (_, after) = text.split_once("taskId")?;
    let id: String = after
        .trim_start_matches(['\\', '"', ':', ' '])
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        .collect();
    (!id.is_empty()).then_some(id)
}

/// The row key of a task's subagent row.
pub fn subagent_key(task_id: &str) -> String {
    format!("subagent:{task_id}")
}

/// A delegate call row: its key, its run, and the task its result named
/// (`Some(None)`: a result that named none; `None`: no result seen).
type Call<'a> = (&'a str, Option<&'a str>, Option<Option<&'a str>>);

/// Where a task's row goes.
enum Anchor {
    /// In the turn its prompt row (or delegated-result row) starts.
    Turn(String),
    /// After everything shown (its turn's prompt is not among the rows).
    End,
    /// Nowhere yet: it belongs to history older than what is loaded.
    Unloaded,
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

    /// A tool's result reached this view: for a delegate call, note the
    /// task it names (or that it named none).
    pub(super) fn note_delegate_result(
        &mut self,
        key: String,
        capability: &str,
        text: Option<&str>,
    ) {
        let delegate = is_delegate_call(capability)
            || matches!(self.live.get(&key).map(|e| &e.item),
                Some(Live::Tool { title, .. }) if is_delegate_call(title));
        if delegate && let Some(text) = text {
            self.delegate_results.insert(key, task_id_in(text));
        }
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

    /// Each live entry's run as [`Transcript::rows`] groups it: a frame
    /// without a run id belongs to the newest run seen before it.
    fn entry_runs(&self) -> HashMap<&str, Option<&str>> {
        let mut seen: Vec<Option<&str>> = Vec::new();
        let mut runs = HashMap::new();
        for key in &self.order {
            let run = self.live[key].run.as_deref();
            if run.is_none()
                && let Some(last) = seen.last()
            {
                runs.insert(key.as_str(), *last);
                continue;
            }
            if !seen.contains(&run) {
                seen.push(run);
            }
            runs.insert(key.as_str(), run);
        }
        runs
    }

    /// Where `task` goes when no call row takes it.
    fn anchor(&self, task: &TaskView, drawn: &HashSet<String>) -> Anchor {
        let shown = |m: &&crate::wire::TimelineMessage| {
            m.kind == MessageKind::User
                && m.is_visible()
                && !matches!(
                    m.status,
                    MessageStatus::Queued | MessageStatus::RejectedBusy
                )
                && drawn.contains(&self.key_of_message(&m.message_id))
        };
        let prompt = match &task.parent_run_id {
            Some(run) => self
                .durable
                .iter()
                .filter(shown)
                .find(|m| m.turn_run_id.as_deref() == Some(run.as_str())),
            // From before runs were recorded: the last prompt sent before it.
            None => self.durable.iter().filter(shown).rfind(|m| {
                m.created_at
                    .is_some_and(|at| at.timestamp_millis() <= task.created_at)
            }),
        };
        if let Some(m) = prompt {
            return Anchor::Turn(self.key_of_message(&m.message_id));
        }
        let oldest = self
            .durable
            .iter()
            .filter_map(|m| m.created_at)
            .map(|at| at.timestamp_millis())
            .min();
        match oldest {
            Some(oldest) if task.created_at < oldest => Anchor::Unloaded,
            _ => Anchor::End,
        }
    }

    /// `rows` with every task drawn as its subagent row.
    pub(super) fn place_subagents(&self, mut rows: Vec<Row>) -> Vec<Row> {
        if self.tasks.is_empty() {
            return rows;
        }
        let runs = self.entry_runs();
        let drawn: HashSet<String> = rows.iter().map(|r| r.key().to_string()).collect();
        // Delegate call rows in call order: key, run, and the task its
        // result named (`Some(None)`: a result that named none).
        let calls: Vec<Call<'_>> = self
            .order
            .iter()
            .filter(|key| drawn.contains(*key))
            .filter_map(|key| match &self.live[key].item {
                Live::Tool { title, status, .. } if is_delegate_call(title) => {
                    let named = self.delegate_results.get(key).map(Option::as_deref);
                    // A failed call whose result this view never saw
                    // started nothing it can show.
                    (named.is_some() || *status != ToolStatus::Failed).then(|| {
                        (
                            key.as_str(),
                            runs.get(key.as_str()).copied().flatten(),
                            named,
                        )
                    })
                }
                _ => None,
            })
            .collect();
        let mut taken: HashSet<usize> = HashSet::new();
        let mut slot: HashMap<&str, &str> = HashMap::new();
        // The call that names a task takes it.
        for task in &self.tasks {
            if let Some((ix, (key, ..))) = calls.iter().enumerate().find(|(ix, (_, _, named))| {
                !taken.contains(ix) && *named == Some(Some(task.task_id.as_str()))
            }) {
                taken.insert(ix);
                slot.insert(task.task_id.as_str(), key);
            }
        }
        // A call whose result this view never saw: its run's next task.
        for task in &self.tasks {
            if slot.contains_key(task.task_id.as_str()) {
                continue;
            }
            let Some(run) = task.parent_run_id.as_deref() else {
                continue;
            };
            if let Some((ix, (key, ..))) = calls.iter().enumerate().find(|(ix, (_, r, named))| {
                !taken.contains(ix) && named.is_none() && *r == Some(run)
            }) {
                taken.insert(ix);
                slot.insert(task.task_id.as_str(), key);
            }
        }
        let mut unplaced: Vec<(&TaskView, Anchor)> = Vec::new();
        for task in &self.tasks {
            let row = subagent_row(task);
            match slot.get(task.task_id.as_str()) {
                Some(key) => {
                    if let Some(at) = rows.iter_mut().find(|r| r.key() == *key) {
                        *at = row;
                    }
                }
                None => unplaced.push((task, self.anchor(task, &drawn))),
            }
        }
        let sent: HashMap<String, i64> = self
            .durable
            .iter()
            .filter(|m| m.kind == MessageKind::User)
            .filter_map(|m| {
                Some((
                    self.key_of_message(&m.message_id),
                    m.created_at?.timestamp_millis(),
                ))
            })
            .collect();
        for (task, anchor) in unplaced {
            let at = match anchor {
                Anchor::Unloaded => continue,
                Anchor::End => rows.len(),
                Anchor::Turn(prompt) => turn_end(&rows, &prompt, task.created_at, &sent),
            };
            rows.insert(at, subagent_row(task));
        }
        rows
    }
}

/// A task's row, its preview worked out once here rather than per frame.
fn subagent_row(task: &TaskView) -> Row {
    Row::Subagent {
        key: subagent_key(&task.task_id),
        task: Box::new(task.clone()),
        preview: preview(task),
    }
}

/// Where a task with no call row goes in the turn `prompt` starts: after
/// the work, above the answer, and before any steer sent after the task
/// started; the end when the prompt is not among the rows.
fn turn_end(rows: &[Row], prompt: &str, created_at: i64, sent: &HashMap<String, i64>) -> usize {
    let Some(start) = rows.iter().position(|r| r.key() == prompt) else {
        return rows.len();
    };
    let body = start + 1;
    let mut end = rows[body..]
        .iter()
        .position(|r| matches!(r, Row::User { steer: None, .. } | Row::Delegated { .. }))
        .map_or(rows.len(), |n| body + n);
    if let Some(n) = rows[body..end].iter().position(|r| {
        matches!(r, Row::User { key, steer: Some(_), .. }
            if sent.get(key).is_some_and(|at| *at > created_at))
    }) {
        end = body + n;
    }
    rows[body..end]
        .iter()
        .rposition(|r| matches!(r, Row::Assistant { .. }))
        .map_or(end, |n| body + n)
}

/// The server's placeholder when a child ends without output; the status
/// already says it.
const GENERIC_END: &str = "child task ended with status";

/// How long a subagent's preview line gets: one truncated line's worth.
const PREVIEW_CHARS: usize = 240;

/// A subagent row's second line (t3code's subagent detail): why it failed,
/// what it found once finished, else what it is saying or doing now.
pub fn preview(task: &TaskView) -> Option<String> {
    let raw = match task.status {
        TaskStatus::Failed | TaskStatus::Interrupted => {
            task.error.as_deref().or(task.summary.as_deref())
        }
        status if !status.is_live() => task.summary.as_deref(),
        _ => task.progress.as_ref().and_then(|p| {
            p.latest_output
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .or(p.last_tool.as_deref())
        }),
    }?;
    let line = plain_line(raw, PREVIEW_CHARS);
    (!line.is_empty() && !line.to_ascii_lowercase().starts_with(GENERIC_END)).then_some(line)
}

/// Markdown as one plain line of at most `max` characters: link targets,
/// code ticks and list bullets dropped, whitespace collapsed (t3code's
/// `plainDetail`). Reads only as much of `text` as the line can show.
pub fn plain_line(text: &str, max: usize) -> String {
    // Markup and runs of whitespace collapse, so a few times the line's
    // length always fills it.
    let text = match text.char_indices().nth(max * 4) {
        Some((cut, _)) => text.get(..cut).unwrap_or(text),
        None => text,
    };
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
    let line = plain
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
        .replace('`', "");
    if line.chars().count() <= max {
        return line;
    }
    let cut: String = line.chars().take(max).collect();
    format!("{}…", cut.trim_end())
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

    /// The delegate call a stored or streamed result names takes its own
    /// task, whatever the order the tasks were made in (review 3): a
    /// refusal keeps its row and takes no task, an idempotent retry names
    /// a task already shown and keeps its row, a child that never started
    /// shows as its failed task in its own call's place.
    #[test]
    fn a_call_takes_the_task_its_result_names() {
        let mut t = Transcript::default();
        t.set_timeline(page(vec![
            message("u1", 1, "user", "run-1", 1_000, "Go"),
            message("a1", 9, "assistant", "run-1", 9_000, "Done."),
        ]));
        let results = [
            (
                "1",
                ToolStatus::Completed,
                r#"{"ok":true,"taskId":"task_b"}"#,
            ),
            (
                "2",
                ToolStatus::Completed,
                r#"{"ok":false,"error":"too_many_tasks"}"#,
            ),
            (
                "3",
                ToolStatus::Completed,
                r#"{"ok":true,"taskId":"task_a"}"#,
            ),
            (
                "4",
                ToolStatus::Completed,
                r#"{"ok":true,"taskId":"task_a"}"#,
            ),
            // A CLI's escaped copy of a failed start's body.
            (
                "5",
                ToolStatus::Failed,
                r#"{\"ok\":false,\"taskId\":\"task_c\"}"#,
            ),
        ];
        for (inv, status, body) in results {
            t.upsert_tool(
                inv.into(),
                Some("run-1".into()),
                "t3_delegate".into(),
                None,
                status,
            );
            if let Some(entry) = t.live.get_mut(&format!("tool:{inv}")) {
                entry.at = 2_000;
            }
            t.note_delegate_result(format!("tool:{inv}"), "builtin.t3_delegate", Some(body));
        }
        t.set_tasks(vec![
            task("task_a", Some("run-1"), 3_000, "running"),
            task("task_b", Some("run-1"), 3_000, "completed"),
            task("task_c", Some("run-1"), 3_000, "failed"),
        ]);
        assert_eq!(
            keys(&t.rows()),
            vec![
                "msg:u1",
                "subagent:task_b",
                "tool:2",
                "subagent:task_a",
                "tool:4",
                "subagent:task_c",
                "msg:a1",
            ]
        );
    }

    /// A live tool frame without a run id belongs to the run it arrived in,
    /// as the rows group it: its task takes it rather than showing twice
    /// (review 6).
    #[test]
    fn a_bare_delegate_frame_pairs_with_its_runs_task() {
        let mut t = Transcript::default();
        t.upsert_tool(
            "1".into(),
            Some("run-1".into()),
            "Read".into(),
            None,
            ToolStatus::Completed,
        );
        t.upsert_tool(
            "2".into(),
            None,
            "t3_delegate".into(),
            None,
            ToolStatus::Completed,
        );
        if let Some(entry) = t.live.get_mut("tool:2") {
            entry.run = None;
        }
        t.set_tasks(vec![task("a", Some("run-1"), 3_000, "running")]);
        assert_eq!(keys(&t.rows()), vec!["tool:1", "subagent:a"]);
    }

    /// Turns end at a delegated result as well as at a prompt (review 2);
    /// a task is drawn before a steer sent after it started (review 7);
    /// one older than the loaded history waits for it (review 5).
    #[test]
    fn a_task_stays_in_its_own_turn() {
        let mut t = Transcript::default();
        let mut steer = message("s1", 3, "user", "run-1", 4_000, "Also check B");
        steer["steer"] = json!({"id": "st1", "state": "read"});
        t.set_timeline(page(vec![
            message("u1", 1, "user", "run-1", 1_000, "Check A"),
            message("a0", 2, "assistant", "run-1", 2_000, "Looking."),
            steer,
            message("a1", 4, "assistant", "run-1", 5_000, "Both fine."),
            message(
                "d1",
                5,
                "user",
                "run-2",
                6_000,
                "Delegated work finished.\n[T3 delegated task \"Look\"] done",
            ),
            message("a2", 6, "assistant", "run-2", 7_000, "Noted."),
        ]));
        let rows = keys(&t.rows());
        assert!(rows.contains(&"msg:d1".to_string()), "{rows:?}");
        t.set_tasks(vec![
            task("early", Some("run-1"), 1_500, "completed"),
            task("late", Some("run-1"), 4_500, "running"),
            task("old", Some("run-0"), 500, "completed"),
        ]);
        let rows = keys(&t.rows());
        let at = |k: &str| rows.iter().position(|r| r == k);
        assert!(
            at("subagent:old").is_none(),
            "older than what is loaded: {rows:?}"
        );
        assert!(
            at("subagent:early") < at("msg:s1"),
            "before the later steer: {rows:?}"
        );
        assert!(at("subagent:late") > at("msg:s1"), "{rows:?}");
        assert!(
            at("subagent:late") < at("msg:d1"),
            "inside its own turn: {rows:?}"
        );
    }

    /// The preview reads only what one line shows (review 4).
    #[test]
    fn a_long_result_previews_one_bounded_line() {
        let mut done = task("d", None, 0, "completed");
        done.summary = Some("word ".repeat(20_000));
        let line = preview(&done).expect("a line");
        assert!(line.chars().count() <= PREVIEW_CHARS + 1, "{}", line.len());
        assert!(line.ends_with('…'));
        assert_eq!(plain_line("a  b", 10), "a b");
        assert_eq!(
            task_id_in(r#"{"taskId": "task_9f"}"#).as_deref(),
            Some("task_9f")
        );
        assert_eq!(task_id_in(r#"{"ok":false}"#), None);
    }
}
