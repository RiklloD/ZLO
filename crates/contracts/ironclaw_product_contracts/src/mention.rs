//! Rich `@`-mentions in a user message.
//!
//! A mention is a Markdown link whose label starts with `@` and whose target
//! is a [`MentionUri`]: `[@main.rs](file:///repo/src/main.rs#L10-20)`,
//! `[@Fix login](zlo://thread/<id>)`. Being plain text, it survives storage,
//! edit and resend unchanged, and any client re-parses it into a chip. The
//! backend expands each mention at send time (file contents, folder tree,
//! thread summary, git diff) and appends the result after
//! [`MENTION_CONTEXT_OPEN`], so the stored message keeps the typed text first
//! and a client shows only that part ([`strip_mention_context`]).
//!
//! Shape follows Zed's `acp_thread::mention::MentionUri` (`to_uri`/`parse`),
//! with ZLO's own `zlo://` scheme for the non-file kinds.

use std::fmt;
use std::ops::Range;

use serde::{Deserialize, Serialize};

/// Inclusive, 1-based line range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LineRange {
    pub start: u32,
    pub end: u32,
}

impl LineRange {
    /// `None` unless `1 <= start <= end`.
    pub fn new(start: u32, end: u32) -> Option<Self> {
        (start >= 1 && start <= end).then_some(Self { start, end })
    }

    fn fragment(&self) -> String {
        if self.start == self.end {
            format!("L{}", self.start)
        } else {
            format!("L{}-{}", self.start, self.end)
        }
    }

    fn parse_fragment(fragment: &str) -> Result<Self, MentionUriError> {
        let body = fragment
            .strip_prefix('L')
            .ok_or(MentionUriError::BadLineRange)?;
        let (start, end) = match body.split_once('-') {
            Some((s, e)) => (s, e.strip_prefix('L').unwrap_or(e)),
            None => (body, body),
        };
        let start = start.parse().map_err(|_| MentionUriError::BadLineRange)?;
        let end = end.parse().map_err(|_| MentionUriError::BadLineRange)?;
        Self::new(start, end).ok_or(MentionUriError::BadLineRange)
    }
}

