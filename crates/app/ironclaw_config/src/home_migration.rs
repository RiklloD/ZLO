//! One-time move of the pre-ZLO data folder `~/.ironclaw/reborn` to `~/.zlo`.
//!
//! Runs only when the home resolves to the default (`ZLO_HOME` and
//! `IRONCLAW_REBORN_HOME` unset). The move is a same-filesystem
//! `rename(2)`, so it is atomic: the data is either all at the old path or
//! all at the new one. Afterwards `~/.ironclaw/reborn` becomes a symlink to
//! `~/.zlo`, so absolute paths already written into config, databases,
//! worktree records, service units and scripts keep resolving.
//!
//! Safety properties:
//!
//! - **Never deletes data.** The only removal is of the empty
//!   `~/.zlo` directory `rename(2)` replaces, and of the pending marker.
//! - **Idempotent.** A migrated home (legacy path is a link to `~/.zlo`) is
//!   a no-op on every later start.
//! - **Concurrent-start safe.** The ZLO app and the backend may resolve the
//!   home at the same moment; the move runs under an exclusive lock on
//!   `~/.ironclaw/.zlo-migration.lock` and re-checks the state once held.
//! - **Crash safe.** A marker file written into the folder before the move
//!   travels with it; a start that finds the marker in `~/.zlo` and no
//!   `~/.ironclaw/reborn` finishes the job by creating the link.
//! - **Fails loudly.** A failed move (cross-device, permissions, …) is a
//!   [`RebornConfigError::HomeMigration`]; ZLO never silently starts on an
//!   empty `~/.zlo` while the data still sits in `~/.ironclaw/reborn`.

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use crate::home::{DEFAULT_HOME_DIR_NAME, RebornConfigError};

const LEGACY_PARENT_DIR: &str = ".ironclaw";
const LEGACY_HOME_DIR: &str = "reborn";
const LOCK_FILE_NAME: &str = ".zlo-migration.lock";
/// Written into the legacy folder right before the move; removed once the
/// compatibility link exists.
pub const MIGRATION_PENDING_MARKER: &str = ".zlo-migration-pending";

/// What resolving the default home did about a pre-ZLO data folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HomeMigration {
    /// Nothing to move: fresh install, explicit home, or already migrated.
    NotNeeded,
    /// This start moved `from` to `to` and linked `from` → `to`.
    Migrated { from: PathBuf, to: PathBuf },
    /// Both folders hold data; `~/.zlo` is used and the old one is left
    /// untouched for the user to merge or remove.
    BothExist { legacy: PathBuf, current: PathBuf },
}

impl HomeMigration {
    /// One line for the user, when there is something to tell them.
    pub fn notice(&self) -> Option<String> {
        match self {
            Self::NotNeeded => None,
            Self::Migrated { from, to } => Some(format!(
                "Moved your data folder from {} to {} (the old path now links to the new one).",
                from.display(),
                to.display()
            )),
            Self::BothExist { legacy, current } => Some(format!(
                "warning: both {} and {} exist; using {}. The old folder was left untouched — \
                 merge anything you still need from it, then remove it.",
                legacy.display(),
                current.display(),
                current.display()
            )),
        }
    }
}

impl fmt::Display for HomeMigration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.notice() {
            Some(notice) => formatter.write_str(&notice),
            None => formatter.write_str("no data folder migration needed"),
        }
    }
}

/// The pre-ZLO data folder for a user home directory.
pub fn legacy_home_path(user_home: &Path) -> PathBuf {
    user_home.join(LEGACY_PARENT_DIR).join(LEGACY_HOME_DIR)
}

/// Move `<user_home>/.ironclaw/reborn` to `<user_home>/.zlo` when needed.
pub fn migrate_legacy_home(user_home: &Path) -> Result<HomeMigration, RebornConfigError> {
    let paths = Paths::new(user_home);
    match paths.state()? {
        State::Fresh | State::Linked => Ok(HomeMigration::NotNeeded),
        State::BothExist => Ok(paths.both_exist()),
        // Something to do: take the lock and decide again, since another
        // process may have finished (or started) the move meanwhile.
        State::NeedsMove | State::NeedsLink => paths.migrate_locked(),
    }
}

/// Where the data lives right now, without moving anything: the pre-ZLO
/// folder while it still holds the data and `~/.zlo` does not, else
/// `~/.zlo`. For read-only callers (log viewers, connection discovery);
/// anything that will write or start a backend uses [`migrate_legacy_home`].
pub(crate) fn current_data_location(user_home: &Path) -> PathBuf {
    let paths = Paths::new(user_home);
    match paths.state() {
        Ok(State::NeedsMove) => paths.legacy,
        Ok(_) => paths.current,
        // silent-ok: a read-only probe; the migrating path reports the error
        Err(_) => paths.current,
    }
}

struct Paths {
    legacy_parent: PathBuf,
    legacy: PathBuf,
    current: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// No legacy folder: fresh install or nothing left to move.
    Fresh,
    /// Legacy path already links to `~/.zlo`.
    Linked,
    /// Legacy folder holds the data; `~/.zlo` is absent or an empty dir.
    NeedsMove,
    /// The move happened but the link was not created (crash or failure).
    NeedsLink,
    /// Both hold data.
    BothExist,
}

impl Paths {
    fn new(user_home: &Path) -> Self {
        Self {
            legacy_parent: user_home.join(LEGACY_PARENT_DIR),
            legacy: legacy_home_path(user_home),
            current: user_home.join(DEFAULT_HOME_DIR_NAME),
        }
    }

