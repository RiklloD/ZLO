//! Terminal session import: coding-agent CLI sessions the person ran outside
//! the app, brought in as ordinary threads on the matching CLI runtime (keyed
//! by the CLI's own session id) that land in the inbox's Settled shelf. A
//! source is an opaque id the backend names (with a display label); the inbox
//! row carries it as [`crate::inbox::InboxRow::imported`]. A run is a
//! background job: the status answers `running` until it ends.
//!
//! Projects: [`SESSION_IMPORT_PROJECTS_VIEW`] lists the folders the
//! sessions ran in (first-run setup picks from it), and
//! [`SessionImportSettings::projects`] limits new imports to some of them.

use std::collections::{BTreeMap, BTreeSet};

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
    /// Only sessions started in these folders become new threads; `None`
    /// imports every folder. Threads imported before keep growing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projects: Option<BTreeSet<String>>,
}

impl Default for SessionImportSettings {
    fn default() -> Self {
        Self {
            sources: BTreeMap::new(),
            background: true,
            projects: None,
        }
    }
}

impl SessionImportSettings {
    pub fn enabled(&self, source: &str) -> bool {
        self.sources.get(source).copied().unwrap_or(true)
    }

    /// A new session started in `folder` may be imported. A session whose
    /// folder is unknown is imported only when every folder is.
    pub fn project_allowed(&self, folder: Option<&str>) -> bool {
        match (&self.projects, folder) {
            (None, _) => true,
            (Some(only), Some(folder)) => only.contains(folder),
            (Some(_), None) => false,
        }
    }
}

/// Which folders new imports come from (a request's `projects`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectSelection {
    /// Every folder (the default).
    All,
    /// These folders only; empty imports nothing new.
    Only(BTreeSet<String>),
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
    /// Left for the next run: this run reached its time or size limit.
    pub deferred: u32,
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
    /// This request started the run that is `running` (a command's answer
    /// only). `false` with `running`: another run was going on, which read
    /// the settings before this request changed them.
    #[serde(default)]
    pub started: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_trigger: Option<ImportTrigger>,
    /// The last run failed before it finished (sanitized).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// The folders new imports are limited to; `None`: every folder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projects: Option<BTreeSet<String>>,
}

/// A folder's git checkout, for grouping folders of one repository.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionImportGit {
    /// `owner/name` from the `origin` remote.
    pub repository: Option<String>,
    /// The remote without scheme, user and `.git` (`github.com/owner/name`):
    /// clones of one repository share it.
    pub remote_key: Option<String>,
}

/// One folder terminal sessions ran in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionImportProject {
    /// The folder, as the sessions recorded it.
    pub path: String,
    /// Its last path component.
    pub title: String,
    /// Source ids with sessions here, in [`SessionImportStatus::sources`] order.
    pub sources: Vec<String>,
    /// Sessions found here (an estimate: empty ones are counted too).
    pub thread_count: u32,
    /// Unix ms of the newest session file's last write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_active_at: Option<i64>,
    /// `None`: not a git checkout (or no longer on disk).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<SessionImportGit>,
    /// Already imported from here before.
    #[serde(default)]
    pub imported: bool,
}

/// Output of [`SESSION_IMPORT_PROJECTS_VIEW`], newest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionImportProjects {
    pub projects: Vec<SessionImportProject>,
    /// The scan hit its limit: some folders may be missing.
    pub truncated: bool,
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
    pub projects: Option<ProjectSelection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<ImportTrigger>,
}

/// Output: [`SessionImportStatus`].
pub const SESSION_IMPORT_VIEW: ProductView<serde_json::Value, serde_json::Value> =
    ProductView::unpaginated("session_import.status");

/// Output: [`SessionImportProjects`]. Reads only the start of each session
/// file; nothing is imported.
pub const SESSION_IMPORT_PROJECTS_VIEW: ProductView<serde_json::Value, serde_json::Value> =
    ProductView::unpaginated("session_import.projects");

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

    #[test]
    fn project_selection_limits_new_imports() {
        let r: SessionImportRequest =
            serde_json::from_str(r#"{"projects":{"only":["/work/app"]}}"#).unwrap();
        let Some(ProjectSelection::Only(only)) = r.projects else {
            panic!("expected a folder list");
        };
        let s = SessionImportSettings {
            projects: Some(only),
            ..SessionImportSettings::default()
        };
        assert!(s.project_allowed(Some("/work/app")));
        assert!(!s.project_allowed(Some("/work/other")));
        assert!(!s.project_allowed(None), "unknown folder, limited list");
        assert!(SessionImportSettings::default().project_allowed(None));
        let all: SessionImportRequest = serde_json::from_str(r#"{"projects":"all"}"#).unwrap();
        assert_eq!(all.projects, Some(ProjectSelection::All));
        // Stored settings from before the field still read.
        let old: SessionImportSettings = serde_json::from_str(r#"{"background":false}"#).unwrap();
        assert_eq!(old.projects, None);
    }
}
