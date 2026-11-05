//! Where a mention may read on disk, and how it opens what it reads.
//!
//! Roots: a message's `mention_roots` are vetted before they grant anything.
//! A root must be an existing folder, and never `/`, the home folder or one
//! of its ancestors, a top-level system folder, `~/Library`, or a path the
//! sensitivity policy flags. The desktop sends the thread's project folder.
//!
//! Opening: a root is itself opened from `/` without following symlinks,
//! and that descriptor is kept. A mentioned path is reached from it,
//! one component at a time with `O_NOFOLLOW`. A symlink anywhere below the
//! root, swapped in before or during the open, fails the open instead of
//! being followed. The returned descriptor is the object that gets read, so
//! there is no gap between checking a path and reading it. A regular file
//! with more than one hard link is refused: its other name may sit outside
//! the root or carry a sensitive name.

use std::os::fd::{AsFd, OwnedFd};
use std::path::{Component, Path, PathBuf};

use ironclaw_safety::sensitive_paths::is_sensitive_path_str;
use rustix::fs::{FileType, Mode, OFlags};

/// Roots one message may grant.
const MAX_ROOTS: usize = 8;
/// Top-level folders that hold other people's or the system's files.
const BROAD_ROOTS: &[&str] = &[
    "/Users",
    "/home",
    "/Volumes",
    "/System",
    "/Library",
    "/Applications",
    "/private",
    "/private/var",
    "/private/tmp",
    "/private/etc",
    "/tmp",
    "/var",
    "/etc",
    "/usr",
    "/bin",
    "/sbin",
    "/opt",
    "/dev",
    "/root",
    "/mnt",
    "/nix",
];

/// One vetted root: the path as the client sent it, its canonical form,
/// and a descriptor on it held from vetting on.
#[derive(Debug)]
pub(super) struct Root {
    sent: PathBuf,
    canonical: PathBuf,
    fd: OwnedFd,
}

/// The vetted roots of one message.
#[derive(Debug, Default)]
pub(super) struct Roots(Vec<Root>);

/// An opened mention target: the descriptor to read and its real path.
pub(super) struct Opened {
    pub(super) fd: OwnedFd,
    pub(super) path: PathBuf,
    pub(super) size: u64,
}

/// What kind of object a mention expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Want {
    File,
    Dir,
}

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

/// Folders a listing never enters.
const HEAVY_DIRS: [&str; 6] = ["node_modules", "target", ".git", "dist", "build", ".venv"];

impl Roots {
    /// Keep the roots that may grant reads. `home` is the user's home
    /// folder (canonical), which no root may contain. Each kept root is
    /// opened from `/` one component at a time without following symlinks,
    /// and that descriptor is what every later open starts from.
    pub(super) fn vet(raw: &[String], home: Option<&Path>) -> Self {
        let home = home.and_then(|h| std::fs::canonicalize(h).ok());
        let mut out = Vec::new();
        for sent in raw.iter().take(MAX_ROOTS).map(PathBuf::from) {
            if !sent.is_absolute() {
                continue;
            }
            let Ok(canonical) = std::fs::canonicalize(&sent) else {
                continue; // silent-ok: an unreadable root grants nothing
            };
            if too_broad(&canonical, home.as_deref()) {
                tracing::debug!("mention root refused");
                continue;
            }
            // A symlink swapped into the path since `canonicalize` fails here.
            let Some(fd) = from_anchor(&canonical) else {
                tracing::debug!("mention root moved while vetted");
                continue;
            };
            out.push(Root {
                sent,
                canonical,
                fd,
            });
        }
        Self(out)
    }

