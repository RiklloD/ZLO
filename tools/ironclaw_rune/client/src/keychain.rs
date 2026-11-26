//! Device tokens, one per saved computer: the macOS Keychain (service
//! `ZLO`, account `<environment_id>`), or a private file on Linux, behind
//! [`TokenStore`] so tests use [`MemoryTokens`]. Calls may block (the
//! Keychain can ask the user): run them off the UI thread.

use std::collections::HashMap;
use std::sync::Mutex;

/// Keychain service name of ZLO's device tokens.
pub const SERVICE: &str = "ZLO";

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("Keychain: {0}")]
pub struct KeychainError(pub String);

pub trait TokenStore: Send + Sync {
    /// `Ok(None)` when no token is saved for the computer.
    fn get(&self, environment_id: &str) -> Result<Option<String>, KeychainError>;
    fn set(&self, environment_id: &str, token: &str) -> Result<(), KeychainError>;
    /// Deleting a missing token is fine.
    fn delete(&self, environment_id: &str) -> Result<(), KeychainError>;
}

/// The login Keychain.
pub struct Keychain;

#[cfg(target_os = "macos")]
mod mac {
    use security_framework::base::Error;
    use security_framework::passwords;

    use super::{KeychainError, SERVICE};

    /// `errSecItemNotFound`.
    const NOT_FOUND: i32 = -25300;

    fn err(e: Error) -> KeychainError {
        KeychainError(e.to_string())
    }

    pub fn get(account: &str) -> Result<Option<String>, KeychainError> {
        match passwords::get_generic_password(SERVICE, account) {
            Ok(bytes) => String::from_utf8(bytes)
                .map(Some)
                .map_err(|e| KeychainError(format!("saved token is not text: {e}"))),
            Err(e) if e.code() == NOT_FOUND => Ok(None),
            Err(e) => Err(err(e)),
        }
    }

    pub fn set(account: &str, token: &str) -> Result<(), KeychainError> {
        passwords::set_generic_password(SERVICE, account, token.as_bytes()).map_err(err)
    }

    pub fn delete(account: &str) -> Result<(), KeychainError> {
        match passwords::delete_generic_password(SERVICE, account) {
            Ok(()) => Ok(()),
            Err(e) if e.code() == NOT_FOUND => Ok(()),
            Err(e) => Err(err(e)),
        }
    }
}

#[cfg(target_os = "macos")]
impl TokenStore for Keychain {
    fn get(&self, environment_id: &str) -> Result<Option<String>, KeychainError> {
        mac::get(environment_id)
    }
    fn set(&self, environment_id: &str, token: &str) -> Result<(), KeychainError> {
        mac::set(environment_id, token)
    }
    fn delete(&self, environment_id: &str) -> Result<(), KeychainError> {
        mac::delete(environment_id)
    }
}

/// Off macOS there is no login Keychain: tokens live in one JSON file
/// readable only by this user (`$XDG_CONFIG_HOME/ZLO/device-tokens.json`,
/// mode 0600), next to `environments.json`, the way the Linux backend keeps
/// its own master key in a private file.
#[cfg(not(target_os = "macos"))]
impl TokenStore for Keychain {
    fn get(&self, environment_id: &str) -> Result<Option<String>, KeychainError> {
        let path = file::default_path()?;
        Ok(file::read(&path)?.remove(environment_id))
    }
    fn set(&self, environment_id: &str, token: &str) -> Result<(), KeychainError> {
        let path = file::default_path()?;
        file::update(&path, |all| {
            all.insert(environment_id.to_string(), token.to_string());
        })
    }
    fn delete(&self, environment_id: &str) -> Result<(), KeychainError> {
        let path = file::default_path()?;
        file::update(&path, |all| {
            all.remove(environment_id);
        })
    }
}

#[cfg(not(target_os = "macos"))]
mod file {
    use std::collections::HashMap;
    use std::fs;
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::path::{Path, PathBuf};

    use super::KeychainError;

    pub(super) fn default_path() -> Result<PathBuf, KeychainError> {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .ok_or_else(|| KeychainError("no home folder for the token file".into()))?;
        Ok(config.join("ZLO").join("device-tokens.json"))
    }

