//! Terminal session import: coding-agent CLI sessions the person ran outside
//! the app, brought in as ordinary threads on the matching CLI runtime (keyed
//! by the CLI's own session id) that land in the inbox's Settled shelf. A
//! source is an opaque id the backend names (with a display label); the inbox
//! row carries it as [`crate::inbox::InboxRow::imported`]. A run is a
//! background job: the status answers `running` until it ends.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::descriptors::{ProductSurfaceCommandDescriptor, ProductView};

/// Why a run started. `Launch` runs only when the person never ran one (the
/// first launch after the upgrade); `Background` only with `background` on
/// and the last run older than [`BACKGROUND_RESCAN_MIN_MS`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportTrigger {
    Manual,
    Launch,
    Background,
}

pub const BACKGROUND_RESCAN_MIN_MS: i64 = 15 * 60 * 1000;

/// Per-user settings. A source missing from `sources` is on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionImportSettings {
    pub sources: BTreeMap<String, bool>,
    /// Rescan now and then for new and grown sessions (an unchanged file is
    /// not read again).
    pub background: bool,
}

impl Default for SessionImportSettings {
    fn default() -> Self {
        Self {
            sources: BTreeMap::new(),
            background: true,
        }
    }
}

impl SessionImportSettings {
    pub fn enabled(&self, source: &str) -> bool {
        self.sources.get(source).copied().unwrap_or(true)
    }
}

/// What one run did with one source's sessions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionImportCounts {
    /// Session files looked at.
    pub found: u32,
    /// New threads.
    pub imported: u32,
    /// Imported sessions that grew; their new messages were added.
    pub updated: u32,
    pub unchanged: u32,
    /// Started by this app, empty, a subagent's, or a deleted thread.
    pub skipped: u32,
    /// Unreadable.
    pub failed: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionImportSource {
    pub source: String,
    pub label: String,
    pub enabled: bool,
    /// The CLI runtime its threads run on is installed; others are not scanned.
    pub available: bool,
    /// The last run's counts, when it scanned this source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counts: Option<SessionImportCounts>,
}

/// Output of [`SESSION_IMPORT_VIEW`] and [`SESSION_IMPORT_COMMAND`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionImportStatus {
    pub sources: Vec<SessionImportSource>,
    pub background: bool,
    #[serde(default)]
    pub running: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_trigger: Option<ImportTrigger>,
    /// The last run failed before it finished (sanitized).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

/// Input of [`SESSION_IMPORT_COMMAND`]: settings to change (left out keeps
/// the value), then a run when `run` is set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionImportRequest {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sources: BTreeMap<String, bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<ImportTrigger>,
}

/// Output: [`SessionImportStatus`].
pub const SESSION_IMPORT_VIEW: ProductView<serde_json::Value, serde_json::Value> =
    ProductView::unpaginated("session_import.status");

/// Output: [`SessionImportStatus`] after the write.
pub const SESSION_IMPORT_COMMAND_ID: &str = "session_import.run";
pub const SESSION_IMPORT_COMMAND: ProductSurfaceCommandDescriptor<
    SessionImportRequest,
    serde_json::Value,
> = ProductSurfaceCommandDescriptor::new(SESSION_IMPORT_COMMAND_ID);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources_default_on_and_requests_reject_unknown_fields() {
        let s: SessionImportSettings = serde_json::from_str(r#"{"sources":{"a":false}}"#).unwrap();
        assert!(!s.enabled("a") && s.enabled("b") && s.background);
        let r: SessionImportRequest =
            serde_json::from_str(r#"{"sources":{"a":true},"run":"manual"}"#).unwrap();
        assert_eq!(r.run, Some(ImportTrigger::Manual));
        assert!(serde_json::from_str::<SessionImportRequest>(r#"{"wipe":true}"#).is_err());
    }
}
