//! Keeps the `zlo` command working: `<ZLO_HOME>/bin/zlo` (default
//! `~/.zlo/bin/zlo`) is a symlink to a ZLO bundle's
//! `Contents/MacOS/zlo-backend`, repaired on launch so a moved or
//! reinstalled app still answers `zlo triage`, `zlo update`, `zlo doctor`.
//!
//! `install.sh` creates the link and puts its folder on the PATH (the app
//! never edits shell profiles). Both writers share one ownership rule: a
//! link is ZLO's when it is absent, points at a ZLO bundle (Info.plist
//! `CFBundleIdentifier` = `com.numoh.desktop`), or dangles at exactly the
//! target recorded in `bin/.zlo-managed`. A regular file, or a symlink to
//! anything else, is never replaced. The app is the gentler writer: it only
//! creates a missing link or repairs a dangling one it owns, and leaves a
//! link to another live ZLO.app alone so two copies never fight over it
//! (`install.sh`, run on purpose, re-points it).
//!
//! The home comes from the bundled backend itself (`zlo-backend config
//! path`), which resolves it the way the backend does and moves a pre-ZLO
//! `~/.ironclaw/reborn` first: writing `~/.zlo/bin` before that move would
//! strand the old data behind a new, nearly empty `~/.zlo`.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The full app's bundle id (`bundle.sh`); ZLO Light has no backend.
const ZLO_BUNDLE_ID: &str = "com.numoh.desktop";

/// Next to the link: the target ZLO last wrote, so a dangling link (the
/// app it pointed into was deleted) can be told apart from someone else's.
const MARKER: &str = ".zlo-managed";

/// This bundle's `Contents/MacOS/zlo-backend`, when the app runs from a
/// ZLO.app that has one (never under `cargo run` or a Light bundle).
pub fn bundled_backend() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let backend = exe.parent()?.join("zlo-backend");
    (is_zlo_backend(&backend) && backend.is_file()).then_some(backend)
}

/// Repair the link off the UI thread. Failures are debug-logged only.
pub fn refresh_in_background() {
    let Some(target) = bundled_backend() else {
        return;
    };
    let spawned = std::thread::Builder::new()
        .name("zlo-cli-link".into())
        .spawn(
            move || match link_into_resolved_home(&target, || resolved_home(&target)) {
                Ok((link, outcome)) => {
                    tracing::debug!(?outcome, link = %link.display(), "zlo command link")
                }
                Err(error) => tracing::debug!(%error, "could not refresh the zlo command link"),
            },
        );
    if let Err(error) = spawned {
        tracing::debug!(%error, "could not start the zlo command link refresh");
    }
}

/// Resolve the home first, then link into its `bin/`; nothing is written
/// when the home does not resolve.
fn link_into_resolved_home(
    target: &Path,
    resolve: impl FnOnce() -> io::Result<PathBuf>,
) -> io::Result<(PathBuf, Outcome)> {
    let link = resolve()?.join("bin/zlo");
    let outcome = refresh(&link, target)?;
    Ok((link, outcome))
}

/// Ask the backend for its (migrated) home: `reborn_home: <path>` from
/// `zlo-backend config path`.
fn resolved_home(backend: &Path) -> io::Result<PathBuf> {
    let out = Command::new(backend)
        .args(["config", "path"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!(
            "`zlo-backend config path` exited with {}",
            out.status
        )));
    }
    parse_reborn_home(&String::from_utf8_lossy(&out.stdout))
        .ok_or_else(|| io::Error::other("`zlo-backend config path` printed no reborn_home"))
}

fn parse_reborn_home(stdout: &str) -> Option<PathBuf> {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("reborn_home: "))
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Already pointed at `target`.
    Unchanged,
    /// Created, or repaired a dangling link ZLO had made.
    Linked,
    /// Something that is not ours (or another live ZLO.app) is there.
    LeftAlone,
}

