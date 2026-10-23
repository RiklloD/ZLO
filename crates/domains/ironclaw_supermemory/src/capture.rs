//! Automatic capture: one completed turn becomes a conversation document.
//!
//! Hermes `sync_turn` behaviour: the turn's user and assistant text is
//! cleaned (injected `<supermemory-context>` blocks removed, inline base64
//! images replaced by `[image]`), rendered as `[role: user]...[user:end]`,
//! and written with a `customId` of `<thread>_<YYYY-MM-DD>_b<hour/4>` so the
//! API appends every turn of a 4-hour window to one document. Failed writes
//! stay pending (bounded at 50 turns / 256 KiB, oldest dropped first) and are
//! retried with the next capture.

use chrono::{DateTime, Timelike, Utc};
use serde_json::{Value, json};

use crate::client::SOURCE_TAG;

const CAPTURE_BUCKET_HOURS: u32 = 4;
pub(crate) const MAX_PENDING_TURNS: usize = 50;
pub(crate) const MAX_PENDING_BYTES: usize = 256 * 1024;
/// Hermes `_clamp_entity_context`.
const MAX_ENTITY_CONTEXT_CHARS: usize = 1500;

const ENTITY_CONTEXT: &str = include_str!("../prompts/entity_context.md");

/// One turn waiting to be written, keyed by its thread so a retry lands in
/// the thread it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedTurn {
    pub thread_key: String,
    pub user: String,
    pub assistant: String,
}

impl CapturedTurn {
    /// Cleaned turn, or `None` when both sides are empty after cleaning.
    pub fn new(thread_key: impl Into<String>, user: &str, assistant: &str) -> Option<Self> {
        let user = clean_for_capture(user);
        let assistant = clean_for_capture(assistant);
        (!user.is_empty() || !assistant.is_empty()).then(|| Self {
            thread_key: thread_key.into(),
            user,
            assistant,
        })
    }

    pub(crate) fn byte_len(&self) -> usize {
        self.user.len() + self.assistant.len()
    }

    /// `[role: x]\n...\n[x:end]` for each non-empty side.
    pub(crate) fn render(&self) -> String {
        [("user", &self.user), ("assistant", &self.assistant)]
            .into_iter()
            .filter(|(_, text)| !text.is_empty())
            .map(|(role, text)| format!("[role: {role}]\n{text}\n[{role}:end]"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Remove `<supermemory-context>` / `<supermemory-containers>` blocks and
/// replace `data:<mime>;base64,...` runs with `[image]`.
pub(crate) fn clean_for_capture(text: &str) -> String {
    let without_blocks = strip_injected_blocks(text);
    replace_data_uris(&without_blocks).trim().to_string()
}

fn strip_injected_blocks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let next = ["<supermemory-context>", "<supermemory-containers>"]
            .iter()
            .filter_map(|open| rest.find(open).map(|at| (at, *open)))
            .min_by_key(|(at, _)| *at);
        let Some((at, open)) = next else {
            out.push_str(rest);
            return out;
        };
        let close = open.replacen('<', "</", 1);
        out.push_str(&rest[..at]);
        match rest[at..].find(&close) {
            Some(end) => {
                rest = rest[at + end + close.len()..].trim_start();
            }
            // Unterminated block: keep the text rather than drop the tail.
            None => {
                out.push_str(&rest[at..]);
                return out;
            }
        }
    }
}

fn replace_data_uris(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("data:") {
        out.push_str(&rest[..at]);
        let candidate = &rest[at..];
        match base64_data_uri_len(candidate) {
            Some(len) => {
                out.push_str("[image]");
                rest = &candidate[len..];
            }
            None => {
                out.push_str("data:");
                rest = &candidate["data:".len()..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Byte length of a `data:<mime>;base64,<payload>` prefix of `text`.
fn base64_data_uri_len(text: &str) -> Option<usize> {
    let after_scheme = &text["data:".len()..];
    let marker = after_scheme.find(";base64,")?;
    let mime = &after_scheme[..marker];
    if mime.is_empty()
        || mime
            .chars()
            .any(|ch| ch.is_whitespace() || ch == ';' || ch == ',')
    {
        return None;
    }
    let payload_start = "data:".len() + marker + ";base64,".len();
    let payload_len = text[payload_start..]
        .bytes()
        .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
        .count();
    (payload_len > 0).then_some(payload_start + payload_len)
}

/// Sanitized thread key for `customId` (Hermes `_sanitize_tag`).
pub(crate) fn sanitize_key(raw: &str) -> String {
    let mapped: String = raw
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect();
    let collapsed = mapped
        .split('_')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("_");
    if collapsed.is_empty() {
        "ironclaw".to_string()
    } else {
        collapsed
    }
}

/// `<thread>_<YYYY-MM-DD>_b<0..5>`.
pub(crate) fn capture_custom_id(thread_key: &str, now: DateTime<Utc>) -> String {
    format!(
        "{}_{}_b{}",
        sanitize_key(thread_key),
        now.format("%Y-%m-%d"),
        now.hour() / CAPTURE_BUCKET_HOURS
    )
}

pub(crate) fn entity_context() -> String {
    ENTITY_CONTEXT
        .trim()
        .chars()
        .take(MAX_ENTITY_CONTEXT_CHARS)
        .collect()
}

/// `/v3/documents` body for a batch of turns from one thread.
pub(crate) fn capture_body(
    container: &str,
    thread_key: &str,
    turns: &[CapturedTurn],
    now: DateTime<Utc>,
) -> Value {
    let content = turns
        .iter()
        .map(CapturedTurn::render)
        .collect::<Vec<_>>()
        .join("\n\n");
    json!({
        "content": content,
        "containerTags": [container],
        "customId": capture_custom_id(thread_key, now),
        "entityContext": entity_context(),
        "metadata": {
            "sm_source": SOURCE_TAG,
            "type": "conversation",
            "session_id": thread_key,
            "timestamp": now.to_rfc3339(),
        },
    })
}

/// Drop the oldest pending turns past the turn and byte caps. Returns how
/// many were dropped.
pub(crate) fn bound_pending(pending: &mut Vec<CapturedTurn>) -> usize {
    let mut total: usize = pending.iter().map(CapturedTurn::byte_len).sum();
    let mut dropped = 0;
    while !pending.is_empty() && (pending.len() > MAX_PENDING_TURNS || total > MAX_PENDING_BYTES) {
        let oldest = pending.remove(0);
        total -= oldest.byte_len();
        dropped += 1;
    }
    dropped
}
