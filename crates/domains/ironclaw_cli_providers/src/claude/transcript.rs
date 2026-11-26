//! Thread history from the CLI's own session journal
//! (`<config>/projects/<encoded cwd>/<session>.jsonl`).

use std::collections::VecDeque;
use std::path::Path;

use ironclaw_orchestrator::runtime::{HistoryMessage, HistoryRole, RuntimeError};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};

pub(super) async fn read(path: &Path, limit: usize) -> Result<Vec<HistoryMessage>, RuntimeError> {
    let unavailable =
        |e: std::io::Error| RuntimeError::Unavailable(format!("claude transcript: {}", e.kind()));
    let file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        // No turn has run yet.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(unavailable(e)),
    };
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    // A prompt's journal `uuid` is the run id the driver sent it with; the
    // rows after it, up to the next prompt, belong to that run. Every row is
    // scanned so a row inside the window still names a prompt before it, but
    // only the newest `limit` messages are kept.
    let mut run: Option<String> = None;
    let mut out: VecDeque<HistoryMessage> = VecDeque::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader
            .read_until(b'\n', &mut line)
            .await
            .map_err(unavailable)?
            == 0
        {
            break;
        }
        // silent-ok: a torn last line of a live journal is skipped
        let Ok(v) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        let m = if let Some(mut m) = steer(&v) {
            // A steer sent mid-turn: the person's row, inside the run.
            m.run_id = run.clone();
            m.at = timestamp(&v);
            m
        } else {
            let Some(mut m) = message(&v) else {
                continue;
            };
            m.hidden = compact_scaffolding(&v, &m);
            m.at = timestamp(&v);
            // Hidden rows (a compaction's summary) stay in the current run.
            if m.role == HistoryRole::User && !m.hidden {
                run = v["uuid"].as_str().map(str::to_string);
            }
            m.run_id = run.clone();
            m
        };
        out.push_back(m);
        if out.len() > limit {
            out.pop_front();
        }
    }
    Ok(out.into())
}

/// A user message the CLI took while a turn ran is journaled as a
/// `queued_command` attachment, not as a `user` row. The CLI also journals
/// absorbed background-task notifications (`commandMode: "task-notification"`,
/// `origin.kind: "task-notification"`) and cross-session peer messages
/// (`isMeta: true`, `origin.kind: "peer"`) in the same envelope; those are
/// system events, not human steers.
fn steer(v: &Value) -> Option<HistoryMessage> {
    if v.get("type")?.as_str()? != "attachment" {
        return None;
    }
    if v.get("isMeta").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let attachment = &v["attachment"];
    if attachment["type"].as_str()? != "queued_command" {
        return None;
    }
    if attachment.get("isMeta").and_then(Value::as_bool) == Some(true)
        || attachment
            .get("commandMode")
            .and_then(Value::as_str)
            .is_some_and(|mode| mode != "prompt")
        || non_human_origin(attachment)
    {
        return None;
    }
    let text = match &attachment["prompt"] {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => return None,
    };
    if attachment.get("commandMode").is_none() && is_task_notification_xml(&text) {
        return None;
    }
    // `source_uuid` is the uuid the driver wrote the steer with: the
    // engine's steer id, so the timeline marks exactly this row.
    let steer_id = attachment["source_uuid"].as_str().map(str::to_string);
    (!text.trim().is_empty()).then(|| HistoryMessage {
        steer_id,
        ..entry(HistoryRole::User, text, Vec::new())
    })
}

