//! Where a mention may read on disk, and how it opens what it reads.
//!
//! Roots: a message's `mention_roots` are vetted before they grant anything.
//! A root must be an existing folder, and never `/`, the home folder or one
//! of its ancestors, a top-level system folder, `~/Library`, or a path the
//! sensitivity policy flags. The desktop sends the thread's project folder.
//!
//! The thread workspace: a client with no view of this machine's disk (ZLO
//! with a remote backend) names files by their workspace path,
//! `/workspace/src/main.rs`. The host adds the folder `/workspace` names for
//! the sender as one more root whose *sent* spelling is that alias, so the
//! mapping from the virtual path to the real one happens here, once, and the
//! rest is opened below the real folder's descriptor exactly like any other
//! root. The host chose that folder, so the client-root breadth rules do not
//! apply to it; containment, no-follow opens and the sensitivity checks do.
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
use std::time::{Duration, Instant};

use rustix::fs::{AtFlags, FileType, Mode, OFlags};

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
    /// `(device, inode)` of `fd`: what another spelling of the root must be.
    id: (u64, u64),
    /// `sent` is a virtual alias (`/workspace`), not a path on this disk.
    alias: bool,
}

/// The vetted roots of one message.
#[derive(Debug, Default)]
pub(super) struct Roots(Vec<Root>);

/// An opened mention target: the descriptor to read and its real path.
pub(super) struct Opened {
    pub(super) fd: OwnedFd,
    pub(super) path: PathBuf,
    /// How the mention named it: the alias path under the thread workspace,
    /// else the real path.
    pub(super) shown: PathBuf,
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
    /// and that descriptor is what every later open starts from. `workspace`
    /// is the host folder the workspace alias names for the sender
    /// (`(alias, folder)`), kept ahead of the client's roots.
    pub(super) fn vet_with_workspace(
        raw: &[String],
        home: Option<&Path>,
        workspace: Option<(&str, &Path)>,
    ) -> Self {
        let home = home.and_then(|h| std::fs::canonicalize(h).ok());
        let mut out = Vec::new();
        if let Some((alias, folder)) = workspace
            && let Some(root) = host_root(alias, folder)
        {
            out.push(root);
        }
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
            let Some(id) = identity(&fd) else {
                continue; // silent-ok: an unreadable root grants nothing
            };
            out.push(Root {
                sent,
                canonical,
                fd,
                id,
                alias: false,
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
        let (root, rel) = self.locate(raw).ok_or("outside the project folder")?;
        let parts: Vec<&std::ffi::OsStr> = rel
            .components()
            .map(|c| match c {
                Component::Normal(name) => Ok(name),
                _ => Err("outside the project folder"),
            })
            .collect::<Result<_, _>>()?;
        let (path, shown) = if parts.is_empty() {
            (root.canonical.clone(), root.sent.clone())
        } else {
            (root.canonical.join(rel), root.sent.join(rel))
        };
        let shown = if root.alias { shown } else { path.clone() };
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
            shown,
            size: u64::try_from(stat.st_size).unwrap_or(0),
        })
    }
}

impl Roots {
    /// The root `raw` lies in, and the rest of `raw` below it. The root's
    /// own spellings match directly; any other spelling of the same folder
    /// (case or Unicode-normalization variants on APFS) is recognized by
    /// the identity of the folder it names. Locating only picks the root:
    /// the rest is still opened no-follow from the root's descriptor.
    fn locate<'a>(&self, raw: &'a Path) -> Option<(&Root, &'a Path)> {
        if let Some(found) = self.0.iter().find_map(|root| {
            raw.strip_prefix(&root.sent)
                .or_else(|_| raw.strip_prefix(&root.canonical))
                .ok()
                .map(|rel| (root, rel))
        }) {
            return Some(found);
        }
        for ancestor in raw.ancestors() {
            let Ok(meta) = std::fs::metadata(ancestor) else {
                continue;
            };
            use std::os::unix::fs::MetadataExt as _;
            let id = (meta.dev(), meta.ino());
            if let Some(root) = self.0.iter().find(|root| root.id == id) {
                return raw.strip_prefix(ancestor).ok().map(|rel| (root, rel));
            }
        }
        None
    }
}

