//! The per-user directory window captures are written to, and the checks
//! that make it safe to write screenshots into and to sweep.
//!
//! The directory must be a real directory (never a symlink), owned by the
//! current user, with mode 0700. A directory we own with a wider mode is
//! narrowed; anything else is refused, and a refused directory is never
//! written to or swept. Capture files are created exclusively with mode 0600
//! and read back without following symlinks.

use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

/// Where captures go under the ZLO data folder.
const CAPTURE_SUBDIR: [&str; 3] = ["cache", "computer-use", "captures"];

/// `$ZLO_HOME/cache/computer-use/captures`, else
/// `$HOME/.zlo/cache/computer-use/captures`; `None` without either, which
/// turns window capture off.
pub(crate) fn default_capture_dir(
    zlo_home: Option<impl Into<PathBuf>>,
    home: Option<impl Into<PathBuf>>,
) -> Option<PathBuf> {
    let base = match zlo_home.map(Into::into).filter(|path| path.is_absolute()) {
        Some(zlo_home) => zlo_home,
        None => home
            .map(Into::into)
            .filter(|path| path.is_absolute())?
            .join(".zlo"),
    };
    Some(
        CAPTURE_SUBDIR
            .iter()
            .fold(base, |path, part| path.join(part)),
    )
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PrivateDirError {
    #[error("capture directory unavailable: {0}")]
    Unavailable(String),
    #[error("capture directory is a symlink")]
    Symlink,
    #[error("capture directory is not a directory")]
    NotADirectory,
    #[error("capture directory is owned by another user")]
    NotOwned,
    #[error("capture directory permissions could not be restricted: {0}")]
    Insecure(String),
}

/// The current user's uid (0 where the platform has none).
pub(crate) fn current_uid() -> u32 {
    #[cfg(unix)]
    {
        // SAFETY: getuid has no preconditions and cannot fail.
        unsafe { libc::getuid() }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// Check `dir` (creating it with mode 0700 when `create` and it is missing).
/// `Ok(false)`: missing and not created.
pub(crate) fn ensure_private_dir(
    dir: &Path,
    owner_uid: u32,
    create: bool,
) -> Result<bool, PrivateDirError> {
    let metadata = match fs::symlink_metadata(dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if !create {
                return Ok(false);
            }
            create_dir(dir)?;
            fs::symlink_metadata(dir)
                .map_err(|error| PrivateDirError::Unavailable(error.to_string()))?
        }
        Err(error) => return Err(PrivateDirError::Unavailable(error.to_string())),
    };
    if metadata.file_type().is_symlink() {
        return Err(PrivateDirError::Symlink);
    }
    if !metadata.is_dir() {
        return Err(PrivateDirError::NotADirectory);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != owner_uid {
            return Err(PrivateDirError::NotOwned);
        }
        if metadata.mode() & 0o777 != 0o700 {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
                .map_err(|error| PrivateDirError::Insecure(error.to_string()))?;
            let narrowed = fs::symlink_metadata(dir)
                .map_err(|error| PrivateDirError::Insecure(error.to_string()))?;
            if narrowed.file_type().is_symlink() || narrowed.mode() & 0o777 != 0o700 {
                return Err(PrivateDirError::Insecure("mode is not 0700".to_string()));
            }
        }
    }
    #[cfg(not(unix))]
    let _ = owner_uid;
    Ok(true)
}

fn create_dir(dir: &Path) -> Result<(), PrivateDirError> {
    if let Some(parent) = dir.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| PrivateDirError::Unavailable(error.to_string()))?;
    }
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    match builder.create(dir) {
        // Raced with another creator: the caller re-checks what is there.
        Err(error) if error.kind() != io::ErrorKind::AlreadyExists => {
            Err(PrivateDirError::Unavailable(error.to_string()))
        }
        _ => Ok(()),
    }
}

/// Create `path` exclusively (it must not exist, not even as a symlink) with
/// mode 0600, so the capture tool writes into a file only we can read.
pub(crate) fn create_capture_file(path: &Path) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path).map(drop)
}

/// Read a capture file without following a symlink at its path.
pub(crate) fn read_capture_file(path: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NOFOLLOW);
    let mut bytes = Vec::new();
    options.open(path)?.read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// `<pid>_<n>.jpg`: the name this crate gives a capture file.
pub(crate) fn is_capture_file_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".jpg") else {
        return false;
    };
    let mut parts = stem.splitn(2, '_');
    let all_digits = |part: Option<&str>| {
        part.is_some_and(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
    };
    all_digits(parts.next()) && all_digits(parts.next())
}

/// `ironclaw_cu_<pid>_<n>.jpg`: what earlier builds wrote into the shared temp
/// directory.
pub(crate) fn is_legacy_capture_file_name(name: &str) -> bool {
    name.strip_prefix("ironclaw_cu_")
        .is_some_and(is_capture_file_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_names_match_only_our_pattern() {
        assert!(is_capture_file_name("123_4.jpg"));
        assert!(!is_capture_file_name("123_4.jpeg"));
        assert!(!is_capture_file_name("photo.jpg"));
        assert!(!is_capture_file_name("12_x.jpg"));
        assert!(!is_capture_file_name("_4.jpg"));
        assert!(is_legacy_capture_file_name("ironclaw_cu_9_1.jpg"));
        assert!(!is_legacy_capture_file_name("ironclaw_cu_notes.jpg"));
    }

    #[test]
    fn the_capture_dir_is_per_user_not_shared_temp() {
        assert_eq!(
            default_capture_dir(Some("/zlo"), Some("/home/u")),
            Some(PathBuf::from("/zlo/cache/computer-use/captures"))
        );
        assert_eq!(
            default_capture_dir(None::<PathBuf>, Some("/home/u")),
            Some(PathBuf::from("/home/u/.zlo/cache/computer-use/captures"))
        );
        assert_eq!(default_capture_dir(None::<PathBuf>, None::<PathBuf>), None);
        assert_eq!(
            default_capture_dir(Some("relative"), None::<PathBuf>),
            None,
            "a relative home is not a location"
        );
    }
}
