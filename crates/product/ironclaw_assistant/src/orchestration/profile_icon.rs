//! A profile folder's own logo, found the way t3code finds a project's
//! favicon (`ProjectFaviconResolver`): well-known favicon and app-icon
//! paths first, then an icon `<link>` (or a JSX `{ rel: "icon", href }`)
//! declared in the folder's entry HTML or root route, then `web/favicon.png`.
//! Every candidate must resolve, symlinks followed, to a file inside the
//! folder, and the file must be an image t3code would show.
//!
//! The inbox list asks for [`revision`] on every read, so answers are
//! cached per folder like t3code's (10 minutes found, 1 minute missing); a
//! cached file that is gone is looked up again.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, UNIX_EPOCH};

use base64::Engine as _;
use ironclaw_product_contracts::profiles::ProfileIcon;

/// t3code's `FAVICON_CANDIDATES`, in its order.
const CANDIDATES: &[&str] = &[
    "favicon.svg",
    "favicon.ico",
    "favicon.png",
    "public/favicon.svg",
    "public/favicon.ico",
    "public/favicon.png",
    "app/favicon.ico",
    "app/favicon.png",
    "app/icon.svg",
    "app/icon.png",
    "app/icon.ico",
    "src/favicon.ico",
    "src/favicon.svg",
    "src/app/favicon.ico",
    "src/app/icon.svg",
    "src/app/icon.png",
    "assets/icon.svg",
    "assets/icon.png",
    "assets/logo.svg",
    "assets/logo.png",
    ".idea/icon.svg",
    "Resources/AppIcon.icns",
    "AppIcon.icns",
];

/// t3code's `FALLBACK_FAVICON_CANDIDATES`.
const FALLBACK_CANDIDATES: &[&str] = &["web/favicon.png"];

/// t3code's `ICON_SOURCE_FILES`: where an icon `<link>` is declared.
const SOURCE_FILES: &[&str] = &[
    "index.html",
    "public/index.html",
    "app/routes/__root.tsx",
    "src/routes/__root.tsx",
    "app/root.tsx",
    "src/root.tsx",
    "src/index.html",
];

/// How much of a source file is scanned (t3code's `ICON_SOURCE_SCAN_BYTES`).
const SOURCE_SCAN_BYTES: u64 = 128 * 1024;

/// The largest logo served; a bigger file is drawn as letters.
const MAX_ICON_BYTES: u64 = 4 * 1024 * 1024;

const FOUND_TTL: Duration = Duration::from_secs(10 * 60);
const MISSING_TTL: Duration = Duration::from_secs(60);
const CACHE_CAPACITY: usize = 512;

/// Extensions t3code shows as a project icon, with their MIME types.
const IMAGE_TYPES: &[(&str, &str)] = &[
    ("avif", "image/avif"),
    ("bmp", "image/bmp"),
    ("gif", "image/gif"),
    ("ico", "image/x-icon"),
    ("heic", "image/heic"),
    ("heif", "image/heif"),
    ("jpeg", "image/jpeg"),
    ("jpg", "image/jpeg"),
    ("png", "image/png"),
    ("svg", "image/svg+xml"),
    ("tif", "image/tiff"),
    ("tiff", "image/tiff"),
    ("webp", "image/webp"),
    ("icns", "image/icns"),
];

type Cache = HashMap<PathBuf, (Option<PathBuf>, Instant)>;

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The folder's logo file, if it has one.
pub(crate) fn find(folder: &Path) -> Option<PathBuf> {
    let now = Instant::now();
    let cached = cache()
        .lock()
        .ok()
        .and_then(|map| map.get(folder).cloned())
        .filter(|(found, at)| {
            let ttl = if found.is_some() {
                FOUND_TTL
            } else {
                MISSING_TTL
            };
            now.duration_since(*at) < ttl
        });
    if let Some((found, _)) = cached
        && found.as_deref().is_none_or(Path::is_file)
    {
        return found;
    }
    let found = resolve(folder);
    if let Ok(mut map) = cache().lock() {
        if map.len() >= CACHE_CAPACITY {
            map.retain(|_, (_, at)| now.duration_since(*at) < FOUND_TTL);
            if map.len() >= CACHE_CAPACITY {
                map.clear();
            }
        }
        map.insert(folder.to_path_buf(), (found.clone(), now));
    }
    found
}

/// A token for the folder's logo that changes when the file does (its
/// path, size and modification time), or `None` without a logo.
pub(crate) fn revision(folder: &Path) -> Option<String> {
    let path = find(folder)?;
    // silent-ok: a logo that vanished since `find` is no logo; letters are drawn.
    let meta = std::fs::metadata(&path).ok()?;
    if meta.len() > MAX_ICON_BYTES {
        return None;
    }
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in path.as_os_str().as_encoded_bytes() {
        hash = (hash ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3);
    }
    Some(format!("{hash:016x}-{:x}-{modified:x}", meta.len()))
}

