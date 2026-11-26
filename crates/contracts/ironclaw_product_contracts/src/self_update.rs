//! "Update all machines": the operator routes a client uses to see whether
//! one installation is behind its source checkout and to start its
//! self-update (`docs/internal/plans/2026-11-08-zlo-remote-environments.md`,
//! "Self-update").
//!
//! Two consumers share these shapes: the WebUI transport that serves them
//! and the ZLO desktop client that calls them. Everything the update runs
//! (repository, branch, build and install steps) comes from the backend's
//! local `[update]` config; no request field names a command, path or
//! branch, so no route takes a body at all.
//!
//! An update runs in one go ([`OPERATOR_UPDATE_PATH`]) or in two steps, the
//! way a desktop app downloads an update and installs it on restart:
//! [`OPERATOR_UPDATE_DOWNLOAD_PATH`] fetches and builds the new version into
//! a staging folder while the running one keeps going, and
//! [`OPERATOR_UPDATE_INSTALL_PATH`] later swaps the staged build in and
//! restarts. [`UpdateStatus::staged`] says what is downloaded.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// `GET` reads [`UpdateStatus`]; `POST` starts an update (`202`).
pub const OPERATOR_UPDATE_PATH: &str = "/api/operator/update";
/// `POST` fetches the source and answers the refreshed [`UpdateStatus`].
pub const OPERATOR_UPDATE_CHECK_PATH: &str = "/api/operator/update/check";

/// `POST` turns self-update on from the checkout this backend was built
/// from (no body: the path comes from the binary, never from a request) and
/// answers the refreshed [`UpdateStatus`].
pub const OPERATOR_UPDATE_ENABLE_PATH: &str = "/api/operator/update/enable";
/// `POST` turns self-update off (no body; already off: a no-op) and answers
/// the refreshed [`UpdateStatus`]. The checkout stays recorded, so enable
/// turns the same one back on.
pub const OPERATOR_UPDATE_DISABLE_PATH: &str = "/api/operator/update/disable";
/// `POST` starts a download job: fetch, fast-forward and build the new
/// version into the backend's staging folder; nothing running changes
/// (`202`).
pub const OPERATOR_UPDATE_DOWNLOAD_PATH: &str = "/api/operator/update/download";
/// `POST` starts an install job: swap the downloaded build in and restart
/// (`202`; `409 nothing_staged` when nothing is downloaded).
pub const OPERATOR_UPDATE_INSTALL_PATH: &str = "/api/operator/update/install";
/// Most commit subjects an [`AvailableUpdate`] carries.
pub const MAX_UPDATE_SUBJECTS: usize = 20;
/// Most characters of one commit subject.
pub const MAX_UPDATE_SUBJECT_CHARS: usize = 200;
/// Most lines of [`UpdateRun::log_tail`].
pub const MAX_UPDATE_LOG_TAIL_LINES: usize = 40;

/// How this installation updates itself (`[update] method`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateMethod {
    /// Fast-forward a local source checkout, rebuild, reinstall.
    Source,
    /// Not set up: updates are refused.
    #[default]
    None,
}

/// The build that is running now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildInfo {
    pub version: String,
    /// Full commit hash the binary was built from, when the build knew it.
    pub commit: Option<String>,
}

/// What the last check found upstream of the running build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailableUpdate {
    /// Full hash of the upstream branch tip.
    pub commit: String,
    /// Commits between the running build and `commit`.
    pub behind: u32,
    /// Newest first, at most [`MAX_UPDATE_SUBJECTS`].
    pub subjects: Vec<String>,
    pub checked_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateRunState {
    /// No update has run here.
    #[default]
    Idle,
    Running,
    Succeeded,
    Failed,
}

/// What an update job does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateJob {
    /// Fetch, build, install and restart in one go. Older backends only
    /// run this, so a record without the field reads as it.
    #[default]
    Update,
    /// Fetch and build into the staging folder; install nothing.
    Download,
    /// Install the downloaded build and restart.
    Install,
}

/// A build that is downloaded and waits to be installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedUpdate {
    /// Full hash of the commit the staged build was built from.
    pub commit: String,
    pub staged_at: DateTime<Utc>,
}

/// The last (or current) update job. Kept on disk by the backend, so it
/// reads the same after the backend restarted itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateRun {
    pub state: UpdateRunState,
    /// What the job does. Absent from older backends (reads as
    /// [`UpdateJob::Update`]).
    #[serde(default)]
    pub job: UpdateJob,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    /// One line: the step in progress, or the outcome.
    pub message: Option<String>,
    /// The end of the job's log, at most [`MAX_UPDATE_LOG_TAIL_LINES`] lines.
    #[serde(default)]
    pub log_tail: String,
}

