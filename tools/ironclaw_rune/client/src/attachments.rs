//! Staging files for a message. A file picked from disk goes by its path
//! when the backend runs on this Mac and copies it itself, so there is no
//! upload and no size limit; for a backend on another computer it is read
//! here and uploads like a generated file (`Backend`). Generated files
//! (SnapShots, dragged-in threads) have no path and upload as base64 within
//! the gateway's limits; these checks only stop a doomed send early with a
//! clear message.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine;
use ironclaw_product_contracts::inbound_requests::ProductInboundAttachment;

use crate::wire::AttachmentLimits;

/// What a staged file sends: its bytes, or its path on this machine.
#[derive(Clone)]
pub enum Body {
    Bytes(Vec<u8>),
    Path(PathBuf),
}

/// A file ready to send.
#[derive(Clone)]
pub struct StagedFile {
    /// Unique per staged file; the composer's inline card carries it.
    pub key: u64,
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
    pub body: Body,
}

/// A fresh [`StagedFile::key`].
pub fn next_key() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

impl StagedFile {
    pub fn from_bytes(filename: String, mime_type: String, bytes: Vec<u8>) -> Self {
        Self {
            key: next_key(),
            filename,
            mime_type,
            size: bytes.len() as u64,
            body: Body::Bytes(bytes),
        }
    }

    pub fn bytes(&self) -> Option<&[u8]> {
        match &self.body {
            Body::Bytes(bytes) => Some(bytes),
            Body::Path(_) => None,
        }
    }

    /// The card's second line: the folder it came from, else its size.
    pub fn subtitle(&self) -> String {
        match &self.body {
            Body::Path(path) => location(path),
            Body::Bytes(_) => format_bytes(self.size),
        }
    }

    pub fn to_wire(&self) -> ProductInboundAttachment {
        let (data_base64, path) = match &self.body {
            Body::Bytes(bytes) => (
                base64::engine::general_purpose::STANDARD.encode(bytes),
                None,
            ),
            Body::Path(path) => (String::new(), Some(path.display().to_string())),
        };
        ProductInboundAttachment {
            mime_type: self.mime_type.clone(),
            filename: Some(self.filename.clone()),
            data_base64,
            path,
        }
    }
}

/// `Downloads/report.pdf` for a file under the home folder, else the full
/// path.
pub fn location(path: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    home.as_deref()
        .and_then(|home| path.strip_prefix(home).ok())
        .unwrap_or(path)
        .display()
        .to_string()
}

impl crate::Client {
    /// Export a dropped thread as a text attachment for either composer.
    pub async fn thread_attachment(
        &self,
        id: &str,
        title: &str,
        limits: &AttachmentLimits,
    ) -> Result<StagedFile, crate::ClientError> {
        let mut messages = self.fork_prefix(id).await?;
        let mut page = self.timeline(id, 500).await?.messages;
        page.sort_by_key(|m| m.sequence);
        messages.append(&mut page);
        let slug: String = title
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '-' })
            .take(48)
            .collect();
        let filename = format!("thread-{}.md", slug.trim_matches('-'));
        let mime_type = mime_for(&filename, limits)
            .or_else(|| mime_for("a.txt", limits))
            .ok_or_else(|| {
                crate::ClientError::Decode("This gateway accepts no text attachments.".into())
            })?;
        Ok(StagedFile::from_bytes(
            filename,
            mime_type,
            crate::transcript::thread_markdown(title, &messages).into_bytes(),
        ))
    }
}

/// The MIME type the accept list pairs with this file's extension.
pub fn mime_for(filename: &str, limits: &AttachmentLimits) -> Option<String> {
    let ext = Path::new(filename).extension()?.to_str()?.to_lowercase();
    let dotted = format!(".{ext}");
    let mut current: Option<&str> = None;
    for entry in &limits.accept {
        if entry.starts_with('.') {
            if *entry == dotted {
                return current.map(str::to_string);
            }
        } else {
            current = Some(entry);
        }
    }
    None
}

/// Check one more file against the limits, given what is already staged.
/// Only uploads count; a file sent by path has no limit.
pub fn check(
    file: &StagedFile,
    staged: &[StagedFile],
    limits: &AttachmentLimits,
) -> Result<(), String> {
    if file.bytes().is_none() {
        return Ok(());
    }
    let size = file.size;
    let uploads = staged.iter().filter(|f| f.bytes().is_some());
    if uploads.clone().count() >= limits.max_count {
        return Err(format!(
            "At most {} uploaded files per message.",
            limits.max_count
        ));
    }
    if size > limits.max_file_bytes {
        return Err(format!(
            "{} is {}; the limit is {} per file.",
            file.filename,
            format_bytes(size),
            format_bytes(limits.max_file_bytes)
        ));
    }
    let total: u64 = uploads.map(|f| f.size).sum::<u64>() + size;
    if total > limits.max_total_bytes {
        return Err(format!(
            "Attachments would total {}; the limit is {} per message.",
            format_bytes(total),
            format_bytes(limits.max_total_bytes)
        ));
    }
    Ok(())
}

/// Where a file picked from disk goes: this Mac's backend copies it by
/// path; a backend on another computer can't see this Mac's disk, so the
/// file is read here and uploaded as bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Local,
    Remote,
}