pub(super) fn message(v: &Value) -> Option<HistoryMessage> {
    // A compaction (idle or the CLI's own) leaves one marker row.
    if v.get("type").and_then(Value::as_str) == Some("system")
        && v.get("subtype").and_then(Value::as_str) == Some("compact_boundary")
    {
        return Some(entry(HistoryRole::System, "Compacted".into(), Vec::new()));
    }
    if v.get("isMeta").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let role = match v.get("type")?.as_str()? {
        "user" => HistoryRole::User,
        "assistant" => HistoryRole::Assistant,
        _ => return None,
    };
    let content = &v["message"]["content"];
    if let Some(text) = content.as_str() {
        if role == HistoryRole::User && is_system_user_row(v, text) {
            return None;
        }
        return Some(entry(role, text.to_string(), Vec::new()));
    }
    let blocks = content.as_array()?;
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let mut tool_result = false;
    // The memories ZLO recalled for the message ride as the first block,
    // with the person's text as a later one: only that block is dropped (a
    // message the person typed is never the first of two text blocks).
    let text_blocks = blocks
        .iter()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        .count();
    for (index, b) in blocks.iter().enumerate() {
        match b.get("type").and_then(Value::as_str) {
            // Text blocks join with a blank line, as the live stream does.
            Some("text") => {
                let block = b["text"].as_str().unwrap_or_default();
                if role == HistoryRole::User
                    && index == 0
                    && text_blocks > 1
                    && block.starts_with(crate::RECALLED_MEMORY_OPEN)
                {
                    continue;
                }
                if !text.is_empty() && !block.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(block);
            }
            Some("tool_use") => {
                tool_calls.push(format!("{}()", b["name"].as_str().unwrap_or("tool")))
            }
            Some("tool_result") => tool_result = true,
            _ => {}
        }
    }
    if text.is_empty() && tool_calls.is_empty() && !tool_result {
        return None;
    }
    let role = if tool_result && role == HistoryRole::User {
        HistoryRole::Tool
    } else {
        role
    };
    if role == HistoryRole::User && is_system_user_row(v, &text) {
        return None;
    }
    Some(entry(role, text, tool_calls))
}

/// A `user` row generated by the CLI itself (such as a background-task
/// `<task-notification>` waking the session up), not something the person sent.
fn is_system_user_row(v: &Value, text: &str) -> bool {
    if v.get("promptSource").and_then(Value::as_str) == Some("system")
        || v.get("turnOrigin")
            .and_then(Value::as_str)
            .is_some_and(|origin| {
                matches!(
                    origin,
                    "task_notification" | "task-notification" | "peer" | "system"
                )
            })
        || non_human_origin(v)
    {
        return true;
    }
    // Older CLIs wrote `<task-notification>` rows without `promptSource` /
    // `turnOrigin` metadata; keep genuine SDK/typed user messages intact.
    v.get("promptSource").is_none()
        && v.get("turnOrigin").is_none()
        && is_task_notification_xml(text)
}

fn non_human_origin(v: &Value) -> bool {
    v.get("origin")
        .and_then(|origin| origin.get("kind"))
        .and_then(Value::as_str)
        .is_some_and(|kind| {
            matches!(
                kind,
                "task-notification" | "task_notification" | "peer" | "system"
            )
        })
}

fn is_task_notification_xml(text: &str) -> bool {
    let trimmed = text.trim();
    trimmed.starts_with("<task-notification>") && trimmed.ends_with("</task-notification>")
}

/// User rows nobody typed: the `/compact` prompt, the CLI's echo of the
/// command and its output, and the summary that replaces the history.
pub(super) fn compact_scaffolding(v: &Value, m: &HistoryMessage) -> bool {
    if m.role != HistoryRole::User {
        return false;
    }
    if v.get("isCompactSummary").and_then(Value::as_bool) == Some(true) {
        return true;
    }
    let text = m.text.trim_start();
    text == super::compact::COMPACT_PROMPT
        || text.contains("<command-name>/compact</command-name>")
        || text.starts_with("<local-command-stdout>")
        || text.starts_with("<local-command-stderr>")
}

/// A row's `timestamp` (RFC 3339) as unix ms.
pub(super) fn timestamp(v: &Value) -> Option<ironclaw_orchestrator::model::Millis> {
    let raw = v.get("timestamp")?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok() // silent-ok: a row without a readable time just has none
        .map(|t| t.timestamp_millis())
}

/// Rows read for a session's working directory before giving up.
const CWD_ROWS: usize = 200;

/// The working directory a session ran in, from its first rows that name
/// one. `--resume` finds a session only from that directory.
pub(super) async fn cwd(path: &Path) -> Option<std::path::PathBuf> {
    let mut lines = crate::session_import::lines::Lines::open(path, 0)
        .await
        .ok()?; // silent-ok: an unreadable transcript resumes from the default folder
    for _ in 0..CWD_ROWS {
        let line = lines.next_line().await.ok()??; // silent-ok: as above
        let Ok(v) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if let Some(cwd) = v
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
        {
            return Some(std::path::PathBuf::from(cwd));
        }
    }
    None
}

