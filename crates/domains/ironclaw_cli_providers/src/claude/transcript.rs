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
            let mut m = message(&v)?;
            if m.role == HistoryRole::User {
                run = v["uuid"].as_str().map(str::to_string);
            }
            m.run_id = run.clone();
            Some(m)
        })
        .collect();
    let skip = out.len().saturating_sub(limit);
    Ok(out.split_off(skip))
}

fn message(v: &Value) -> Option<HistoryMessage> {
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
            r#"{"type":"assistant","uuid":"a2","message":{"content":[{"type":"text","text":"Done."}]}}"#,
            r#"{"type":"user","uuid":"run-2","message":{"role":"user","content":"again"}}"#,
        ];
        tokio::fs::write(&path, lines.join("\n")).await.unwrap();
        let rows = read(&path, 10).await.unwrap();
        let runs: Vec<_> = rows.iter().map(|m| m.run_id.as_deref()).collect();
        // The tool result does not start a run.
        assert_eq!(
            runs,
            [
                Some("run-1"),
                Some("run-1"),
                Some("run-1"),
                Some("run-1"),
                Some("run-2")
            ]
        );
    }
}
