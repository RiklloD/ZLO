//! Private per-user directories under the ZLO data folder for window
//! captures (screenshots) and cursor observation files, and every file
//! operation inside them.
//!
//! [`PrivateDir::open`] walks from `ZLO_HOME` down to the directory one
//! component at a time with `openat(O_DIRECTORY | O_NOFOLLOW)`, creating a
//! missing component with mode 0700, and checks each level on the opened
//! descriptor (`fstat`): a real directory, owned by the current uid, not
//! group- or world-writable; the leaf must be exactly 0700 (narrowed with
//! `fchmod` when we own it). It then holds the leaf's descriptor, and every
//! later read, stat, and delete is relative to that descriptor, so a path
//! component swapped after validation cannot redirect them. The one by-path
//! use — `screencapture` writing a fresh unpredictable name — goes through an
//! ancestry no other user can write to. A file is read only after `O_NOFOLLOW` open and an `fstat` that
//! shows a regular file owned by us (and, for captures, with one link).

use std::io;
use std::path::{Path, PathBuf};

/// Where window captures live, relative to `ZLO_HOME`.
pub(crate) const CAPTURE_DIR: [&str; 3] = ["cache", "computer-use", "captures"];
/// Where cursor observation files live, relative to `ZLO_HOME`.
pub(crate) const CURSOR_DIR: [&str; 3] = ["cache", "computer-use", "cursor"];

