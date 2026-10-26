//! Thread history from the CLI's own session journal
//! (`<config>/projects/<encoded cwd>/<session>.jsonl`).

use std::path::Path;

use ironclaw_orchestrator::runtime::{HistoryMessage, HistoryRole, RuntimeError};
use serde_json::Value;

pub(super) async fn read(path: &Path, limit: usize) -> Result<Vec<HistoryMessage>, RuntimeError> {
    let raw = match tokio::fs::read_to_string(path).await {
        Ok(raw) => raw,
        // No turn has run yet.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(RuntimeError::Unavailable(format!(
                "claude transcript: {}",
                e.kind()
            )));
        }
    };
    // A prompt's journal `uuid` is the run id the driver sent it with; the
    // rows after it, up to the next prompt, belong to that run.
    let mut run: Option<String> = None;
    let mut out: Vec<HistoryMessage> = raw
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok()) // silent-ok: a torn last line of a live journal is skipped
        .filter_map(|v| {
            // A steer sent mid-turn: the person's row, inside the run.
            if let Some(mut m) = steer(&v) {
                m.run_id = run.clone();
                return Some(m);
            }
            let mut m = message(&v)?;
            m.hidden = compact_scaffolding(&v, &m);
            // Hidden rows (a compaction's summary) stay in the current run.
            if m.role == HistoryRole::User && !m.hidden {
                run = v["uuid"].as_str().map(str::to_string);
            }
            m.run_id = run.clone();
            Some(m)
        })
        .collect();
    let skip = out.len().saturating_sub(limit);
    Ok(out.split_off(skip))
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

fn message(v: &Value) -> Option<HistoryMessage> {
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
fn compact_scaffolding(v: &Value, m: &HistoryMessage) -> bool {
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

fn entry(role: HistoryRole, text: String, tool_calls: Vec<String>) -> HistoryMessage {
    HistoryMessage {
        role,
        text,
        tool_calls,
        run_id: None,
        hidden: false,
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
