//! Shape-based scrubbing of one line of free text (no known values needed).
//!
//! A line is scanned for credential spans first, then every remaining
//! whitespace-delimited word is checked for secret shapes:
//!
//! - `key=value`, `key: value`, `key => value`, `"key":"value"`, the same
//!   with JSON-escaped quotes, and `--flag value` / `--flag=value`, when the
//!   key or flag names a credential. A quoted value is redacted through its
//!   closing quote (or to end of line when unclosed); a `{…}`/`[…]` value
//!   through its matching bracket; a value starting with an auth-scheme word
//!   (`Bearer`, `Basic`, …) keeps the scheme and loses the rest of the line;
//!   any other value loses everything up to the next whitespace.
//! - a standalone `Bearer`/`Basic` loses the word after it.
//! - URL userinfo, query and fragment; secret-shaped path segments.
//! - secret-shaped words: the `ironclaw_config` inline-secret detector
//!   (`sk-`, `xox?-`, `ghp_`, `AKIA`, `AIza`, `ya29.`, `hf_`, JWTs, long
//!   hex…), Telegram bot tokens (`123456:AA…`, also as `bot<token>`),
//!   Notion `secret_…`/`ntn_…`, Google refresh tokens (`1//…`).
//!
//! Whitespace means any Unicode whitespace, not only spaces.

use super::redact::{REDACTED, is_sensitive_key};

const AUTH_SCHEMES: &[&str] = &[
    "bearer",
    "basic",
    "digest",
    "token",
    "bot",
    "negotiate",
    "ntlm",
];

pub(super) fn scrub_plain_line(line: &str) -> String {
    let mut spans: Vec<(usize, usize)> = Vec::new();
    key_value_spans(line, &mut spans);
    flag_and_scheme_spans(line, &mut spans);
    spans.sort_unstable();
    let mut out = String::with_capacity(line.len());
    let mut pos = 0;
    for (start, end) in spans {
        out.push_str(&scrub_words(&line[pos..start]));
        out.push_str(REDACTED);
        pos = end;
    }
    out.push_str(&scrub_words(&line[pos..]));
    out
}

/// `true` when `prefix` ends in `<sensitive key><separator>` (optionally
/// followed by whitespace), so whatever value follows it is a credential.
pub(super) fn ends_with_sensitive_separator(prefix: &str) -> bool {
    let trimmed = prefix.trim_end();
    let Some(before) = trimmed
        .strip_suffix("=>")
        .or_else(|| trimmed.strip_suffix('='))
        .or_else(|| trimmed.strip_suffix(':'))
    else {
        return false;
    };
    let key = key_before(before.as_bytes(), before.len()).map(|(start, end)| &before[start..end]);
    key.is_some_and(is_sensitive_key)
}

fn push_span(spans: &mut Vec<(usize, usize)>, span: (usize, usize)) {
    if span.0 < span.1 && !spans.iter().any(|&(s, e)| span.0 < e && s < span.1) {
        spans.push(span);
    }
}

fn is_key_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')
}

fn is_space(byte: u8) -> bool {
    byte.is_ascii_whitespace()
}

/// The key ending just before `end` (skipping whitespace and an optional,
/// possibly backslash-escaped, closing quote).
fn key_before(bytes: &[u8], mut end: usize) -> Option<(usize, usize)> {
    while end > 0 && is_space(bytes[end - 1]) {
        end -= 1;
    }
    if end > 0 && matches!(bytes[end - 1], b'"' | b'\'') {
        end -= 1;
        if end > 0 && bytes[end - 1] == b'\\' {
            end -= 1;
        }
    }
    let mut start = end;
    while start > 0 && is_key_byte(bytes[start - 1]) {
        start -= 1;
    }
    (start < end).then_some((start, end))
}

fn key_value_spans(line: &str, spans: &mut Vec<(usize, usize)>) {
    let bytes = line.as_bytes();
    for index in 0..bytes.len() {
        let separator = bytes[index];
        if separator != b'=' && separator != b':' {
            continue;
        }
        if line[index..].starts_with("://") || line[index..].starts_with("::") {
            continue;
        }
        let Some((start, end)) = key_before(bytes, index) else {
            continue;
        };
        if !is_sensitive_key(&line[start..end]) {
            continue;
        }
        let mut value = index + 1;
        if separator == b'=' && bytes.get(value) == Some(&b'>') {
            value += 1;
        }
        while value < bytes.len() && is_space(bytes[value]) {
            value += 1;
        }
        if let Some(span) = value_span(line, value) {
            push_span(spans, span);
        }
    }
}

