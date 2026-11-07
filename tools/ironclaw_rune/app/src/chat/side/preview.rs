//! One file shown inside the pane (GPUI `chat/side/preview.rs`): code in
//! colour (rune-kit Markdown's fenced blocks), Markdown rendered, images
//! drawn, PDF, Office, slides, HTML and media in a native view
//! (`native_view`), anything else offered to its default app. Shared by
//! Outputs and Files.
//!
//! Images: PNG, JPEG and WebP decode here (the `image` crate, off the UI
//! thread) and draw as Rune pixels; the other image types the GPUI app drew
//! (GIF, BMP, ICO, TIFF, SVG) go to QuickLook, which draws them natively.

use std::path::{Path, PathBuf};

use rune::{El, Pixels, SurfaceFit, col, el, row};
use rune_kit::{Kit, Markdown};

use super::ui::{self, sm, sm_med, xs};
use crate::icons;
use crate::state::State;

/// Text past this is cut; the full file opens in its app.
const TEXT_LIMIT: usize = 256 * 1024;
/// Files past this are not read at all.
const READ_LIMIT: u64 = 8 * 1024 * 1024;

pub(super) enum Preview {
    Loading,
    Text {
        text: String,
        /// The fence language, or `None` for Markdown rendered as such.
        lang: Option<&'static str>,
        cut: bool,
    },
    /// Decoded pixels and their aspect (width / height).
    Image(Pixels, f32),
    /// Drawn by QuickLook or WebKit.
    Native,
    Binary(u64),
    TooLarge(u64),
    Failed(String),
}

/// A file opened in a tab, with the local copy that Open and Reveal use.
pub(super) struct Opened {
    pub title: String,
    pub path: PathBuf,
    pub preview: Preview,
}

impl Opened {
    pub fn loading(title: String, path: PathBuf) -> Self {
        Self {
            title,
            path,
            preview: Preview::Loading,
        }
    }
}

/// Read `path` and decide how to show it. Blocking: run with `jobs::blocking`.
pub(super) fn read(path: &Path) -> Preview {
    let ext = ext(path);
    if decodes(&ext) {
        return decode(path);
    }
    if is_image(&ext) || crate::native_view::shows_natively(path) {
        return Preview::Native;
    }
    let size = match std::fs::metadata(path) {
        Ok(m) => m.len(),
        Err(e) => return Preview::Failed(e.to_string()),
    };
    if size > READ_LIMIT {
        return Preview::TooLarge(size);
    }
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => return Preview::Failed(e.to_string()),
    };
    // NUL bytes in the first 8 KB: treat as binary, as git does.
    if bytes.iter().take(8192).any(|b| *b == 0) {
        return Preview::Binary(size);
    }
    let Ok(mut text) = String::from_utf8(bytes) else {
        return Preview::Binary(size);
    };
    let cut = text.len() > TEXT_LIMIT;
    if cut {
        let mut end = TEXT_LIMIT;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    let lang = if matches!(ext.as_str(), "md" | "markdown" | "mdx") {
        None
    } else {
        Some(lang_for(&ext, path))
    };
    Preview::Text { text, lang, cut }
}

fn decode(path: &Path) -> Preview {
    match std::fs::metadata(path) {
        Ok(m) if m.len() > READ_LIMIT * 4 => return Preview::TooLarge(m.len()),
        Ok(_) => {}
        Err(e) => return Preview::Failed(e.to_string()),
    }
    match image::open(path) {
        Ok(img) => {
            let rgba = img.into_rgba8();
            let (w, h) = rgba.dimensions();
            let pixels = Pixels::new();
            pixels.set(w, h, rgba.into_raw());
            Preview::Image(pixels, w as f32 / (h.max(1)) as f32)
        }
        // Formats the decoder doesn't know still show through QuickLook.
        Err(e) => {
            tracing::debug!(error = %e, "image decode failed, previewing natively");
            Preview::Native
        }
    }
}

fn ext(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// Image types decoded here.
fn decodes(ext: &str) -> bool {
    matches!(ext, "png" | "jpg" | "jpeg" | "webp")
}

pub(super) fn is_image(ext: &str) -> bool {
    matches!(
        ext,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "ico" | "tiff"
    )
}

/// Fence names for common extensions; anything else is plain text.
fn lang_for(ext: &str, path: &Path) -> &'static str {
    match ext {
        "rs" => "rust",
        "ts" | "mts" | "cts" => "typescript",
        "tsx" => "tsx",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "jsx",
        "py" => "python",
        "go" => "go",
        "rb" => "ruby",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
        "cs" => "csharp",
        "php" => "php",
        "sh" | "bash" | "zsh" => "bash",
        "json" | "jsonc" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "html" | "htm" => "html",
        "css" | "scss" => "css",
        "sql" => "sql",
        "lua" => "lua",
        "xml" | "plist" => "xml",
        "tex" => "latex",
        _ => match path.file_name().and_then(|n| n.to_str()) {
            Some("Dockerfile") => "dockerfile",
            Some("Makefile") => "makefile",
            _ => "text",
        },
    }
}

