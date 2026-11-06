//! Search projections of transcripts kept outside this crate.
//!
//! A thread run by a coding-agent CLI (Claude, Codex) keeps its transcript in
//! the runtime's own store (the CLI's session file or journal); this crate
//! never holds it. To make those threads searchable, the product layer reads
//! the runtime transcript through the orchestrator's history readers and
//! upserts one *search projection* per visible message: the bounded visible
//! text, its role and time, and the navigation id the timeline renders for it.
//! A projection is a derived search entry, never a second authoritative
//! transcript: it is not read back as conversation history, the original
//! transcript is never touched, and re-ingesting the same rows is a no-op.
//!
//! Each `(thread, source)` keeps a monotonic cursor (`ingested_through`) so an
//! interrupted ingestion resumes, and a per-thread summary so a thread that
//! has no canonical session record (a CLI-only fork or delegated child) still
//! groups and titles its hits.

use chrono::{DateTime, Utc};
use ironclaw_host_api::ids::ThreadId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{SearchHitRole, ThreadMessageId, ThreadScope};

/// One runtime transcript feeding a thread: a segment of a thread that
/// switched engines, or the whole history of one that never did.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ExternalTranscriptSource {
    /// Position of the segment in the thread (0 for an unswitched thread).
    pub segment: u32,
    pub runtime_id: String,
    pub session_id: Option<String>,
}

impl ExternalTranscriptSource {
    /// Path-safe, stable key: the segment, then a digest of runtime and
    /// session (runtime ids and CLI session ids are opaque text).
    pub(crate) fn key(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(self.runtime_id.as_bytes());
        digest.update([0]);
        digest.update(self.session_id.as_deref().unwrap_or_default().as_bytes());
        let hex: String = digest
            .finalize()
            .iter()
            .take(8)
            .map(|b| format!("{b:02x}"))
            .collect();
        format!("s{:04}-{hex}", self.segment)
    }
}

/// One visible message of an external transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalTranscriptMessage {
    /// Position of the row in its source, from 0.
    pub ordinal: u64,
    /// The id the thread timeline renders for this row, so a hit opens it.
    pub navigation_message_id: ThreadMessageId,
    pub role: SearchHitRole,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

/// What a hit needs to show its thread when there is no session record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalThreadSummary {
    pub thread_id: ThreadId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpsertExternalTranscriptRequest {
    pub scope: ThreadScope,
    pub thread: ExternalThreadSummary,
    pub source: ExternalTranscriptSource,
    /// Rows at or after the source's cursor (earlier rows are re-checked and
    /// left alone when unchanged). Order does not matter.
    pub messages: Vec<ExternalTranscriptMessage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExternalTranscriptUpsert {
    pub written: usize,
    pub unchanged: usize,
    /// The source's cursor after the upsert: one past the highest ordinal.
    pub ingested_through: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalTranscriptCursorRequest {
    pub scope: ThreadScope,
    pub thread_id: ThreadId,
    pub source: ExternalTranscriptSource,
}

/// Progress of one bounded indexing step over the scope's own transcripts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MessageSearchIndexProgress {
    pub complete: bool,
    /// Messages examined this step.
    pub scanned: usize,
    /// Messages whose projection was rewritten this step.
    pub reprojected: usize,
}

/// Stored projection record (`search-projections/<thread>/<source>/m-<ordinal>.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredExternalProjection {
    pub(crate) thread_id: ThreadId,
    pub(crate) source_key: String,
    pub(crate) segment: u32,
    #[serde(flatten)]
    pub(crate) message: ExternalTranscriptMessage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredExternalCursor {
    pub(crate) ingested_through: u64,
}
