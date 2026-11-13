//! Asking a provider's own CLI for its limits over JSON lines, as t3code does
//! (`claude -p` control requests, `codex app-server` JSON-RPC). The CLI uses
//! and refreshes its own login, so no token is read here.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(25);

/// What the caller wants after one line from the CLI.
pub(crate) enum Step {
    /// Keep reading; write these frames first.
    Send(Vec<Value>),
    Done(Value),
}

/// Spawn `binary args`, write `frames`, and feed every JSON line to `want`
/// until it returns [`Step::Done`]. `None` on spawn failure, EOF or timeout;
/// the child is killed either way.
pub(crate) async fn run(
    binary: &Path,
    args: &[&str],
    env: &[(&str, Option<&Path>)],
    frames: Vec<Value>,
    mut want: impl FnMut(&Value) -> Step,
) -> Option<Value> {
    let mut cmd = tokio::process::Command::new(binary);
    cmd.args(args)
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    for (key, value) in env {
        match value {
            Some(value) => cmd.env(key, value),
            None => cmd.env_remove(key),
        };
    }
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            tracing::debug!(%error, binary = %binary.display(), "usage probe did not start");
            return None;
        }
    };
    let mut stdin = child.stdin.take()?;
    let mut lines = BufReader::new(child.stdout.take()?).lines();
    let exchange = async {
        write(&mut stdin, &frames).await?;
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            match want(&message) {
                Step::Send(more) => write(&mut stdin, &more).await?,
                Step::Done(value) => return Some(value),
            }
        }
        None
    };
    let result = tokio::time::timeout(PROBE_TIMEOUT, exchange)
        .await
        .ok()
        .flatten();
    if result.is_none() {
        tracing::debug!(binary = %binary.display(), "usage probe gave no answer");
    }
    let _ = child.start_kill(); // silent-ok: the child may have exited already
    result
}

async fn write(stdin: &mut tokio::process::ChildStdin, frames: &[Value]) -> Option<()> {
    for frame in frames {
        let mut line = frame.to_string();
        line.push('\n');
        stdin.write_all(line.as_bytes()).await.ok()?;
    }
    stdin.flush().await.ok()
}

/// `name` on `PATH`, then the installers' usual homes: a backend started by
/// launchd runs with a minimal `PATH`.
pub(crate) fn find_binary(home: &Path, name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .chain([
            home.join(".local/bin"),
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/local/bin"),
        ])
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

/// Unix seconds or millis to millis.
pub(crate) fn epoch_ms(raw: &Value) -> Option<i64> {
    let n = raw.as_f64()?;
    (n > 0.0).then_some(if n < 1e12 { n * 1000.0 } else { n } as i64)
}

/// "max" → "Max".
pub(crate) fn plan_name(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let mut chars = raw.chars();
    let first = chars.next()?;
    Some(first.to_uppercase().chain(chars).collect())
}