    /// Open `raw` (absolute, as the mention names it) below a root's held
    /// descriptor without following symlinks.
    pub(super) fn open(&self, raw: &str, want: Want) -> Result<Opened, &'static str> {
        if self.0.is_empty() {
            return Err("no project folder was shared with this message");
        }
        let raw = Path::new(raw);
        let (root, rel) = self
            .0
            .iter()
            .find_map(|root| {
                raw.strip_prefix(&root.sent)
                    .or_else(|_| raw.strip_prefix(&root.canonical))
                    .ok()
                    .map(|rel| (root, rel))
            })
            .ok_or("outside the project folder")?;
        let parts: Vec<&std::ffi::OsStr> = rel
            .components()
            .map(|c| match c {
                Component::Normal(name) => Ok(name),
                _ => Err("outside the project folder"),
            })
            .collect::<Result<_, _>>()?;
        let path = if parts.is_empty() {
            root.canonical.clone()
        } else {
            root.canonical.join(rel)
        };
        if is_sensitive_path_str(&path.to_string_lossy()) {
            return Err("sensitive file");
        }
        let mut fd = root.fd.try_clone().map_err(|_| "unreadable")?;
        let last = parts.len().checked_sub(1);
        for (ix, name) in parts.iter().enumerate() {
            let flags = if Some(ix) == last && want == Want::File {
                // Non-blocking, so a FIFO swapped in cannot hang the open.
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK
            } else {
                DIR_FLAGS
            };
            fd = rustix::fs::openat(fd.as_fd(), *name, flags, Mode::empty())
                .map_err(|_| "not found, or reached through a symlink")?;
        }
        let stat = rustix::fs::fstat(&fd).map_err(|_| "unreadable")?;
        let kind = FileType::from_raw_mode(stat.st_mode);
        match want {
            Want::Dir if kind != FileType::Directory => return Err("not a folder"),
            Want::File if kind != FileType::RegularFile => return Err("not a regular file"),
            Want::File if stat.st_nlink > 1 => return Err("hard-linked file"),
            _ => {}
        }
        Ok(Opened {
            fd,
            path,
            size: u64::try_from(stat.st_size).unwrap_or(0),
        })
    }
}

/// `path` (canonical) opened from `/`, one no-follow component at a time.
fn from_anchor(path: &Path) -> Option<OwnedFd> {
    let mut fd = rustix::fs::open("/", DIR_FLAGS, Mode::empty()).ok()?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => {
                fd = rustix::fs::openat(fd.as_fd(), name, DIR_FLAGS, Mode::empty()).ok()?;
            }
            _ => return None,
        }
    }
    Some(fd)
}

/// Relative paths of the files below an opened folder, read through
/// descriptors only: subfolders are opened no-follow from their parent, so
/// nothing outside it is listed. Hidden and build folders are skipped;
/// `limit` bounds the entries read. Returns whether the walk was complete.
pub(super) fn list(dir: &OwnedFd, depth: usize, limit: usize) -> (Vec<String>, bool) {
    let mut files = Vec::new();
    let mut complete = true;
    let Ok(first) = dir.try_clone() else {
        return (files, false);
    };
    let mut stack: Vec<(OwnedFd, String, usize)> = vec![(first, String::new(), 0)];
    while let Some((fd, prefix, level)) = stack.pop() {
        let Ok(entries) = rustix::fs::Dir::read_from(&fd) else {
            complete = false;
            continue;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                complete = false;
                break;
            };
            if files.len() >= limit {
                return (files, false);
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || HEAVY_DIRS.contains(&name.as_str()) {
                continue;
            }
            let rel = format!("{prefix}{name}");
            let kind = match entry.file_type() {
                FileType::Unknown => rustix::fs::statat(
                    &fd,
                    entry.file_name(),
                    rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
                )
                .map(|st| FileType::from_raw_mode(st.st_mode))
                .unwrap_or(FileType::Unknown),
                kind => kind,
            };
            if kind == FileType::Directory {
                if level + 1 >= depth {
                    complete = false;
                    files.push(format!("{rel}/"));
                    continue;
                }
                match rustix::fs::openat(&fd, entry.file_name(), DIR_FLAGS, Mode::empty()) {
                    Ok(sub) => stack.push((sub, format!("{rel}/"), level + 1)),
                    Err(_) => complete = false,
                }
            } else {
                files.push(rel);
            }
        }
    }
    (files, complete)
}