/// What a mention points at. Paths are absolute host paths on the machine
/// running the gateway; the backend only expands those inside the roots the
/// sender grants for the message (`ProductSubmitTurnRequest::mention_roots`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MentionUri {
    File {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        line_range: Option<LineRange>,
    },
    Directory {
        path: String,
    },
    Thread {
        id: String,
    },
    /// The uncommitted changes of the repository at `cwd`.
    GitDiff {
        cwd: String,
    },
    /// Lines picked in the Changes tab.
    Selection {
        path: String,
        range: LineRange,
    },
    Skill {
        name: String,
    },
    Extension {
        name: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MentionUriError {
    #[error("unknown mention scheme")]
    UnknownScheme,
    #[error("mention path must be absolute with no `.` or `..` segments")]
    BadPath,
    #[error("mention line range is malformed")]
    BadLineRange,
    #[error("mention target is empty or malformed")]
    BadTarget,
    #[error("mention percent-encoding is malformed")]
    BadEncoding,
}

const FILE_SCHEME: &str = "file://";
const ZLO_SCHEME: &str = "zlo://";

impl MentionUri {
    /// The link target, percent-encoded so it never holds `)` or spaces.
    pub fn to_uri(&self) -> String {
        match self {
            Self::File { path, line_range } => {
                let mut uri = format!("{FILE_SCHEME}{}", encode(path));
                if let Some(range) = line_range {
                    uri.push('#');
                    uri.push_str(&range.fragment());
                }
                uri
            }
            Self::Directory { path } => {
                format!("{FILE_SCHEME}{}/", encode(path.trim_end_matches('/')))
            }
            Self::Thread { id } => format!("{ZLO_SCHEME}thread/{}", encode_segment(id)),
            Self::GitDiff { cwd } => format!("{ZLO_SCHEME}git-diff{}", encode(cwd)),
            Self::Selection { path, range } => {
                format!("{ZLO_SCHEME}selection{}#{}", encode(path), range.fragment())
            }
            Self::Skill { name } => format!("{ZLO_SCHEME}skill/{}", encode_segment(name)),
            Self::Extension { name } => {
                format!("{ZLO_SCHEME}extension/{}", encode_segment(name))
            }
        }
    }

    /// Inverse of [`to_uri`](Self::to_uri).
    pub fn parse(uri: &str) -> Result<Self, MentionUriError> {
        let (body, fragment) = match uri.split_once('#') {
            Some((body, fragment)) => (body, Some(fragment)),
            None => (uri, None),
        };
        if let Some(rest) = body.strip_prefix(FILE_SCHEME) {
            if let Some(dir) = rest.strip_suffix('/').filter(|d| !d.is_empty()) {
                if fragment.is_some() {
                    return Err(MentionUriError::BadTarget);
                }
                return Ok(Self::Directory {
                    path: decode_path(dir)?,
                });
            }
            let line_range = fragment.map(LineRange::parse_fragment).transpose()?;
            return Ok(Self::File {
                path: decode_path(rest)?,
                line_range,
            });
        }
        let rest = body
            .strip_prefix(ZLO_SCHEME)
            .ok_or(MentionUriError::UnknownScheme)?;
        let no_fragment = |value: Self| match fragment {
            None => Ok(value),
            Some(_) => Err(MentionUriError::BadTarget),
        };
        if let Some(id) = rest.strip_prefix("thread/") {
            return no_fragment(Self::Thread {
                id: decode_segment(id)?,
            });
        }
        if let Some(cwd) = rest.strip_prefix("git-diff") {
            return no_fragment(Self::GitDiff {
                cwd: decode_path(cwd)?,
            });
        }
        if let Some(path) = rest.strip_prefix("selection") {
            let range = LineRange::parse_fragment(fragment.ok_or(MentionUriError::BadLineRange)?)?;
            return Ok(Self::Selection {
                path: decode_path(path)?,
                range,
            });
        }
        if let Some(name) = rest.strip_prefix("skill/") {
            return no_fragment(Self::Skill {
                name: decode_segment(name)?,
            });
        }
        if let Some(name) = rest.strip_prefix("extension/") {
            return no_fragment(Self::Extension {
                name: decode_segment(name)?,
            });
        }
        Err(MentionUriError::UnknownScheme)
    }

    /// The host path this mention reads, if any.
    pub fn path(&self) -> Option<&str> {
        match self {
            Self::File { path, .. } | Self::Directory { path } | Self::Selection { path, .. } => {
                Some(path)
            }
            Self::GitDiff { cwd } => Some(cwd),
            Self::Thread { .. } | Self::Skill { .. } | Self::Extension { .. } => None,
        }
    }

    /// The short name a chip shows when a link carries no label.
    pub fn default_label(&self) -> String {
        let base = |path: &str| {
            path.trim_end_matches('/')
                .rsplit('/')
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or(path)
                .to_string()
        };
        match self {
            Self::File {
                path,
                line_range: Some(range),
            }
            | Self::Selection { path, range } => format!("{}:{}", base(path), range.fragment()),
            Self::File { path, .. } | Self::Directory { path } => base(path),
            Self::Thread { id } => id.clone(),
            Self::GitDiff { .. } => "diff".into(),
            Self::Skill { name } | Self::Extension { name } => name.clone(),
        }
    }

    /// The mention as a Markdown link: `[@label](uri)`.
    pub fn to_link(&self, label: &str) -> String {
        format!("[@{}]({})", escape_label(label), self.to_uri())
    }
}

impl fmt::Display for MentionUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_uri())
    }
}

/// One mention link found in a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MentionSpan {
    /// Byte range of the whole `[@label](uri)` in the text.
    pub range: Range<usize>,
    /// The label without its leading `@`, unescaped.
    pub label: String,
    pub uri: MentionUri,
}

/// One run of a message: plain text, or a mention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MentionPart<'a> {
    Text(&'a str),
    Mention(MentionSpan),
}

/// Every mention link in `text`, in order. Links whose target is not a
/// [`MentionUri`] (an ordinary `[@x](https://…)`) are left as text.
pub fn mention_spans(text: &str) -> Vec<MentionSpan> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = text[from..].find("[@") {
        let start = from + at;
        match parse_link(text, start) {
            Some(span) => {
                from = span.range.end;
                out.push(span);
            }
            None => from = start + 2,
        }
    }
    out
}