/// Create `link` → `target`, or repair a dangling link ZLO owns; leave
/// everything else alone.
pub fn refresh(link: &Path, target: &Path) -> io::Result<Outcome> {
    let dir = link
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "link has no folder"))?;
    match std::fs::symlink_metadata(link) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            std::fs::create_dir_all(dir)?;
            // Exclusive: `symlink` fails when anything appeared meanwhile.
            match std::os::unix::fs::symlink(target, link) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    return Ok(Outcome::LeftAlone);
                }
                Err(e) => return Err(e),
            }
            write_marker(dir, target)?;
            Ok(Outcome::Linked)
        }
        Err(e) => Err(e),
        Ok(meta) if !meta.file_type().is_symlink() => Ok(Outcome::LeftAlone),
        Ok(_) => {
            let raw = std::fs::read_link(link)?;
            if absolute_in(dir, &raw) == target {
                write_marker(dir, target)?;
                return Ok(Outcome::Unchanged);
            }
            if !owned_dangling(dir, &raw) {
                return Ok(Outcome::LeftAlone);
            }
            replace(link, &raw, target)
        }
    }
}

/// Swap `link` (which read as `expected`) for one to `target`: staged
/// beside it, re-checked just before the rename so a link or file that
/// appeared since the check is never replaced.
fn replace(link: &Path, expected: &Path, target: &Path) -> io::Result<Outcome> {
    let dir = link
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "link has no folder"))?;
    let staged = dir.join(format!(".zlo-link-{}", std::process::id()));
    let _ = std::fs::remove_file(&staged); // silent-ok: a leftover from a crashed launch, if any
    std::os::unix::fs::symlink(target, &staged)?;
    let still_ours = std::fs::symlink_metadata(link)
        .is_ok_and(|meta| meta.file_type().is_symlink())
        && std::fs::read_link(link).is_ok_and(|now| now == expected);
    if !still_ours {
        let _ = std::fs::remove_file(&staged); // silent-ok: cleanup of our own staged link
        return Ok(Outcome::LeftAlone);
    }
    if let Err(error) = std::fs::rename(&staged, link) {
        let _ = std::fs::remove_file(&staged); // silent-ok: cleanup; the rename error is returned
        return Err(error);
    }
    write_marker(dir, target)?;
    Ok(Outcome::Linked)
}

fn absolute_in(dir: &Path, raw: &Path) -> PathBuf {
    if raw.is_relative() {
        dir.join(raw)
    } else {
        raw.to_path_buf()
    }
}

/// A link whose target is gone and is exactly what ZLO last wrote.
fn owned_dangling(dir: &Path, raw: &Path) -> bool {
    let gone = matches!(
        std::fs::metadata(absolute_in(dir, raw)),
        Err(e) if e.kind() == io::ErrorKind::NotFound
    );
    gone && std::fs::read_to_string(dir.join(MARKER))
        .is_ok_and(|recorded| Path::new(recorded.trim_end_matches('\n')) == raw)
}

fn write_marker(dir: &Path, target: &Path) -> io::Result<()> {
    let marker = dir.join(MARKER);
    let line = format!("{}\n", target.display());
    if std::fs::read_to_string(&marker).is_ok_and(|now| now == line) {
        return Ok(());
    }
    std::fs::write(marker, line)
}

/// `…/<Name>.app/Contents/MacOS/zlo-backend` of an app whose Info.plist
/// names ZLO's bundle id.
fn is_zlo_backend(path: &Path) -> bool {
    let Some(contents) = path
        .parent()
        .filter(|_| path.file_name().is_some_and(|n| n == "zlo-backend"))
        .filter(|macos| macos.ends_with("Contents/MacOS"))
        .and_then(Path::parent)
    else {
        return false;
    };
    let is_app = contents
        .parent()
        .and_then(Path::extension)
        .is_some_and(|ext| ext == "app");
    is_app
        && std::fs::read_to_string(contents.join("Info.plist"))
            .is_ok_and(|plist| bundle_id(&plist) == Some(ZLO_BUNDLE_ID))
}