fn flag_and_scheme_spans(line: &str, spans: &mut Vec<(usize, usize)>) {
    let words = words(line);
    for (position, &(start, end)) in words.iter().enumerate() {
        let word = &line[start..end];
        let Some(&(next_start, _)) = words.get(position + 1) else {
            continue;
        };
        let is_flag = word.starts_with("--") && !word.contains('=') && is_sensitive_key(word);
        let core = trim_punctuation(word);
        let is_scheme = core.eq_ignore_ascii_case("bearer") || core.eq_ignore_ascii_case("basic");
        if is_flag || is_scheme {
            if let Some(span) = value_span(line, next_start) {
                push_span(spans, span);
            }
        }
    }
}

/// `(start, end)` byte ranges of the whitespace-delimited words of `line`.
fn words(line: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = None;
    for (index, ch) in line.char_indices() {
        match (ch.is_whitespace(), start) {
            (true, Some(begin)) => {
                out.push((begin, index));
                start = None;
            }
            (false, None) => start = Some(index),
            _ => {}
        }
    }
    if let Some(begin) = start {
        out.push((begin, line.len()));
    }
    out
}

/// The span to redact for a value starting at byte `value`.
fn value_span(line: &str, value: usize) -> Option<(usize, usize)> {
    let bytes = line.as_bytes();
    let first = *bytes.get(value)?;
    let len = bytes.len();
    match first {
        b'"' | b'\'' | b'`' => {
            let close = find_closing_quote(bytes, value + 1, first).unwrap_or(len);
            Some((value + 1, close))
        }
        b'\\' if matches!(bytes.get(value + 1), Some(b'"' | b'\'')) => {
            let quote = bytes[value + 1];
            let close = (value + 2..len.saturating_sub(1))
                .find(|&i| bytes[i] == b'\\' && bytes[i + 1] == quote)
                .unwrap_or(len);
            Some((value + 2, close))
        }
        b'{' | b'[' => Some((
            value,
            matching_bracket(bytes, value).map_or(len, |end| end + 1),
        )),
        _ => {
            let word_end = line[value..]
                .find(char::is_whitespace)
                .map_or(len, |offset| value + offset);
            let word = trim_punctuation(&line[value..word_end]);
            if AUTH_SCHEMES
                .iter()
                .any(|scheme| word.eq_ignore_ascii_case(scheme))
            {
                let rest = line[word_end..]
                    .find(|ch: char| !ch.is_whitespace())
                    .map(|offset| word_end + offset)?;
                Some((rest, len))
            } else {
                Some((value, word_end))
            }
        }
    }
}

fn find_closing_quote(bytes: &[u8], from: usize, quote: u8) -> Option<usize> {
    let mut index = from;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            byte if byte == quote => return Some(index),
            _ => index += 1,
        }
    }
    None
}