/// Whether `root` (canonical) would grant far more than one project.
fn too_broad(root: &Path, home: Option<&Path>) -> bool {
    let normal = root
        .components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .count();
    normal < 2
        || BROAD_ROOTS.iter().any(|b| root == Path::new(b))
        || home.is_some_and(|home| home.starts_with(root) || root.starts_with(home.join("Library")))
        || is_sensitive_path_str(&root.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vet(roots: &[&Path], home: &Path) -> Roots {
        let raw: Vec<String> = roots.iter().map(|r| r.to_string_lossy().into()).collect();
        Roots::vet(&raw, Some(home))
    }

    #[test]
    fn broad_roots_grant_nothing() {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join("code/app");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(home.path().join("Library/Mail")).unwrap();
        let parent = home.path().parent().unwrap().to_path_buf();
        for broad in [
            Path::new("/"),
            Path::new("/Users"),
            Path::new("/tmp"),
            home.path(),
            parent.as_path(),
            &home.path().join("Library/Mail"),
        ] {
            assert!(
                vet(&[broad], home.path()).0.is_empty(),
                "{broad:?} was accepted"
            );
        }
        assert!(!vet(&[&project], home.path()).0.is_empty());
        assert!(!vet(&[&home.path().join("code")], home.path()).0.is_empty());
    }

    #[test]
    fn opens_never_follow_symlinks_or_hard_links() {
        let home = tempfile::tempdir().unwrap();
        let outside = home.path().join("outside");
        let project = home.path().join("code/app");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(project.join("src")).unwrap();
        std::fs::write(outside.join("secret.txt"), "OUTSIDE").unwrap();
        std::fs::write(project.join("src/ok.rs"), "fine").unwrap();
        let roots = vet(&[&project], home.path());
        let open = |p: PathBuf, want| roots.open(&p.to_string_lossy(), want).map(|o| o.path);

        assert_eq!(
            open(project.join("src/ok.rs"), Want::File).unwrap(),
            std::fs::canonicalize(project.join("src/ok.rs")).unwrap()
        );
        // A final-component symlink (what a swap leaves at open time).
        std::os::unix::fs::symlink(outside.join("secret.txt"), project.join("link.txt")).unwrap();
        assert!(open(project.join("link.txt"), Want::File).is_err());
        // An ancestor swapped for a symlink to elsewhere.
        std::fs::rename(project.join("src"), project.join("src-real")).unwrap();
        std::os::unix::fs::symlink(&outside, project.join("src")).unwrap();
        assert!(open(project.join("src/secret.txt"), Want::File).is_err());
        assert!(open(project.join("src"), Want::Dir).is_err());
        // A hard link keeps an innocent name for an outside inode.
        std::fs::hard_link(outside.join("secret.txt"), project.join("notes.txt")).unwrap();
        assert_eq!(
            open(project.join("notes.txt"), Want::File).unwrap_err(),
            "hard-linked file"
        );
        // Outside and sensitive paths never open.
        assert!(open(outside.join("secret.txt"), Want::File).is_err());
        std::fs::write(project.join(".env"), "K=v").unwrap();
        assert_eq!(
            open(project.join(".env"), Want::File).unwrap_err(),
            "sensitive file"
        );
    }

    /// The root is held as a descriptor from vetting on: an ancestor
    /// swapped for a symlink afterwards cannot redirect later opens, which
    /// keep reading the folder that was vetted.
    #[test]
    fn a_swapped_ancestor_cannot_redirect_the_root() {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join("code/app");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("notes.txt"), "VETTED").unwrap();
        let decoy = home.path().join("decoy/app");
        std::fs::create_dir_all(&decoy).unwrap();
        std::fs::write(decoy.join("notes.txt"), "DECOY").unwrap();
        let roots = vet(&[&project], home.path());
        std::fs::rename(home.path().join("code"), home.path().join("code-moved")).unwrap();
        std::os::unix::fs::symlink(home.path().join("decoy"), home.path().join("code")).unwrap();
        let opened = roots
            .open(&project.join("notes.txt").to_string_lossy(), Want::File)
            .unwrap();
        let text = std::fs::read_to_string(format!("/dev/fd/{}", {
            use std::os::fd::AsRawFd as _;
            opened.fd.as_raw_fd()
        }))
        .unwrap();
        assert_eq!(text, "VETTED");
    }
}
