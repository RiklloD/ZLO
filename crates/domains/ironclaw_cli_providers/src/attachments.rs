//! Files a person attached to a CLI turn (same shape as T3 Code).
//!
//! The product layer lands each file under `~/.ironclaw/attachments/<thread>/`
//! and appends one line per file to the prompt:
//! `[Attached image "shot.jpg" is saved at: /Users/me/.ironclaw/attachments/…]`.
//! The text alone is enough for every engine (the agent reads the path with
//! its own tools), and it survives the queue and restarts. Engines that take
//! pictures natively (Claude, Codex) also get each image inline, found here
//! from those same lines.

use std::path::{Path, PathBuf};

use base64::Engine as _;

const MARKER_PREFIX: &str = "[Attached image \"";
const MARKER_PATH: &str = "\" is saved at: ";
/// Inline pictures above this size are left to the agent's own file reading.
const MAX_INLINE_BYTES: u64 = 5 * 1024 * 1024;
const MAX_INLINE_IMAGES: usize = 8;

/// One picture to send inline.
pub(crate) struct InlineImage {
    pub mime: &'static str,
    pub base64: String,
}

impl InlineImage {
    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.mime, self.base64)
    }
}

/// `~/.ironclaw/attachments`, the only folder inline pictures are read from.
pub(crate) fn root() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".ironclaw/attachments"))
}

/// The pictures the prompt's attachment lines point at, read and encoded.
/// Lines naming a path outside [`root`], an unknown image type, or a file
/// that is gone are skipped: the line itself still tells the agent the path.
pub(crate) async fn inline_images(text: &str) -> Vec<InlineImage> {
    let Some(root) = root() else {
        return Vec::new();
    };
    let Ok(root) = tokio::fs::canonicalize(&root).await else {
        return Vec::new(); // silent-ok: no attachments folder means nothing was attached
    };
    let mut out = Vec::new();
    for path in image_paths(text).into_iter().take(MAX_INLINE_IMAGES) {
        if let Some(image) = read_image(&root, &path).await {
            out.push(image);
        }
    }
    out
}

/// Every image line's path, wherever it sits: a line may stand alone or
/// sit inside a sentence, where the person placed the picture.
fn image_paths(text: &str) -> Vec<PathBuf> {
    text.match_indices(MARKER_PREFIX)
        .filter_map(|(at, _)| {
            let rest = &text[at + MARKER_PREFIX.len()..];
            let rest = rest.lines().next()?;
            let (_, path) = rest.split_once(MARKER_PATH)?;
            let (path, _) = path.split_once(']')?;
            Some(PathBuf::from(path))
        })
        .collect()
}

async fn read_image(root: &Path, path: &Path) -> Option<InlineImage> {
    let mime = image_mime(path)?;
    // Canonical paths: a `..` or a symlink cannot leave the attachments folder.
    let real = tokio::fs::canonicalize(path).await.ok()?; // silent-ok: a vanished file stays a path line
    if !real.starts_with(root) {
        tracing::debug!("cli attachments: image outside the attachments folder skipped");
        return None;
    }
    let meta = tokio::fs::metadata(&real).await.ok()?; // silent-ok: as above
    if !meta.is_file() || meta.len() > MAX_INLINE_BYTES {
        return None;
    }
    let bytes = match tokio::fs::read(&real).await {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::debug!(%error, "cli attachments: image not readable");
            return None;
        }
    };
    Some(InlineImage {
        mime,
        base64: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

fn image_mime(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn only_images_inside_the_root_are_inlined() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("attachments");
        std::fs::create_dir_all(root.join("t1")).unwrap();
        std::fs::write(root.join("t1/a.png"), b"png").unwrap();
        std::fs::write(dir.path().join("outside.png"), b"png").unwrap();
        std::fs::write(root.join("t1/notes.txt"), b"txt").unwrap();
        let text = format!(
            "look at [Attached image \"a.png\" is saved at: {a}] here\n\
             [Attached image \"x.png\" is saved at: {root}/../outside.png]\n\
             [Attached file \"notes.txt\" is saved at: {root}/t1/notes.txt]",
            a = root.join("t1/a.png").display(),
            root = root.display(),
        );
        let root = std::fs::canonicalize(&root).unwrap();
        let mut found = Vec::new();
        for path in image_paths(&text) {
            if let Some(image) = read_image(&root, &path).await {
                found.push(image);
            }
        }
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].mime, "image/png");
        assert_eq!(found[0].data_url(), "data:image/png;base64,cG5n");
    }
}