fn matching_bracket(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut index = open;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => index = find_closing_quote(bytes, index + 1, b'"')?,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

/// Scrub each word of a segment, preserving the whitespace between words.
fn scrub_words(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    let mut pos = 0;
    for (start, end) in words(segment) {
        out.push_str(&segment[pos..start]);
        out.push_str(&scrub_token(&segment[start..end]));
        pos = end;
    }
    out.push_str(&segment[pos..]);
    out
}

fn scrub_token(token: &str) -> String {
    if contains_google_refresh_token(token) {
        return REDACTED.to_string();
    }
    if let Some(scheme_end) = token.find("://") {
        let start = token[..scheme_end]
            .char_indices()
            .rev()
            .take_while(|(_, ch)| ch.is_ascii_alphanumeric() || matches!(ch, '+' | '.' | '-'))
            .last()
            .map_or(scheme_end, |(index, _)| index);
        let (prefix, url) = token.split_at(start);
        return format!("{}{}", scrub_token_plain(prefix), scrub_url(url));
    }
    scrub_token_plain(token)
}

fn scrub_token_plain(token: &str) -> String {
    if is_secret_shape(trim_punctuation(token)) {
        return REDACTED.to_string();
    }
    for separator in ['=', ':'] {
        if let Some((key, value)) = token.split_once(separator)
            && !key.is_empty()
            && !trim_punctuation(value).is_empty()
        {
            if is_sensitive_key(key) {
                return format!("{key}{separator}{REDACTED}");
            }
            return format!("{key}{separator}{}", scrub_token_plain(value));
        }
    }
    token.to_string()
}

/// `scheme://user:pass@host/p/a/t/h?query#frag` →
/// `scheme://[REDACTED]@host/p/a/t/h?[REDACTED]`; secret-shaped path
/// segments (webhook paths, `/bot<telegram token>/`) are replaced too.
fn scrub_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return scrub_token_plain(url);
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let authority = match authority.rsplit_once('@') {
        Some((_, host)) => format!("{REDACTED}@{host}"),
        None if is_secret_shape(authority) => REDACTED.to_string(),
        None => authority.to_string(),
    };
    let (path, had_query) = match tail.find(['?', '#']) {
        Some(index) => (&tail[..index], true),
        None => (tail, false),
    };
    let path = path
        .split('/')
        .map(|segment| {
            if is_secret_shape(segment) {
                REDACTED.to_string()
            } else {
                segment.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/");
    let query = if had_query {
        format!("?{REDACTED}")
    } else {
        String::new()
    };
    format!("{scheme}://{authority}{path}{query}")
}

pub(super) fn is_secret_shape(core: &str) -> bool {
    ironclaw_config::reject_inline_secret("diagnostics", core).is_err()
        || is_telegram_bot_token(core.strip_prefix("bot").unwrap_or(core))
        || contains_prefixed_token(core, "secret_", 20)
        || contains_prefixed_token(core, "ntn_", 20)
        || contains_google_refresh_token(core)
}

/// `<5-12 digits>:<30+ of [A-Za-z0-9_-]>`.
fn is_telegram_bot_token(value: &str) -> bool {
    let Some((id, secret)) = value.split_once(':') else {
        return false;
    };
    (5..=12).contains(&id.len())
        && id.bytes().all(|byte| byte.is_ascii_digit())
        && secret.len() >= 30
        && secret
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// `prefix` at a word boundary followed by at least `min` alphanumerics.
fn contains_prefixed_token(value: &str, prefix: &str, min: usize) -> bool {
    value.match_indices(prefix).any(|(index, _)| {
        let boundary = index == 0
            || !value.as_bytes()[index - 1].is_ascii_alphanumeric()
                && value.as_bytes()[index - 1] != b'_';
        boundary
            && value[index + prefix.len()..]
                .bytes()
                .take_while(u8::is_ascii_alphanumeric)
                .count()
                >= min
    })
}

/// Google OAuth refresh tokens: `1//` followed by 20+ base64url characters.
fn contains_google_refresh_token(value: &str) -> bool {
    value.match_indices("1//").any(|(index, _)| {
        value[index + 3..]
            .bytes()
            .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            .count()
            >= 20
    })
}

pub(super) fn trim_punctuation(value: &str) -> &str {
    value.trim_matches(|ch: char| {
        matches!(
            ch,
            '"' | '\''
                | '`'
                | ','
                | ';'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '<'
                | '>'
                | ':'
                | '='
                | '\\'
        )
    })
}

/// Replace PEM blocks whole. An unterminated `-----BEGIN` loses the rest of
/// the text; an `-----END` with no `-----BEGIN` before it (a block cut by a
/// log tail) loses everything before it.
pub(super) fn redact_pem(text: &str) -> String {
    const BEGIN: &str = "-----BEGIN ";
    const END: &str = "-----END ";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    if let Some(end) = rest.find(END)
        && rest.find(BEGIN).is_none_or(|begin| begin > end)
    {
        out.push_str(REDACTED);
        rest = after_marker(rest, end + END.len());
    }
    while let Some(begin) = rest.find(BEGIN) {
        out.push_str(&rest[..begin]);
        out.push_str(REDACTED);
        rest = match rest[begin..].find(END) {
            Some(offset) => after_marker(rest, begin + offset + END.len()),
            None => "",
        };
    }
    out.push_str(rest);
    out
}

/// The text after the `-----` that closes a marker whose label starts at
/// `from` (or after the label's line when the closing dashes are missing).
fn after_marker(text: &str, from: usize) -> &str {
    match text[from..].find("-----") {
        Some(offset) => &text[from + offset + 5..],
        None => match text[from..].find('\n') {
            Some(offset) => &text[from + offset..],
            None => "",
        },
    }
}