/// A fence longer than any backtick run inside, so code can't close it.
fn fenced(lang: &str, text: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    let fence = "`".repeat((longest + 1).max(3));
    format!("{fence}{lang}\n{text}\n{fence}")
}

/// The file's body. `key` must be stable per file so Markdown keeps its blocks.
fn render(s: &State, k: &Kit, opened: &Opened, key: &str) -> El {
    let p = &s.palette;
    let note = |content: String| ui::para(content, sm(), p.text_muted);
    match &opened.preview {
        Preview::Loading => ui::loading(k),
        Preview::Failed(e) => ui::para(e.clone(), sm(), p.danger),
        Preview::TooLarge(size) => note(format!(
            "{} is too large to show here. Open it in its app.",
            crate::jobs::size(*size)
        )),
        Preview::Binary(size) => note(format!(
            "Binary file, {}. Open it in its app.",
            crate::jobs::size(*size)
        )),
        Preview::Native => crate::native_view::surface(crate::native_view::Slot::Preview),
        Preview::Image(pixels, aspect) => el()
            .key(format!("{key}-image"))
            .w_full()
            .aspect_ratio(aspect.max(0.05))
            .pixels_fit(pixels, SurfaceFit::Fit),
        Preview::Text { text, lang, cut } => {
            let source = match lang {
                None => text.clone(),
                Some(lang) => fenced(lang, text),
            };
            let mut col = col().w_full().gap(8.).child(
                Markdown::new(&source)
                    .selectable(true)
                    .build(k)
                    .key(format!("side-preview:{key}")),
            );
            if *cut {
                col = col.child(ui::line("Showing the first 256 KB.", xs(), p.text_faint));
            }
            col
        }
    }
}

/// The body below the toolbar: a native view fills it edge to edge,
/// everything else scrolls with padding.
pub(super) fn body(s: &State, k: &Kit, opened: &Opened, key: &str) -> El {
    let file = opened.path.display().to_string();
    if matches!(opened.preview, Preview::Native) {
        return ui::fill_col().child(render(s, k, opened, &file));
    }
    ui::scroll(
        key.to_string(),
        el().w_full()
            .padding(12.)
            .child(render(s, k, opened, &file)),
    )
}

/// Back, the file name, then Open and Reveal for the local copy.
pub(super) fn toolbar(
    s: &State,
    k: &Kit,
    opened: &Opened,
    key: &str,
    on_back: fn(&mut State),
) -> El {
    let p = &s.palette;
    let open = opened.path.clone();
    let reveal = opened.path.clone();
    col()
        .w_full()
        .child(
            row()
                .w_full()
                .px(8.)
                .py(4.)
                .gap(4.)
                .items_center()
                .child(
                    ui::ibtn(&icons::CHEVRON_LEFT, "Back")
                        .on_press(on_back)
                        .build(k)
                        .key(format!("{key}-back")),
                )
                .child(
                    ui::line(opened.title.clone(), sm_med(), p.text)
                        .grow(1.)
                        .min_w(0.),
                )
                .child(
                    ui::ibtn(&icons::EXTERNAL_LINK, "Open in its app")
                        .on_press(move |_: &mut State| ui::open_path(&open, false))
                        .build(k)
                        .key(format!("{key}-open")),
                )
                .child(
                    ui::ibtn(&icons::FOLDER, "Show in Finder")
                        .on_press(move |_: &mut State| ui::open_path(&reveal, true))
                        .build(k)
                        .key(format!("{key}-reveal")),
                ),
        )
        .child(el().w_full().h(1.).fill(p.border))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fences_outgrow_backtick_runs() {
        assert_eq!(fenced("rust", "a"), "```rust\na\n```");
        assert!(fenced("md", "x ```` y").starts_with("`````md"));
    }

    #[test]
    fn text_files_pick_a_fence_language() {
        assert_eq!(lang_for("rs", Path::new("a.rs")), "rust");
        assert_eq!(lang_for("", Path::new("Dockerfile")), "dockerfile");
    }
}