/// `text` split into plain runs and mentions.
pub fn mention_parts(text: &str) -> Vec<MentionPart<'_>> {
    let mut parts = Vec::new();
    let mut last = 0;
    for span in mention_spans(text) {
        if span.range.start > last {
            parts.push(MentionPart::Text(&text[last..span.range.start]));
        }
        last = span.range.end;
        parts.push(MentionPart::Mention(span));
    }
    if last < text.len() {
        parts.push(MentionPart::Text(&text[last..]));
    }
    parts
}

/// Separates the typed message from the context the backend expanded from
/// its mentions. Model-readable on purpose: the agent sees the same text.
pub const MENTION_CONTEXT_OPEN: &str =
    "\n\n---\nMentioned context (expanded by ZLO from the @-mentions above):\n";
/// Ends the expanded mention context.
pub const MENTION_CONTEXT_CLOSE: &str = "\n[End of mentioned context]";

/// The typed part of a stored message, without the expanded mention context.
pub fn strip_mention_context(text: &str) -> &str {
    if !text.ends_with(MENTION_CONTEXT_CLOSE) {
        return text;
    }
    text.rfind(MENTION_CONTEXT_OPEN)
        .map_or(text, |open| &text[..open])
}

fn parse_link(text: &str, start: usize) -> Option<MentionSpan> {
    let after = &text[start + 2..];
    let mut label = String::new();
    let mut chars = after.char_indices();
    let close = loop {
        let (i, c) = chars.next()?;
        match c {
            '\\' => {
                let (_, escaped) = chars.next()?;
                label.push(escaped);
            }
            ']' => break i,
            '\n' => return None,
            c => label.push(c),
        }
    };
    let rest = after[close + 1..].strip_prefix('(')?;
    let end = rest.find(')')?;
    let uri = MentionUri::parse(&rest[..end]).ok()?;
    let total = 2 + close + 1 + 1 + end + 1;
    Some(MentionSpan {
        range: start..start + total,
        label,
        uri,
    })
}

fn escape_label(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    for c in label.chars() {
        match c {
            '\\' | '[' | ']' => {
                out.push('\\');
                out.push(c);
            }
            // A label is one line: control characters would break the link.
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

fn keep(byte: u8, slash: bool) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(byte, b'-' | b'.' | b'_' | b'~')
        || (slash && byte == b'/')
}

fn encode_with(value: &str, slash: bool) -> String {
    let mut out = String::with_capacity(value.len());
    for &byte in value.as_bytes() {
        if keep(byte, slash) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn encode(path: &str) -> String {
    encode_with(path, true)
}

fn encode_segment(value: &str) -> String {
    encode_with(value, false)
}

fn decode(value: &str) -> Result<String, MentionUriError> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = value
                    .get(i + 1..i + 3)
                    .ok_or(MentionUriError::BadEncoding)?;
                let byte = u8::from_str_radix(hex, 16).map_err(|_| MentionUriError::BadEncoding)?;
                out.push(byte);
                i += 3;
            }
            // Only `to_uri`'s output parses: a raw space, bracket or quote
            // means this is not a mention target.
            byte if keep(byte, true) => {
                out.push(byte);
                i += 1;
            }
            _ => return Err(MentionUriError::BadEncoding),
        }
    }
    String::from_utf8(out).map_err(|_| MentionUriError::BadEncoding)
}

fn decode_segment(value: &str) -> Result<String, MentionUriError> {
    let decoded = decode(value)?;
    if decoded.is_empty() || decoded.contains('/') || decoded.chars().any(char::is_control) {
        return Err(MentionUriError::BadTarget);
    }
    Ok(decoded)
}

