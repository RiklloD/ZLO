//! Scrubbing for everything that enters a `zlo doctor --bundle` archive.
//!
//! Applied to every text and JSON string before it is written:
//!
//! 1. **PEM blocks** are replaced whole.
//! 2. **Known values** the bundle knows are secret on this machine (see
//!    `known_secret_values` in the parent module) are replaced in every
//!    spelling [`super::encodings::forms`] enumerates: literal, JSON-escaped,
//!    percent-encoded, base64 (standard/URL-safe, any alignment).
//! 3. **JSON** embedded in a log line is parsed and scrubbed structurally.
//! 4. **Shapes** in free text: see [`super::line`].
//! 5. **JSON documents**: any value under a credential-named key is replaced
//!    outright (string, number, array or object; `null`/booleans are kept),
//!    and object keys are scrubbed like any other string.
//!
//! The last check, [`Scrubber::contains_known_secret`], additionally decodes
//! JSON and percent escapes leniently, so a known value that survived in a
//! spelling replacement did not enumerate stops the archive.
//!
//! Over-redaction is the intended failure mode: a bug report missing a value
//! is recoverable, a leaked credential is not. Secrets the bundle does not
//! know and whose shape is not recognized cannot be found in free text.

use serde_json::Value;

use super::encodings;
use super::line;

pub(crate) const REDACTED: &str = "[REDACTED]";

/// From this many characters a known secret is replaced wherever it occurs.
const MIN_EXACT_SECRET_CHARS: usize = 8;
/// Shorter known secrets, down to this many characters, are replaced only as
/// whole tokens (bounded by non-alphanumeric ASCII or the text's edges), so
/// replacing them does not mangle words that merely contain them. Anything
/// shorter cannot be redacted safely at all: see
/// [`Scrubber::has_unredactable_secret`].
const MIN_TOKEN_SECRET_CHARS: usize = 4;

/// Bound on JSON values parsed out of one log line.
const MAX_JSON_ATTEMPTS_PER_LINE: usize = 16;

/// Substrings that make a key credential-named.
const SENSITIVE_KEY_MARKERS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "passphrase",
    "api_key",
    "apikey",
    "authorization",
    "credential",
    "cookie",
    "private_key",
    "access_key",
    "session_key",
    "signature",
    "jwt",
];

/// Whole `_`-separated key segments that make a key credential-named
/// (`pass=…`, `pwd=…`, `auth=user:pass`, `pin=1234`). Matched as segments so
/// `bypass`, `author` and `pinned` are not.
const SENSITIVE_KEY_SEGMENTS: &[&str] = &["pass", "pwd", "auth", "pin", "otp"];

/// Segments that are too common as ordinary field names to replace a
/// non-string JSON value under (`summary.pass` is a count): under these only
/// string values are replaced.
const STRING_ONLY_SEGMENTS: &[&str] = &["pass", "auth"];

/// Keys that carry the *name* of a credential source (an env var, a file)
/// or a count, never the credential itself.
const NON_SECRET_KEY_SUFFIXES: &[&str] = &[
    "_env",
    "_var",
    "_kind",
    "_required",
    "_present",
    "_file",
    "_path",
    "_count",
    "tokens",
];

pub(crate) struct Scrubber {
    /// Every spelling of every known secret of at least
    /// [`MIN_EXACT_SECRET_CHARS`], longest first.
    forms: Vec<String>,
    /// Every spelling of every shorter known secret of at least
    /// [`MIN_TOKEN_SECRET_CHARS`], longest first; matched as whole tokens.
    token_forms: Vec<String>,
    /// The short known secrets themselves, searched as whole tokens inside
    /// decoded base64 blobs (a short secret embedded in a larger encoded
    /// value has no spelling of its own to match).
    token_secrets: Vec<String>,
    /// A known secret is shorter than [`MIN_TOKEN_SECRET_CHARS`].
    unredactable: bool,
}

