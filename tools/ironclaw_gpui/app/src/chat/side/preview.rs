//! One file shown inside the pane: code in color (syntect through the
//! Markdown view's fenced blocks), Markdown rendered, images drawn, anything
//! else offered to its default app. Shared by Outputs and Files.

use std::path::{Path, PathBuf};

use crate::design::Palette;
use crate::ui::*;
use gpui_kit::StyledImage as _;

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
    Image,
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

/// Read `path` and decide how to show it. Blocking: run on the background.
pub(super) fn read(path: &Path) -> Preview {
    let ext = ext(path);
    if is_image(&ext) {
        return Preview::Image;
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

fn ext(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

pub(super) fn is_image(ext: &str) -> bool {
    matches!(
        ext,
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" | "ico" | "tiff"
    )
}

/// syntect's names for common extensions; anything else is plain text.
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

/// The file's body. `key` must be stable per file so the view keeps its parse.
pub(super) fn render(opened: &Opened, key: &str, cx: &App) -> AnyElement {
    let p = Palette::of(cx);
    let note = |text: String| {
        div()
            .text_sm()
            .text_color(p.text_muted)
            .whitespace_normal()
            .child(text)
            .into_any_element()
    };
    match &opened.preview {
        Preview::Loading => h_flex()
            .justify_center()
            .py_6()
            .child(Spinner::new())
            .into_any_element(),
        Preview::Failed(e) => div()
            .text_sm()
            .text_color(p.danger)
            .whitespace_normal()
            .child(e.clone())
            .into_any_element(),
        Preview::TooLarge(size) => note(format!(
            "{} is too large to show here. Open it in its app.",
            crate::jobs::size(*size)
        )),
        Preview::Binary(size) => note(format!(
            "Binary file, {}. Open it in its app.",
            crate::jobs::size(*size)
        )),
        Preview::Image => img(opened.path.clone())
            .w_full()
            .object_fit(gpui_kit::ObjectFit::Contain)
            .into_any_element(),
        Preview::Text { text, lang, cut } => {
            let source = match lang {
                None => text.clone(),
                Some(lang) => fenced(lang, text),
            };
            v_flex()
                .gap_2()
                .text_sm()
                .child(markdown(
                    SharedString::from(format!("side-preview:{key}")),
                    source,
                    false,
                ))
                .when(*cut, |d| {
                    d.child(
                        div()
                            .text_xs()
                            .text_color(p.text_faint)
                            .child("Showing the first 256 KB."),
                    )
                })
                .into_any_element()
        }
    }
}

/// Back, the file name, then Open and Reveal for the local copy.
pub(super) fn toolbar(
    opened: &Opened,
    id: &'static str,
    on_back: impl Fn(&mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let p = Palette::of(cx);
    let open = opened.path.clone();
    let reveal = opened.path.clone();
    h_flex()
        .px_2()
        .py_1()
        .gap_1()
        .items_center()
        .border_b_1()
        .border_color(p.border)
        .child(
            Button::new(SharedString::from(format!("{id}-back")))
                .icon(IconName::ChevronLeft)
                .ghost()
                .xsmall()
                .tooltip("Back")
                .accessibility_label("Back")
                .on_click(move |_, window, cx| on_back(window, cx)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_sm()
                .font_weight(gpui_kit::FontWeight::MEDIUM)
                .child(opened.title.clone()),
        )
        .child(
            Button::new(SharedString::from(format!("{id}-open")))
                .icon(IconName::ExternalLink)
                .ghost()
                .xsmall()
                .tooltip("Open in its app")
                .accessibility_label("Open in its app")
                .on_click(move |_, _, cx| cx.open_with_system(&open)),
        )
        .child(
            Button::new(SharedString::from(format!("{id}-reveal")))
                .icon(IconName::Folder)
                .ghost()
                .xsmall()
                .tooltip("Show in Finder")
                .accessibility_label("Show in Finder")
                .on_click(move |_, _, cx| cx.reveal_path(&reveal)),
        )
        .into_any_element()
}
