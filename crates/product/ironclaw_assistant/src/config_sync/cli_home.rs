//! `cli_home` domain handler: the person's own rules and skills files in
//! the homes of the coding-agent CLIs, which those CLIs read by themselves
//! and no ZLO store holds.
//!
//! The roots (a folder and the entries in it that count) are handed in by
//! the host; this handler knows no CLI by name and touches nothing outside
//! them. An item is one file, keyed `<root id>/<path inside the root>`, so
//! the same file matches on two computers whatever the root folder is
//! called there.
//!
//! One-way, the source wins; a file only the receiver has is kept, and
//! nothing is deleted.
//!
//! Every read and write goes down from a descriptor of the root, one name
//! at a time, never through a link: a root, folder or file that is a
//! symbolic link is not read and not written through, and swapping one in
//! while a pass runs changes nothing, because nothing is looked up by path
//! again. Hidden names, hard-linked files and names that look like a
//! sign-in or settings file are left where they are.

use std::collections::BTreeMap;
use std::io::{self, Read as _, Write as _};
use std::os::fd::{AsFd, OwnedFd};
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use async_trait::async_trait;
use ironclaw_product_contracts::config_sync::{
    ConfigSyncDomain, ConfigSyncDomainDigest, ConfigSyncDomainPayload, ItemizedDomainPayload,
};
use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use uuid::Uuid;

use super::{
    ConfigSyncContext, ConfigSyncDomainHandler, ConfigSyncError, DomainApplyResult, items,
};

const DOMAIN: ConfigSyncDomain = ConfigSyncDomain::CliHome;
/// The mode note of an executable file (`ConfigSyncItemFile::path`).
const EXECUTABLE: &str = "executable";
/// Folders below an entry that are walked, and names an item key may have
/// below its entry.
const MAX_DEPTH: usize = 8;
/// Files listed across every root.
const MAX_FILES: usize = 4_000;
/// Names read from one folder.
const MAX_FOLDER_NAMES: usize = 2_000;
/// Names looked at across every root, carried or not.
const MAX_VISITED: usize = 20_000;
/// Lines saying what was left out; the rest are counted.
const MAX_NOTES: usize = 40;

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
/// Non-blocking, so a FIFO put in a file's place cannot hang the open.
const ANY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC)
    .union(OFlags::NONBLOCK);

/// One folder of the person's and the entries in it that are carried.
#[derive(Debug, Clone)]
pub struct CliHomeRoot {
    /// Stable name of the root, the first segment of every item key.
    pub id: String,
    pub dir: PathBuf,
    pub entries: Vec<CliHomeEntry>,
}

/// An entry of a root, by its path inside the root: exactly one file, or a
/// folder and everything carried below it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliHomeEntry {
    pub path: String,
    pub folder: bool,
}

/// What a file's digest was computed from: any change to the bytes, the
/// mode or the file itself changes one of these.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    size: i128,
    modified: (i128, i128),
    changed: (i128, i128),
    mode: u32,
    inode: u128,
}

impl Stamp {
    fn of(stat: &rustix::fs::Stat) -> Self {
        Self {
            size: stat.st_size as i128,
            modified: (stat.st_mtime as i128, stat.st_mtime_nsec as i128),
            changed: (stat.st_ctime as i128, stat.st_ctime_nsec as i128),
            mode: stat.st_mode as u32,
            inode: stat.st_ino as u128,
        }
    }
}

/// Everything carried here: item key to digest, and what was left out
/// and why.
#[derive(Default)]
struct Listing {
    files: BTreeMap<String, String>,
    not_carried: Vec<String>,
    /// Left out beyond the first [`MAX_NOTES`].
    more_not_carried: usize,
    /// Names looked at so far.
    visited: usize,
    /// The digests of this listing, to answer the next one.
    seen: BTreeMap<String, (Stamp, String)>,
}

pub struct CliHomeDomainHandler {
    roots: Vec<CliHomeRoot>,
    /// Digests by item key, valid while the file's stamp holds, so a
    /// manifest does not read every file again. Replaced by each listing,
    /// so it never holds more than one listing's files.
    seen: Mutex<BTreeMap<String, (Stamp, String)>>,
}

fn rx(error: rustix::io::Errno) -> io::Error {
    io::Error::from(error)
}

impl Listing {
    fn note(&mut self, line: String) {
        if self.not_carried.len() < MAX_NOTES {
            self.not_carried.push(line);
        } else {
            self.more_not_carried += 1;
        }
    }
}

/// A name that can be carried at all: printable ASCII, so that no other
/// spelling of it names the same file on a volume that folds case or
/// Unicode; no path separators; and not ending in a dot or a space, which
/// some volumes drop.
fn plain(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| (b.is_ascii_graphic() || b == b' ') && b != b'/' && b != b'\\')
        && !name.ends_with(['.', ' '])
}

