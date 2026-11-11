//! Scrubbing for everything that enters a `zlo doctor --bundle` archive.
//!
//! Three layers, applied to every text and JSON string before it is written:
//!
//! 1. **Exact values** the bundle knows are secret on this machine (the
//!    WebUI token file, the values of every env var the config or provider
//!    catalog names as a credential source) are replaced wherever they occur.
//! 2. **Shapes**: URL userinfo, query strings and fragments; `key=value` /
//!    `key: value` pairs whose key names a credential; the word after
//!    `Bearer`/`Basic`; and any token that `ironclaw_config`'s inline-secret
//!    guard ([`ironclaw_config::reject_inline_secret`]) classifies as secret
//!    material — the same detector that keeps secrets out of `config.toml`.
//! 3. **JSON object keys** that name a credential have their string value
//!    replaced outright.
//!
//! Over-redaction is the intended failure mode: a bug report missing a value
//! is recoverable, a leaked credential is not.

use serde_json::Value;

pub(crate) const REDACTED: &str = "[REDACTED]";

/// Below this many characters an "exact secret" is too short to replace
/// without mangling unrelated text (and too short to be a real credential).
const MIN_EXACT_SECRET_CHARS: usize = 8;

const SENSITIVE_KEY_MARKERS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "api_key",
    "apikey",
    "api-key",
    "authorization",
    "credential",
    "cookie",
    "private_key",
    "access_key",
    "session_key",
    "signature",
];

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
    exact: Vec<String>,
}

impl Scrubber {
    pub(crate) fn new(known_secrets: impl IntoIterator<Item = String>) -> Self {
        let mut exact: Vec<String> = known_secrets
            .into_iter()
            .map(|value| value.trim().to_string())
            .filter(|value| value.chars().count() >= MIN_EXACT_SECRET_CHARS)
            .collect();
        // Longest first so a secret that contains another is replaced whole.
        exact.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        exact.dedup();
        Self { exact }
    }

