//! Idle compactions for the Usage page, read from the Claude CLI driver's
//! log (`~/.ironclaw/usage/compactions.jsonl`, one JSON line per run).

use std::path::Path;

use ironclaw_product_contracts::usage::UsageCompaction;

pub(crate) const LOG_FILE: &str = "compactions.jsonl";
/// The page lists the newest runs only.
const MAX_ROWS: usize = 200;

/// Runs at or after `since_ms`, newest first. A missing log is no runs; a
/// torn or foreign line is skipped.
pub(crate) async fn read(path: &Path, since_ms: i64) -> Vec<UsageCompaction> {
    let raw = match tokio::fs::read_to_string(path).await {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            tracing::debug!(%error, "usage: compaction log unreadable");
            return Vec::new();
        }
    };
    let mut rows: Vec<UsageCompaction> = raw
        .lines()
        .filter_map(|line| serde_json::from_str::<UsageCompaction>(line).ok()) // silent-ok: a torn last line of a live log is skipped
        .filter(|row| row.at >= since_ms)
        .collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.at));
    rows.truncate(MAX_ROWS);
    rows
}