/// `GET /api/operator/update`, and the body of the check and start replies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateStatus {
    pub method: UpdateMethod,
    pub current: BuildInfo,
    /// `None` until a check ran, or when the check found nothing newer.
    pub available: Option<AvailableUpdate>,
    pub last_run: UpdateRun,
    /// Self-update is off, and this backend knows the source checkout it was
    /// built from: [`OPERATOR_UPDATE_ENABLE_PATH`] can turn it on. Absent
    /// from older backends (reads as `false`).
    #[serde(default)]
    pub can_enable: bool,
    /// A downloaded build waiting for [`OPERATOR_UPDATE_INSTALL_PATH`].
    /// Absent from older backends, which cannot download.
    #[serde(default)]
    pub staged: Option<StagedUpdate>,
}

/// The `error` code of a refused update call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateErrorCode {
    /// `[update] method` is `none` (or the section is missing).
    NotConfigured,
    /// Another update holds the lock.
    UpdateRunning,
    /// The fetch or the checkout's state refused the check; `message` says why.
    CheckFailed,
    /// Self-update could not be turned on from here: this backend was not
    /// built from a checkout that is still there; `message` says what to do.
    NoCheckout,
    /// An install was asked for, but nothing is downloaded.
    NothingStaged,
}

impl UpdateErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotConfigured => "not_configured",
            Self::UpdateRunning => "update_running",
            Self::CheckFailed => "check_failed",
            Self::NoCheckout => "no_checkout",
            Self::NothingStaged => "nothing_staged",
        }
    }
}

/// Bound one commit subject to [`MAX_UPDATE_SUBJECT_CHARS`] characters
/// (by characters, never bytes), dropping control characters.
pub fn bound_subject(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_control())
        .take(MAX_UPDATE_SUBJECT_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_matches_the_wire_contract() {
        let json = serde_json::json!({
            "method": "source",
            "current": { "version": "1.2.0", "commit": "aaaa" },
            "available": {
                "commit": "bbbb", "behind": 2,
                "subjects": ["Fix it", "Add it"],
                "checked_at": "2026-11-08T21:00:00Z"
            },
            "last_run": {
                "state": "failed",
                "job": "download",
                "started_at": "2026-11-08T21:01:00Z",
                "finished_at": "2026-11-08T21:02:00Z",
                "message": "The checkout has local changes.",
                "log_tail": "git status\n M a.rs"
            },
            "can_enable": false,
            "staged": { "commit": "bbbb", "staged_at": "2026-11-08T21:02:00Z" }
        });
        let parsed: UpdateStatus = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(parsed.last_run.state, UpdateRunState::Failed);
        assert_eq!(parsed.last_run.job, UpdateJob::Download);
        assert_eq!(
            parsed.staged.as_ref().map(|s| s.commit.as_str()),
            Some("bbbb")
        );
        assert_eq!(serde_json::to_value(&parsed).unwrap(), json);
        assert_eq!(UpdateErrorCode::UpdateRunning.as_str(), "update_running");
        assert_eq!(
            serde_json::to_value(UpdateErrorCode::NoCheckout).unwrap(),
            UpdateErrorCode::NoCheckout.as_str()
        );
        // An older backend's status has no `can_enable`.
        let mut older = json;
        older.as_object_mut().unwrap().remove("can_enable");
        older.as_object_mut().unwrap().remove("staged");
        older["last_run"].as_object_mut().unwrap().remove("job");
        let parsed: UpdateStatus = serde_json::from_value(older).unwrap();
        assert!(!parsed.can_enable);
        assert_eq!(parsed.staged, None, "an older backend downloads nothing");
        assert_eq!(parsed.last_run.job, UpdateJob::Update);
        assert_eq!(UpdateErrorCode::NothingStaged.as_str(), "nothing_staged");
        assert_eq!(
            serde_json::to_value(UpdateErrorCode::NotConfigured).unwrap(),
            UpdateErrorCode::NotConfigured.as_str()
        );
    }

    #[test]
    fn subjects_are_bounded_by_characters() {
        let long = "é".repeat(MAX_UPDATE_SUBJECT_CHARS + 5);
        assert_eq!(
            bound_subject(&long).chars().count(),
            MAX_UPDATE_SUBJECT_CHARS
        );
        assert_eq!(bound_subject("a\u{1b}[31mb"), "a[31mb");
    }
}
