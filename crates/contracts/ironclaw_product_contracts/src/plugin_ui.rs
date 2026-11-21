//! Plugin UI slots over the product surface.
//!
//! Two read-only views: the slots the caller may see, and one slot rendered
//! (optionally after a UI event). Rendering is side-effect-free by manifest
//! rule — a render tool declares no write, network, or secret effect — so
//! both are views even when an event is carried. The tree vocabulary is
//! `ironclaw_extension_contracts::plugin_ui`.
//!
//! [`PluginUiHost`] is implemented by composition, which alone can see the
//! active extension set, the installation owners, and the host runtime; the
//! product surface (`ironclaw_assistant::plugin_ui`) answers the views,
//! validates every tree, and bounds render time.

use async_trait::async_trait;
use ironclaw_extension_contracts::plugin_ui::{
    PluginUiAction, PluginUiNode, PluginUiSlotDescriptor, PluginUiSlotId, PluginUiSlotKind,
};
use ironclaw_host_api::ids::{ExtensionId, ThreadId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::descriptors::ProductView;
use crate::surface::{ProductSurfaceCaller, ProductSurfaceError};

/// `GET /api/webchat/v2/plugin-ui/slots`.
pub const PLUGIN_UI_SLOTS_VIEW: ProductView<PluginUiSlotsRequest, PluginUiSlotsResponse> =
    ProductView::unpaginated("plugin_ui.slots");

/// `POST /api/webchat/v2/plugin-ui/render` and `/event`.
pub const PLUGIN_UI_RENDER_VIEW: ProductView<PluginUiRenderRequest, PluginUiRenderResponse> =
    ProductView::unpaginated("plugin_ui.render");

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginUiSlotsRequest {}

/// One slot the caller can render.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginUiSlotInfo {
    pub extension_id: ExtensionId,
    pub slot_id: PluginUiSlotId,
    pub slot: PluginUiSlotKind,
    pub title: String,
    /// Package-relative icon asset path, when the slot declares one.
    pub icon: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginUiSlotsResponse {
    pub slots: Vec<PluginUiSlotInfo>,
}

/// Where the slot is being shown.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginUiRenderContext {
    #[serde(default)]
    pub thread_id: Option<ThreadId>,
}

/// A user activation of a node's `action`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginUiEvent {
    pub action: PluginUiAction,
    #[serde(default)]
    pub payload: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginUiRenderRequest {
    pub extension_id: ExtensionId,
    pub slot_id: PluginUiSlotId,
    #[serde(default)]
    pub context: PluginUiRenderContext,
    #[serde(default)]
    pub event: Option<PluginUiEvent>,
}

/// `{ "tree": node }` or `{ "error": "<plain reason>" }`. A render error is a
/// user-visible outcome of a successful request, not a transport error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginUiRenderResponse {
    Tree(PluginUiNode),
    Error(String),
}

/// One slot the caller may see, with the extension that declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginUiSlotBinding {
    pub extension_id: ExtensionId,
    pub descriptor: PluginUiSlotDescriptor,
}

/// What one render-tool invocation produced.
#[derive(Debug, Clone, PartialEq)]
pub enum PluginUiRenderOutcome {
    /// The tool completed; its raw output, not yet validated as a tree.
    Output(Value),
    /// The capability path refused or failed the call; a plain reason safe to
    /// show the user.
    Refused(String),
}

/// Host side of plugin UI: which slots a caller may see, and one render-tool
/// call through the mediated capability path with product origin.
#[async_trait]
pub trait PluginUiHost: Send + Sync {
    /// Slots of active extensions visible to `caller`.
    async fn list_slots(
        &self,
        caller: &ProductSurfaceCaller,
    ) -> Result<Vec<PluginUiSlotBinding>, ProductSurfaceError>;

    /// Invoke `binding`'s render tool with `input`. Never persists a result.
    async fn invoke_render(
        &self,
        caller: &ProductSurfaceCaller,
        binding: &PluginUiSlotBinding,
        input: Value,
    ) -> Result<PluginUiRenderOutcome, ProductSurfaceError>;
}
