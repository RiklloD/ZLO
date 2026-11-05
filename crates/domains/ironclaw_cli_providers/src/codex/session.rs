//! What IronClaw knows about a Codex session beyond its transcript, kept
//! append-only at `<state_dir>/sessions/<session>.jsonl`:
//!
//! - `turn`: an admitted turn — IronClaw's run id, Codex's native turn id,
//!   and the canonical workspace root the turn ran in;
//! - `revert`: the runs a revert hides, written *before* Codex is asked, so
//!   a revert is never done natively without its record;
//! - `revert_cancelled`: Codex refused or failed that revert; its runs stay.
//!
//! Writes report failure ([`append`] returns the error): a revert does not
//! proceed when its intent cannot be recorded. Nothing is ever removed;
//! reverted turns stay in the journal, hidden by [`SessionLog::reverted`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use ironclaw_orchestrator::runtime::RuntimeError;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

/// One admitted turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TurnRecord {
    pub(super) run_id: String,
    pub(super) turn_id: String,
    pub(super) root: String,
}

/// The session's records, folded.
#[derive(Debug, Default)]
pub(super) struct SessionLog {
    turns: Vec<TurnRecord>,
    /// `(native turn id, runs it hides)`, in order.
    reverts: Vec<(String, Vec<String>)>,
    cancelled: HashSet<String>,
}

impl SessionLog {
    pub(super) fn parse(raw: &str) -> Self {
        let mut log = Self::default();
        // silent-ok: a torn last line (a crash mid-append) is skipped
        for record in raw
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        {
            let s = |k: &str| record.get(k).and_then(Value::as_str).map(str::to_string);
            match record.get("kind").and_then(Value::as_str) {
                Some("turn") => {
                    if let (Some(run_id), Some(turn_id), Some(root)) =
                        (s("run_id"), s("turn_id"), s("root"))
                    {
                        log.turns.push(TurnRecord {
                            run_id,
                            turn_id,
                            root,
                        });
                    }
                }
                Some("revert") => {
                    let runs = record["runs"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect();
                    if let Some(turn) = s("turn_id") {
                        log.reverts.push((turn, runs));
                    }
                }
                Some("revert_cancelled") => {
                    if let Some(turn) = s("turn_id") {
                        log.cancelled.insert(turn);
                    }
                }
                _ => {}
            }
        }
        log
    }

    /// Runs whose turns a revert undid (and Codex did not refuse).
    pub(super) fn reverted(&self) -> HashSet<String> {
        self.reverts
            .iter()
            .filter(|(turn, _)| !self.cancelled.contains(turn))
            .flat_map(|(_, runs)| runs.iter().cloned())
            .collect()
    }

    pub(super) fn turn_of(&self, run_id: &str) -> Option<&TurnRecord> {
        self.turns.iter().find(|t| t.run_id == run_id)
    }

    /// `run_id` and every later recorded run not reverted yet: what a
    /// revert before `run_id`'s turn removes from the session.
    pub(super) fn runs_from(&self, run_id: &str) -> Vec<String> {
        let reverted = self.reverted();
        self.turns
            .iter()
            .skip_while(|t| t.run_id != run_id)
            .filter(|t| !reverted.contains(&t.run_id))
            .map(|t| t.run_id.clone())
            .collect()
    }

    /// The root of the session's latest turn.
    pub(super) fn root(&self) -> Option<&str> {
        self.turns.last().map(|t| t.root.as_str())
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

pub(super) fn revert_record(turn_id: &str, runs: &[String]) -> Value {
    json!({"kind": "revert", "turn_id": turn_id, "runs": runs})
}

pub(super) fn revert_cancelled_record(turn_id: &str) -> Value {
    json!({"kind": "revert_cancelled", "turn_id": turn_id})
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

    fn log(records: &[Value]) -> SessionLog {
        let raw: String = records.iter().map(|r| format!("{r}\n")).collect();
        SessionLog::parse(&raw)
    }

    #[test]
    fn a_revert_hides_its_run_and_every_later_one_unless_cancelled() {
        let mut records = vec![
            turn_record("r1", "t1", "/w"),
            turn_record("r2", "t2", "/w"),
            turn_record("r3", "t3", "/w"),
        ];
        let l = log(&records);
        assert_eq!(l.runs_from("r2"), ["r2", "r3"]);
        assert_eq!(l.turn_of("r2").unwrap().turn_id, "t2");
        records.push(revert_record("t2", &["r2".into(), "r3".into()]));
        let l = log(&records);
        assert_eq!(
            l.reverted(),
            HashSet::from(["r2".to_string(), "r3".to_string()])
        );
        // A later revert does not list runs already reverted.
        assert_eq!(l.runs_from("r1"), ["r1"]);
        records.push(revert_cancelled_record("t2"));
        assert!(log(&records).reverted().is_empty());
    }

    #[test]
    fn a_torn_line_is_skipped_and_the_root_is_the_latest_turns() {
        let raw = format!(
            "{}\n{}\n{{\"kind\":\"tu",
            turn_record("r1", "t1", "/a"),
            turn_record("r2", "t2", "/b")
        );
        let l = SessionLog::parse(&raw);
        assert_eq!(l.root(), Some("/b"));
        assert!(l.turn_of("r3").is_none());
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
    }
}