/// `CFBundleIdentifier` from an XML Info.plist (`bundle.sh` writes XML).
fn bundle_id(plist: &str) -> Option<&str> {
    let after = plist.split_once("<key>CFBundleIdentifier</key>")?.1;
    let value = after.trim_start().strip_prefix("<string>")?;
    Some(value.split_once("</string>")?.0.trim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn app_in(root: &Path, app: &str, id: &str) -> PathBuf {
        let contents = root.join(app).join("Contents");
        fs::create_dir_all(contents.join("MacOS")).unwrap();
        fs::write(
            contents.join("Info.plist"),
            format!(
                "<dict>\n    <key>CFBundleIdentifier</key>\n    <string>{id}</string>\n</dict>"
            ),
        )
        .unwrap();
        let backend = contents.join("MacOS/zlo-backend");
        fs::write(&backend, b"").unwrap();
        backend
    }

    fn marker(link: &Path) -> String {
        fs::read_to_string(link.parent().unwrap().join(MARKER)).unwrap()
    }

    #[test]
    fn the_link_is_created_and_repaired_only_where_zlo_owns_it() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let link = root.join("home/.zlo/bin/zlo");
        let old = app_in(root, "Old/ZLO.app", ZLO_BUNDLE_ID);
        let new = app_in(root, "Applications/ZLO.app", ZLO_BUNDLE_ID);
        assert!(is_zlo_backend(&old));
        assert!(!is_zlo_backend(&app_in(
            root,
            "Other.app",
            "com.example.other"
        )));

        // Missing: created exclusively, folder and all; ownership recorded.
        assert_eq!(refresh(&link, &old).unwrap(), Outcome::Linked);
        assert_eq!(fs::read_link(&link).unwrap(), old);
        assert_eq!(marker(&link), format!("{}\n", old.display()));
        assert_eq!(refresh(&link, &old).unwrap(), Outcome::Unchanged);

        // Another live ZLO.app holds it: no tug of war between two copies.
        assert_eq!(refresh(&link, &new).unwrap(), Outcome::LeftAlone);
        assert_eq!(fs::read_link(&link).unwrap(), old);

        // The app it pointed into is gone, and the marker says it was ours.
        fs::remove_dir_all(root.join("Old")).unwrap();
        assert_eq!(refresh(&link, &new).unwrap(), Outcome::Linked);
        assert_eq!(fs::read_link(&link).unwrap(), new);
        assert_eq!(marker(&link), format!("{}\n", new.display()));

        // A dangling link ZLO did not write (the marker names another) stays.
        fs::remove_file(&link).unwrap();
        let theirs = root.join("gone/zlo");
        std::os::unix::fs::symlink(&theirs, &link).unwrap();
        assert_eq!(refresh(&link, &new).unwrap(), Outcome::LeftAlone);
        assert_eq!(fs::read_link(&link).unwrap(), theirs);

        // A symlink to something else stays.
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
        // No staging leftovers: the link and the marker only.
        assert_eq!(fs::read_dir(link.parent().unwrap()).unwrap().count(), 2);
    }

    #[test]
    fn a_link_that_changed_after_the_check_is_not_replaced() {
        let root = tempfile::tempdir().unwrap();
        let link = root.path().join("bin/zlo");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        let new = app_in(root.path(), "ZLO.app", ZLO_BUNDLE_ID);
        // Checked as `/gone/zlo-backend`, but by the rename it is someone else's.
        std::os::unix::fs::symlink("/elsewhere/zlo", &link).unwrap();
        assert_eq!(
            replace(&link, Path::new("/gone/zlo-backend"), &new).unwrap(),
            Outcome::LeftAlone
        );
        assert_eq!(fs::read_link(&link).unwrap(), Path::new("/elsewhere/zlo"));
        assert_eq!(fs::read_dir(link.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn the_link_waits_for_the_resolved_home() {
        let root = tempfile::tempdir().unwrap();
        let backend = app_in(root.path(), "ZLO.app", ZLO_BUNDLE_ID);
        let home = root.path().join(".zlo");
        let failed = link_into_resolved_home(&backend, || Err(io::Error::other("no home")));
        assert!(failed.is_err());
        assert!(
            !home.exists(),
            "nothing is written before the home resolves"
        );

        let (link, outcome) = link_into_resolved_home(&backend, || Ok(home.clone())).unwrap();
        assert_eq!((link, outcome), (home.join("bin/zlo"), Outcome::Linked));

        let out =
            "IronClaw Reborn config path\nreborn_home: /Users/me/.zlo\nhome_source: default\n";
        assert_eq!(
            parse_reborn_home(out),
            Some(PathBuf::from("/Users/me/.zlo"))
        );
        assert_eq!(parse_reborn_home("reborn_home: relative\n"), None);
    }
}