/// The ZLO data folder: `$ZLO_HOME`, else the legacy `$IRONCLAW_REBORN_HOME`,
/// else `$HOME/.zlo` — the same explicit-variable precedence as
/// `ironclaw_config`. Only absolute paths count; `None` turns capture off.
pub(crate) fn default_zlo_home(
    zlo_home: Option<PathBuf>,
    legacy_home: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    let absolute = |path: Option<PathBuf>| path.filter(|path| path.is_absolute());
    absolute(zlo_home)
        .or_else(|| absolute(legacy_home))
        .or_else(|| absolute(home).map(|home| home.join(".zlo")))
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PrivateDirError {
    #[error("directory unavailable: {0}")]
    Unavailable(String),
    #[error("{0} is not a directory (or is a symlink)")]
    NotADirectory(String),
    #[error("{0} is owned by another user")]
    NotOwned(String),
    #[error("{0} is writable by group or others")]
    Writable(String),
    #[error("directory permissions could not be restricted: {0}")]
    Insecure(String),
    #[cfg(not(unix))]
    #[error("private directories are not supported on this platform")]
    Unsupported,
}

/// The current user's uid (0 where the platform has none).
pub(crate) fn current_uid() -> u32 {
    #[cfg(unix)]
    {
        rustix::process::getuid().as_raw()
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// A validated private directory, held open by descriptor.
#[derive(Debug)]
pub(crate) struct PrivateDir {
    #[cfg(unix)]
    fd: rustix::fd::OwnedFd,
    path: PathBuf,
    owner_uid: u32,
}

#[cfg(unix)]
mod imp {
    use std::ffi::CStr;
    use std::fs::File;
    use std::io::{self, Read as _};

    use rustix::fd::{AsFd, OwnedFd};
    use rustix::fs::{
        AtFlags, Dir, FileType, Mode, OFlags, Stat, fchmod, fstat, mkdirat, open, openat, statat,
        unlinkat,
    };
    use rustix::io::Errno;

    use super::{PrivateDir, PrivateDirError};

    const DIR_FLAGS: OFlags = OFlags::RDONLY
        .union(OFlags::DIRECTORY)
        .union(OFlags::NOFOLLOW)
        .union(OFlags::CLOEXEC);
    const GROUP_OR_WORLD_WRITE: u32 = 0o022;

    fn check_level(stat: &Stat, owner_uid: u32, what: &str) -> Result<(), PrivateDirError> {
        if FileType::from_raw_mode(stat.st_mode as _) != FileType::Directory {
            return Err(PrivateDirError::NotADirectory(what.to_string()));
        }
        if stat.st_uid != owner_uid {
            return Err(PrivateDirError::NotOwned(what.to_string()));
        }
        Ok(())
    }

    fn not_writable_by_others(stat: &Stat, what: &str) -> Result<(), PrivateDirError> {
        if (stat.st_mode as u32) & GROUP_OR_WORLD_WRITE != 0 {
            return Err(PrivateDirError::Writable(what.to_string()));
        }
        Ok(())
    }

    fn unavailable(error: Errno) -> PrivateDirError {
        PrivateDirError::Unavailable(io::Error::from(error).to_string())
    }

    /// Open `name` under `parent` as a directory without following a symlink.
    /// A symlink fails with `ELOOP`/`ENOTDIR`, reported as not-a-directory.
    fn open_child(parent: &OwnedFd, name: &str, create: bool) -> Result<OwnedFd, PrivateDirError> {
        match openat(parent, name, DIR_FLAGS, Mode::empty()) {
            Ok(fd) => Ok(fd),
            Err(Errno::NOENT) if create => {
                match mkdirat(parent, name, Mode::RWXU) {
                    Ok(()) | Err(Errno::EXIST) => {}
                    Err(error) => return Err(unavailable(error)),
                }
                openat(parent, name, DIR_FLAGS, Mode::empty()).map_err(|error| match error {
                    Errno::LOOP | Errno::NOTDIR => PrivateDirError::NotADirectory(name.to_string()),
                    other => unavailable(other),
                })
            }
            Err(Errno::LOOP | Errno::NOTDIR) => {
                Err(PrivateDirError::NotADirectory(name.to_string()))
            }
            Err(error) => Err(unavailable(error)),
        }
    }

    impl PrivateDir {
        pub(crate) fn open(
            zlo_home: &std::path::Path,
            components: &[&str],
            owner_uid: u32,
        ) -> Result<Self, PrivateDirError> {
            // ZLO_HOME itself must exist; it is never created here.
            let mut fd = open(zlo_home, DIR_FLAGS, Mode::empty()).map_err(|error| match error {
                Errno::LOOP | Errno::NOTDIR => {
                    PrivateDirError::NotADirectory("ZLO_HOME".to_string())
                }
                other => unavailable(other),
            })?;
            let stat = fstat(&fd).map_err(unavailable)?;
            check_level(&stat, owner_uid, "ZLO_HOME")?;
            not_writable_by_others(&stat, "ZLO_HOME")?;
            let mut path = zlo_home.to_path_buf();
            for (index, component) in components.iter().enumerate() {
                let leaf = index + 1 == components.len();
                let child = open_child(&fd, component, true)?;
                let stat = fstat(&child).map_err(unavailable)?;
                check_level(&stat, owner_uid, component)?;
                if leaf {
                    if (stat.st_mode as u32) & 0o777 != 0o700 {
                        fchmod(&child, Mode::RWXU).map_err(|error| {
                            PrivateDirError::Insecure(io::Error::from(error).to_string())
                        })?;
                        let narrowed = fstat(&child).map_err(unavailable)?;
                        if (narrowed.st_mode as u32) & 0o777 != 0o700 {
                            return Err(PrivateDirError::Insecure("mode is not 0700".to_string()));
                        }
                    }
                } else {
                    not_writable_by_others(&stat, component)?;
                }
                path.push(component);
                fd = child;
            }
            Ok(Self {
                fd,
                path,
                owner_uid,
            })
        }

        /// Re-check the held directory: still ours and still 0700.
        pub(crate) fn verify(&self) -> Result<(), PrivateDirError> {
            let stat = fstat(&self.fd).map_err(unavailable)?;
            check_level(&stat, self.owner_uid, "capture directory")?;
            if (stat.st_mode as u32) & 0o777 != 0o700 {
                return Err(PrivateDirError::Insecure("mode is not 0700".to_string()));
            }
            Ok(())
        }

        /// Read `name` (relative to the held directory) without following a
        /// symlink, only if it is a regular file owned by us — and, with
        /// `single_link`, not hard-linked elsewhere.
        pub(crate) fn read_owned_file(&self, name: &str, single_link: bool) -> io::Result<Vec<u8>> {
            let fd = openat(
                &self.fd,
                name,
                // NONBLOCK: a FIFO planted at the name must not block the open.
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
                Mode::empty(),
            )?;
            let stat = fstat(&fd)?;
            if FileType::from_raw_mode(stat.st_mode as _) != FileType::RegularFile {
                return Err(io::Error::other("not a regular file"));
            }
            if stat.st_uid != self.owner_uid {
                return Err(io::Error::other("not owned by this user"));
            }
            if single_link && stat.st_nlink != 1 {
                return Err(io::Error::other("file has other links"));
            }
            let mut bytes = Vec::new();
            File::from(fd).read_to_end(&mut bytes)?;
            Ok(bytes)
        }

        /// Delete `name` relative to the held directory (a symlink there is
        /// removed itself, never its target).
        pub(crate) fn remove(&self, name: &str) -> io::Result<()> {
            unlinkat(&self.fd, name, AtFlags::empty()).map_err(io::Error::from)
        }

        /// Names of regular files owned by us whose name `matches` and that
        /// were last modified at least `older_than` ago. Symlinks and other
        /// entries are skipped, never followed.
        pub(crate) fn stale_files(
            &self,
            matches: &dyn Fn(&str) -> bool,
            older_than: std::time::Duration,
        ) -> Vec<String> {
            let dup = match rustix::io::fcntl_dupfd_cloexec(&self.fd, 0) {
                Ok(dup) => dup,
                Err(error) => {
                    tracing::debug!(%error, "computer-use private directory unreadable for sweep");
                    return Vec::new();
                }
            };
            let dir = match Dir::read_from(dup.as_fd()) {
                Ok(dir) => dir,
                Err(error) => {
                    tracing::debug!(%error, "computer-use private directory unreadable for sweep");
                    return Vec::new();
                }
            };
            let now = std::time::SystemTime::now();
            let mut stale = Vec::new();
            for entry in dir.flatten() {
                let Some(name) = utf8(entry.file_name()) else {
                    continue;
                };
                if !matches(name) {
                    continue;
                }
                let Ok(stat) = statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW) else {
                    continue; // silent-ok: entry vanished; nothing to delete
                };
                if FileType::from_raw_mode(stat.st_mode as _) != FileType::RegularFile
                    || stat.st_uid != self.owner_uid
                {
                    continue;
                }
                let modified = std::time::UNIX_EPOCH
                    + std::time::Duration::from_secs(u64::try_from(stat.st_mtime).unwrap_or(0));
                if now.duration_since(modified).unwrap_or_default() >= older_than {
                    stale.push(name.to_string());
                }
            }
            stale
        }
    }

    fn utf8(name: &CStr) -> Option<&str> {
        name.to_str().ok()
    }
}

#[cfg(not(unix))]
impl PrivateDir {
    pub(crate) fn open(_: &Path, _: &[&str], _: u32) -> Result<Self, PrivateDirError> {
        Err(PrivateDirError::Unsupported)
    }
    pub(crate) fn verify(&self) -> Result<(), PrivateDirError> {
        Err(PrivateDirError::Unsupported)
    }
    pub(crate) fn read_owned_file(&self, _: &str, _: bool) -> io::Result<Vec<u8>> {
        Err(io::Error::other("unsupported"))
    }
    pub(crate) fn remove(&self, _: &str) -> io::Result<()> {
        Err(io::Error::other("unsupported"))
    }
    pub(crate) fn stale_files(
        &self,
        _: &dyn Fn(&str) -> bool,
        _: std::time::Duration,
    ) -> Vec<String> {
        Vec::new()
    }
}

impl PrivateDir {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// The file name of `path` if it lies directly in this directory.
    pub(crate) fn name_of<'a>(&self, path: &'a Path) -> Option<&'a str> {
        (path.parent() == Some(self.path.as_path()))
            .then(|| path.file_name().and_then(|name| name.to_str()))
            .flatten()
    }
}