/// A name that is hidden, or is where a sign-in, a key or a CLI's settings
/// are kept: never read and never written, wherever it sits.
///
/// Judged on the name's stem (up to its first dot) and its extensions, so
/// `settings.json.bak` and `api_token.txt` are held back while
/// `tokenizer.md` and `oauth.md` are ordinary files.
fn withheld(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower.starts_with('.') {
        return true;
    }
    let (stem, extensions) = lower.split_once('.').unwrap_or((lower.as_str(), ""));
    const STEMS: [&str; 14] = [
        "settings",
        "auth",
        "credentials",
        "credential",
        "secrets",
        "secret",
        "token",
        "tokens",
        "password",
        "passwords",
        "oauth_creds",
        "id_rsa",
        "id_dsa",
        "id_ecdsa",
    ];
    const ENDINGS: [&str; 8] = [
        "token",
        "tokens",
        "secret",
        "secrets",
        "credentials",
        "password",
        "passwords",
        "key",
    ];
    const EXTENSIONS: [&str; 12] = [
        "pem", "key", "p12", "pfx", "jks", "keystore", "kdbx", "env", "ppk", "gpg", "asc", "netrc",
    ];
    STEMS.contains(&stem)
        || stem.starts_with("id_ed25519")
        || ENDINGS.iter().any(|end| {
            stem.strip_suffix(end)
                .is_some_and(|rest| rest.ends_with(['_', '-']))
        })
        || extensions.split('.').any(|ext| EXTENSIONS.contains(&ext))
        || lower == "config.toml"
}

/// Bytes that are a private key, whatever the file is called.
fn is_private_key(bytes: &[u8]) -> bool {
    const MARK: &[u8] = b"PRIVATE KEY-----";
    bytes.windows(MARK.len()).any(|window| window == MARK)
}

/// The root folder, opened without following a link at its last name,
/// however the path was spelled (a trailing `/` or `/.` would otherwise
/// follow one). `create`: make that one folder when it is missing; its
/// parent is the person's own and must be there already.
fn open_root(dir: &std::path::Path, create: bool) -> io::Result<OwnedFd> {
    // A relative root has no place of its own to be found from.
    if !dir.is_absolute() {
        return Err(io::Error::other("the root is not an absolute path"));
    }
    let dir: PathBuf = dir.components().collect();
    let (Some(parent), Some(name)) = (dir.parent(), dir.file_name()) else {
        return Err(io::Error::other("the root has no name"));
    };
    let parent = rustix::fs::open(
        parent,
        DIR_FLAGS.difference(OFlags::NOFOLLOW),
        Mode::empty(),
    )
    .map_err(rx)?;
    match rustix::fs::openat(parent.as_fd(), name, DIR_FLAGS, Mode::empty()) {
        Err(rustix::io::Errno::NOENT) if create => {
            rustix::fs::mkdirat(parent.as_fd(), name, Mode::RWXU).map_err(rx)?;
            rustix::fs::openat(parent.as_fd(), name, DIR_FLAGS, Mode::empty()).map_err(rx)
        }
        opened => opened.map_err(rx),
    }
}

/// Whether `name` is in `dir` under exactly that spelling. A name there
/// that differs only in case is, on a volume that ignores case, the same
/// entry: it is the receiver's own, and going through it or replacing it
/// is refused.
fn held_exactly(dir: &OwnedFd, name: &str) -> io::Result<bool> {
    let mut found = false;
    let mut read = 0usize;
    for entry in rustix::fs::Dir::read_from(dir).map_err(rx)?.flatten() {
        read += 1;
        if read > MAX_FOLDER_NAMES + 2 {
            return Err(io::Error::other(
                "the folder has too many entries to check a name against",
            ));
        }
        let held = entry.file_name().to_string_lossy();
        if held == name {
            found = true;
        } else if held.eq_ignore_ascii_case(name) {
            return Err(io::Error::other(format!(
                "`{held}` is there, with a name that differs only in case"
            )));
        }
    }
    // Not listed under this spelling, and still there when asked for by
    // it: the volume takes another spelling (a letter it folds, a
    // different Unicode form) for the same entry.
    if !found && rustix::fs::statat(dir.as_fd(), name, AtFlags::SYMLINK_NOFOLLOW).is_ok() {
        return Err(io::Error::other(
            "an entry is there under another spelling of this name",
        ));
    }
    Ok(found)
}

/// Run a walk of the disk off the async workers where the runtime allows.
fn off_the_reactor<T>(work: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(work)
        }
        _ => work(),
    }
}

fn file_hash(bytes: &[u8], exec: bool) -> String {
    let mode = if exec { EXECUTABLE } else { "" };
    items::files_hash(&[(mode.to_string(), bytes.to_vec())])
}

fn is_executable(stat: &rustix::fs::Stat) -> bool {
    stat.st_mode as u32 & 0o111 != 0
}

/// The whole of an open regular file, which is at most `limit` bytes.
fn read_all(fd: OwnedFd, limit: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    std::fs::File::from(fd)
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(io::Error::other("the file grew past the limit"));
    }
    Ok(bytes)
}

impl CliHomeDomainHandler {
    pub fn new(roots: Vec<CliHomeRoot>) -> Self {
        Self {
            roots,
            seen: Mutex::new(BTreeMap::new()),
        }
    }

    /// The handler over the roots the CLI runtimes name for the person
    /// whose home is `home` (`ironclaw_cli_providers::user_rules`).
    pub fn for_home(home: PathBuf) -> Self {
        let roots = ironclaw_cli_providers::user_rules::roots(&home)
            .into_iter()
            .map(|root| CliHomeRoot {
                id: root.id.to_string(),
                dir: root.dir,
                entries: root
                    .entries
                    .iter()
                    .map(|entry| CliHomeEntry {
                        path: entry.path().to_string(),
                        folder: matches!(
                            entry,
                            ironclaw_cli_providers::user_rules::UserRulesEntry::Folder(_)
                        ),
                    })
                    .collect(),
            })
            .collect();
        Self::new(roots)
    }