impl Scrubber {
    pub(crate) fn new(known_secrets: impl IntoIterator<Item = String>) -> Self {
        let mut forms: Vec<String> = Vec::new();
        let mut token_forms: Vec<String> = Vec::new();
        let mut token_secrets: Vec<String> = Vec::new();
        let mut unredactable = false;
        let mut add = |value: &str| match value.chars().count() {
            0 => {}
            n if n < MIN_TOKEN_SECRET_CHARS => unredactable = true,
            n if n < MIN_EXACT_SECRET_CHARS => {
                token_forms.extend(encodings::forms(value));
                token_secrets.push(value.to_string());
            }
            _ => forms.extend(encodings::forms(value)),
        };
        for value in known_secrets {
            let value = value.trim().to_string();
            add(&value);
            let stripped = strip_control(&value);
            if stripped != value {
                add(&stripped);
            }
        }
        // Longest first so a spelling that contains another is replaced whole.
        token_secrets.sort();
        token_secrets.dedup();
        for list in [&mut forms, &mut token_forms] {
            list.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
            list.dedup();
        }
        Self {
            forms,
            token_forms,
            token_secrets,
            unredactable,
        }
    }

    /// Every spelling replacement removes (exact and whole-token), longest
    /// first within each kind.
    pub(crate) fn forms(&self) -> Vec<String> {
        self.forms
            .iter()
            .chain(&self.token_forms)
            .cloned()
            .collect()
    }

    /// A known secret is too short (under [`MIN_TOKEN_SECRET_CHARS`]) to be
    /// redacted without destroying unrelated text, so free text that could
    /// contain it (log tails) must be left out of the bundle.
    pub(crate) fn has_unredactable_secret(&self) -> bool {
        self.unredactable
    }