/// The workspace root: `folder` (host-chosen) held like a vetted root, named
/// `alias` by mentions. `None` when it does not exist yet or is not a folder
/// reachable without symlinks.
fn host_root(alias: &str, folder: &Path) -> Option<Root> {
    let canonical = std::fs::canonicalize(folder).ok()?; // silent-ok: a workspace not created yet grants nothing
    let deep = canonical
        .components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .count();
    if deep < 2 || is_sensitive_path_str(&canonical.to_string_lossy()) {
        tracing::debug!("workspace mention root refused");
        return None;
    }
    let fd = from_anchor(&canonical)?;
    let id = identity(&fd)?;
    Some(Root {
        sent: PathBuf::from(alias),
        canonical,
        fd,
        id,
        alias: true,
    })
}

/// `(device, inode)` of an open descriptor.
fn identity(fd: &OwnedFd) -> Option<(u64, u64)> {
    let stat = rustix::fs::fstat(fd).ok()?;
    Some((stat.st_dev as u64, stat.st_ino as u64))
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

/// Bounds of one folder listing.
#[derive(Debug, Clone, Copy)]
pub(super) struct Limits {
    /// Folder levels entered below the listed one.
    pub(super) depth: usize,
    /// Directory entries read in all, listed or not.
    pub(super) max_visited: usize,
    /// Names returned.
    pub(super) max_files: usize,
    /// The walk stops here.
    pub(super) deadline: Instant,
}

impl Limits {
    pub(super) fn new(depth: usize, max_visited: usize, max_files: usize) -> Self {
        Self {
            depth,
            max_visited,
            max_files,
            deadline: Instant::now() + LIST_TIME,
        }
    }
}

/// What a listing found.
#[derive(Debug, Default)]
pub(super) struct Listing {
    /// Paths relative to the listed folder; folders end in `/` when the
    /// walk stopped at them.
    pub(super) files: Vec<String>,
    /// No bound cut the walk.
    pub(super) complete: bool,
    /// Most descriptors the walk held at once.
    pub(super) max_open: usize,
}

/// A listing stops after this long.
const LIST_TIME: Duration = Duration::from_secs(2);

/// The files below an opened folder, read through descriptors only: each
/// subfolder is opened no-follow relative to its parent's descriptor, depth
/// first, and closed when done, so a handful of descriptors are open at
/// once and nothing outside the folder is reached. Hidden and build folders
/// are skipped; symlinks and hard-linked files are not listed.
pub(super) fn list(dir: &OwnedFd, limits: Limits) -> Listing {
    let mut walk = Walk {
        limits,
        visited: 0,
        open: 0,
        out: Listing {
            complete: true,
            ..Listing::default()
        },
    };
    walk.visit(dir, "", 0);
    walk.out
}

struct Walk {
    limits: Limits,
    visited: usize,
    open: usize,
    out: Listing,
}

impl Walk {
    fn opened(&mut self, n: usize) {
        self.open += n;
        self.out.max_open = self.out.max_open.max(self.open);
    }

    /// Whether a bound is hit; marks the listing cut when so.
    fn spent(&mut self) -> bool {
        let spent = self.visited >= self.limits.max_visited
            || self.out.files.len() >= self.limits.max_files
            || Instant::now() >= self.limits.deadline;
        if spent {
            self.out.complete = false;
        }
        spent
    }

    fn visit(&mut self, fd: &OwnedFd, prefix: &str, level: usize) {
        // `Dir` reads through its own descriptor on the folder.
        let Ok(entries) = rustix::fs::Dir::read_from(fd) else {
            self.out.complete = false;
            return;
        };
        self.opened(1);
        for entry in entries {
            if self.spent() {
                break;
            }
            self.visited += 1;
            let Ok(entry) = entry else {
                self.out.complete = false;
                break;
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "." || name == ".." {
                continue;
            }
            // The kind comes from `lstat` relative to the parent: symlinks
            // are seen as symlinks, and the link count is known.
            let Ok(stat) = rustix::fs::statat(fd, entry.file_name(), AtFlags::SYMLINK_NOFOLLOW)
            else {
                continue;
            };
            let rel = format!("{prefix}{name}");
            match FileType::from_raw_mode(stat.st_mode) {
                FileType::Directory => {
                    if name.starts_with('.') || HEAVY_DIRS.contains(&name.as_str()) {
                        continue;
                    }
                    if level + 1 >= self.limits.depth {
                        self.out.complete = false;
                        self.out.files.push(format!("{rel}/"));
                        continue;
                    }
                    let Ok(sub) =
                        rustix::fs::openat(fd, entry.file_name(), DIR_FLAGS, Mode::empty())
                    else {
                        self.out.complete = false;
                        continue;
                    };
                    self.opened(1);
                    self.visit(&sub, &format!("{rel}/"), level + 1);
                    drop(sub);
                    self.open -= 1;
                }
                FileType::RegularFile if stat.st_nlink == 1 => self.out.files.push(rel),
                // Symlinks, hard-linked files, devices, FIFOs, sockets.
                _ => {}
            }
        }
        self.open -= 1;
    }
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
        Roots::vet_with_workspace(&raw, Some(home), None)
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

    fn open_dir(path: &Path) -> OwnedFd {
        rustix::fs::open(path, DIR_FLAGS, Mode::empty()).unwrap()
    }

    /// 500 sibling folders list completely while only a handful of
    /// descriptors are open at once; visited entries, depth and time are
    /// bounded, and `complete` says when a bound cut the walk.
    #[test]
    fn listings_are_bounded_and_keep_few_descriptors_open() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..500 {
            let sub = dir.path().join(format!("d{i:03}"));
            std::fs::create_dir(&sub).unwrap();
            std::fs::write(sub.join("f.txt"), "").unwrap();
        }
        let fd = open_dir(dir.path());
        let all = list(&fd, Limits::new(4, 10_000, 10_000));
        assert_eq!(all.files.len(), 500, "{}", all.files.len());
        assert!(all.complete);
        assert!(
            all.max_open <= 2 * 4 + 2,
            "{} descriptors open at once",
            all.max_open
        );

        let few = list(&fd, Limits::new(4, 10_000, 50));
        assert!(!few.complete && few.files.len() <= 50);
        let shallow = list(&fd, Limits::new(1, 10_000, 10_000));
        // At the depth cut, folders are named but not entered.
        assert!(!shallow.complete && shallow.files.iter().all(|f| f.ends_with('/')));
        let late = list(
            &fd,
            Limits {
                deadline: std::time::Instant::now(),
                ..Limits::new(4, 10_000, 10_000)
            },
        );
        assert!(!late.complete && late.files.len() < 500);
    }

    /// A listing names no symlink and no hard-linked file.
    #[test]
    fn listings_skip_links() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("p");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("plain.txt"), "").unwrap();
        std::fs::write(dir.path().join(".env"), "S=1").unwrap();
        std::fs::hard_link(dir.path().join(".env"), project.join("innocent.txt")).unwrap();
        std::os::unix::fs::symlink(dir.path(), project.join("up")).unwrap();
        let listed = list(&open_dir(&project), Limits::new(4, 100, 100));
        assert_eq!(listed.files, ["plain.txt"]);
    }

    /// A mention whose path names the vetted root by another spelling of the
    /// same folder (case, Unicode normalization) still resolves to it.
    #[test]
    fn aliases_of_the_root_resolve_by_identity() {
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join("code/Café");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("notes.txt"), "N").unwrap();
        let roots = vet(&[&project], home.path());
        let lower = home.path().join("code/café");
        let nfd = home.path().join("code/Cafe\u{301}");
        for alias in [lower, nfd] {
            if !alias.exists() {
                continue; // a case- or normalization-sensitive file system
            }
            let opened = roots.open(&alias.join("notes.txt").to_string_lossy(), Want::File);
            assert!(opened.is_ok(), "{alias:?}: {:?}", opened.err());
        }
        // A different folder that merely shares a prefix is still outside.
        let sibling = home.path().join("code/Café2");
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("x.txt"), "X").unwrap();
        assert!(
            roots
                .open(&sibling.join("x.txt").to_string_lossy(), Want::File)
                .is_err()
        );
    }
}
