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

/// Below this many characters an "exact secret" is too short to replace
/// without mangling unrelated text (and too short to be a real credential).
const MIN_EXACT_SECRET_CHARS: usize = 8;

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
    /// Every spelling of every known secret, longest first.
    forms: Vec<String>,
}

impl Scrubber {
    pub(crate) fn new(known_secrets: impl IntoIterator<Item = String>) -> Self {
        let mut forms: Vec<String> = Vec::new();
        for value in known_secrets {
            let value = value.trim().to_string();
            if value.chars().count() < MIN_EXACT_SECRET_CHARS {
                continue;
            }
            let stripped = strip_control(&value);
            forms.extend(encodings::forms(&value));
            if stripped != value && stripped.chars().count() >= MIN_EXACT_SECRET_CHARS {
                forms.extend(encodings::forms(&stripped));
            }
        }
        // Longest first so a spelling that contains another is replaced whole.
        forms.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        forms.dedup();
        Self { forms }
    }

    /// Every spelling replacement removes, longest first.
    pub(crate) fn forms(&self) -> &[String] {
        &self.forms
    }

    /// Scrub free text (log lines, error details).
    pub(crate) fn text(&self, input: &str) -> String {
        let mut text = line::redact_pem(&strip_control(input));
        for form in &self.forms {
            if text.contains(form.as_str()) {
                text = text.replace(form.as_str(), REDACTED);
            }
        }
        text.split('\n')
            .map(|line| self.line(line))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// One line: JSON values inside it are scrubbed structurally, the text
    /// around them by shape.
    fn line(&self, text: &str) -> String {
        let mut attempts = 0;
        for (index, _) in text.match_indices(['{', '[']) {
            if attempts == MAX_JSON_ATTEMPTS_PER_LINE {
                break;
            }
            attempts += 1;
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
        if self.forms.is_empty() {
            return false;
        }
        encodings::decoded_views(bytes).iter().any(|view| {
            self.forms
                .iter()
                .any(|form| contains_subslice(view, form.as_bytes()))
        })
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