/// The folder's logo with its bytes, for `profiles.icon`.
pub(crate) fn read(folder: &Path) -> Option<ProfileIcon> {
    let revision = revision(folder)?;
    let path = find(folder)?;
    let mime = mime_for(&path)?;
    // silent-ok: an unreadable logo is no logo; the app draws letters.
    let bytes = std::fs::read(&path).ok()?;
    if bytes.is_empty() || bytes.len() as u64 > MAX_ICON_BYTES {
        return None;
    }
    Some(ProfileIcon {
        mime: mime.to_string(),
        data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        revision,
    })
}

fn mime_for(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    IMAGE_TYPES
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, mime)| *mime)
}

/// t3code's `resolvePathUncached`, without its per-project override (ZLO
/// profiles carry none): the first hit wins and an unshowable type is no
/// logo, as t3code then falls back to the monogram.
fn resolve(folder: &Path) -> Option<PathBuf> {
    // silent-ok: a missing or unreadable folder has no logo.
    let root = std::fs::canonicalize(folder).ok()?;
    if !root.is_dir() {
        return None;
    }
    let found = first_file(&root, CANDIDATES.iter().map(PathBuf::from))
        .or_else(|| {
            SOURCE_FILES
                .iter()
                .find_map(|source| icon_from_source(&root, source))
        })
        .or_else(|| first_file(&root, FALLBACK_CANDIDATES.iter().map(PathBuf::from)))?;
    mime_for(&found).map(|_| found)
}

/// The first candidate (relative to `root`) that is a file inside it.
fn first_file(root: &Path, candidates: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    candidates
        .into_iter()
        .find_map(|relative| inside(root, &relative))
}

/// `root/relative` when it exists as a file whose real path stays inside
/// `root` (a `..` or a symlink out of the folder is refused).
fn inside(root: &Path, relative: &Path) -> Option<PathBuf> {
    if relative.is_absolute() {
        return None;
    }
    // silent-ok: a candidate that does not exist is the usual miss.
    let real = std::fs::canonicalize(root.join(relative)).ok()?;
    (real.starts_with(root) && real.is_file()).then_some(real)
}

/// t3code's `findIconFromSource`: icon hrefs declared in `source`, each
/// tried under `public/` and then as written.
fn icon_from_source(root: &Path, source: &str) -> Option<PathBuf> {
    let path = inside(root, Path::new(source))?;
    let text = read_head(&path)?;
    let candidates = icon_hrefs(&text)
        .into_iter()
        .flat_map(|href| resolve_href(&href));
    first_file(root, candidates)
}

fn read_head(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    // silent-ok: an unreadable source file declares no icon; the next is tried.
    std::fs::File::open(path)
        .ok()?
        .take(SOURCE_SCAN_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// t3code's `resolveIconHref`: a site-relative href, as `public/<href>`
/// then `<href>`; URLs (`scheme:` or `//host`) are not files.
fn resolve_href(href: &str) -> Vec<PathBuf> {
    let href = href.trim();
    if has_scheme(href) || href.starts_with("//") {
        return Vec::new();
    }
    let clean = percent_decode(href.strip_prefix('/').unwrap_or(href));
    if clean.contains('\0') || clean.is_empty() {
        return Vec::new();
    }
    vec![Path::new("public").join(&clean), PathBuf::from(clean)]
}

/// `^[a-z][a-z\d+.-]*:`, case-insensitively.
fn has_scheme(href: &str) -> bool {
    let mut chars = href.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_alphabetic()) {
        return false;
    }
    for c in chars {
        if c == ':' {
            return true;
        }
        if !(c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-')) {
            return false;
        }
    }
    false
}

/// `decodeURIComponent`, or the text unchanged when it is not valid.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            match hex {
                Some(b) => {
                    out.push(b);
                    i += 3;
                }
                None => return text.to_string(),
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| text.to_string())
}

const fn is_html_space(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\u{c}' | '\r' | ' ')
}

const fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// t3code's `extractIconHrefs`: every `<link>` whose `rel` holds the
/// `icon` token (its `href`, before any `?` or `#`), then every JSX-style
/// `rel: "… icon …"` with an `href: "…"` in the same `}`-delimited run.
fn icon_hrefs(source: &str) -> Vec<String> {
    let mut out = Vec::new();
    for tag in link_tags(source) {
        let attributes = attributes(&tag[5..]);
        let rel_has_icon = attributes
            .iter()
            .find(|(name, _, _)| name == "rel")
            .is_some_and(|(_, value, _)| {
                value
                    .to_lowercase()
                    .split(is_html_space)
                    .any(|token| token == "icon")
            });
        if !rel_has_icon {
            continue;
        }
        let Some((_, href, quoted)) = attributes.iter().find(|(name, _, _)| name == "href") else {
            continue;
        };
        if !quoted && href.starts_with('{') {
            continue;
        }
        let path = href.split(['?', '#']).next().unwrap_or_default();
        if !path.is_empty() {
            out.push(path.to_string());
        }
    }
    for run in source.split('}') {
        if jsx_rel_is_icon(run)
            && let Some(href) = jsx_href(run)
        {
            out.push(href);
        }
    }
    out
}

/// `<link\b[^<>]*>`, case-insensitively.
fn link_tags(source: &str) -> Vec<&str> {
    let lower = source.to_ascii_lowercase();
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = lower[from..].find("<link").map(|i| from + i) {
        from = at + 5;
        if lower[from..].chars().next().is_some_and(is_word) {
            continue;
        }
        let rest = &source[from..];
        match rest.find(['<', '>']) {
            Some(end) if rest.as_bytes()[end] == b'>' => {
                out.push(&source[at..from + end + 1]);
                from += end + 1;
            }
            _ => {}
        }
    }
    out
}

