//! The transcript IronClaw keeps for agents whose own session store it cannot
//! read (Antigravity's conversation database, Muse's session log): one JSONL
//! file per session, a `HistoryMessage` per line, append-only.

use std::path::Path;

use ironclaw_orchestrator::runtime::{HistoryMessage, HistoryRole, RuntimeError};
use tokio::io::AsyncWriteExt;

pub(crate) async fn append(path: &Path, messages: &[HistoryMessage]) {
    let mut body = String::new();
    for message in messages {
        match serde_json::to_string(message) {
            Ok(line) => {
                body.push_str(&line);
                body.push('\n');
            }
            Err(error) => tracing::debug!(%error, "journal: message not encoded"),
        }
    }
    let written = async {
        if let Some(dir) = path.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        file.write_all(body.as_bytes()).await?;
        file.flush().await
    }
    .await;
    if let Err(error) = written {
        tracing::debug!(%error, "journal: transcript not written");
    }
}

pub(crate) async fn read(path: &Path, limit: usize) -> Result<Vec<HistoryMessage>, RuntimeError> {
    let raw = match tokio::fs::read_to_string(path).await {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(RuntimeError::Unavailable(format!(
                "transcript: {}",
                e.kind()
            )));
        }
    };
    let mut out: Vec<HistoryMessage> = raw
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok()) // silent-ok: a torn last line is skipped
        .collect();
    let skip = out.len().saturating_sub(limit);
    Ok(out.split_off(skip))
}

pub(crate) fn message(role: HistoryRole, text: String, tool_calls: Vec<String>) -> HistoryMessage {
    HistoryMessage {
        role,
        text,
        tool_calls,
        run_id: None,
        hidden: false,
    }
}

/// Persist a CLI's own limit reading for the Usage page (atomic replace).
/// Losing one is harmless: the next turn writes another.
pub(crate) async fn write_limit_reading(path: &Path, key: &str, reading: &serde_json::Value) {
    let body =
        serde_json::json!({"observedAt": ironclaw_orchestrator::model::now_ms(), key: reading})
            .to_string();
    let tmp = path.with_extension("json.tmp");
    let written = async {
        if let Some(dir) = path.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        tokio::fs::write(&tmp, body).await?;
        tokio::fs::rename(&tmp, path).await
    }
    .await;
    if let Err(error) = written {
        tracing::debug!(%error, "limit reading not saved");
    }
}

/// Words a provider uses when a subscription or quota limit stops a turn.
pub(crate) fn mentions_usage_limit(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "usage limit",
        "rate limit",
        "quota",
        "resource_exhausted",
        "resource exhausted",
        "weekly limit",
        "session limit",
    ]
    .iter()
    .any(|p| lower.contains(p))
}
