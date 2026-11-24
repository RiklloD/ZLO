//! Read-only queries over agy's own session logs through the system
//! `sqlite3` tool rather than a linked driver (the persistence driver cone
//! stays where `reborn_persistence_driver_boundary.rs` allows it). A failed
//! query keeps `sqlite3`'s own words, so a locked log, a schema agy changed
//! and a corrupt file read differently in the debug log.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::process::Command;

/// One `sqlite3` read, output included.
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);
/// The most of `sqlite3`'s stderr an error keeps.
const MAX_STDERR: usize = 512;

/// Why a query gave no rows.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(super) enum QueryError {
    #[error("no sqlite3 on this host")]
    NoTool,
    #[error("sqlite3 did not start: {0}")]
    Spawn(String),
    #[error("sqlite3 timed out")]
    Timeout,
    /// `sqlite3`'s exit status and stderr: `database is locked`, `no such
    /// table: steps`, `file is not a database`, ...
    #[error("sqlite3 failed ({status}): {stderr}")]
    Failed { status: String, stderr: String },
    #[error("sqlite3 output is not UTF-8")]
    Output,
}

/// The system `sqlite3`, if any.
pub(super) fn tool() -> Option<PathBuf> {
    let system = PathBuf::from("/usr/bin/sqlite3");
    if system.is_file() {
        return Some(system);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    crate::rpc::find_on_path(&home, "sqlite3")
}

/// Run one read-only query, bounded.
pub(super) async fn query(db: &Path, sql: &str) -> Result<String, QueryError> {
    let tool = tool().ok_or(QueryError::NoTool)?;
    let run = Command::new(tool)
        .args(["-readonly", "-batch", "-noheader", "-separator", "|"])
        .args(["-cmd", ".timeout 100"])
        .arg(db)
        .arg(sql)
        .env_clear()
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(QUERY_TIMEOUT, run)
        .await
        .map_err(|_elapsed| QueryError::Timeout)?
        .map_err(|e| QueryError::Spawn(e.kind().to_string()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stderr = stderr.trim();
        let cut = (0..=MAX_STDERR.min(stderr.len()))
            .rev()
            .find(|i| stderr.is_char_boundary(*i))
            .unwrap_or(0);
        return Err(QueryError::Failed {
            status: out.status.to_string(),
            stderr: stderr[..cut].to_string(),
        });
    }
    String::from_utf8(out.stdout).map_err(|_| QueryError::Output)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lock contention, a changed schema and a corrupt file stay apart.
    #[tokio::test]
    async fn failures_keep_sqlite_words() {
        let Some(sqlite) = tool() else {
            return; // no sqlite3 on this host
        };
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("s.db");
        assert!(
            std::process::Command::new(&sqlite)
                .arg(&db)
                .arg("CREATE TABLE other (x);")
                .status()
                .unwrap()
                .success()
        );
        let schema = query(&db, "SELECT max(idx) FROM steps;").await.unwrap_err();
        assert!(
            matches!(&schema, QueryError::Failed { stderr, .. } if stderr.contains("no such table")),
            "{schema:?}"
        );
        let junk = dir.path().join("junk.db");
        std::fs::write(&junk, vec![7u8; 4096]).unwrap();
        let corrupt = query(&junk, "SELECT 1 FROM steps;").await.unwrap_err();
        assert!(
            matches!(&corrupt, QueryError::Failed { stderr, .. } if stderr.contains("not a database")),
            "{corrupt:?}"
        );
    }
}
