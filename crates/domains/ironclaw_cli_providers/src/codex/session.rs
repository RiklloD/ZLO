//! What IronClaw knows about a Codex session beyond its transcript, kept
//! append-only at `<state_dir>/sessions/<session>.jsonl`: one `turn` record
//! per admitted turn — IronClaw's run id, Codex's native turn id, and the
//! canonical workspace root the turn ran in. Review reads the root back to
//! refuse another checkout; the run ↔ native turn pairs are what a future
//! revert would target. Nothing is ever removed.

use std::path::{Path, PathBuf};

use ironclaw_orchestrator::runtime::RuntimeError;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

/// The session's records, folded to what is read back today: the
/// workspace root of each recorded turn, oldest first.
#[derive(Debug, Default)]
pub(super) struct SessionLog {
    roots: Vec<String>,
}

impl SessionLog {
    pub(super) fn parse(raw: &str) -> Self {
        let roots = raw
            .lines()
            // silent-ok: a torn last line (a crash mid-append) is skipped
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|r| {
                r.get("kind").and_then(Value::as_str) == Some("turn")
                    && r.get("run_id").is_some_and(Value::is_string)
                    && r.get("turn_id").is_some_and(Value::is_string)
            })
            .filter_map(|r| r.get("root").and_then(Value::as_str).map(str::to_string))
            .collect();
        Self { roots }
    }

    /// The root of the session's latest turn.
    pub(super) fn root(&self) -> Option<&str> {
        self.roots.last().map(String::as_str)
    }
}

pub(super) fn path(state_dir: &Path, session: &str) -> PathBuf {
    state_dir.join("sessions").join(format!("{session}.jsonl"))
}

pub(super) async fn load(state_dir: &Path, session: &str) -> Result<SessionLog, RuntimeError> {
    match tokio::fs::read_to_string(path(state_dir, session)).await {
        Ok(raw) => Ok(SessionLog::parse(&raw)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(SessionLog::default()),
        Err(e) => Err(RuntimeError::Unavailable(format!(
            "codex session record: {}",
            e.kind()
        ))),
    }
}

/// Append one record; the error is the caller's to act on.
pub(super) async fn append(
    state_dir: &Path,
    session: &str,
    record: &Value,
) -> Result<(), RuntimeError> {
    let file = path(state_dir, session);
    let written = async {
        if let Some(dir) = file.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        let mut out = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file)
            .await?;
        out.write_all(format!("{record}\n").as_bytes()).await?;
        out.flush().await?;
        out.sync_data().await
    }
    .await;
    written.map_err(|e| RuntimeError::Unavailable(format!("codex session record: {}", e.kind())))
}

pub(super) fn turn_record(run_id: &str, turn_id: &str, root: &str) -> Value {
    json!({"kind": "turn", "run_id": run_id, "turn_id": turn_id, "root": root})
}

/// The canonical root of `dir`'s workspace: its git top level when it is in
/// a repository, else the folder itself.
pub(super) async fn workspace_root(dir: &Path) -> String {
    let top = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .await
        .ok() // silent-ok: no git means the folder itself is the workspace
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|t| !t.is_empty());
    canonical(Path::new(top.as_deref().map_or(dir, Path::new)))
}

/// `path` resolved (`/tmp` is `/private/tmp` on macOS); unresolvable paths
/// stay as given, so they never match a real root by accident of spelling.
pub(super) fn canonical(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf()) // silent-ok: compared, never opened
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_torn_line_is_skipped_and_the_root_is_the_latest_turns() {
        let raw = format!(
            "{}\n{}\n{{\"kind\":\"tu",
            turn_record("r1", "t1", "/a"),
            turn_record("r2", "t2", "/b")
        );
        let l = SessionLog::parse(&raw);
        assert_eq!(l.root(), Some("/b"));
        assert_eq!(l.roots, ["/a", "/b"]);
    }

    #[tokio::test]
    async fn append_reports_an_unwritable_record() {
        let dir = tempfile::tempdir().unwrap();
        // A file where the sessions folder should be: nothing can be written.
        std::fs::write(dir.path().join("sessions"), "").unwrap();
        let err = append(dir.path(), "s", &turn_record("r", "t", "/w")).await;
        assert!(err.is_err());
        // Reading it back fails loudly too, rather than as an empty session.
        assert!(load(dir.path(), "s").await.is_err());
        // A session with no turns yet has no root.
        let empty = tempfile::tempdir().unwrap();
        assert!(load(empty.path(), "s").await.unwrap().root().is_none());
    }
}