/// t3code's `HTML_ATTRIBUTE_RE` over a tag's attributes: `(name, value,
/// quoted)`, names lowercased, the first of a repeated name kept.
fn attributes(tag: &str) -> Vec<(String, String, bool)> {
    let excluded = |c: char| is_html_space(c) || matches!(c, '"' | '\'' | '<' | '>' | '/' | '=');
    let chars: Vec<char> = tag.chars().collect();
    let mut out: Vec<(String, String, bool)> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if excluded(chars[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len() && !excluded(chars[i]) {
            i += 1;
        }
        let name: String = chars[start..i].iter().collect::<String>().to_lowercase();
        let mut value = (String::new(), false);
        let mut j = i;
        while j < chars.len() && is_html_space(chars[j]) {
            j += 1;
        }
        if chars.get(j) == Some(&'=') {
            j += 1;
            while j < chars.len() && is_html_space(chars[j]) {
                j += 1;
            }
            let quoted = chars.get(j).copied().filter(|c| matches!(c, '"' | '\''));
            let closing = quoted.and_then(|q| {
                chars[j + 1..]
                    .iter()
                    .position(|c| *c == q)
                    .map(|p| j + 1 + p)
            });
            if let Some(end) = closing {
                value = (chars[j + 1..end].iter().collect(), true);
                i = end + 1;
            } else {
                let start = j;
                while j < chars.len() && !is_html_space(chars[j]) && chars[j] != '>' {
                    j += 1;
                }
                if j > start {
                    value = (chars[start..j].iter().collect(), false);
                    i = j;
                }
            }
        }
        if !out.iter().any(|(n, _, _)| *n == name) {
            out.push((name, value.0, value.1));
        }
    }
    out
}

/// `\brel\s*:\s*("…icon…"|'…icon…')`, with `icon` a whole whitespace-
/// separated token (t3code's `ICON_REL_RE`), case-insensitively.
fn jsx_rel_is_icon(run: &str) -> bool {
    let lower = run.to_ascii_lowercase();
    let chars: Vec<char> = lower.chars().collect();
    let mut i = 0;
    while i + 3 <= chars.len() {
        let boundary = i == 0 || !is_word(chars[i - 1]);
        if boundary && chars[i..i + 3] == ['r', 'e', 'l'] {
            let mut j = i + 3;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if chars.get(j) == Some(&':') {
                j += 1;
                while j < chars.len() && chars[j].is_whitespace() {
                    j += 1;
                }
                if let Some(q) = chars.get(j).copied().filter(|c| matches!(c, '"' | '\''))
                    && quoted_has_icon(&chars[j + 1..], q)
                {
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

/// Whether a quoted run (after its opening `q`) holds the token `icon`
/// before its closing quote, ending at whitespace or that quote.
fn quoted_has_icon(rest: &[char], q: char) -> bool {
    let body_end = rest.iter().position(|c| *c == q).unwrap_or(rest.len());
    (0..body_end).any(|k| {
        (k == 0 || is_html_space(rest[k - 1]))
            && rest[k..].starts_with(&['i', 'c', 'o', 'n'])
            && rest
                .get(k + 4)
                .is_some_and(|c| is_html_space(*c) || *c == q)
    })
}

/// `\bhref\s*:\s*["']([^"'?#]+)`, case-insensitively (t3code's
/// `ICON_HREF_RE`): the first one in the run.
fn jsx_href(run: &str) -> Option<String> {
    let lower = run.to_ascii_lowercase();
    let chars: Vec<char> = run.chars().collect();
    let lower: Vec<char> = lower.chars().collect();
    let mut i = 0;
    while i + 4 <= lower.len() {
        let boundary = i == 0 || !is_word(lower[i - 1]);
        if boundary && lower[i..i + 4] == ['h', 'r', 'e', 'f'] {
            let mut j = i + 4;
            while j < lower.len() && lower[j].is_whitespace() {
                j += 1;
            }
            if lower.get(j) == Some(&':') {
                j += 1;
                while j < lower.len() && lower[j].is_whitespace() {
                    j += 1;
                }
                if lower.get(j).is_some_and(|c| matches!(c, '"' | '\'')) {
                    let start = j + 1;
                    let mut end = start;
                    while end < chars.len() && !matches!(chars[end], '"' | '\'' | '?' | '#') {
                        end += 1;
                    }
                    if end > start {
                        return Some(chars[start..end].iter().collect());
                    }
                }
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(files: &[(&str, &[u8])]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (path, bytes) in files {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        dir
    }

    fn found(dir: &tempfile::TempDir) -> Option<String> {
        let root = std::fs::canonicalize(dir.path()).unwrap();
        resolve(dir.path()).map(|p| {
            p.strip_prefix(&root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/")
        })
    }

    #[test]
    fn well_known_paths_win_in_t3codes_order() {
        let dir = folder(&[
            ("public/favicon.png", b"p"),
            ("favicon.ico", b"i"),
            ("index.html", br#"<link rel="icon" href="/logo.svg">"#),
            ("logo.svg", b"<svg/>"),
        ]);
        assert_eq!(found(&dir).as_deref(), Some("favicon.ico"));
    }

    #[test]
    fn an_icon_link_in_the_entry_html_is_followed() {
        let dir = folder(&[
            (
                "index.html",
                br#"<head><link rel="stylesheet" href="/a.css"><LINK REL='shortcut icon' href="/brand/mark.svg?v=2"></head>"#,
            ),
            ("public/brand/mark.svg", b"<svg/>"),
        ]);
        assert_eq!(found(&dir).as_deref(), Some("public/brand/mark.svg"));
    }

    #[test]
    fn a_jsx_root_route_declares_its_icon() {
        let dir = folder(&[
            (
                "src/routes/__root.tsx",
                br#"links: () => [{ rel: "apple-touch-icon", href: "/x.png" }, { rel: "icon", href: "/fav.png" }]"#,
            ),
            ("fav.png", b"p"),
        ]);
        assert_eq!(found(&dir).as_deref(), Some("fav.png"));
    }

    #[test]
    fn urls_escapes_and_unshowable_files_are_no_logo() {
        let outside = folder(&[("secret.png", b"s")]);
        let escape = format!(
            r#"<link rel="icon" href="https://cdn/x.png"><link rel="icon" href="../{}/secret.png">"#,
            outside.path().file_name().unwrap().to_string_lossy()
        );
        let dir = folder(&[("index.html", escape.as_bytes())]);
        assert_eq!(found(&dir), None);

        let text = folder(&[
            ("index.html", br#"<link rel="icon" href="/README.md">"#),
            ("README.md", b"x"),
        ]);
        assert_eq!(found(&text), None);
    }

    #[test]
    fn the_fallback_comes_last() {
        let dir = folder(&[("web/favicon.png", b"p")]);
        assert_eq!(found(&dir).as_deref(), Some("web/favicon.png"));
        assert_eq!(found(&folder(&[])), None);
    }

    #[test]
    fn read_serves_bytes_with_a_revision_that_follows_the_file() {
        let dir = folder(&[("favicon.svg", b"<svg/>")]);
        let icon = read(dir.path()).expect("an icon");
        assert_eq!(icon.mime, "image/svg+xml");
        assert_eq!(icon.data_base64, "PHN2Zy8+");
        assert_eq!(Some(icon.revision.clone()), revision(dir.path()));

        std::fs::write(dir.path().join("favicon.svg"), b"<svg></svg>").unwrap();
        assert_ne!(revision(dir.path()), Some(icon.revision));
    }

    #[test]
    fn link_attributes_parse_like_t3codes_regex() {
        let hrefs = icon_hrefs(
            r#"<link rel=icon href=/a.png><link href="/b.png" rel="ICON"><link rel="icon" href={logo}><linkx rel="icon" href="/c.png">"#,
        );
        assert_eq!(hrefs, vec!["/a.png".to_string(), "/b.png".to_string()]);
        assert_eq!(percent_decode("a%20b.png"), "a b.png");
        assert_eq!(percent_decode("bad%zz"), "bad%zz");
        assert!(resolve_href("data:image/png;base64,x").is_empty());
    }
}
