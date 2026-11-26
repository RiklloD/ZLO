//! Continuous configuration sync wire DTOs and `ProductSurface` descriptors.
//!
//! Declared here so `ironclaw_assistant` (`ConfigSyncService` +
//! `ConfigSyncSurface`), `ironclaw_webui` (`/api/webchat/v2/operator/config-sync/*`),
//! and the desktop client share one typed contract without coupling transport
//! to product implementation.
//!
//! Later phases (2–4, 6) extend [`ConfigSyncDomain`] and
//! [`ConfigSyncDomainPayload`] with additional variants without changing the
//! envelope or route shapes.

use std::collections::BTreeMap;
use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::descriptors::{ProductSurfaceCommandDescriptor, ProductView};
use crate::profiles::ProfileSettings;

/// Current snapshot schema version understood by this backend.
pub const CONFIG_SYNC_SCHEMA_VERSION: u32 = 1;

/// Maximum serialized snapshot size accepted by `apply` (4 MiB for Phase 1).
pub const CONFIG_SYNC_MAX_SNAPSHOT_BYTES: usize = 4 * 1024 * 1024;

/// Maximum `SYSTEM.md` payload size accepted by `prompts` (64 KiB).
pub const CONFIG_SYNC_MAX_SYSTEM_PROMPT_BYTES: usize = 64 * 1024;

/// Strongly typed sync domain identifier.
///
/// Serialized in `snake_case`. Unknown domain names are rejected at the
/// boundary before any domain writes occur.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigSyncDomain {
    /// Portable sections of `config.toml` (never `[boot]`, `[identity]`,
    /// `[storage]`, `[webui]`, `[remote]`, `[update]`).
    Config,
    /// Orchestrator settings (`orch_settings`).
    OrchestratorSettings,
    /// User-editable `SYSTEM.md` prompt.
    Prompts,
    /// Global auto-approve toggle and per-tool permission overrides
    /// (never `orch_cli_approvals`).
    Approvals,
    /// Agent profiles (`orch_profiles`, with `$HOME`/`$ZLO_HOME` path
    /// normalization).
    Profiles,
}

impl ConfigSyncDomain {
    /// All Phase 1 domains in canonical order.
    pub const ALL: &'static [Self] = &[
        Self::Config,
        Self::OrchestratorSettings,
        Self::Prompts,
        Self::Approvals,
        Self::Profiles,
    ];

    /// Canonical wire name for this domain.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::OrchestratorSettings => "orchestrator_settings",
            Self::Prompts => "prompts",
            Self::Approvals => "approvals",
            Self::Profiles => "profiles",
        }
    }

    /// Parse a canonical wire domain name.
    pub fn from_wire(raw: &str) -> Option<Self> {
        match raw {
            "config" => Some(Self::Config),
            "orchestrator_settings" => Some(Self::OrchestratorSettings),
            "prompts" => Some(Self::Prompts),
            "approvals" => Some(Self::Approvals),
            "profiles" => Some(Self::Profiles),
            _ => None,
        }
    }
}

impl fmt::Display for ConfigSyncDomain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Request body for `operator.config_sync.manifest` (empty).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSyncManifestRequest {}

/// Lightweight per-domain digest returned by `manifest`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigSyncDomainDigest {
    /// Canonical content hash formatted as `sha256:<64 lowercase hex chars>`.
    pub hash: String,
    /// Latest known update timestamp for the domain, if tracked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
    /// Number of logical items in this domain.
    pub item_count: u64,
}

/// Response for `GET /api/webchat/v2/operator/config-sync/manifest`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigSyncManifest {
    pub schema_version: u32,
    pub environment_id: String,
    pub os: String,
    pub domains: BTreeMap<ConfigSyncDomain, ConfigSyncDomainDigest>,
}

/// Request body for `POST /api/webchat/v2/operator/config-sync/export`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSyncExportRequest {
    /// Domains to export. Empty means all supported domains on this backend.
    #[serde(default)]
    pub domains: Vec<ConfigSyncDomain>,
}

/// Portable `config.toml` sections (paths normalized with `$HOME`/`$ZLO_HOME`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigDomainPayload {
    pub hash: String,
    /// Portable sections keyed by top-level TOML section name; never a
    /// machine-local one (boot, identity, storage, listeners, remote access,
    /// update).
    #[serde(default)]
    pub sections: BTreeMap<String, serde_json::Value>,
}

/// Payload for [`ConfigSyncDomain::OrchestratorSettings`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrchestratorSettingsDomainPayload {
    pub hash: String,
    /// The orchestrator's own settings document, as it stores it. Opaque
    /// here: the orchestrator owns its shape and validates it on apply.
    pub settings: serde_json::Value,
}

/// Payload for [`ConfigSyncDomain::Prompts`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptsDomainPayload {
    pub hash: String,
    /// User-editable `SYSTEM.md` contents (`None` when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_md: Option<String>,
}

/// Synced per-tool approval override state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolApprovalSyncState {
    AlwaysAllow,
    AskEachTime,
    Disabled,
}

impl ToolApprovalSyncState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AlwaysAllow => "always_allow",
            Self::AskEachTime => "ask_each_time",
            Self::Disabled => "disabled",
        }
    }
}

/// One per-tool approval override entry in [`ApprovalsDomainPayload`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolApprovalOverrideEntry {
    pub capability_id: String,
    pub state: ToolApprovalSyncState,
}

/// Payload for [`ConfigSyncDomain::Approvals`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalsDomainPayload {
    pub hash: String,
    pub global_auto_approve: bool,
    #[serde(default)]
    pub tool_overrides: Vec<ToolApprovalOverrideEntry>,
}

