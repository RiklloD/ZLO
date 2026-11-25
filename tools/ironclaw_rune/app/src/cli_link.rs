//! Keeps the `zlo` command working: `<ZLO_HOME>/bin/zlo` (default
//! `~/.zlo/bin/zlo`) is a symlink to the running bundle's
//! `Contents/MacOS/zlo-backend`, refreshed on every launch so a moved or
//! reinstalled app still answers `zlo triage`, `zlo update`, `zlo doctor`.
//!
//! `install.sh` creates the link and puts its folder on the PATH (the app
//! never edits shell profiles). The app only replaces a missing link or one
//! that points at some `.app`'s `zlo-backend`; a regular file, or a symlink
//! to anything else (a `cargo install`, a hand-made link), is left alone.

use std::io;
use std::path::{Path, PathBuf};

/// `<ZLO_HOME>/bin/zlo`, the same folder `install.sh` puts on the PATH.
pub fn link_path() -> Option<PathBuf> {
    let home = std::env::var_os("ZLO_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".zlo")))?;
    Some(home.join("bin/zlo"))
}

/// This bundle's `Contents/MacOS/zlo-backend`, when the app runs from a
/// `.app` that has one (never under `cargo run` or a Light bundle).
pub fn bundled_backend() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let backend = exe.parent()?.join("zlo-backend");
    (is_bundle_backend(&backend) && backend.is_file()).then_some(backend)
}

/// Whether the `zlo` link resolves to a file, so `zlo …` works in a new
/// terminal (if its folder is on the PATH, which only install.sh arranges).
pub fn command_works() -> bool {
    link_path().is_some_and(|link| link.is_file())
}

/// Refresh the link off the UI thread. Failures are debug-logged only.
pub fn refresh_in_background() {
    let (Some(link), Some(target)) = (link_path(), bundled_backend()) else {
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("zlo-cli-link".into())
        .spawn(move || match refresh(&link, &target) {
            Ok(outcome) => tracing::debug!(?outcome, link = %link.display(), "zlo command link"),
            Err(error) => {
                tracing::debug!(%error, link = %link.display(), "could not refresh the zlo command link")
            }
        });
    if let Err(error) = spawned {
        tracing::debug!(%error, "could not start the zlo command link refresh");
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Already pointed at `target`.
    Unchanged,
    /// Created, or replaced a link to another `.app`'s backend.
    Linked,
    /// Something that is not ours is there; left alone.
    LeftAlone,
}

/// Point `link` at `target` unless something not made by ZLO sits there.
pub fn refresh(link: &Path, target: &Path) -> io::Result<Outcome> {
    match std::fs::symlink_metadata(link) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
        Ok(meta) if !meta.file_type().is_symlink() => return Ok(Outcome::LeftAlone),
        Ok(_) => {
            let points_at = std::fs::read_link(link)?;
            let points_at = match link.parent() {
                Some(dir) if points_at.is_relative() => dir.join(points_at),
                _ => points_at,
            };
            if points_at == target {
                return Ok(Outcome::Unchanged);
            }
            if !is_bundle_backend(&points_at) {
                return Ok(Outcome::LeftAlone);
            }
        }
    }
    let dir = link
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "link has no folder"))?;
    std::fs::create_dir_all(dir)?;
    // Make the new link beside the old one and rename it over, so `zlo` is
    // never missing mid-swap.
    let staged = dir.join(format!(".zlo-link-{}", std::process::id()));
    let _ = std::fs::remove_file(&staged); // silent-ok: a leftover from a crashed launch, if any
    std::os::unix::fs::symlink(target, &staged)?;
    if let Err(error) = std::fs::rename(&staged, link) {
        let _ = std::fs::remove_file(&staged); // silent-ok: cleanup; the rename error is returned
        return Err(error);
    }
    Ok(Outcome::Linked)
}

/// `…/<Name>.app/Contents/MacOS/zlo-backend`.
fn is_bundle_backend(path: &Path) -> bool {
    path.file_name().is_some_and(|n| n == "zlo-backend")
        && path.parent().is_some_and(|macos| {
            macos.ends_with("Contents/MacOS")
                && macos
                    .parent()
                    .and_then(Path::parent)
                    .and_then(Path::extension)
                    .is_some_and(|ext| ext == "app")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn backend_in(root: &Path, app: &str) -> PathBuf {
        let macos = root.join(app).join("Contents/MacOS");
        fs::create_dir_all(&macos).unwrap();
        let backend = macos.join("zlo-backend");
        fs::write(&backend, b"").unwrap();
        backend
    }

    #[test]
    fn the_link_follows_the_running_bundle_and_leaves_other_things_alone() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let link = root.join("home/.zlo/bin/zlo");
        let old = backend_in(root, "Old/ZLO.app");
        let new = backend_in(root, "Applications/ZLO.app");

        // Missing: created, folder and all.
        assert_eq!(refresh(&link, &old).unwrap(), Outcome::Linked);
        assert_eq!(fs::read_link(&link).unwrap(), old);
        // Same bundle: nothing to do.
        assert_eq!(refresh(&link, &old).unwrap(), Outcome::Unchanged);
        // The app moved, and the old copy is gone: the stale link follows.
        fs::remove_dir_all(root.join("Old")).unwrap();
        assert_eq!(refresh(&link, &new).unwrap(), Outcome::Linked);
        assert_eq!(fs::read_link(&link).unwrap(), new);

        // A symlink to something that is not a bundle's backend stays.
        let cargo_zlo = root.join("cargo/bin/zlo");
        fs::create_dir_all(cargo_zlo.parent().unwrap()).unwrap();
        fs::write(&cargo_zlo, b"").unwrap();
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&cargo_zlo, &link).unwrap();
        assert_eq!(refresh(&link, &new).unwrap(), Outcome::LeftAlone);
        assert_eq!(fs::read_link(&link).unwrap(), cargo_zlo);

        // A regular file stays.
        fs::remove_file(&link).unwrap();
        fs::write(&link, b"mine").unwrap();
        assert_eq!(refresh(&link, &new).unwrap(), Outcome::LeftAlone);
        assert_eq!(fs::read(&link).unwrap(), b"mine");
        // No staging leftovers.
        assert_eq!(fs::read_dir(link.parent().unwrap()).unwrap().count(), 1);
    }
}