    fn error(&self, reason: impl fmt::Display) -> RebornConfigError {
        RebornConfigError::HomeMigration {
            from: self.legacy.clone(),
            to: self.current.clone(),
            reason: reason.to_string(),
        }
    }

    fn io_error(&self, action: &str, path: &Path, error: io::Error) -> RebornConfigError {
        self.error(format_args!("{action} {}: {error}", path.display()))
    }

    fn both_exist(&self) -> HomeMigration {
        HomeMigration::BothExist {
            legacy: self.legacy.clone(),
            current: self.current.clone(),
        }
    }

    fn state(&self) -> Result<State, RebornConfigError> {
        let legacy = self.metadata(&self.legacy)?;
        let current = self.metadata(&self.current)?;
        match legacy {
            None => {
                let pending = self.current.join(MIGRATION_PENDING_MARKER);
                if current.is_some() && self.metadata(&pending)?.is_some() {
                    Ok(State::NeedsLink)
                } else {
                    Ok(State::Fresh)
                }
            }
            Some(meta) if meta.file_type().is_symlink() && self.links_to_current()? => {
                Ok(State::Linked)
            }
            // A real folder, or a link the user made to somewhere else (an
            // external disk): either way that is where the data lives.
            Some(_) => match current {
                None => Ok(State::NeedsMove),
                Some(meta) if meta.is_dir() && self.is_empty_dir(&self.current)? => {
                    Ok(State::NeedsMove)
                }
                Some(_) => Ok(State::BothExist),
            },
        }
    }

    fn metadata(&self, path: &Path) -> Result<Option<fs::Metadata>, RebornConfigError> {
        match fs::symlink_metadata(path) {
            Ok(meta) => Ok(Some(meta)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(self.io_error("inspect", path, error)),
        }
    }

    fn links_to_current(&self) -> Result<bool, RebornConfigError> {
        let target = fs::read_link(&self.legacy)
            .map_err(|error| self.io_error("read link", &self.legacy, error))?;
        if target == self.current {
            return Ok(true);
        }
        // A relative or otherwise-spelled link: compare where both resolve.
        match (self.legacy.canonicalize(), self.current.canonicalize()) {
            (Ok(legacy), Ok(current)) => Ok(legacy == current),
            // A dangling link, or no `~/.zlo` yet: not linked to it.
            _ => Ok(false), // silent-ok: canonicalize failure means "not the same folder"
        }
    }

    fn is_empty_dir(&self, path: &Path) -> Result<bool, RebornConfigError> {
        let mut entries = fs::read_dir(path).map_err(|error| self.io_error("list", path, error))?;
        Ok(entries.next().is_none())
    }

    fn migrate_locked(&self) -> Result<HomeMigration, RebornConfigError> {
        let lock_path = self.legacy_parent.join(LOCK_FILE_NAME);
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| self.io_error("open lock", &lock_path, error))?;
        lock.lock()
            .map_err(|error| self.io_error("lock", &lock_path, error))?;

        let outcome = match self.state()? {
            State::Fresh | State::Linked => Ok(HomeMigration::NotNeeded),
            State::BothExist => Ok(self.both_exist()),
            State::NeedsMove => self.move_and_link(),
            State::NeedsLink => self.link().map(|()| HomeMigration::Migrated {
                from: self.legacy.clone(),
                to: self.current.clone(),
            }),
        };
        drop(lock); // released on close; the empty lock file is left in place
        outcome
    }

    fn move_and_link(&self) -> Result<HomeMigration, RebornConfigError> {
        // The marker goes in *before* the move so it travels with the data:
        // a crash between the move and the link leaves `~/.zlo` carrying it,
        // and the next start (State::NeedsLink) finishes the job. Writing
        // through a link the user made is fine — it lands in their folder.
        let marker = self.legacy.join(MIGRATION_PENDING_MARKER);
        fs::write(&marker, self.current.to_string_lossy().as_bytes())
            .map_err(|error| self.io_error("write", &marker, error))?;

        if let Err(error) = fs::rename(&self.legacy, &self.current) {
            // Nothing moved; leave the folder exactly as it was.
            let cleanup = fs::remove_file(&marker);
            let mut reason = format!("move failed: {error}");
            if error.kind() == io::ErrorKind::CrossesDevices {
                reason.push_str(" (the two folders are on different disks)");
            }
            if let Err(cleanup) = cleanup {
                reason.push_str(&format!(
                    "; also could not remove {}: {cleanup}",
                    marker.display()
                ));
            }
            return Err(self.error(reason));
        }

        self.link()?;
        Ok(HomeMigration::Migrated {
            from: self.legacy.clone(),
            to: self.current.clone(),
        })
    }

    /// Create `~/.ironclaw/reborn → ~/.zlo`, then drop the pending marker.
    fn link(&self) -> Result<(), RebornConfigError> {
        symlink_dir(&self.current, &self.legacy).map_err(|error| {
            self.error(format_args!(
                "the data is now in {} but linking {} to it failed: {error}",
                self.current.display(),
                self.legacy.display()
            ))
        })?;
        let marker = self.current.join(MIGRATION_PENDING_MARKER);
        fs::remove_file(&marker).map_err(|error| self.io_error("remove", &marker, error))
    }
}

#[cfg(unix)]
fn symlink_dir(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn symlink_dir(target: &Path, link: &Path) -> io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}
