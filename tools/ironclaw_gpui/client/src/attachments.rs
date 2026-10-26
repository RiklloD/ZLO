//! Staging files for a message: type lookup, size limits, base64 encoding.
//! The gateway decodes and re-validates every upload; these checks only stop
//! a doomed send early with a clear message.

use std::path::Path;

use base64::Engine;
use ironclaw_product_contracts::inbound_requests::ProductInboundAttachment;

use crate::wire::AttachmentLimits;

/// A file read from disk and ready to send.
#[derive(Clone)]
pub struct StagedFile {
    pub filename: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

impl StagedFile {
    pub fn to_wire(&self) -> ProductInboundAttachment {
        ProductInboundAttachment {
            mime_type: self.mime_type.clone(),
            filename: Some(self.filename.clone()),
            data_base64: base64::engine::general_purpose::STANDARD.encode(&self.bytes),
        }
    }
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
        Ok(StagedFile {
            filename,
            mime_type,
            bytes: crate::transcript::thread_markdown(title, &messages).into_bytes(),
        })
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
pub fn check(
    file: &StagedFile,
    staged: &[StagedFile],
    limits: &AttachmentLimits,
) -> Result<(), String> {
    let size = file.bytes.len() as u64;
    if staged.len() >= limits.max_count {
        return Err(format!("At most {} files per message.", limits.max_count));
    }
    if size > limits.max_file_bytes {
        return Err(format!(
            "{} is {}; the limit is {} per file.",
            file.filename,
            format_bytes(size),
            format_bytes(limits.max_file_bytes)
        ));
    }
    let total: u64 = staged.iter().map(|f| f.bytes.len() as u64).sum::<u64>() + size;
    if total > limits.max_total_bytes {
        return Err(format!(
            "Attachments would total {}; the limit is {} per message.",
            format_bytes(total),
            format_bytes(limits.max_total_bytes)
        ));
    }
    Ok(())
}

/// Read a file from disk and resolve its type, or say why it can't be sent.
pub async fn stage(path: &Path, limits: &AttachmentLimits) -> Result<StagedFile, String> {
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("attachment")
        .to_string();
    let mime_type = mime_for(&filename, limits)
        .ok_or_else(|| format!("{filename}: this file type can't be attached."))?;
    // Stat first so a huge file is refused without reading it into memory.
    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|e| format!("{filename}: {e}"))?;
    if meta.len() > limits.max_file_bytes {
        return Err(format!(
            "{filename} is {}; the limit is {} per file.",
            format_bytes(meta.len()),
            format_bytes(limits.max_file_bytes)
        ));
    }
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| format!("{filename}: {e}"))?;
    Ok(StagedFile {
        filename,
        mime_type,
        bytes,
    })
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

        let file = |n: usize| StagedFile {
            filename: "f.png".into(),
            mime_type: "image/png".into(),
            bytes: vec![0; n],
        };
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
        assert_eq!(format_bytes(1536), "1.5 KB");
    }
}
