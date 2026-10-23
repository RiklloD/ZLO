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
    let mut out: Vec<HistoryMessage> = raw
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok()) // silent-ok: a torn last line of a live journal is skipped
        .filter_map(|v| message(&v))
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
            Some("text") => text.push_str(b["text"].as_str().unwrap_or_default()),
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
