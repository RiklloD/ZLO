//! `[[ui.slots]]` — plugin UI slot declarations (v3 manifests).
//!
//! The slot vocabulary and the node-tree contract live in
//! `ironclaw_extension_contracts::plugin_ui`; this module owns the two
//! manifest-wide rules only the parser can check, because they need the
//! extension's own `[[tools]]`:
//!
//! - `render` must name a tool this manifest declares; and
//! - that tool is a **render tool**: the host invokes it with `product`
//!   origin on every slot render, with no approval gate in the way, so it must
//!   be explicitly allowed for `product` origin (`consent_sufficient` or
//!   `ungated`) and must be side-effect-free — no write effects, no network,
//!   no secrets, no credentials. A plugin that needs fresh remote data renders
//!   what an ordinary (gated) tool already fetched; the panel never becomes an
//!   ungated egress path.

use std::collections::BTreeSet;

use ironclaw_extension_contracts::plugin_ui::{PLUGIN_UI_MAX_SLOTS, PluginUiSlotDescriptor};
use ironclaw_host_api::capability::{CapabilityDescriptor, EffectKind, OriginGatePolicy};
use serde::Deserialize;

use crate::v2::CapabilityDeclV2;

/// Raw `[ui]` table.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawUiV3 {
    #[serde(default)]
    pub(crate) slots: Vec<PluginUiSlotDescriptor>,
}

/// Validate the declared slots against the manifest's normalized tools.
/// Returns a path-qualified reason on failure.
pub(crate) fn validate_ui_slots(
    slots: &[PluginUiSlotDescriptor],
    tools: &[CapabilityDeclV2],
) -> Result<(), String> {
    if slots.len() > PLUGIN_UI_MAX_SLOTS {
        return Err(format!(
            "[[ui.slots]] declares {} slots; at most {PLUGIN_UI_MAX_SLOTS} are allowed",
            slots.len()
        ));
    }
    let mut seen = BTreeSet::new();
    for slot in slots {
        slot.validate().map_err(|error| error.to_string())?;
        if !seen.insert(slot.id.as_str()) {
            return Err(format!("[[ui.slots]] id `{}` is declared twice", slot.id));
        }
        let tool = tools
            .iter()
            .find(|tool| tool.id == slot.render)
            .ok_or_else(|| {
                format!(
                    "ui slot `{}` render tool `{}` is not one of this extension's [[tools]]",
                    slot.id, slot.render
                )
            })?;
        validate_render_tool(slot, tool)?;
    }
    Ok(())
}

fn validate_render_tool(
    slot: &PluginUiSlotDescriptor,
    tool: &CapabilityDeclV2,
) -> Result<(), String> {
    let product = tool
        .origin_gate_matrix
        .as_ref()
        .map(|matrix| matrix.product)
        .unwrap_or_default();
    if !matches!(
        product,
        OriginGatePolicy::ConsentSufficient | OriginGatePolicy::Ungated
    ) {
        return Err(format!(
            "ui slot `{}` render tool `{}` must allow product origin \
             (origin_gate_matrix.product = \"consent_sufficient\" or \"ungated\")",
            slot.id, tool.id
        ));
    }
    if let Some(effect) = tool.effects.iter().find(|effect| !is_render_safe(**effect)) {
        return Err(format!(
            "ui slot `{}` render tool `{}` must be side-effect-free; it declares the `{}` effect",
            slot.id,
            tool.id,
            effect_name(*effect)
        ));
    }
    if !tool.runtime_credentials.is_empty() || !tool.network_targets.is_empty() {
        return Err(format!(
            "ui slot `{}` render tool `{}` must not declare credentials or network targets",
            slot.id, tool.id
        ));
    }
    Ok(())
}

/// The render-tool rule against the live descriptor the host is about to
/// invoke. The manifest check above runs at admission; this one runs at the
/// call, so a descriptor that reached the registry by any other path (or was
/// changed after admission) still cannot run as an ungated panel render.
pub fn check_render_descriptor(descriptor: &CapabilityDescriptor) -> Result<(), String> {
    let product = descriptor
        .origin_gate_matrix
        .as_ref()
        .map(|matrix| matrix.product)
        .unwrap_or_default();
    if !matches!(
        product,
        OriginGatePolicy::ConsentSufficient | OriginGatePolicy::Ungated
    ) {
        return Err(format!(
            "render tool `{}` does not allow product origin",
            descriptor.id
        ));
    }
    if let Some(effect) = descriptor
        .effects
        .iter()
        .find(|effect| !is_render_safe(**effect))
    {
        return Err(format!(
            "render tool `{}` declares the `{}` effect",
            descriptor.id,
            effect_name(*effect)
        ));
    }
    if !descriptor.runtime_credentials.is_empty() || !descriptor.network_targets.is_empty() {
        return Err(format!(
            "render tool `{}` carries credentials or network targets",
            descriptor.id
        ));
    }
    Ok(())
}

/// Effects a render tool may carry: reading its own package files, and the
/// dispatch marker every normalized tool gets.
fn is_render_safe(effect: EffectKind) -> bool {
    matches!(
        effect,
        EffectKind::ReadFilesystem | EffectKind::DispatchCapability
    )
}

fn effect_name(effect: EffectKind) -> String {
    serde_json::to_value(effect)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string()) // silent-ok: display-only effect name in a parse error
}