    /// Scrub free text (log lines, error details).
    pub(crate) fn text(&self, input: &str) -> String {
        let mut text = line::redact_pem(&strip_control(input));
        for form in &self.forms {
            if text.contains(form.as_str()) {
                text = text.replace(form.as_str(), REDACTED);
            }
        }
        for form in &self.token_forms {
            if text.contains(form.as_str()) {
                text = replace_tokens(&text, form);
            }
        }
        if !self.token_secrets.is_empty() {
            text = self.redact_encoded_short_secrets(&text);
        }
        text.split('\n')
            .map(|line| self.line(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// One line: JSON values inside it are scrubbed structurally, the text
    /// around them by shape.
    fn line(&self, text: &str) -> String {
        for (index, _) in text
            .match_indices(['{', '['])
            .take(MAX_JSON_ATTEMPTS_PER_LINE)
        {
            let mut stream =
                serde_json::Deserializer::from_str(&text[index..]).into_iter::<Value>();
            let Some(Ok(mut value)) = stream.next() else {
                continue;
            };
            let end = index + stream.byte_offset();
            let prefix = &text[..index];
            let json = if line::ends_with_sensitive_separator(prefix) {
                REDACTED.to_string()
            } else {
                self.json(&mut value);
                // silent-ok: a Value always serializes; on the impossible
                // error the whole value is withheld.
                serde_json::to_string(&value).unwrap_or_else(|_| REDACTED.to_string())
            };
            return format!(
                "{}{json}{}",
                line::scrub_plain_line(prefix),
                self.line(&text[end..])
            );
        }
        line::scrub_plain_line(text)
    }

    /// Scrub a JSON document in place.
    pub(crate) fn json(&self, value: &mut Value) {
        match value {
            Value::String(text) => *text = self.text(text),
            Value::Array(items) => items.iter_mut().for_each(|item| self.json(item)),
            Value::Object(map) => {
                let old = std::mem::take(map);
                for (key, mut item) in old {
                    let replace = match sensitivity(&key) {
                        Sensitivity::None => false,
                        Sensitivity::StringsOnly => item.is_string(),
                        Sensitivity::Any => !matches!(item, Value::Null | Value::Bool(_)),
                    };
                    if replace {
                        item = Value::String(REDACTED.to_string());
                    } else {
                        self.json(&mut item);
                    }
                    let scrubbed = self.text(&key);
                    let mut new_key = scrubbed.clone();
                    let mut suffix = 2;
                    while map.contains_key(&new_key) {
                        new_key = format!("{scrubbed}#{suffix}");
                        suffix += 1;
                    }
                    map.insert(new_key, item);
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }

    /// `true` when `bytes` still contains a known secret in any enumerated
    /// spelling, or literally after leniently decoding JSON and percent
    /// escapes. Used as the last check before the archive is written.
    pub(crate) fn contains_known_secret(&self, bytes: &[u8]) -> bool {
        if self.forms.is_empty() && self.token_forms.is_empty() {
            return false;
        }
        encodings::decoded_views(bytes).iter().any(|view| {
            self.forms
                .iter()
                .any(|form| contains_subslice(view, form.as_bytes()))
                || self
                    .token_forms
                    .iter()
                    .any(|form| contains_token(view, form.as_bytes()))
                || self.base64_span_holding_short_secret(view).is_some()
        })
    }

    /// The first base64-shaped span of `bytes` that decodes (either alphabet)
    /// to text holding a short known secret as a whole token.
    fn base64_span_holding_short_secret(&self, bytes: &[u8]) -> Option<(usize, usize)> {
        encodings::base64_spans(bytes)
            .into_iter()
            .find(|&(start, end)| {
                encodings::base64_decodings(&bytes[start..end])
                    .iter()
                    .any(|decoded| {
                        self.token_secrets
                            .iter()
                            .any(|secret| contains_token(decoded, secret.as_bytes()))
                    })
            })
    }

    /// Replace every base64-shaped span whose decoding holds a short known
    /// secret as a whole token with [`REDACTED`], whole.
    fn redact_encoded_short_secrets(&self, text: &str) -> String {
        let bytes = text.as_bytes();
        let mut out = String::with_capacity(text.len());
        let mut copied = 0;
        for (start, end) in encodings::base64_spans(bytes) {
            let holds = encodings::base64_decodings(&bytes[start..end])
                .iter()
                .any(|decoded| {
                    self.token_secrets
                        .iter()
                        .any(|secret| contains_token(decoded, secret.as_bytes()))
                });
            if holds {
                // Spans are ASCII-only, so both ends are char boundaries.
                out.push_str(&text[copied..start]);
                out.push_str(REDACTED);
                copied = end;
            }
        }
        out.push_str(&text[copied..]);
        out
    }
}

enum Sensitivity {
    None,
    StringsOnly,
    Any,
}

fn normalize_key(key: &str) -> String {
    line::trim_punctuation(key)
        .trim_start_matches('-')
        .to_ascii_lowercase()
        .replace(['-', '.', ' '], "_")
}

fn sensitivity(key: &str) -> Sensitivity {
    let key = normalize_key(key);
    if NON_SECRET_KEY_SUFFIXES
        .iter()
        .any(|suffix| key.ends_with(suffix))
    {
        return Sensitivity::None;
    }
    if SENSITIVE_KEY_MARKERS
        .iter()
        .any(|marker| key.contains(marker))
    {
        return Sensitivity::Any;
    }
    let segments: Vec<&str> = key.split('_').collect();
    if segments
        .iter()
        .any(|segment| SENSITIVE_KEY_SEGMENTS.contains(segment))
    {
        if segments.iter().all(|segment| {
            !SENSITIVE_KEY_SEGMENTS.contains(segment) || STRING_ONLY_SEGMENTS.contains(segment)
        }) {
            return Sensitivity::StringsOnly;
        }
        return Sensitivity::Any;
    }
    Sensitivity::None
}

pub(crate) fn is_sensitive_key(key: &str) -> bool {
    !matches!(sensitivity(key), Sensitivity::None)
}

/// Whether `bytes[start..end]` is a whole token: the bytes on either side
/// are absent or not ASCII alphanumeric.
fn is_token_at(bytes: &[u8], start: usize, end: usize) -> bool {
    let before = start.checked_sub(1).and_then(|index| bytes.get(index));
    let after = bytes.get(end);
    before.is_none_or(|byte| !byte.is_ascii_alphanumeric())
        && after.is_none_or(|byte| !byte.is_ascii_alphanumeric())
}

/// `needle` occurs in `haystack` as a whole token.
fn contains_token(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack.len() >= needle.len()
        && haystack
            .windows(needle.len())
            .enumerate()
            .any(|(start, window)| {
                window == needle && is_token_at(haystack, start, start + needle.len())
            })
}

/// Replace every whole-token occurrence of `form` in `text`.
fn replace_tokens(text: &str, form: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for (start, _) in text.match_indices(form) {
        if start < copied || !is_token_at(text.as_bytes(), start, start + form.len()) {
            continue;
        }
        out.push_str(&text[copied..start]);
        out.push_str(REDACTED);
        copied = start + form.len();
    }
    out.push_str(&text[copied..]);
    out
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack.len() >= needle.len()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// Drop ANSI escape sequences (service logs are colorized) and every other
/// control character except newline and tab.
pub(crate) fn strip_control(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                // CSI: parameters then one final byte in '@'..='~'.
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            } else {
                chars.next();
            }
            continue;
        }
        if ch == '\n' || ch == '\t' || !ch.is_control() {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests;
