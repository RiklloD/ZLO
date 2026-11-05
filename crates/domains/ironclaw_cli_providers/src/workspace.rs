//! The folder an imported terminal session belongs to. A CLI session is
//! bound to the directory it ran in (Claude files it by that path; Codex
//! resumes into it), so an imported session records that directory and its
//! identity (device and inode) at import. Before a turn, the driver checks
//! the folder is still there and still the same folder: a deleted worktree,
//! or another checkout at the same path, ends the turn with an error the
//! person can act on instead of resuming somewhere else.
//!
//! Sessions without a record (the app's own) are not affected.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use ironclaw_orchestrator::runtime::RuntimeError;
use serde_json::{Value, json};

/// The folder's identity, `None` when it is not a directory now.
pub(crate) fn identity(dir: &Path) -> Option<String> {
    let meta = std::fs::metadata(dir).ok()?; // silent-ok: absent means "not there"
    meta.is_dir()
        .then(|| format!("{}:{}", meta.dev(), meta.ino()))
}

/// Record `cwd` (and what it is now) for a session, privately.
pub(crate) async fn record(file: &Path, cwd: &Path) -> std::io::Result<()> {
    let body = json!({
        "cwd": cwd.display().to_string(),
        "dir": identity(cwd).unwrap_or_default(),
    })
    .to_string();
    crate::session_import::files::write_private(file, body.as_bytes()).await
}

fn shown(dir: &Path) -> String {
    dir.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "its folder".into())
}

/// The folder a turn must run in: `Ok(None)` without a record, the recorded
/// folder when it is the same one, and a correctable error otherwise.
pub(crate) async fn resolve(file: &Path) -> Result<Option<PathBuf>, RuntimeError> {
    let raw = match tokio::fs::read(file).await {
        Ok(raw) => raw,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(RuntimeError::Unavailable(format!(
                "imported session folder record: {}",
                e.kind()
            )));
        }
    };
    let doc: Value = serde_json::from_slice(&raw).map_err(|_| {
        RuntimeError::Unavailable("imported session folder record unreadable".into())
    })?;
    let Some(cwd) = doc["cwd"]
        .as_str()
        .filter(|c| !c.is_empty())
        .map(PathBuf::from)
    else {
        return Ok(None);
    };
    let recorded = doc["dir"].as_str().unwrap_or_default();
    match identity(&cwd) {
        None => Err(RuntimeError::Rejected(format!(
            "This session ran in the folder \"{}\", which no longer exists. Restore it to continue the session.",
            shown(&cwd)
        ))),
        Some(now) if now != recorded => Err(RuntimeError::Rejected(format!(
            "The folder \"{}\" this session ran in was replaced by a different folder. Restore the original to continue the session.",
            shown(&cwd)
        ))),
        Some(_) => Ok(Some(cwd)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_moved_or_replaced_folder_is_an_error_not_a_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        std::fs::create_dir_all(&app).unwrap();
        let file = dir.path().join("state").join("s.json");
        assert_eq!(resolve(&file).await.unwrap(), None, "no record, no change");
        record(&file, &app).await.unwrap();
        assert_eq!(resolve(&file).await.unwrap(), Some(app.clone()));

        std::fs::remove_dir(&app).unwrap();
        let gone = resolve(&file).await.unwrap_err();
        assert!(matches!(&gone, RuntimeError::Rejected(m) if m.contains("no longer exists")));
        assert!(
            !format!("{gone}").contains(dir.path().to_str().unwrap()),
            "no host path"
        );

        std::fs::create_dir_all(&app).unwrap();
        let replaced = resolve(&file).await.unwrap_err();
        assert!(matches!(&replaced, RuntimeError::Rejected(m) if m.contains("replaced")));
    }
}