    fn list(&self) -> Listing {
        off_the_reactor(|| self.list_now())
    }

    fn list_now(&self) -> Listing {
        let known = self
            .seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let mut out = Listing::default();
        for root in &self.roots {
            // A root that is missing, or is itself a link, has nothing.
            let Ok(top) = open_root(&root.dir, false) else {
                continue;
            };
            for entry in &root.entries {
                let names: Vec<&str> = entry.path.split('/').collect();
                let Some((last, folders)) = names.split_last() else {
                    continue;
                };
                let Ok(mut dir) = top.try_clone() else {
                    continue;
                };
                let reached = folders.iter().all(|name| {
                    match rustix::fs::openat(dir.as_fd(), *name, DIR_FLAGS, Mode::empty()) {
                        Ok(next) => {
                            dir = next;
                            true
                        }
                        Err(_) => false,
                    }
                });
                if reached {
                    let key = format!("{}/{}", root.id, entry.path);
                    // A file entry is that file only; a folder entry, that
                    // folder only.
                    let depth = if entry.folder { Some(0) } else { None };
                    walk(&dir, last, key, depth, &known, &mut out);
                }
            }
        }
        if out.more_not_carried > 0 {
            out.not_carried
                .push(format!("{} more files", out.more_not_carried));
        }
        *self.seen.lock().unwrap_or_else(PoisonError::into_inner) = out.seen.clone();
        out
    }

    /// Where an incoming item belongs here: its root and the names below
    /// it. `None` when the key names no root and entry of this host (a
    /// file entry is itself and nothing below it), is deeper than a
    /// listing goes, or has a name that is not plain or is withheld.
    fn target<'a>(&self, key: &'a str) -> Option<(&CliHomeRoot, Vec<&'a str>)> {
        let (id, rel) = key.split_once('/')?;
        let root = self.roots.iter().find(|root| root.id == id)?;
        let entry = root.entries.iter().find(|entry| {
            rel == entry.path
                || (entry.folder
                    && rel
                        .strip_prefix(entry.path.as_str())
                        .is_some_and(|rest| rest.starts_with('/')))
        })?;
        let names: Vec<&str> = rel.split('/').collect();
        let below = names.len() - entry.path.split('/').count();
        // A folder entry is not itself a file.
        if entry.folder && below == 0 {
            return None;
        }
        // The entry's own names are the host's; what lies below must be a
        // plain name that is not withheld.
        let allowed = names[names.len() - below..]
            .iter()
            .all(|name| plain(name) && !withheld(name));
        (allowed && below <= MAX_DEPTH).then_some((root, names))
    }

    /// One listed file's bytes and mode, read the way it was listed.
    fn read(&self, key: &str) -> io::Result<(Vec<u8>, bool)> {
        let (root, names) = self
            .target(key)
            .ok_or_else(|| io::Error::other("not a carried file"))?;
        let mut fd = open_root(&root.dir, false)?;
        let last = names.len() - 1;
        for (at, name) in names.iter().enumerate() {
            let flags = if at == last { ANY_FLAGS } else { DIR_FLAGS };
            fd = rustix::fs::openat(fd.as_fd(), *name, flags, Mode::empty()).map_err(rx)?;
        }
        let stat = rustix::fs::fstat(&fd).map_err(rx)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile || stat.st_nlink > 1 {
            return Err(io::Error::other("not a plain file"));
        }
        let exec = is_executable(&stat);
        Ok((read_all(fd, items::ITEM_CONTENT_BYTES)?, exec))
    }

    /// Write one file in place of what is there: down from the root's
    /// descriptor, making missing folders on the way, through a temporary
    /// file beside it. Readable by the person only.
    fn write(
        &self,
        root: &CliHomeRoot,
        names: &[&str],
        bytes: &[u8],
        exec: bool,
    ) -> io::Result<()> {
        let Some((last, folders)) = names.split_last() else {
            return Err(io::Error::other("the file has no name"));
        };
        let mut dir = open_root(&root.dir, true)?;
        for name in folders {
            dir = if held_exactly(&dir, name)? {
                rustix::fs::openat(dir.as_fd(), *name, DIR_FLAGS, Mode::empty())
                    .map_err(|_| io::Error::other("a link or a file is where a folder belongs"))?
            } else {
                rustix::fs::mkdirat(dir.as_fd(), *name, Mode::RWXU).map_err(rx)?;
                rustix::fs::openat(dir.as_fd(), *name, DIR_FLAGS, Mode::empty()).map_err(rx)?
            };
        }
        let replaces = held_exactly(&dir, last)?;
        if replaces {
            let stat =
                rustix::fs::statat(dir.as_fd(), *last, AtFlags::SYMLINK_NOFOLLOW).map_err(rx)?;
            if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
                return Err(io::Error::other("a link or a folder is in its place"));
            }
            // Another name for a file elsewhere: replacing it here would
            // leave that one behind, and it is not ours to judge.
            if stat.st_nlink > 1 {
                return Err(io::Error::other("a hard link is in its place"));
            }
        }
        let temporary = format!(".zlo-sync-{}.tmp", Uuid::new_v4());
        let mode = if exec {
            Mode::RWXU
        } else {
            Mode::RUSR | Mode::WUSR
        };
        let fd = rustix::fs::openat(
            dir.as_fd(),
            temporary.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            mode,
        )
        .map_err(rx)?;
        // From here the temporary file is this write's own to remove.
        let written = (|| {
            // The mode asked for, whatever the process's mask took away.
            rustix::fs::fchmod(&fd, mode).map_err(rx)?;
            let mut file = std::fs::File::from(fd);
            file.write_all(bytes)?;
            file.sync_all()?;
            if replaces {
                rustix::fs::renameat(dir.as_fd(), temporary.as_str(), dir.as_fd(), *last)
                    .map_err(rx)
            } else {
                // Nothing was there: published only if nothing still is.
                rustix::fs::renameat_with(
                    dir.as_fd(),
                    temporary.as_str(),
                    dir.as_fd(),
                    *last,
                    rustix::fs::RenameFlags::NOREPLACE,
                )
                .map_err(rx)
            }
        })();
        if written.is_err()
            && let Err(error) =
                rustix::fs::unlinkat(dir.as_fd(), temporary.as_str(), AtFlags::empty())
        {
            tracing::debug!(%error, "config sync: temporary file not removed");
        }
        written
    }
}