/// Deletes files of one [`PrivateDir`] fd-relative, for [`crate::leftovers`].
pub(crate) struct PrivateDirRemover(pub(crate) std::sync::Arc<PrivateDir>);

impl crate::leftovers::FileRemover for PrivateDirRemover {
    fn remove(&self, path: &Path) -> io::Result<()> {
        let name = self
            .0
            .name_of(path)
            .ok_or_else(|| io::Error::other("path is outside the private directory"))?;
        self.0.remove(name)
    }
}

/// `cap-<32 hex>.jpg`: the unpredictable name given to each capture.
pub(crate) fn is_capture_file_name(name: &str) -> bool {
    name.strip_prefix("cap-")
        .and_then(|rest| rest.strip_suffix(".jpg"))
        .is_some_and(|id| id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

/// A fresh, unpredictable capture file name.
pub(crate) fn new_capture_file_name() -> String {
    format!("cap-{}.jpg", uuid::Uuid::new_v4().simple())
}

/// `ironclaw_cu_<pid>_<n>.jpg`: what earlier builds wrote into the shared temp
/// directory.
pub(crate) fn is_legacy_capture_file_name(name: &str) -> bool {
    let Some(stem) = name
        .strip_prefix("ironclaw_cu_")
        .and_then(|rest| rest.strip_suffix(".jpg"))
    else {
        return false;
    };
    let mut parts = stem.splitn(2, '_');
    let digits = |part: Option<&str>| {
        part.is_some_and(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
    };
    digits(parts.next()) && digits(parts.next())
}

#[cfg(test)]
#[path = "private_dir_tests.rs"]
mod tests;