    /// Scrub free text (log lines, error details) line by line.
    pub(crate) fn text(&self, input: &str) -> String {
        let mut text = strip_control(input);
        for secret in &self.exact {
            text = text.replace(secret.as_str(), REDACTED);
        }
        text.split('\n')
            .map(scrub_line)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Scrub a JSON document in place: credential-named keys lose their
    /// string values, every other string goes through [`Self::text`].
    pub(crate) fn json(&self, value: &mut Value) {
        match value {
            Value::String(text) => *text = self.text(text),
            Value::Array(items) => items.iter_mut().for_each(|item| self.json(item)),
            Value::Object(map) => {
                for (key, item) in map.iter_mut() {
                    if is_sensitive_key(key) && item.is_string() {
                        *item = Value::String(REDACTED.to_string());
                    } else {
                        self.json(item);
                    }
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }

    /// `true` when `bytes` still contains a known secret value. Used as the
    /// last check before the archive is written.
    pub(crate) fn contains_known_secret(&self, bytes: &[u8]) -> bool {
        self.exact
            .iter()
            .any(|secret| contains_subslice(bytes, secret.as_bytes()))
    }
}

pub(crate) fn is_sensitive_key(key: &str) -> bool {
    let key = trim_punctuation(key).to_ascii_lowercase();
    if NON_SECRET_KEY_SUFFIXES
        .iter()
        .any(|suffix| key.ends_with(suffix))
    {
        return false;
    }
    SENSITIVE_KEY_MARKERS
        .iter()
        .any(|marker| key.contains(marker))
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// Drop ANSI escape sequences (service logs are colorized) and every other
/// control character except newline and tab.
fn strip_control(input: &str) -> String {
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

fn scrub_line(line: &str) -> String {
    let mut out = Vec::new();
    let mut redact_next = false;
    for token in line.split(' ') {
        if token.is_empty() {
            out.push(String::new());
            continue;
        }
        let core = trim_punctuation(token);
        let is_auth_scheme =
            core.eq_ignore_ascii_case("bearer") || core.eq_ignore_ascii_case("basic");
        if redact_next && !is_auth_scheme {
            out.push(REDACTED.to_string());
            redact_next = false;
            continue;
        }
        // `Authorization: Bearer <value>`: keep the scheme word, redact the
        // value after it.
        if is_auth_scheme {
            redact_next = true;
            out.push(token.to_string());
            continue;
        }
        // `key: value` / `key = value` split across tokens.
        if (token.ends_with(':') || token.ends_with('=')) && is_sensitive_key(token) {
            redact_next = true;
            out.push(token.to_string());
            continue;
        }
        if token == "=" || token == ":" {
            // `key = value`: decided by the previous token.
            if out
                .last()
                .is_some_and(|previous| is_sensitive_key(previous))
            {
                redact_next = true;
            }
            out.push(token.to_string());
            continue;
        }
        out.push(scrub_token(token));
    }
    out.join(" ")
}

fn scrub_token(token: &str) -> String {
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
    for separator in ['=', ':'] {
        if let Some((key, value)) = token.split_once(separator)
            && !key.is_empty()
            && !value.is_empty()
        {
            if is_sensitive_key(key) {
                return format!("{key}{separator}{REDACTED}");
            }
            return format!("{key}{separator}{}", scrub_token_plain(value));
        }
    }
    let core = trim_punctuation(token);
    if ironclaw_config::reject_inline_secret("diagnostics", core).is_err() {
        return REDACTED.to_string();
    }
    token.to_string()
}

/// `scheme://user:pass@host/p/a/t/h?query#frag` →
/// `scheme://[REDACTED]@host/p/a/t/h?[REDACTED]`; secret-shaped path
/// segments (webhook/bot tokens) are replaced too.
fn scrub_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return scrub_token_plain(url);
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let authority = match authority.rsplit_once('@') {
        Some((_, host)) => format!("{REDACTED}@{host}"),
        None => authority.to_string(),
    };
    let (path, had_query) = match tail.find(['?', '#']) {
        Some(index) => (&tail[..index], true),
        None => (tail, false),
    };
    let path = path
        .split('/')
        .map(|segment| {
            if ironclaw_config::reject_inline_secret("diagnostics", segment).is_err() {
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

fn trim_punctuation(value: &str) -> &str {
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
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scrubber() -> Scrubber {
        Scrubber::new(["known-secret-value-123".to_string(), "short".to_string()])
    }

    #[test]
    fn url_userinfo_query_and_fragment_are_removed() {
        let out =
            scrubber().text("connect postgres://admin:hunter2pw@db.internal:5432/zlo?sslkey=abc#x");
        assert_eq!(
            out,
            "connect postgres://[REDACTED]@db.internal:5432/zlo?[REDACTED]"
        );
    }

    #[test]
    fn credential_pairs_and_bearer_values_are_removed() {
        let out = scrubber()
            .text("api_key=plainvalue password: opensesame Authorization: Bearer abc.def");
        assert!(!out.contains("plainvalue"), "{out}");
        assert!(!out.contains("opensesame"), "{out}");
        assert!(!out.contains("abc.def"), "{out}");
        assert!(out.contains("api_key=[REDACTED]"), "{out}");
    }

    #[test]
    fn env_var_names_and_token_counts_survive() {
        let out = scrubber().text("api_key_env=OPENAI_API_KEY input_tokens=42 model=gpt-5-mini");
        assert_eq!(
            out,
            "api_key_env=OPENAI_API_KEY input_tokens=42 model=gpt-5-mini"
        );
    }

    #[test]
    fn secret_shaped_tokens_and_exact_values_are_removed() {
        let slack = format!("xoxb-{}", "1234567890-abcdefghij");
        let out = scrubber().text(&format!(
            "sent {slack} then known-secret-value-123 and short"
        ));
        assert!(!out.contains(&slack), "{out}");
        assert!(!out.contains("known-secret-value-123"), "{out}");
        assert!(
            out.ends_with("and short"),
            "short values are not exact-matched: {out}"
        );
    }

    #[test]
    fn ansi_escapes_are_stripped() {
        assert_eq!(
            scrubber().text("\u{1b}[2m2026\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m ok"),
            "2026  INFO ok"
        );
    }

    #[test]
    fn json_redacts_credential_keys_and_scrubs_strings() {
        let mut value = serde_json::json!({
            "webui.env_token_var": "ZLO_WEBUI_TOKEN",
            "client_secret": "abc",
            "nested": [{"detail": "see https://u:p@h/x?y=z"}],
            "count": 3
        });
        scrubber().json(&mut value);
        assert_eq!(value["webui.env_token_var"], "ZLO_WEBUI_TOKEN");
        assert_eq!(value["client_secret"], REDACTED);
        assert_eq!(
            value["nested"][0]["detail"],
            "see https://[REDACTED]@h/x?[REDACTED]"
        );
        assert_eq!(value["count"], 3);
    }
}