    pub(super) fn read(path: &Path) -> Result<HashMap<String, String>, KeychainError> {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
                KeychainError(format!("token file {} unreadable: {e}", path.display()))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
            Err(e) => Err(KeychainError(format!("token file {}: {e}", path.display()))),
        }
    }

    /// Read, change and write back the file under an exclusive lock on a
    /// sibling lock file, held across the whole read-modify-rename, so two
    /// processes (ZLO and ZLO Light share the file) never drop each other's
    /// change. The new content goes to a temp file created exclusively at
    /// mode 0600 (never through an existing file or link), then renamed over
    /// the old one, so a crash never leaves half a file.
    pub(super) fn update(
        path: &Path,
        change: impl FnOnce(&mut HashMap<String, String>),
    ) -> Result<(), KeychainError> {
        let fail = |what: &str, e: std::io::Error| {
            KeychainError(format!("token file {} ({what}): {e}", path.display()))
        };
        let dir = path
            .parent()
            .ok_or_else(|| KeychainError(format!("token file {} has no folder", path.display())))?;
        fs::create_dir_all(dir).map_err(|e| fail("folder", e))?;
        let _lock = lock(dir).map_err(|e| fail("lock", e))?;
        let mut all = read(path)?;
        change(&mut all);
        let bytes = serde_json::to_vec_pretty(&all)
            .map_err(|e| KeychainError(format!("tokens not serialized: {e}")))?;
        let tmp = dir.join(format!(".device-tokens.{}.tmp", std::process::id()));
        // A temp file left by a crashed run of this pid; the lock is ours.
        match fs::remove_file(&tmp) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(fail("stale temp file", e)),
        }
        let written = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .and_then(|mut f| {
                f.write_all(&bytes)?;
                f.sync_all()
            })
            .and_then(|()| fs::rename(&tmp, path));
        written.map_err(|e| {
            let _ = fs::remove_file(&tmp); // silent-ok: best-effort cleanup of our own temp file
            fail("write", e)
        })
    }

    /// The exclusive lock on `dir/.device-tokens.lock`, released when the
    /// returned file drops. A lock file that is a symlink is refused.
    fn lock(dir: &Path) -> std::io::Result<fs::File> {
        let path = dir.join(".device-tokens.lock");
        if fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{} is a symlink; not locking through it", path.display()),
            ));
        }
        // Never written to: only locked.
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(&path)?;
        file.lock()?;
        Ok(file)
    }
}

/// In-memory tokens for tests and headless runs.
#[derive(Default)]
pub struct MemoryTokens(Mutex<HashMap<String, String>>);

impl MemoryTokens {
    fn map(&self) -> Result<std::sync::MutexGuard<'_, HashMap<String, String>>, KeychainError> {
        self.0
            .lock()
            .map_err(|e| KeychainError(format!("token map poisoned: {e}")))
    }
}

impl TokenStore for MemoryTokens {
    fn get(&self, environment_id: &str) -> Result<Option<String>, KeychainError> {
        Ok(self.map()?.get(environment_id).cloned())
    }
    fn set(&self, environment_id: &str, token: &str) -> Result<(), KeychainError> {
        self.map()?
            .insert(environment_id.to_string(), token.to_string());
        Ok(())
    }
    fn delete(&self, environment_id: &str) -> Result<(), KeychainError> {
        self.map()?.remove(environment_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_tokens_behave_like_the_keychain() {
        let t = MemoryTokens::default();
        assert_eq!(t.get("e"), Ok(None));
        t.set("e", "icd_x_y").unwrap();
        assert_eq!(t.get("e"), Ok(Some("icd_x_y".into())));
        t.delete("e").unwrap();
        t.delete("e").unwrap();
        assert_eq!(t.get("e"), Ok(None));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn tokens_round_trip_in_a_private_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ZLO").join("device-tokens.json");
        assert!(
            file::read(&path).unwrap().is_empty(),
            "a missing file holds no tokens"
        );
        file::update(&path, |all| {
            all.insert("env-a".into(), "icd_a".into());
        })
        .unwrap();
        file::update(&path, |all| {
            all.insert("env-b".into(), "icd_b".into());
        })
        .unwrap();
        file::update(&path, |all| {
            all.remove("env-a");
        })
        .unwrap();
        let all = file::read(&path).unwrap();
        assert_eq!(all.get("env-b").map(String::as_str), Some("icd_b"));
        assert!(!all.contains_key("env-a"));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "only this user reads the tokens");
    }

    /// Writers racing on the file (two processes, here threads with their
    /// own lock handles) each keep their token: none is lost to a stale read.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn concurrent_writers_never_drop_a_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ZLO").join("device-tokens.json");
        let threads: Vec<_> = (0..16)
            .map(|n| {
                let path = path.clone();
                std::thread::spawn(move || {
                    file::update(&path, |all| {
                        all.insert(format!("env-{n}"), format!("icd_{n}"));
                    })
                    .unwrap();
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(file::read(&path).unwrap().len(), 16);
    }
}
