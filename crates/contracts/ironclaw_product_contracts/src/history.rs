//! History search descriptors and wire DTOs: search across the caller's
//! past chats.
//!
//! One view, four modes inferred from the arguments:
//!
//! - **discover** (`query`) — ranked threads, each with highlighted snippets
//!   of the matching messages; `thread_id` narrows to one thread;
//! - **scroll** (`thread_id` + `around_message_id` or `around_sequence`) — a
//!   window of messages around one message;
//! - **read** (`thread_id`) — a bounded read of the thread's messages;
//! - **browse** (no arguments) — the most recent threads.
//!
//! Only conversation text is searched or returned: the person's messages and
//! the assistant's finished replies. Tool output and hidden/system text never
//! appear. Automation and subagent threads rank below the person's own
//! threads, and forks collapse into their lineage root.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::descriptors::ProductView;

/// `GET /api/webchat/v2/history/search` and the `ironclaw.history.search`
/// model tool answer this view.
pub const HISTORY_SEARCH_VIEW: ProductView<serde_json::Value, serde_json::Value> =
    ProductView::unpaginated("history.search");

/// Longest accepted query, in characters; transports reject longer text.
pub const HISTORY_QUERY_MAX_CHARS: usize = 512;
/// Default and maximum thread groups per discover/browse answer.
pub const HISTORY_DEFAULT_THREADS: u32 = 10;
pub const HISTORY_MAX_THREADS: u32 = 50;
/// Default and maximum messages on each side of a scroll anchor.
pub const HISTORY_DEFAULT_WINDOW: u32 = 6;
pub const HISTORY_MAX_WINDOW: u32 = 25;
/// Default and maximum messages in one read.
pub const HISTORY_DEFAULT_READ_MESSAGES: u32 = 60;
pub const HISTORY_MAX_READ_MESSAGES: u32 = 200;
/// Characters kept per message in scroll and read answers.
pub const HISTORY_MESSAGE_MAX_CHARS: usize = 4_000;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistorySearchRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub around_message_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub around_sequence: Option<u64>,
    /// Threads (discover/browse) or messages (read).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Messages on each side of the scroll anchor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<u32>,
    /// Read only: first message sequence to return (inclusive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_sequence: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistorySearchMode {
    Discover,
    Scroll,
    Read,
    Browse,
}

impl HistorySearchRequest {
    /// The mode the arguments ask for.
    pub fn mode(&self) -> HistorySearchMode {
        let has_query = self.query.as_deref().is_some_and(|q| !q.trim().is_empty());
        let has_anchor = self.around_message_id.is_some() || self.around_sequence.is_some();
        match (has_query, self.thread_id.is_some(), has_anchor) {
            (true, _, _) => HistorySearchMode::Discover,
            (false, true, true) => HistorySearchMode::Scroll,
            (false, true, false) => HistorySearchMode::Read,
            (false, false, _) => HistorySearchMode::Browse,
        }
    }
}

/// Where a thread came from; anything but `Own` ranks below the person's
/// own threads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryThreadOrigin {
    Own,
    Automation,
    Subagent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryRole {
    User,
    Assistant,
}

/// A highlighted UTF-8 byte range inside a snippet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryHighlight {
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryHit {
    pub thread_id: String,
    pub message_id: String,
    pub sequence: u64,
    pub role: HistoryRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    pub snippet: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub highlights: Vec<HistoryHighlight>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryThreadResult {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
    pub origin: HistoryThreadOrigin,
    /// The thread this one was forked from, transitively; set when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lineage_root_thread_id: Option<String>,
    /// Other threads of the same lineage collapsed into this group.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub related_thread_ids: Vec<String>,
    /// Best first; empty in browse mode.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hits: Vec<HistoryHit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryMessage {
    pub message_id: String,
    pub sequence: u64,
    pub role: HistoryRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    pub text: String,
    /// The text was cut at `HISTORY_MESSAGE_MAX_CHARS`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistorySearchResponse {
    pub mode: HistorySearchMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Discover and browse.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub threads: Vec<HistoryThreadResult>,
    /// Scroll and read: the thread the messages belong to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<HistoryThreadResult>,
    /// Scroll and read, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<HistoryMessage>,
    /// Read: the sequence to pass as `from_sequence` for the next page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_from_sequence: Option<u64>,
    /// `false` while older chats are still being added to the index.
    pub index_complete: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_is_inferred_from_arguments() {
        let mut r = HistorySearchRequest::default();
        assert_eq!(r.mode(), HistorySearchMode::Browse);
        r.thread_id = Some("t".into());
        assert_eq!(r.mode(), HistorySearchMode::Read);
        r.around_sequence = Some(4);
        assert_eq!(r.mode(), HistorySearchMode::Scroll);
        r.query = Some("deploy".into());
        assert_eq!(r.mode(), HistorySearchMode::Discover);
        r.query = Some("   ".into());
        assert_eq!(r.mode(), HistorySearchMode::Scroll);
    }

    #[test]
    fn unknown_request_fields_are_rejected() {
        let error = serde_json::from_value::<HistorySearchRequest>(serde_json::json!({
            "query": "x", "sql": "drop"
        }));
        assert!(error.is_err());
    }
}