/// List `name` inside `dir`. `depth` is `None` for a file entry (that
/// file and nothing else), or how far below a folder entry `name` is (the
/// entry itself is 0 and must be a folder).
fn walk(
    dir: &OwnedFd,
    name: &str,
    key: String,
    depth: Option<usize>,
    known: &BTreeMap<String, (Stamp, String)>,
    out: &mut Listing,
) {
    out.visited += 1;
    if out.visited > MAX_VISITED {
        if out.visited == MAX_VISITED + 1 {
            out.note(format!(
                "everything past the first {MAX_VISITED} names (too many to look at)"
            ));
        }
        return;
    }
    // A link fails to open; so does anything that went away.
    let Ok(fd) = rustix::fs::openat(dir.as_fd(), name, ANY_FLAGS, Mode::empty()) else {
        return;
    };
    let Ok(stat) = rustix::fs::fstat(&fd) else {
        return;
    };
    match (FileType::from_raw_mode(stat.st_mode), depth) {
        // A file: as a file entry, or below a folder entry.
        (FileType::RegularFile, None | Some(1..)) => {
            if stat.st_nlink > 1 {
                out.note(format!("{key} (a hard link)"));
                return;
            }
            if out.files.len() >= MAX_FILES {
                out.note(format!("{key} (more than {MAX_FILES} files)"));
                return;
            }
            let size = usize::try_from(stat.st_size).unwrap_or(usize::MAX);
            if size > items::ITEM_CONTENT_BYTES {
                out.note(format!("{key} (too large, {} KiB)", size.div_ceil(1024)));
                return;
            }
            let stamp = Stamp::of(&stat);
            let hash = match known.get(&key) {
                Some((held, hash)) if *held == stamp => hash.clone(),
                _ => match read_all(fd, items::ITEM_CONTENT_BYTES) {
                    Ok(bytes) if is_private_key(&bytes) => {
                        out.note(format!("{key} (holds a private key)"));
                        return;
                    }
                    Ok(bytes) => file_hash(&bytes, is_executable(&stat)),
                    Err(error) => {
                        tracing::debug!(%error, %key, "config sync: file not read");
                        return;
                    }
                },
            };
            out.seen.insert(key.clone(), (stamp, hash.clone()));
            out.files.insert(key, hash);
        }
        (FileType::Directory, Some(depth)) if depth < MAX_DEPTH => {
            let Ok(entries) = rustix::fs::Dir::read_from(&fd) else {
                return;
            };
            let mut names: Vec<String> = Vec::new();
            let mut read = 0usize;
            for entry in entries.flatten() {
                let child = entry.file_name().to_string_lossy();
                if child == "." || child == ".." {
                    continue;
                }
                // Every name read counts, carried or not, here and overall.
                read += 1;
                out.visited += 1;
                if out.visited > MAX_VISITED {
                    break;
                }
                if read > MAX_FOLDER_NAMES {
                    out.note(format!(
                        "{key} (more than {MAX_FOLDER_NAMES} entries; the rest were left)"
                    ));
                    break;
                }
                // Hidden names are everywhere and say nothing; any other
                // name held back is worth a line.
                if child.starts_with('.') {
                    continue;
                }
                if !plain(&child) {
                    out.note(format!("{key}/{child} (its name is not plain ASCII)"));
                } else if withheld(&child) {
                    out.note(format!(
                        "{key}/{child} (looks like a sign-in or settings file)"
                    ));
                } else {
                    names.push(child.into_owned());
                }
            }
            names.sort();
            for child in names {
                let key = format!("{key}/{child}");
                walk(&fd, &child, key, Some(depth + 1), known, out);
            }
        }
        _ => {}
    }
}

fn page_of(payload: &ConfigSyncDomainPayload) -> Result<&ItemizedDomainPayload, ConfigSyncError> {
    match payload {
        ConfigSyncDomainPayload::CliHome(page) => Ok(page),
        _ => Err(ConfigSyncError::validation(
            "cli_home",
            "expected cli_home payload",
        )),
    }
}

