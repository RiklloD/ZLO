//! One JSON line per tool call: tool, ok, refusal code and seconds. Never
//! arguments, typed text, values or screen content. Rotates at 5 MB.

use std::path::Path;
use std::time::Duration;

use serde_json::json;
use tokio::io::AsyncWriteExt;

const MAX_BYTES: u64 = 5 * 1024 * 1024;

pub(crate) async fn record(
    path: &Path,
    thread: &str,
    tool: &str,
    ok: bool,
    took: Duration,
    code: Option<&str>,
) {
    let mut entry = json!({
        "ts": crate::live::now_ms(),
        "session": thread.chars().take(40).collect::<String>(),
        "tool": tool,
        "ok": ok,
        "s": (took.as_secs_f64() * 100.0).round() / 100.0,
    });
    if let Some(code) = code {
        entry["code"] = json!(code);
    }
    if let Err(error) = append(path, entry.to_string()).await {
        tracing::debug!(%error, "computer-use trace write failed");
    }
}

async fn append(path: &Path, mut line: String) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    if tokio::fs::metadata(path)
        .await
        .is_ok_and(|meta| meta.len() > MAX_BYTES)
    {
        tokio::fs::rename(path, path.with_extension("jsonl.1")).await?;
    }
    line.push('\n');
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    file.write_all(line.as_bytes()).await
}
