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
/// `queued_command` attachment, not as a `user` row.
fn steer(v: &Value) -> Option<HistoryMessage> {
    if v.get("type")?.as_str()? != "attachment" {
        return None;
    }
    let attachment = &v["attachment"];
    if attachment["type"].as_str()? != "queued_command" {
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
    (!text.trim().is_empty()).then(|| entry(HistoryRole::User, text, Vec::new()))
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
        return Some(entry(role, text.to_string(), Vec::new()));
    }
    let blocks = content.as_array()?;
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let mut tool_result = false;
    for b in blocks {
        match b.get("type").and_then(Value::as_str) {
            // Text blocks join with a blank line, as the live stream does.
            Some("text") => {
                let block = b["text"].as_str().unwrap_or_default();
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
    Some(entry(role, text, tool_calls))
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rows_carry_the_run_of_the_prompt_before_them() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let lines = [
            r#"{"type":"user","uuid":"run-1","message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"assistant","uuid":"a1","message":{"content":[{"type":"text","text":"Let me look."},{"type":"tool_use","name":"Bash"}]}}"#,
            r#"{"type":"user","uuid":"x","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#,
            r#"{"type":"attachment","uuid":"q","attachment":{"type":"queued_command","prompt":"steer"}}"#,
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
}