#[async_trait]
impl ConfigSyncDomainHandler for CliHomeDomainHandler {
    fn domain(&self) -> ConfigSyncDomain {
        DOMAIN
    }

    async fn manifest(
        &self,
        _ctx: &ConfigSyncContext,
    ) -> Result<ConfigSyncDomainDigest, ConfigSyncError> {
        let listing = self.list();
        Ok(ConfigSyncDomainDigest {
            hash: items::domain_hash(&listing.files),
            updated_at: None,
            item_count: listing.files.len() as u64,
        })
    }

    async fn export(
        &self,
        ctx: &ConfigSyncContext,
    ) -> Result<ConfigSyncDomainPayload, ConfigSyncError> {
        let listing = self.list();
        let mut page = items::page(&listing.files, &ctx.page, |key| {
            let listed = listing.files.get(&key).cloned();
            let read = self.read(&key);
            async move {
                Ok(match read {
                    // The bytes carried are the bytes that were hashed.
                    Ok((bytes, exec)) if listed.as_deref() == Some(&file_hash(&bytes, exec)) => {
                        let mode = if exec { EXECUTABLE } else { "" };
                        Some(vec![(mode.to_string(), bytes)])
                    }
                    _ => None,
                })
            }
        })
        .await?;
        // Said once, with the first page.
        if ctx.page.after.is_none() {
            let room = items::PAGE_NOTES.saturating_sub(page.not_carried.len());
            let left = listing.not_carried.len().saturating_sub(room);
            page.not_carried
                .extend(listing.not_carried.into_iter().take(room));
            if left > 0 {
                page.not_carried.push(format!("{left} more files"));
            }
        }
        Ok(ConfigSyncDomainPayload::CliHome(page))
    }

    fn validate(&self, payload: &ConfigSyncDomainPayload) -> Result<(), ConfigSyncError> {
        let page = page_of(payload)?;
        items::validate(DOMAIN, page)?;
        for item in &page.items {
            let one_file = item.files.as_ref().is_none_or(|files| {
                files.len() == 1 && matches!(files[0].path.as_str(), "" | EXECUTABLE)
            });
            if !one_file {
                return Err(ConfigSyncError::validation(
                    "cli_home.items",
                    format!("item `{}` is not one file", item.key),
                ));
            }
        }
        Ok(())
    }

    async fn apply(
        &self,
        ctx: &ConfigSyncContext,
        payload: &ConfigSyncDomainPayload,
    ) -> Result<DomainApplyResult, ConfigSyncError> {
        let page = page_of(payload)?;
        // Files go one way; a return leg has nothing to take.
        if ctx.merge_back {
            return Ok(DomainApplyResult::unchanged(page.items.len() as u64));
        }
        let held = self.list().files;
        let (mut applied, mut skipped) = (0u64, 0u64);
        let mut warnings: Vec<String> = page
            .not_carried
            .iter()
            .map(|item| format!("{item} was not carried"))
            .collect();
        for item in &page.items {
            let Some((mode, bytes)) = items::decode(DOMAIN, item)?.and_then(|mut f| f.pop()) else {
                skipped += 1;
                continue;
            };
            let exec = mode == EXECUTABLE;
            // A key this host has no place for (a CLI it does not list) is
            // not an error: the other computer simply has more.
            let Some((root, names)) = self.target(&item.key) else {
                skipped += 1;
                continue;
            };
            if held.get(&item.key).map(String::as_str) == Some(&file_hash(&bytes, exec)) {
                skipped += 1;
                continue;
            }
            if is_private_key(&bytes) {
                warnings.push(format!(
                    "{} holds a private key and was not written",
                    item.key
                ));
                skipped += 1;
                continue;
            }
            match off_the_reactor(|| self.write(root, &names, &bytes, exec)) {
                Ok(()) => applied += 1,
                Err(error) => {
                    tracing::debug!(key = %item.key, %error, "config sync: file not written");
                    warnings.push(format!("{} was not synced: {error}", item.key));
                    skipped += 1;
                }
            }
        }
        let mut result = if applied == 0 {
            DomainApplyResult::unchanged(skipped)
        } else {
            DomainApplyResult::applied(applied, skipped)
        };
        result.outcome.warnings = warnings;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use ironclaw_host_api::ids::{TenantId, UserId};
    use ironclaw_product_contracts::config_sync::{
        ConfigSyncItem, ConfigSyncItemFile, ItemizedDomainPayload,
    };
    use ironclaw_product_contracts::surface::ProductSurfaceCaller;

    use std::path::Path;

    use super::*;

    struct Host {
        dir: tempfile::TempDir,
        handler: CliHomeDomainHandler,
    }

    impl Host {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let home = dir.path().join("home");
            std::fs::create_dir_all(&home).expect("home");
            let roots = vec![
                CliHomeRoot {
                    id: "alpha".into(),
                    dir: home.join(".alpha"),
                    entries: vec![entry("RULES.md", false), entry("skills", true)],
                },
                CliHomeRoot {
                    id: "beta".into(),
                    // Spelled as an environment variable might: a trailing
                    // `/.` must not make the open follow a link.
                    dir: home.join(".beta").join("."),
                    entries: vec![entry("config/skills", true)],
                },
            ];
            let handler = CliHomeDomainHandler::new(roots);
            Self { dir, handler }
        }
        fn path(&self, rel: &str) -> PathBuf {
            self.dir.path().join("home").join(rel)
        }
        fn put(&self, rel: &str, text: &str) {
            let path = self.path(rel);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
            std::fs::write(path, text).expect("write");
        }
        fn get(&self, rel: &str) -> Option<String> {
            std::fs::read_to_string(self.path(rel)).ok()
        }
        async fn hash(&self) -> String {
            self.handler.manifest(&ctx()).await.expect("manifest").hash
        }
    }