/// One profile row in [`ProfilesDomainPayload`].
///
/// `settings.working_folder` is normalized with `$HOME`/`$ZLO_HOME` on export
/// and expanded to the target's home/data folder on apply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileSyncItem {
    pub profile_id: String,
    pub updated_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<i64>,
    pub settings: ProfileSettings,
}

/// Payload for [`ConfigSyncDomain::Profiles`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfilesDomainPayload {
    pub hash: String,
    #[serde(default)]
    pub items: Vec<ProfileSyncItem>,
}

/// Tagged per-domain payload carried inside [`ConfigSnapshotV1`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConfigSyncDomainPayload {
    Config(ConfigDomainPayload),
    OrchestratorSettings(OrchestratorSettingsDomainPayload),
    Prompts(PromptsDomainPayload),
    Approvals(ApprovalsDomainPayload),
    Profiles(ProfilesDomainPayload),
}

impl ConfigSyncDomainPayload {
    /// The domain this payload variant belongs to.
    pub const fn domain(&self) -> ConfigSyncDomain {
        match self {
            Self::Config(_) => ConfigSyncDomain::Config,
            Self::OrchestratorSettings(_) => ConfigSyncDomain::OrchestratorSettings,
            Self::Prompts(_) => ConfigSyncDomain::Prompts,
            Self::Approvals(_) => ConfigSyncDomain::Approvals,
            Self::Profiles(_) => ConfigSyncDomain::Profiles,
        }
    }

    /// Canonical `sha256:...` digest recorded on the payload.
    pub fn hash(&self) -> &str {
        match self {
            Self::Config(p) => &p.hash,
            Self::OrchestratorSettings(p) => &p.hash,
            Self::Prompts(p) => &p.hash,
            Self::Approvals(p) => &p.hash,
            Self::Profiles(p) => &p.hash,
        }
    }
}

/// Versioned configuration snapshot produced by `export` and consumed by `apply`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSnapshotV1 {
    pub schema_version: u32,
    pub source_environment_id: String,
    pub source_os: String,
    pub generated_at: DateTime<Utc>,
    #[serde(default)]
    pub domains: BTreeMap<ConfigSyncDomain, ConfigSyncDomainPayload>,
}

/// Request body for `POST /api/webchat/v2/operator/config-sync/apply`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSyncApplyRequest {
    pub snapshot: ConfigSnapshotV1,
    /// Optional subset of domains from `snapshot.domains` to apply. When empty
    /// or omitted, every domain present in `snapshot.domains` is applied.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub domains: Vec<ConfigSyncDomain>,
}

/// Per-domain outcome status in [`ConfigSyncApplyReport`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DomainApplyStatus {
    Applied,
    SkippedUnchanged,
    SkippedWithReason,
}

impl DomainApplyStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::SkippedUnchanged => "skipped_unchanged",
            Self::SkippedWithReason => "skipped_with_reason",
        }
    }
}

/// Per-domain apply result. Never carries secret values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainApplyOutcome {
    pub status: DomainApplyStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub applied_count: u64,
    pub skipped_count: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Response for `POST /api/webchat/v2/operator/config-sync/apply`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigSyncApplyReport {
    pub schema_version: u32,
    pub target_environment_id: String,
    pub domains: BTreeMap<ConfigSyncDomain, DomainApplyOutcome>,
    /// Config sections whose on-disk values changed and require a backend
    /// restart to take effect (never secret values).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restart_required: Vec<String>,
}

/// Result of `operator.config_sync.export` across the `ProductSurface` boundary,
/// distinguishing a valid snapshot from a user-visible validation rejection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ConfigSyncExportOutcome {
    Exported(ConfigSnapshotV1),
    Rejected { field: String, reason: String },
}

/// Result of `operator.config_sync.apply` across the `ProductSurface` boundary,
/// distinguishing a completed apply report from a pre-write validation
/// rejection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ConfigSyncApplyOutcome {
    Completed(ConfigSyncApplyReport),
    Rejected { field: String, reason: String },
}

pub const CONFIG_SYNC_MANIFEST_VIEW_ID: &str = "operator.config_sync.manifest";
/// Read per-domain content digests for this backend.
pub const CONFIG_SYNC_MANIFEST_VIEW: ProductView<ConfigSyncManifestRequest, ConfigSyncManifest> =
    ProductView::unpaginated(CONFIG_SYNC_MANIFEST_VIEW_ID);

pub const CONFIG_SYNC_EXPORT_VIEW_ID: &str = "operator.config_sync.export";
/// Export requested configuration domains as a [`ConfigSnapshotV1`].
///
/// Accepts raw JSON at the surface boundary so unknown domain names produce a
/// structured [`ConfigSyncExportOutcome::Rejected`] reason.
pub const CONFIG_SYNC_EXPORT_VIEW: ProductView<serde_json::Value, ConfigSyncExportOutcome> =
    ProductView::unpaginated(CONFIG_SYNC_EXPORT_VIEW_ID);

pub const CONFIG_SYNC_APPLY_COMMAND_ID: &str = "operator.config_sync.apply";
/// Validate and atomically apply a [`ConfigSnapshotV1`] subset.
///
/// Accepts raw JSON at the surface boundary so bad schema versions, oversized
/// snapshots, and unknown domain names produce a structured
/// [`ConfigSyncApplyOutcome::Rejected`] reason before anything is written.
pub const CONFIG_SYNC_APPLY_COMMAND: ProductSurfaceCommandDescriptor<
    serde_json::Value,
    ConfigSyncApplyOutcome,
> = ProductSurfaceCommandDescriptor::new(CONFIG_SYNC_APPLY_COMMAND_ID);
