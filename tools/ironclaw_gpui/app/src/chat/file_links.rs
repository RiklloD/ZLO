//! File paths in a finished reply become links. Agents often write the
//! document they made as `` `/workspace/report.pdf` `` or a bare path; here
//! such a path (inline code or plain text, outside fenced blocks and
//! existing links) turns into `[report.pdf](/workspace/report.pdf)`, which
//! `ChatView::open_link` previews in the pane. Only known document and media
//! extensions count, and a path on this Mac must exist.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;

use crate::ui::SharedString;

const EXTENSIONS: &[&str] = &[
    "pdf", "doc", "docx", "rtf", "odt", "pages", "xls", "xlsx", "csv", "tsv", "ods", "numbers",
    "ppt", "pptx", "odp", "key", "md", "markdown", "txt", "html", "htm", "json", "png", "jpg",
    "jpeg", "gif", "webp", "svg", "heic", "mp3", "wav", "m4a", "mp4", "mov",
];

/// Rewritten texts by source, so a frame doesn't redo the scan and the
/// disk checks. Cleared when full.
const CACHE_LIMIT: usize = 512;

thread_local! {
    static CACHE: RefCell<HashMap<SharedString, SharedString>> = RefCell::default();
}

pub(super) fn link_paths(text: &SharedString) -> SharedString {
    if !text.contains('/') {
        return text.clone();
    }
    if let Some(hit) = CACHE.with(|c| c.borrow().get(text).cloned()) {
        return hit;
    }
    let out: SharedString = match rewrite(text) {
        Some(s) => s.into(),
        None => text.clone(),
    };
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= CACHE_LIMIT {
            c.clear();
        }
        c.insert(text.clone(), out.clone());
    });
    out
}

/// `None` when nothing changed.
fn rewrite(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len() + 64);
    let mut changed = false;
    let mut fence: Option<&str> = None;
    for line in text.split_inclusive('\n') {
        let head = line.trim_start();
        let marker = ["```", "~~~"].into_iter().find(|m| head.starts_with(m));
        match (fence, marker) {
            (None, Some(m)) => fence = Some(m),
            (Some(f), Some(m)) if f == m => fence = None,
            _ if fence.is_none() => {
                changed |= link_line(line, &mut out);
                continue;
            }
            _ => {}
        }
        out.push_str(line);
    }
    changed.then_some(out)
}

fn link_line(line: &str, out: &mut String) -> bool {
    let mut changed = false;
    let mut rest = line;
    while !rest.is_empty() {
        let c = rest.chars().next().unwrap_or(' ');
        // Inline code: link the whole span when it is a path.
        if c == '`' {
            let ticks = rest.len() - rest.trim_start_matches('`').len();
            let body = &rest[ticks..];
            if let Some(end) = body.find(&rest[..ticks]) {
                let inner = &body[..end];
                match href_for(inner.trim()) {
                    Some(href) => {
                        push_link(out, inner.trim(), &href);
                        changed = true;
                    }
                    None => out.push_str(&rest[..ticks * 2 + end]),
                }
                rest = &body[end + ticks..];
                continue;
            }
            out.push_str(&rest[..ticks]);
            rest = body;
            continue;
        }
        // An existing link `[..](..)` or autolink `<..>` stays as written.
        if c == '[' || c == '<' {
            let close = if c == '[' { "](" } else { ">" };
            if let Some(i) = rest.find(close) {
                let end = if c == '[' {
                    rest[i..].find(')').map(|j| i + j + 1)
                } else {
                    Some(i + 1)
                };
                if let Some(end) = end {
                    out.push_str(&rest[..end]);
                    rest = &rest[end..];
                    continue;
                }
            }
        }
        if c.is_whitespace() || c == '[' || c == '<' {
            out.push(c);
            rest = &rest[c.len_utf8()..];
            continue;
        }
        // A plain word: strip the punctuation around it, link the path.
        let end = rest
            .find(|ch: char| ch.is_whitespace() || ch == '`' || ch == '<')
            .unwrap_or(rest.len());
        let word = &rest[..end];
        let lead = word.len() - word.trim_start_matches(['(', '"', '\'', '*', '_']).len();
        let core =
            word[lead..].trim_end_matches(['.', ',', ';', ':', '!', '?', ')', '"', '\'', '*', '_']);
        match href_for(core) {
            Some(href) => {
                out.push_str(&word[..lead]);
                push_link(out, core, &href);
                out.push_str(&word[lead + core.len()..]);
                changed = true;
            }
            None => out.push_str(word),
        }
        rest = &rest[end..];
    }
    changed
}

/// The link target for a path an agent wrote, if it names a document.
fn href_for(path: &str) -> Option<String> {
    if path.is_empty() || path.contains("://") && !path.starts_with("file://") {
        return None;
    }
    let path = path.strip_prefix("file://").unwrap_or(path);
    if !path.contains('/') || path.contains(['\n', '[', ']', '*']) {
        return None;
    }
    let ext = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    if !EXTENSIONS.contains(&ext.as_str()) {
        return None;
    }
    let full = match path.strip_prefix("~/") {
        Some(rest) => dirs::home_dir()?.join(rest).to_string_lossy().into_owned(),
        None => path.to_string(),
    };
    if full.starts_with('/') && !full.starts_with("/workspace/") && !Path::new(&full).is_file() {
        return None;
    }
    Some(
        full.replace('%', "%25")
            .replace(' ', "%20")
            .replace('(', "%28")
            .replace(')', "%29")
            .replace('<', "%3C")
            .replace('>', "%3E"),
    )
}

fn push_link(out: &mut String, path: &str, href: &str) {
    let name = path.rsplit('/').next().unwrap_or(path);
    out.push('[');
    for ch in name.chars() {
        if matches!(ch, '[' | ']' | '*' | '_' | '\\' | '`') {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push_str("](");
    out.push_str(href);
    out.push(')');
}

#[cfg(test)]
mod tests {
    use super::rewrite;

    #[test]
    fn paths_become_links() {
        assert_eq!(
            rewrite("Saved to `/workspace/out/My deck.pptx`.").as_deref(),
            Some("Saved to [My deck.pptx](/workspace/out/My%20deck.pptx).")
        );
        assert_eq!(
            rewrite("See (/workspace/ten_words.pdf), done").as_deref(),
            Some("See ([ten\\_words.pdf](/workspace/ten_words.pdf)), done")
        );
        // Existing links, code fences, missing local files and code stay.
        assert_eq!(rewrite("[a](/workspace/a.pdf)"), None);
        assert_eq!(rewrite("```\n/workspace/a.pdf\n```\n"), None);
        assert_eq!(rewrite("`/nope/missing.pdf`"), None);
        assert_eq!(rewrite("edit `src/main.rs` and https://x.io/a.pdf"), None);
    }
}
