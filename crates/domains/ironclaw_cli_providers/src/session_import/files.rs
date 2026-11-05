//! Filesystem access for the import. Session folders are walked without
//! following symlinks, only regular files are read, and a file is opened
//! without following a link and non-blocking (a FIFO named `*.jsonl` is
//! rejected rather than waited on), then checked to be the regular file that
//! was listed. What the import writes (transcript copies, saved thread ids,
//! workspace records) is private: folders 0700, files 0600, since session
//! text may hold anything the person pasted.

use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use ironclaw_orchestrator::model::Millis;
use nix::fcntl::OFlag;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

/// Bytes before a checkpoint whose digest proves the file still holds what
/// was read up to it.
pub(crate) const FINGERPRINT_WINDOW: u64 = 4096;

/// A regular file as listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileInfo {
    pub(crate) size: u64,
    pub(crate) mtime_ms: Millis,
    /// `dev:ino`: the same name over a different file reads as another file.
    pub(crate) id: String,
}

fn info(meta: &std::fs::Metadata) -> FileInfo {
    let mtime_ms = meta
        .modified()
        .ok() // silent-ok: a filesystem without mtimes compares on identity and digest
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    FileInfo {
        size: meta.len(),
        mtime_ms,
        id: format!("{}:{}", meta.dev(), meta.ino()),
    }
}

/// Entries of `dir` that are real directories (symlinks are not followed).
pub(crate) async fn real_dirs(dir: &Path, cap: usize) -> Vec<PathBuf> {
    entries(dir, cap, |m| m.file_type().is_dir())
        .await
        .into_iter()
        .map(|(p, _)| p)
        .collect()
}

/// Regular files of `dir` (no symlinks, devices, FIFOs or sockets).
pub(crate) async fn regular_files(dir: &Path, cap: usize) -> Vec<(PathBuf, FileInfo)> {
    entries(dir, cap, |m| m.file_type().is_file())
        .await
        .into_iter()
        .map(|(p, m)| (p, info(&m)))
        .collect()
}

async fn entries(
    dir: &Path,
    cap: usize,
    keep: fn(&std::fs::Metadata) -> bool,
) -> Vec<(PathBuf, std::fs::Metadata)> {
    let mut out = Vec::new();
    let Ok(mut read) = tokio::fs::read_dir(dir).await else {
        return out; // silent-ok: a CLI that never ran has no sessions folder
    };
    while let Ok(Some(entry)) = read.next_entry().await {
        if out.len() >= cap {
            break;
        }
        let path = entry.path();
        // `symlink_metadata`: a link is reported as a link, never followed.
        if let Ok(meta) = tokio::fs::symlink_metadata(&path).await
            && keep(&meta)
        {
            out.push((path, meta));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Open `path` for reading only if it is still the regular file `expect`
/// names: not through a symlink, never blocking on a FIFO or device.
pub(crate) async fn open_regular(path: &Path, expect: &str) -> std::io::Result<tokio::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK).bits());
    let path = path.to_path_buf();
    let file = tokio::task::spawn_blocking(move || options.open(path))
        .await
        .map_err(std::io::Error::other)??;
    let meta = file.metadata()?;
    if !meta.file_type().is_file() || info(&meta).id != expect {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not the regular file that was listed",
        ));
    }
    Ok(tokio::fs::File::from_std(file))
}

/// Digest of the [`FINGERPRINT_WINDOW`] bytes ending at `offset`.
pub(crate) async fn fingerprint(file: &mut tokio::fs::File, offset: u64) -> std::io::Result<String> {
    let start = offset.saturating_sub(FINGERPRINT_WINDOW);
    let mut buf = vec![0u8; usize::try_from(offset - start).unwrap_or(0)];
    file.seek(std::io::SeekFrom::Start(start)).await?;
    file.read_exact(&mut buf).await?;
    Ok(hex(&Sha256::digest(&buf)[..16]))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Create `dir` (and parents) and make it private.
pub(crate) async fn private_dir(dir: &Path) -> std::io::Result<()> {
    tokio::fs::create_dir_all(dir).await?;
    tokio::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).await
}

/// Open `path` for writing, private (0600), its folder private (0700).
pub(crate) async fn private_writer(path: &Path, append: bool) -> std::io::Result<tokio::fs::File> {
    if let Some(dir) = path.parent() {
        private_dir(dir).await?;
    }
    let file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .mode(0o600)
        .open(path)
        .await?;
    // An existing file keeps its mode on open: make it private too.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .await?;
    Ok(file)
}

/// Replace `path` with `body`, privately.
pub(crate) async fn write_private(path: &Path, body: &[u8]) -> std::io::Result<()> {
    let mut file = private_writer(path, false).await?;
    file.write_all(body).await?;
    file.sync_all().await
}