/// Stage a file from disk for `backend`. Local: nothing is read, the
/// backend copies the file when the message is sent. Remote: the file is
/// read now, bounded by the gateway's per-file limit (`check` applies the
/// count and total limits), and never sends this Mac's path.
pub async fn stage(
    path: &Path,
    limits: &AttachmentLimits,
    backend: Backend,
) -> Result<StagedFile, String> {
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("attachment")
        .to_string();
    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("{filename}: {e}"))?;
    if !meta.is_file() {
        return Err(format!(
            "{filename} is a folder; attach the files inside it."
        ));
    }
    // Any type goes: the agent reads the file with its own tools.
    let mime_type =
        mime_for(&filename, limits).unwrap_or_else(|| "application/octet-stream".into());
    if backend == Backend::Remote {
        let bytes = read_bounded(path, &filename, meta.len(), limits.max_file_bytes).await?;
        return Ok(StagedFile::from_bytes(filename, mime_type, bytes));
    }
    Ok(StagedFile {
        key: next_key(),
        filename,
        mime_type,
        size: meta.len(),
        body: Body::Path(path.to_path_buf()),
    })
}

/// Read a file for upload, refusing one over `max` before reading it (and
/// again if it grew since).
async fn read_bounded(path: &Path, filename: &str, len: u64, max: u64) -> Result<Vec<u8>, String> {
    let too_big = |size: u64| {
        format!(
            "{filename} is {}; a file sent to another computer can be at most {}.",
            format_bytes(size),
            format_bytes(max)
        )
    };
    if len > max {
        return Err(too_big(len));
    }
    let owned = path.to_path_buf();
    let bytes = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
        use std::io::Read;
        let mut bytes = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
        std::fs::File::open(owned)?
            .take(max.saturating_add(1))
            .read_to_end(&mut bytes)?;
        Ok(bytes)
    })
    .await
    .map_err(|e| format!("{filename}: {e}"))?
    .map_err(|e| format!("{filename}: {e}"))?;
    if bytes.len() as u64 > max {
        return Err(too_big(bytes.len() as u64));
    }
    Ok(bytes)
}

/// Files staged by path, which only this Mac's backend can open: a remote
/// send refuses them (a draft staged before switching computers) rather
/// than hand another computer this Mac's paths.
pub fn local_paths_for_remote(files: &[StagedFile]) -> Result<(), String> {
    let names: Vec<&str> = files
        .iter()
        .filter(|f| matches!(f.body, Body::Path(_)))
        .map(|f| f.filename.as_str())
        .collect();
    if names.is_empty() {
        return Ok(());
    }
    Err(format!(
        "Attach {} again: it was added for this Mac's backend.",
        names.join(", ")
    ))
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 3] = ["KB", "MB", "GB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.;
    let mut unit = 0;
    while value >= 1024. && unit < UNITS.len() - 1 {
        value /= 1024.;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_lookup_and_limits() {
        let limits = AttachmentLimits {
            accept: ["image/png", ".png", "image/jpeg", ".jpg", ".jpeg"]
                .map(String::from)
                .to_vec(),
            max_count: 2,
            max_file_bytes: 10,
            max_total_bytes: 15,
        };
        assert_eq!(mime_for("a.JPEG", &limits).as_deref(), Some("image/jpeg"));
        assert_eq!(mime_for("a.png", &limits).as_deref(), Some("image/png"));
        assert!(mime_for("a.exe", &limits).is_none() && mime_for("noext", &limits).is_none());

        let file =
            |n: usize| StagedFile::from_bytes("f.png".into(), "image/png".into(), vec![0; n]);
        assert!(check(&file(8), &[], &limits).is_ok());
        assert!(check(&file(11), &[], &limits).is_err());
        assert!(
            check(&file(8), &[file(8)], &limits).is_err(),
            "total over 15"
        );
        assert!(
            check(&file(1), &[file(1), file(1)], &limits).is_err(),
            "count"
        );
        let by_path = StagedFile {
            size: u64::MAX,
            body: Body::Path("/a/b.mov".into()),
            ..file(0)
        };
        assert!(
            check(&by_path, &[file(1), file(1)], &limits).is_ok(),
            "no limit by path"
        );
        assert_eq!(format_bytes(1536), "1.5 KB");
    }

    fn limits(max_file_bytes: u64) -> AttachmentLimits {
        AttachmentLimits {
            accept: ["text/plain", ".txt"].map(String::from).to_vec(),
            max_count: 4,
            max_file_bytes,
            max_total_bytes: 100,
        }
    }

    fn temp_file(name: &str, bytes: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zlo-attach-{}", next_key()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[tokio::test]
    async fn local_backend_stages_by_path() {
        let path = temp_file("notes.txt", b"hello");
        let file = stage(&path, &limits(2), Backend::Local).await.unwrap();
        assert!(matches!(&file.body, Body::Path(p) if *p == path));
        assert_eq!(file.to_wire().path.as_deref(), Some(path.to_str().unwrap()));
        assert!(local_paths_for_remote(std::slice::from_ref(&file)).is_err());
    }

    #[tokio::test]
    async fn remote_backend_uploads_bytes_never_the_path() {
        let path = temp_file("notes.txt", b"hello");
        let file = stage(&path, &limits(10), Backend::Remote).await.unwrap();
        assert_eq!(file.bytes(), Some(&b"hello"[..]));
        assert_eq!(file.mime_type, "text/plain");
        let wire = file.to_wire();
        assert_eq!(wire.path, None);
        assert_eq!(wire.data_base64, "aGVsbG8=");
        assert!(local_paths_for_remote(&[file]).is_ok());
    }

    #[tokio::test]
    async fn remote_backend_refuses_a_file_over_the_limit() {
        let path = temp_file("big.txt", &[b'x'; 2048]);
        let problem = stage(&path, &limits(1024), Backend::Remote)
            .await
            .err()
            .unwrap();
        assert_eq!(
            problem,
            "big.txt is 2.0 KB; a file sent to another computer can be at most 1.0 KB."
        );
    }
}
