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
    /// The person's own skills, whole bundles (never system skills).
    Skills,
    /// Skill pins and the usage ledger. Two-way: uses merge, pins follow
    /// the source.
    SkillUsage,
    /// Local memory documents. Two-way: a document changed on one side
    /// moves to the other; one changed on both is kept twice.
    Memory,
    /// The person's own rules and skills files in the homes of the coding
    /// agent CLIs (never their logins or settings). One way; files only the
    /// receiver has are kept.
    CliHome,
}

impl ConfigSyncDomain {
    /// All Phase 1 domains in canonical order.
    pub const ALL: &'static [Self] = &[
        Self::Config,
        Self::OrchestratorSettings,
        Self::Prompts,
        Self::Approvals,
        Self::Profiles,
        Self::Skills,
        Self::SkillUsage,
        Self::Memory,
        Self::CliHome,
    ];

    /// Sent in pages of items ([`ItemizedDomainPayload`]), one domain per
    /// export, so a large domain never needs one oversized snapshot.
    pub const fn itemized(self) -> bool {
        matches!(self, Self::Skills | Self::Memory | Self::CliHome)
    }

    /// Both computers write it: after the source's changes are applied to
    /// the target, the target's are applied back with
    /// [`ConfigSyncApplyRequest::merge_back`].
    pub const fn two_way(self) -> bool {
        matches!(self, Self::SkillUsage | Self::Memory)
    }

    /// Canonical wire name for this domain.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::OrchestratorSettings => "orchestrator_settings",
            Self::Prompts => "prompts",
            Self::Approvals => "approvals",
            Self::Profiles => "profiles",
            Self::Skills => "skills",
            Self::SkillUsage => "skill_usage",
            Self::Memory => "memory",
            Self::CliHome => "cli_home",
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
            "skills" => Some(Self::Skills),
            "skill_usage" => Some(Self::SkillUsage),
            "memory" => Some(Self::Memory),
            "cli_home" => Some(Self::CliHome),
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
    /// Itemized domains: answer with item keys and hashes only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub index_only: bool,
    /// Itemized domains: what the other side already holds (item key to
    /// hash). A matching item is exported without its content.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub have: BTreeMap<String, String>,
    /// Itemized domains: continue after this item key (the last page's
    /// [`ItemizedDomainPayload::next`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    /// Itemized domains: `have` is complete only up to this item key, so
    /// the page stops there (its `next` is then this key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
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

/// One file of an item: a skill bundle's file, or a memory document's text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSyncItemFile {
    /// Path inside the item (`SKILL.md`, `scripts/run.sh`); empty for an
    /// item that is one document. For `cli_home`, whose items are single
    /// host files, it is the file's mode note: empty, or `executable`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    /// Standard base64 of the file's bytes.
    pub data_b64: String,
}

/// One item of an itemized domain: a skill (key: its name) or a memory
/// document (key: its path).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigSyncItem {
    pub key: String,
    /// Content digest; equal on two computers exactly when the item is.
    pub hash: String,
    /// `None`: the receiver already holds this content (or only an index
    /// was asked for).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<ConfigSyncItemFile>>,
}

/// A page of an itemized domain (`skills`, `memory`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemizedDomainPayload {
    /// The whole domain's digest when this page was made.
    pub hash: String,
    #[serde(default)]
    pub items: Vec<ConfigSyncItem>,
    /// More items follow: export again with `after` set to this key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    /// Items that could not be carried, each with why ("key (too large,
    /// 3000 KiB)", "key (changed while syncing)"). They stay where they
    /// are; the receiver reports them and the pass counts as unfinished.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_carried: Vec<String>,
}

/// Payload for the `skill_usage` domain: the usage document's pins and
/// per-source ledgers, opaque here (its owner validates it).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillUsageDomainPayload {
    pub hash: String,
    pub document: serde_json::Value,
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
    Skills(ItemizedDomainPayload),
    SkillUsage(SkillUsageDomainPayload),
    Memory(ItemizedDomainPayload),
    CliHome(ItemizedDomainPayload),
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
            Self::Skills(_) => ConfigSyncDomain::Skills,
            Self::SkillUsage(_) => ConfigSyncDomain::SkillUsage,
            Self::Memory(_) => ConfigSyncDomain::Memory,
            Self::CliHome(_) => ConfigSyncDomain::CliHome,
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
            Self::Skills(p) | Self::Memory(p) | Self::CliHome(p) => &p.hash,
            Self::SkillUsage(p) => &p.hash,
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
    /// The return leg of a two-way domain: only what merges is taken (uses,
    /// documents the receiver did not change); nothing the receiver owns is
    /// replaced.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub merge_back: bool,
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
