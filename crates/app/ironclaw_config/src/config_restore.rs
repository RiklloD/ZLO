//! Compare-and-restore of `config.toml`, for a caller that wrote the file and
//! then could not apply what it wrote (the Settings → Inference save whose
//! live provider swap failed). Restoring under the same update lock the
//! writers take, and only while the file still holds exactly what that
//! caller wrote, means a concurrent hand edit or Settings save is never
//! overwritten by the rollback.

use std::fs;
use std::path::Path;

use crate::config_file::{
    RebornConfigFileUpdateError, acquire_update_lock, write_config_text_atomic,
};

/// The current text of `path`, or `None` when it does not exist.
pub fn read_config_text(path: &Path) -> Result<Option<String>, RebornConfigFileUpdateError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(RebornConfigFileUpdateError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Put `previous` back (or remove the file when `previous` is `None`, i.e.
/// the write created it) if `path` still holds `written`. Returns whether
/// the file was restored; `false` means someone else changed it since, and
/// their version is left in place.
pub fn restore_config_text_if_unchanged(
    path: &Path,
    written: Option<&str>,
    previous: Option<&str>,
) -> Result<bool, RebornConfigFileUpdateError> {
    let _lock = acquire_update_lock(path)?;
    if read_config_text(path)?.as_deref() != written {
        return Ok(false);
    }
    match previous {
        Some(text) => write_config_text_atomic(path, text)?,
        None => match fs::remove_file(path) {
            Ok(()) => {}
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(RebornConfigFileUpdateError::Write {
                    path: path.to_path_buf(),
                    source,
                });
            }
        },
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restores_only_while_the_file_still_holds_what_was_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "written\n").expect("write");

        assert!(
            restore_config_text_if_unchanged(&path, Some("written\n"), Some("before\n"))
                .expect("restore")
        );
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "before\n");

        // Changed by someone else since: left alone.
        assert!(
            !restore_config_text_if_unchanged(&path, Some("written\n"), Some("older\n"))
                .expect("restore")
        );
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "before\n");

        // The write created the file: restoring removes it.
        assert!(restore_config_text_if_unchanged(&path, Some("before\n"), None).expect("restore"));
        assert!(!path.exists());
    }
}
