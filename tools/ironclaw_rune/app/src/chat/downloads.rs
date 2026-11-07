//! Where files fetched from the gateway land on this Mac.
//!
//! Every id and name here comes from the gateway, so none of it is used as
//! a path: the folder is a hash of the ids (one per distinct remote file),
//! and the file keeps only the last plain component of its name. Bytes go
//! to a fresh `create_new` temp file in that folder first and are renamed
//! into place, so two downloads of one file never interleave and a rename
//! replaces (never follows) whatever sits at the destination.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The app's download root under the user's temp folder.
fn root() -> PathBuf {
    std::env::temp_dir().join("zlo")
}

/// FNV-1a over the parts, each followed by a separator byte, so
/// `["ab", "c"]` and `["a", "bc"]` differ. Stable across builds.
fn hash(parts: &[&str]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for b in part.bytes().chain(std::iter::once(0xff)) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

/// The folder for one remote file, named `kind-<hash of parts>`. `kind` is
/// one of this app's constants, never gateway data.
pub(crate) fn dir_for(kind: &'static str, parts: &[&str]) -> PathBuf {
    root().join(format!("{kind}-{:016x}", hash(parts)))
}

/// The last plain component of a gateway-supplied file name; "file" when
/// there is none (empty, `.`, `..`, a bare root).
pub(crate) fn file_name(name: &str) -> OsString {
    match Path::new(name).components().next_back() {
        Some(Component::Normal(n)) => n.to_owned(),
        _ => "file".into(),
    }
}

/// Write `bytes` as `dir/name` (`name` already passed through
/// [`file_name`]) through a per-request temp file and a rename. Blocking.
pub(crate) fn save(dir: &Path, name: &OsString, bytes: &[u8]) -> std::io::Result<PathBuf> {
    use std::io::Write;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    std::fs::create_dir_all(dir)?;
    let path = dir.join(name);
    let part = dir.join(format!(
        ".part-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let written = (|| {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part)?;
        f.write_all(bytes)?;
        f.flush()?;
        std::fs::rename(&part, &path)
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&part);
        return Err(e);
    }
    Ok(path)
}

/// [`save`] off the network runtime's worker threads.
pub(crate) async fn save_async(
    dir: PathBuf,
    name: OsString,
    bytes: Vec<u8>,
) -> std::io::Result<PathBuf> {
    tokio::task::spawn_blocking(move || save(&dir, &name, &bytes))
        .await
        .map_err(std::io::Error::other)?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hostile_ids_stay_inside_the_download_root() {
        let root = root();
        for ids in [
            ["/etc", "passwd"],
            ["../../..", "x"],
            ["..", ".."],
            ["a/../../b", "/"],
        ] {
            let dir = dir_for("attachment", &ids);
            assert_eq!(dir.parent(), Some(root.as_path()), "{ids:?} → {dir:?}");
            let name = dir_for("attachment", &ids)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            assert!(name.starts_with("attachment-"), "{name}");
        }
    }

    #[test]
    fn names_keep_only_their_last_plain_component() {
        assert_eq!(file_name("/etc/passwd"), "passwd");
        assert_eq!(file_name("../../.zshrc"), ".zshrc");
        assert_eq!(file_name(".."), "file");
        assert_eq!(file_name("a/.."), "file");
        assert_eq!(file_name(""), "file");
        assert_eq!(file_name("/"), "file");
        assert_eq!(file_name("report.pdf"), "report.pdf");
    }

    #[test]
    fn same_basename_in_different_folders_gets_different_dirs() {
        let a = dir_for("output", &["t1", "/out/a/report.md"]);
        let b = dir_for("output", &["t1", "/out/b/report.md"]);
        assert_ne!(a, b);
        assert_ne!(dir_for("x", &["ab", "c"]), dir_for("x", &["a", "bc"]));
    }

    #[test]
    fn save_replaces_a_symlink_instead_of_following_it() {
        let base = std::env::temp_dir().join(format!("zlo-dl-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let victim = base.join("victim.txt");
        std::fs::write(&victim, "keep").unwrap();
        let dir = base.join("dl");
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(&victim, dir.join("f.txt")).unwrap();
        let path = save(&dir, &file_name("f.txt"), b"new").unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "keep");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with(".part"))
            .collect();
        assert!(leftovers.is_empty());
        let _ = std::fs::remove_dir_all(&base);
    }
}
