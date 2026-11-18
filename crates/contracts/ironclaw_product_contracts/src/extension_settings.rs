//! Package-declared user settings: descriptors, wire DTOs, and the source
//! port the product surface answers from.
//!
//! An installed extension (user-facing: a "plugin") may declare typed,
//! non-secret settings in its manifest (`[[settings]]`). This module is the
//! product boundary for listing them with the caller's current values and for
//! setting one value. The declaration grammar, value validation, and the
//! per-user store belong to `ironclaw_extension_registry::settings`; the
//! source is implemented by `ironclaw_extension_manager` and answered by
//! `ironclaw_assistant::extension_settings_page`.
//!
//! Wire (`/api/webchat/v2/extensions/{extension_id}/settings`):
//!
//! - `GET` → [`ExtensionSettingsResponse`] — `{ "settings": [ … ] }`
//! - `PUT …/{setting_id}` body [`ExtensionSettingValueBody`] → the updated
//!   [`ExtensionSettingView`], or a 4xx with a plain `reason` when the value
//!   does not fit the declaration ([`ExtensionSettingSetOutcome::Rejected`]).
//!
//! Settings never carry secrets; a declaration shaped like a credential slot
//! is refused when the manifest is parsed.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::descriptors::{ProductSurfaceCommandDescriptor, ProductView};
use crate::surface::{ProductSurfaceCaller, ProductSurfaceError};

/// Longest extension id a client may name, in bytes.
pub const MAX_EXTENSION_SETTINGS_ID_BYTES: usize = 128;

/// `GET /api/webchat/v2/extensions/{extension_id}/settings` parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionSettingsRequest {
    pub extension_id: String,
}

/// `GET /api/webchat/v2/extensions/{extension_id}/settings`.
pub const EXTENSION_SETTINGS_VIEW: ProductView<
    ExtensionSettingsRequest,
    ExtensionSettingsResponse,
> = ProductView::unpaginated("extensions.settings");

/// The closed setting vocabulary, as rendered on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtensionSettingKindView {
    Switch,
    Choice,
    Text,
    Number,
}

/// One option of a `choice` setting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionSettingOptionView {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
}

/// One declared setting with the caller's current value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionSettingView {
    pub id: String,
    pub kind: ExtensionSettingKindView,
    pub label: String,
    pub description: String,
    pub section: Option<String>,
    /// Empty unless `kind` is `choice`.
    pub options: Vec<ExtensionSettingOptionView>,
    pub default: serde_json::Value,
    /// The caller's effective value: what they set, or `default`.
    pub value: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtensionSettingsResponse {
    /// In manifest declaration order.
    pub settings: Vec<ExtensionSettingView>,
}

/// `PUT …/settings/{setting_id}` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionSettingValueBody {
    pub value: serde_json::Value,
}

/// Command input: the path segments plus the body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtensionSettingSetRequest {
    pub extension_id: String,
    pub setting_id: String,
    pub value: serde_json::Value,
}

/// Result of setting one value. A value that does not fit the declaration is
/// a user-correctable outcome with a plain reason, not a host error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ExtensionSettingSetOutcome {
    Updated { setting: ExtensionSettingView },
    Rejected { reason: String },
}

pub const EXTENSION_SETTING_SET_COMMAND_ID: &str = "extensions.settings.set";
/// `PUT /api/webchat/v2/extensions/{extension_id}/settings/{setting_id}`.
pub const EXTENSION_SETTING_SET_COMMAND: ProductSurfaceCommandDescriptor<
    ExtensionSettingSetRequest,
    ExtensionSettingSetOutcome,
> = ProductSurfaceCommandDescriptor::new(EXTENSION_SETTING_SET_COMMAND_ID);

/// Where the surface reads and writes package settings. Scope always comes
/// from the authenticated `caller`.
#[async_trait]
pub trait ExtensionSettingsSource: Send + Sync {
    /// Every declared setting of `extension_id` with the caller's value.
    /// `not_found` when the extension is not active.
    async fn list(
        &self,
        caller: &ProductSurfaceCaller,
        extension_id: &str,
    ) -> Result<Vec<ExtensionSettingView>, ProductSurfaceError>;

    /// Validate and store one value for the caller.
    async fn set(
        &self,
        caller: &ProductSurfaceCaller,
        extension_id: &str,
        setting_id: &str,
        value: serde_json::Value,
    ) -> Result<ExtensionSettingSetOutcome, ProductSurfaceError>;
}