/// An absolute path with no `.`/`..` segments and no NUL.
fn decode_path(value: &str) -> Result<String, MentionUriError> {
    let decoded = decode(value)?;
    let valid = decoded.starts_with('/')
        && !decoded.contains('\0')
        && decoded
            .split('/')
            .skip(1)
            .all(|segment| segment != "." && segment != "..");
    if !valid {
        return Err(MentionUriError::BadPath);
    }
    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Vec<MentionUri> {
        vec![
            MentionUri::File {
                path: "/Users/me/My Repo/src/(main).rs".into(),
                line_range: None,
            },
            MentionUri::File {
                path: "/repo/a.rs".into(),
                line_range: LineRange::new(10, 20),
            },
            MentionUri::File {
                path: "/repo/ünï.rs".into(),
                line_range: LineRange::new(7, 7),
            },
            MentionUri::Directory {
                path: "/repo/src".into(),
            },
            MentionUri::Thread {
                id: "thread-123_abc".into(),
            },
            MentionUri::GitDiff {
                cwd: "/repo".into(),
            },
            MentionUri::Selection {
                path: "/repo/b.rs".into(),
                range: LineRange { start: 3, end: 9 },
            },
            MentionUri::Skill {
                name: "security-review".into(),
            },
            MentionUri::Extension {
                name: "github".into(),
            },
        ]
    }

    #[test]
    fn every_kind_round_trips_through_its_uri_and_link() {
        for uri in all() {
            let text = uri.to_uri();
            assert!(!text.contains(')') && !text.contains(' '), "{text}");
            assert_eq!(MentionUri::parse(&text), Ok(uri.clone()), "{text}");
            let label = "we[i]rd \\ name";
            let message = format!("see {} please", uri.to_link(label));
            let spans = mention_spans(&message);
            assert_eq!(spans.len(), 1, "{message}");
            assert_eq!(spans[0].uri, uri);
            assert_eq!(spans[0].label, label);
            assert_eq!(&message[..spans[0].range.start], "see ");
            assert_eq!(&message[spans[0].range.end..], " please");
            // Serde (the typed wire form) round-trips too.
            let json = serde_json::to_string(&uri).unwrap();
            assert_eq!(serde_json::from_str::<MentionUri>(&json).unwrap(), uri);
        }
    }

    #[test]
    fn known_uri_shapes_are_stable() {
        assert_eq!(all()[1].to_uri(), "file:///repo/a.rs#L10-20");
        assert_eq!(all()[3].to_uri(), "file:///repo/src/");
        assert_eq!(all()[4].to_uri(), "zlo://thread/thread-123_abc");
        assert_eq!(all()[5].to_uri(), "zlo://git-diff/repo");
        assert_eq!(all()[6].to_uri(), "zlo://selection/repo/b.rs#L3-9");
    }

    #[test]
    fn traversal_and_relative_paths_are_rejected() {
        for bad in [
            "file://relative/x",
            "file:///repo/../etc/passwd",
            "file:///repo/%2E%2E/etc/passwd",
            "file:///repo/./x",
            "file:///repo/a%00b",
            "zlo://git-diff../x",
            "zlo://selection/repo/x",
            "zlo://thread/a%2Fb",
            "zlo://thread/",
            "zlo://nope/x",
            "https://example.com",
            "file:///repo/a.rs#L0",
            "file:///repo/a.rs#L5-2",
            "file:///repo/%ZZ",
        ] {
            assert!(MentionUri::parse(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn ordinary_links_and_broken_links_stay_text() {
        let text = "a [@site](https://x.y) b [@x](file:///a.rs c [@ok](file:///ok.rs)";
        let parts = mention_parts(text);
        assert_eq!(parts.len(), 2);
        assert!(matches!(&parts[0], MentionPart::Text(t) if t.ends_with("c ")));
        assert!(matches!(&parts[1], MentionPart::Mention(m) if m.label == "ok"));
    }

    #[test]
    fn mention_context_is_stripped_for_display() {
        let typed = "explain [@a.rs](file:///a.rs)";
        let stored = format!("{typed}{MENTION_CONTEXT_OPEN}file a.rs ...{MENTION_CONTEXT_CLOSE}");
        assert_eq!(strip_mention_context(&stored), typed);
        assert_eq!(strip_mention_context(typed), typed);
    }

    #[test]
    fn default_labels_name_the_target() {
        assert_eq!(all()[1].default_label(), "a.rs:L10-20");
        assert_eq!(all()[3].default_label(), "src");
        assert_eq!(all()[5].default_label(), "diff");
    }
}