fn entry(role: HistoryRole, text: String, tool_calls: Vec<String>) -> HistoryMessage {
    HistoryMessage {
        role,
        text,
        tool_calls,
        run_id: None,
        hidden: false,
        at: None,
        steer_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_recalled_memory_block_rides_the_prompt_but_not_the_transcript() {
        let ironclaw_input = ironclaw_orchestrator::runtime::TurnInput {
            text: "where is the key?".into(),
            memory: Some(ironclaw_orchestrator::runtime::RecalledMemory {
                guidance: "g".into(),
                recalled: Some("Untrusted memory content: it is in 1Password".into()),
            }),
            origin: ironclaw_orchestrator::runtime::TurnOrigin::UserPrompt,
            model: None,
            goal: None,
            effort: None,
            profile: None,
            files: Vec::new(),
            work_dir: None,
        };
        let block = crate::recalled_block(&ironclaw_input).unwrap();
        let frame =
            super::super::control::prompt_with_recalled("run-1", "where is the key?", Some(&block))
                .await;
        let sent: serde_json::Value = serde_json::from_str(&frame).unwrap();
        let content = sent["message"]["content"].as_array().unwrap();
        assert!(
            content[0]["text"]
                .as_str()
                .unwrap()
                .contains("in 1Password")
        );
        assert_eq!(content.last().unwrap()["text"], "where is the key?");

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        tokio::fs::write(&path, &frame).await.unwrap();
        let rows = read(&path, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "where is the key?", "the person's words only");

        // Someone typing the tag themselves keeps their message, with or
        // without a picture.
        let typed = format!("{} not a memory", crate::RECALLED_MEMORY_OPEN);
        let alone = super::super::control::prompt_with_recalled("run-2", &typed, None).await;
        let with_image = r#"{"type":"user","uuid":"run-3","message":{"role":"user","content":[{"type":"image","source":{}},{"type":"text","text":"<zlo-recalled-memory> mine"}]}}"#;
        tokio::fs::write(&path, format!("{alone}\n{with_image}"))
            .await
            .unwrap();
        let rows = read(&path, 10).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].text, typed);
        assert_eq!(rows[1].text, "<zlo-recalled-memory> mine");
    }

    #[tokio::test]
    async fn rows_carry_the_run_of_the_prompt_before_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let lines = [
            r#"{"type":"user","uuid":"run-1","message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"assistant","uuid":"a1","message":{"content":[{"type":"text","text":"Let me look."},{"type":"tool_use","name":"Bash"}]}}"#,
            r#"{"type":"user","uuid":"x","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#,
            r#"{"type":"attachment","uuid":"q","attachment":{"type":"queued_command","prompt":"steer","source_uuid":"steer-1"}}"#,
            r#"{"type":"assistant","uuid":"a2","message":{"content":[{"type":"text","text":"Done."}]}}"#,
            r#"{"type":"user","uuid":"run-2","message":{"role":"user","content":"again"}}"#,
        ];
        tokio::fs::write(&path, lines.join("\n")).await.unwrap();
        let rows = read(&path, 10).await.unwrap();
        let runs: Vec<_> = rows.iter().map(|m| m.run_id.as_deref()).collect();
        // Neither the tool result nor a mid-turn steer starts a run.
        assert_eq!(
            runs,
            [
                Some("run-1"),
                Some("run-1"),
                Some("run-1"),
                Some("run-1"),
                Some("run-1"),
                Some("run-2")
            ]
        );
        assert_eq!(rows[3].role, HistoryRole::User);
        assert_eq!(rows[3].text, "steer");
    }

    #[tokio::test]
    async fn rows_keep_their_original_time_and_the_session_its_folder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let lines = [
            r#"{"type":"queue-operation","operation":"enqueue"}"#,
            r#"{"type":"user","uuid":"r","cwd":"/work/app","timestamp":"2026-10-01T10:00:00.000Z","message":{"role":"user","content":"hi"}}"#,
        ];
        tokio::fs::write(&path, format!("{}\n", lines.join("\n")))
            .await
            .unwrap();
        let rows = read(&path, 10).await.unwrap();
        assert_eq!(rows[0].at, Some(1_790_848_800_000));
        assert_eq!(
            cwd(&path).await.as_deref(),
            Some(std::path::Path::new("/work/app"))
        );
    }

    #[tokio::test]
    async fn a_limit_keeps_the_newest_rows_with_the_run_of_a_prompt_before_the_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut lines = vec![
            r#"{"type":"user","uuid":"run-old","message":{"role":"user","content":"first"}}"#
                .to_string(),
        ];
        for i in 0..500 {
            lines.push(format!(
                r#"{{"type":"assistant","uuid":"a{i}","message":{{"content":[{{"type":"text","text":"step {i}"}}]}}}}"#
            ));
            lines.push(r#"{"type":"queue-operation","operation":"enqueue"}"#.to_string());
        }
        lines.push(
            r#"{"type":"user","uuid":"run-new","message":{"role":"user","content":"second"}}"#
                .to_string(),
        );
        lines.push(r#"{"type":"system","subtype":"compact_boundary","uuid":"b"}"#.to_string());
        lines.push(
            r#"{"type":"user","uuid":"summary","isCompactSummary":true,"message":{"role":"user","content":"summary"}}"#
                .to_string(),
        );
        lines.push(
            r#"{"type":"attachment","uuid":"q","attachment":{"type":"queued_command","prompt":"steer"}}"#
                .to_string(),
        );
        lines.push(
            r#"{"type":"assistant","uuid":"z","message":{"content":[{"type":"text","text":"Done."}]}}"#
                .to_string(),
        );
        // A live writer's torn last line.
        lines.push(
            r#"{"type":"assistant","uuid":"torn","message":{"content":[{"type":"te"#.to_string(),
        );
        tokio::fs::write(&path, lines.join("\n")).await.unwrap();

        let all = read(&path, usize::MAX).await.unwrap();
        assert_eq!(all.len(), 506);
        assert_eq!(all[0].text, "first");

        let tail = read(&path, 7).await.unwrap();
        assert_eq!(tail, all[all.len() - 7..]);
        let seen: Vec<_> = tail
            .iter()
            .map(|m| (m.text.as_str(), m.run_id.as_deref(), m.hidden))
            .collect();
        assert_eq!(
            seen,
            [
                ("step 498", Some("run-old"), false),
                ("step 499", Some("run-old"), false),
                ("second", Some("run-new"), false),
                ("Compacted", Some("run-new"), false),
                ("summary", Some("run-new"), true),
                ("steer", Some("run-new"), false),
                ("Done.", Some("run-new"), false),
            ]
        );

        // The window starts after every prompt: rows still name their run.
        let last_two = read(&path, 2).await.unwrap();
        assert!(
            last_two
                .iter()
                .all(|m| m.run_id.as_deref() == Some("run-new"))
        );
        assert!(read(&path, 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_shrunken_journal_reads_as_what_is_there_now() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let prompt = r#"{"type":"user","uuid":"run-1","message":{"role":"user","content":"hi"}}"#;
        let reply = r#"{"type":"assistant","uuid":"a","message":{"content":[{"type":"text","text":"Done."}]}}"#;
        tokio::fs::write(&path, format!("{prompt}\n{reply}\n"))
            .await
            .unwrap();
        assert_eq!(read(&path, 1).await.unwrap()[0].text, "Done.");
        tokio::fs::write(&path, format!("{prompt}\n"))
            .await
            .unwrap();
        let rows = read(&path, 1).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "hi");
    }

    #[tokio::test]
    async fn a_compaction_summary_does_not_start_a_run() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let lines = [
            r#"{"type":"user","uuid":"run-1","message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"system","subtype":"compact_boundary","uuid":"b"}"#,
            r#"{"type":"user","uuid":"summary-1","isCompactSummary":true,"message":{"role":"user","content":"summary"}}"#,
            r#"{"type":"attachment","uuid":"q","attachment":{"type":"queued_command","prompt":"steer"}}"#,
            r#"{"type":"assistant","uuid":"a1","message":{"content":[{"type":"text","text":"Done."}]}}"#,
        ];
        tokio::fs::write(&path, lines.join("\n")).await.unwrap();
        let rows = read(&path, 10).await.unwrap();
        assert!(rows.iter().all(|m| m.run_id.as_deref() == Some("run-1")));
        assert!(rows[2].hidden);
    }

    #[tokio::test]
    async fn task_notifications_and_peer_queued_commands_are_not_user_messages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let lines = [
            // Genuine SDK prompt starts run-1.
            r#"{"type":"user","uuid":"run-1","promptSource":"sdk","turnOrigin":"sdk","message":{"role":"user","content":"start login"}}"#,
            r#"{"type":"assistant","uuid":"a1","message":{"content":[{"type":"text","text":"Starting in background."}]}}"#,
            // Mid-turn absorbed background task notification (attachment).
            r#"{"type":"attachment","uuid":"att-1","renderedRole":"system","attachment":{"type":"queued_command","commandMode":"task-notification","origin":{"kind":"task-notification","producer":"session-task"},"prompt":"<task-notification>\n<task-id>bxlcemct5</task-id>\n<status>completed</status>\n</task-notification>"}}"#,
            // Mid-turn cross-session peer message (attachment with isMeta + origin.kind=peer).
            r#"{"type":"attachment","uuid":"att-2","renderedRole":"system","attachment":{"type":"queued_command","commandMode":"prompt","isMeta":true,"origin":{"kind":"peer"},"prompt":"<cross-session-message from=\"peer\">hi</cross-session-message>"}}"#,
            // Genuine human mid-turn steer is kept inside run-1.
            r#"{"type":"attachment","uuid":"att-3","renderedRole":"system","attachment":{"type":"queued_command","commandMode":"prompt","prompt":"also check workspace"}}"#,
            r#"{"type":"assistant","uuid":"a2","message":{"content":[{"type":"text","text":"Waiting for login."}]}}"#,
            // Post-turn background task notification (user row with promptSource=system & turnOrigin=task_notification).
            r#"{"type":"user","uuid":"notif-1","promptSource":"system","turnOrigin":"task_notification","origin":{"kind":"task-notification","producer":"session-task"},"message":{"role":"user","content":"<task-notification>\n<task-id>be7lg2bcu</task-id>\n<status>completed</status>\n</task-notification>"}}"#,
            // Variant without turnOrigin (only promptSource=system & origin.kind=task-notification).
            r#"{"type":"user","uuid":"notif-2","promptSource":"system","origin":{"kind":"task-notification"},"message":{"role":"user","content":"<task-notification>\n<task-id>b6mgp7zq5</task-id>\n</task-notification>"}}"#,
            // Legacy CLI notification without metadata fields.
            r#"{"type":"user","uuid":"notif-3","message":{"role":"user","content":"<task-notification>\n<task-id>legacy</task-id>\n</task-notification>"}}"#,
            // Follow-up assistant reply after task notification keeps run-1 (not hijacked by notif-*).
            r#"{"type":"assistant","uuid":"a3","message":{"content":[{"type":"text","text":"Login active."}]}}"#,
            // A real SDK user message that quotes a task-notification verbatim is preserved.
            r#"{"type":"user","uuid":"run-2","promptSource":"sdk","turnOrigin":"sdk","message":{"role":"user","content":"<task-notification>\n<task-id>quoted</task-id>\n</task-notification>"}}"#,
        ];
        tokio::fs::write(&path, lines.join("\n")).await.unwrap();

        let rows = read(&path, 20).await.unwrap();
        let seen: Vec<_> = rows
            .iter()
            .map(|m| (m.role, m.text.as_str(), m.run_id.as_deref()))
            .collect();
        assert_eq!(
            seen,
            [
                (HistoryRole::User, "start login", Some("run-1")),
                (
                    HistoryRole::Assistant,
                    "Starting in background.",
                    Some("run-1")
                ),
                (HistoryRole::User, "also check workspace", Some("run-1")),
                (HistoryRole::Assistant, "Waiting for login.", Some("run-1")),
                (HistoryRole::Assistant, "Login active.", Some("run-1")),
                (
                    HistoryRole::User,
                    "<task-notification>\n<task-id>quoted</task-id>\n</task-notification>",
                    Some("run-2")
                ),
            ]
        );
    }
}