    fn entry(path: &str, folder: bool) -> CliHomeEntry {
        CliHomeEntry {
            path: path.into(),
            folder,
        }
    }

    fn ctx() -> ConfigSyncContext {
        ConfigSyncContext {
            caller: ProductSurfaceCaller::new(
                TenantId::new("tenant-a").expect("tenant"),
                UserId::new("alice").expect("user"),
                None,
                None,
            ),
            data_folder: "/data".into(),
            home_dir: "/home/alice".into(),
            page: items::PageRequest::default(),
            merge_back: false,
            peer_environment_id: Some("peer".into()),
        }
    }

    async fn carry(from: &Host, to: &Host) -> DomainApplyResult {
        let payload = from.handler.export(&ctx()).await.expect("export");
        to.handler.validate(&payload).expect("valid");
        to.handler.apply(&ctx(), &payload).await.expect("apply")
    }

    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path).expect("meta").permissions().mode() & 0o777
    }

    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    #[tokio::test]
    async fn rules_and_skills_arrive_and_nothing_else_does() {
        let (mac, boxed) = (Host::new(), Host::new());
        mac.put(".alpha/RULES.md", "Be brief.\n");
        mac.put(".alpha/skills/deploy/SKILL.md", "# Deploy\n");
        mac.put(".alpha/skills/deploy/run.sh", "#!/bin/sh\necho hi\n");
        mac.put(".beta/config/skills/review/SKILL.md", "# Review\n");
        make_executable(&mac.path(".alpha/skills/deploy/run.sh"));
        // Never carried either: a settings or sign-in file inside a carried
        // folder, by its name.
        mac.put(".alpha/skills/deploy/settings.json", "{}");
        mac.put(".alpha/skills/deploy/api_token.txt", "secret");
        mac.put(".alpha/skills/deploy/id_ed25519", "key");
        mac.put(".alpha/skills/deploy/cert.pfx", "bundle");
        mac.put(
            ".alpha/skills/deploy/notes.txt",
            "-----BEGIN OPENSSH PRIVATE KEY-----\nabc\n-----END OPENSSH PRIVATE KEY-----\n",
        );
        // Ordinary files whose names merely contain such a word travel.
        mac.put(".alpha/skills/deploy/tokenizer.md", "# Tokenizer\n");
        mac.put(".alpha/skills/deploy/oauth.md", "# OAuth notes\n");
        // Never carried: a settings file beside the rules, a login, a
        // hidden file inside a carried folder.
        mac.put(".alpha/settings.json", "{\"token\":\"secret\"}");
        mac.put(".beta/oauth_creds.json", "{}");
        mac.put(".alpha/skills/.credentials", "secret");
        // The box's own: kept.
        boxed.put(".alpha/RULES.md", "Old rules.\n");
        boxed.put(".alpha/skills/box-only/SKILL.md", "# Local\n");
        boxed.put(".alpha/settings.json", "{\"box\":true}");

        let result = carry(&mac, &boxed).await;
        assert_eq!(result.outcome.applied_count, 6, "{:?}", result.outcome);
        for kept_back in ["id_ed25519", "cert.pfx", "notes.txt"] {
            assert_eq!(
                boxed.get(&format!(".alpha/skills/deploy/{kept_back}")),
                None,
                "{kept_back}"
            );
        }
        assert!(boxed.get(".alpha/skills/deploy/tokenizer.md").is_some());
        assert!(boxed.get(".alpha/skills/deploy/oauth.md").is_some());
        assert_eq!(boxed.get(".alpha/RULES.md").as_deref(), Some("Be brief.\n"));
        assert_eq!(
            boxed.get(".alpha/skills/deploy/SKILL.md").as_deref(),
            Some("# Deploy\n")
        );
        assert_eq!(
            boxed.get(".beta/config/skills/review/SKILL.md").as_deref(),
            Some("# Review\n")
        );
        // The script stays runnable; everything is the person's alone.
        assert_eq!(mode(&boxed.path(".alpha/skills/deploy/run.sh")), 0o700);
        assert_eq!(mode(&boxed.path(".alpha/RULES.md")), 0o600);
        assert_eq!(boxed.get(".alpha/skills/deploy/settings.json"), None);
        assert_eq!(boxed.get(".alpha/skills/deploy/api_token.txt"), None);
        assert_eq!(
            boxed.get(".alpha/settings.json").as_deref(),
            Some("{\"box\":true}")
        );
        assert_eq!(boxed.get(".beta/oauth_creds.json"), None);
        assert_eq!(boxed.get(".alpha/skills/.credentials"), None);
        assert!(boxed.get(".alpha/skills/box-only/SKILL.md").is_some());
        // The Mac was only read, and a second pass writes nothing.
        assert_eq!(mac.get(".alpha/skills/box-only/SKILL.md"), None);
        let again = carry(&mac, &boxed).await;
        assert_eq!(again.outcome.applied_count, 0);

        // Without a file of its own the two sides hash the same.
        std::fs::remove_dir_all(boxed.path(".alpha/skills/box-only")).expect("remove");
        assert_eq!(mac.hash().await, boxed.hash().await);
    }

    fn item(key: &str, text: &str) -> ConfigSyncItem {
        let files = items::encode(vec![(String::new(), text.as_bytes().to_vec())]);
        ConfigSyncItem {
            key: key.into(),
            hash: file_hash(text.as_bytes(), false),
            files: Some(files),
            base: None,
        }
    }

    #[tokio::test]
    async fn a_key_outside_the_listed_entries_writes_nothing() {
        let boxed = Host::new();
        let page = ItemizedDomainPayload {
            hash: String::new(),
            items: vec![
                item("alpha/settings.json", "x"),
                item("alpha/skills/../settings.json", "x"),
                item("alpha/skills/.hidden", "x"),
                item("alpha/skillsX/a.md", "x"),
                item("alpha//etc/passwd", "x"),
                item("gamma/RULES.md", "x"),
                item("alpha", "x"),
                item("alpha/skills/auth.json", "x"),
                // A name that a case- or Unicode-folding volume takes for
                // the settings file, a backup of it, odd endings, and a
                // path below a file entry or at a folder entry itself.
                item("alpha/skills/\u{17f}ettings.json", "x"),
                item("alpha/skills/Settings.JSON.bak", "x"),
                item("alpha/skills/notes.md.", "x"),
                item("alpha/skills/caf\u{e9}.md", "x"),
                item("alpha/RULES.md/inner.md", "x"),
                item("alpha/skills", "x"),
                item("alpha/skills/a/b/c/d/e/f/g/h/i/j/k.md", "x"),
            ],
            next: None,
            not_carried: Vec::new(),
        };
        let result = boxed
            .handler
            .apply(&ctx(), &ConfigSyncDomainPayload::CliHome(page))
            .await
            .expect("apply");
        assert_eq!(result.outcome.applied_count, 0);
        assert!(!boxed.path(".alpha").exists(), "nothing was created");
    }

    #[tokio::test]
    async fn nothing_is_read_or_written_through_a_link() {
        let (mac, boxed) = (Host::new(), Host::new());
        // On the receiver: a carried folder that is a link, even to a
        // folder inside the home.
        let elsewhere = boxed.path("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("elsewhere");
        std::fs::create_dir_all(boxed.path(".alpha")).expect("root");
        std::os::unix::fs::symlink(&elsewhere, boxed.path(".alpha/skills")).expect("link");
        mac.put(".alpha/skills/deploy/SKILL.md", "# Deploy\n");
        let result = carry(&mac, &boxed).await;
        assert_eq!(result.outcome.applied_count, 0);
        assert_eq!(result.outcome.warnings.len(), 1, "{:?}", result.outcome);
        assert!(!elsewhere.join("deploy").exists());

        // On the sender: a listed file that is a link to a sign-in file,
        // a linked file inside a carried folder, and a hard link.
        mac.put(".alpha/auth.json", "{\"login\":true}");
        std::fs::remove_file(mac.path(".alpha/skills/deploy/SKILL.md")).expect("remove");
        std::os::unix::fs::symlink(mac.path(".alpha/auth.json"), mac.path(".alpha/RULES.md"))
            .expect("link");
        std::os::unix::fs::symlink(
            mac.path(".alpha/auth.json"),
            mac.path(".alpha/skills/deploy/notes.md"),
        )
        .expect("link");
        std::fs::hard_link(
            mac.path(".alpha/auth.json"),
            mac.path(".alpha/skills/deploy/copy.md"),
        )
        .expect("hard link");
        let ConfigSyncDomainPayload::CliHome(page) =
            mac.handler.export(&ctx()).await.expect("export")
        else {
            panic!("payload");
        };
        assert!(page.items.is_empty(), "{:?}", page.items);
        assert!(
            page.not_carried
                .iter()
                .any(|note| note.contains("hard link")),
            "{:?}",
            page.not_carried
        );

        // A whole root that is a link has nothing and takes nothing.
        let linked = Host::new();
        std::os::unix::fs::symlink(&elsewhere, linked.path(".alpha")).expect("link");
        let page = ItemizedDomainPayload {
            hash: String::new(),
            items: vec![item("alpha/RULES.md", "x")],
            next: None,
            not_carried: Vec::new(),
        };
        let result = linked
            .handler
            .apply(&ctx(), &ConfigSyncDomainPayload::CliHome(page))
            .await
            .expect("apply");
        assert_eq!(result.outcome.applied_count, 0);
        assert!(!elsewhere.join("RULES.md").exists());
    }

    #[tokio::test]
    async fn a_mode_change_shows_and_a_name_differing_only_in_case_is_not_replaced() {
        let (mac, boxed) = (Host::new(), Host::new());
        mac.put(".alpha/skills/run.sh", "echo hi\n");
        let before = mac.hash().await;
        // Same bytes, same size, same modification time: only the mode.
        make_executable(&mac.path(".alpha/skills/run.sh"));
        assert_ne!(mac.hash().await, before);

        mac.put(".alpha/skills/Review.md", "from the mac\n");
        boxed.put(".alpha/skills/review.md", "the box's own\n");
        let result = carry(&mac, &boxed).await;
        assert_eq!(
            boxed.get(".alpha/skills/review.md").as_deref(),
            Some("the box's own\n")
        );
        assert!(
            result
                .outcome
                .warnings
                .iter()
                .any(|w| w.contains("differs only in case")),
            "{:?}",
            result.outcome
        );
    }

    #[tokio::test]
    async fn a_linked_root_spelled_with_a_trailing_dot_is_still_refused() {
        let (mac, boxed) = (Host::new(), Host::new());
        let elsewhere = boxed.path("elsewhere");
        std::fs::create_dir_all(&elsewhere).expect("elsewhere");
        std::os::unix::fs::symlink(&elsewhere, boxed.path(".beta")).expect("link");
        mac.put(".beta/config/skills/review/SKILL.md", "# Review\n");
        let result = carry(&mac, &boxed).await;
        assert_eq!(result.outcome.applied_count, 0, "{:?}", result.outcome);
        assert!(!elsewhere.join("config").exists());
        // A missing root is made, but never its parents.
        let orphan = CliHomeDomainHandler::new(vec![CliHomeRoot {
            id: "beta".into(),
            dir: boxed.path("no/such/parent/.beta"),
            entries: vec![entry("config/skills", true)],
        }]);
        let payload = mac.handler.export(&ctx()).await.expect("export");
        let result = orphan.apply(&ctx(), &payload).await.expect("apply");
        assert_eq!(result.outcome.applied_count, 0);
        assert!(!boxed.path("no").exists());
    }

    #[tokio::test]
    async fn a_folder_whose_name_differs_only_in_case_is_not_written_into() {
        let (mac, boxed) = (Host::new(), Host::new());
        mac.put(".alpha/skills/Deploy/run.sh", "from the mac\n");
        boxed.put(".alpha/skills/deploy/run.sh", "the box's own\n");
        let result = carry(&mac, &boxed).await;
        assert_eq!(result.outcome.applied_count, 0, "{:?}", result.outcome);
        assert_eq!(
            boxed.get(".alpha/skills/deploy/run.sh").as_deref(),
            Some("the box's own\n")
        );
        // And a folder standing where a file entry belongs is not walked.
        mac.put(".alpha/RULES.md/secret-plans.md", "x");
        let ConfigSyncDomainPayload::CliHome(page) =
            mac.handler.export(&ctx()).await.expect("export")
        else {
            panic!("payload");
        };
        assert!(
            page.items.iter().all(|item| !item.key.contains("RULES.md")),
            "{:?}",
            page.items
        );
    }

    #[tokio::test]
    async fn a_receivers_file_under_another_spelling_is_not_replaced() {
        let (mac, boxed) = (Host::new(), Host::new());
        // A Kelvin sign for the K: the same entry as `Kitchen.md` where
        // the volume folds it, another file where it does not.
        boxed.put(".alpha/skills/\u{212a}itchen.md", "the box's own\n");
        mac.put(".alpha/skills/Kitchen.md", "from the mac\n");
        carry(&mac, &boxed).await;
        assert_eq!(
            boxed.get(".alpha/skills/\u{212a}itchen.md").as_deref(),
            Some("the box's own\n")
        );
        // A relative root is not looked for anywhere.
        let relative = CliHomeDomainHandler::new(vec![CliHomeRoot {
            id: "alpha".into(),
            dir: PathBuf::from("relative-root"),
            entries: vec![entry("skills", true)],
        }]);
        let payload = mac.handler.export(&ctx()).await.expect("export");
        let result = relative.apply(&ctx(), &payload).await.expect("apply");
        assert_eq!(result.outcome.applied_count, 0);
        assert!(!Path::new("relative-root").exists());
    }

    #[test]
    fn names_that_keep_a_sign_in_are_withheld_and_ordinary_ones_are_not() {
        for name in [
            "settings.json",
            "Settings.JSON.bak",
            "auth.json",
            "api_token.txt",
            "API-KEY.md",
            "id_rsa",
            "id_ed25519.pub",
            "cert.pfx",
            "server.key",
            "prod.env",
            "secrets.yaml",
            ".hidden",
            "config.toml",
            "db_password",
        ] {
            assert!(withheld(name), "{name}");
        }
        for name in [
            "tokenizer.md",
            "oauth.md",
            "SKILL.md",
            "authoring-guide.md",
            "keyboard.md",
            "settings-explained",
            "run.sh",
            "environment.md",
        ] {
            assert!(!withheld(name), "{name}");
        }
    }

    #[test]
    fn a_payload_item_must_be_one_file() {
        let handler = Host::new().handler;
        let mut bad = item("alpha/RULES.md", "x");
        bad.files = Some(vec![ConfigSyncItemFile {
            path: "a".into(),
            data_b64: String::new(),
        }]);
        let page = ItemizedDomainPayload {
            hash: String::new(),
            items: vec![bad],
            next: None,
            not_carried: Vec::new(),
        };
        assert!(
            handler
                .validate(&ConfigSyncDomainPayload::CliHome(page))
                .is_err()
        );
    }
}
