//! Profile operation descriptors and wire DTOs (stage 3 profiles).
//!
//! A profile is the unit a thread belongs to for its whole life: its own
//! SOUL.md, capability allowlist, default provider and model, working folder
//! and Supermemory container. Skills, secrets, MCP server definitions and CLI
//! logins stay shared; a profile only hides or enables a subset of them.
//!
//! Declared here so the WebUI routes and the desktop client share one shape;
//! records live in `ironclaw_orchestrator`, behavior in `ironclaw_assistant`'s
//! orchestration decorator.

use serde::{Deserialize, Serialize};

use crate::descriptors::{ProductSurfaceCommandDescriptor, ProductView};

/// Id of the profile every thread without one belongs to.
pub const DEFAULT_PROFILE_ID: &str = "default";

/// Supermemory settings carried by a profile. Stored only; the Supermemory
/// integration reads them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileSupermemory {
    /// Primary container: recalled every turn, new facts are stored here.
    pub container: String,
    /// Other containers this profile may search.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_containers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// The editable part of a profile.
/// Flattened into the request bodies, so it cannot `deny_unknown_fields`
/// (serde does not support the pair); unknown keys are ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileSettings {
    pub display_name: String,
    /// SOUL.md text, added to the system prompt of every turn on the
    /// profile's threads. Empty means none.
    #[serde(default)]
    pub soul: String,
    /// Capability ids the model may see (`extension.capability`). `None`
    /// shows everything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_capabilities: Option<Vec<String>>,
    /// Shared skills this profile hides. Empty shows all.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hidden_skills: Vec<String>,
    /// Shared MCP server definitions this profile enables. `None` enables all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_mcp_servers: Option<Vec<String>>,
    /// Provider new threads start on. Saving rejects one the catalog does
    /// not list (`unknown_provider`), and a model it lists none of
    /// (`unknown_model`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
    /// Model for turns on threads that did not pick one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// Absolute working folder. `None` uses `~/.ironclaw/profiles/<id>/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_folder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supermemory: Option<ProfileSupermemory>,
    /// Whether gated tool calls on this profile's threads run without asking.
    /// `None` follows the global auto-approve setting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_mode: Option<ApprovalMode>,
}

/// Per-profile approval mode. Hard safety floors (actions the policy always
/// gates or denies) apply in both modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    /// Approval-eligible tool calls run without an approval card.
    FullAccess,
    /// Tool calls that need approval open an approval card.
    AskFirst,
}

impl ApprovalMode {
    /// Whether eligible tool calls are auto-approved: the profile's mode when
    /// set, otherwise the global setting.
    pub fn resolve_auto_approve(mode: Option<Self>, global_auto_approve: bool) -> bool {
        match mode {
            Some(Self::FullAccess) => true,
            Some(Self::AskFirst) => false,
            None => global_auto_approve,
        }
    }
}

impl ProfileSettings {
    /// [`ApprovalMode::resolve_auto_approve`] for this profile.
    pub fn auto_approve(&self, global_auto_approve: bool) -> bool {
        ApprovalMode::resolve_auto_approve(self.approval_mode, global_auto_approve)
    }
}

/// One profile as the picker and the Profiles page see it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileInfo {
    pub profile_id: String,
    #[serde(flatten)]
    pub settings: ProfileSettings,
    pub is_default: bool,
    /// The folder CLI agents run in (configured or the default one).
    pub effective_folder: String,
    /// Current branch when the folder is a git repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    pub thread_count: u64,
    /// Unix ms of the latest user activity on one of its threads.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Profiles ordered by last use (most recent first, never-used last by name).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileListResponse {
    pub profiles: Vec<ProfileInfo>,
    /// The profile the picker highlights.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_profile_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileRequest {
    pub profile_id: String,
}

/// Create (new id) or update (existing id) one profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileWriteRequest {
    pub profile_id: String,
    #[serde(flatten)]
    pub settings: ProfileSettings,
}

/// Create a thread that belongs to `profile_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileThreadCreateRequest {
    pub profile_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_action_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileThreadCreateResponse {
    pub thread_id: String,
    pub profile_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileImportSkipped {
    pub source: String,
    pub reason: String,
}

/// Result of the one-time Hermes import. Re-running it imports nothing new.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HermesImportResponse {
    pub imported: Vec<String>,
    pub skipped: Vec<ProfileImportSkipped>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HermesImportRequest {}

// Outputs are JSON documents: the success shape named on each descriptor, or
// the orchestrator's correctable error document `{"ok": false, "error",
// "message"}` (HTTP 200), like `crate::orchestration`.

/// Output: [`ProfileListResponse`].
pub const PROFILES_VIEW: ProductView<serde_json::Value, serde_json::Value> =
    ProductView::unpaginated("profiles.list");
/// Output: [`ProfileInfo`].
pub const PROFILE_VIEW: ProductView<ProfileRequest, serde_json::Value> =
    ProductView::unpaginated("profiles.get");

/// Output: [`ProfileInfo`].
pub const PROFILE_CREATE_COMMAND_ID: &str = "profiles.create";
pub const PROFILE_CREATE_COMMAND: ProductSurfaceCommandDescriptor<
    ProfileWriteRequest,
    serde_json::Value,
> = ProductSurfaceCommandDescriptor::new(PROFILE_CREATE_COMMAND_ID);

/// Output: [`ProfileInfo`].
pub const PROFILE_UPDATE_COMMAND_ID: &str = "profiles.update";
pub const PROFILE_UPDATE_COMMAND: ProductSurfaceCommandDescriptor<
    ProfileWriteRequest,
    serde_json::Value,
> = ProductSurfaceCommandDescriptor::new(PROFILE_UPDATE_COMMAND_ID);

/// Archive a profile: it leaves the list, its threads keep their profile.
/// Output: `{"ok": true}`.
pub const PROFILE_DELETE_COMMAND_ID: &str = "profiles.delete";
pub const PROFILE_DELETE_COMMAND: ProductSurfaceCommandDescriptor<
    ProfileRequest,
    serde_json::Value,
> = ProductSurfaceCommandDescriptor::new(PROFILE_DELETE_COMMAND_ID);

/// Output: [`ProfileThreadCreateResponse`].
pub const PROFILE_THREAD_CREATE_COMMAND_ID: &str = "profiles.thread_create";
pub const PROFILE_THREAD_CREATE_COMMAND: ProductSurfaceCommandDescriptor<
    ProfileThreadCreateRequest,
    serde_json::Value,
> = ProductSurfaceCommandDescriptor::new(PROFILE_THREAD_CREATE_COMMAND_ID);

/// Output: [`HermesImportResponse`].
pub const PROFILE_IMPORT_HERMES_COMMAND_ID: &str = "profiles.import_hermes";
pub const PROFILE_IMPORT_HERMES_COMMAND: ProductSurfaceCommandDescriptor<
    HermesImportRequest,
    serde_json::Value,
> = ProductSurfaceCommandDescriptor::new(PROFILE_IMPORT_HERMES_COMMAND_ID);
