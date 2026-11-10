//! Plan domain tools profile (`plan_tools`) — the native agent loop's
//! checklist, `builtin.plan_update`, alone on the surface. See
//! `harness/options.rs` for the `ToolsProfile` pattern.

use ironclaw_host_api::{capability::EffectKind, ids::CapabilityId, mount::MountView};
use ironclaw_host_runtime::PLAN_UPDATE_CAPABILITY_ID;

use super::super::options::{HostRuntimeHarnessOptions, ToolsProfile};
use super::super::{HarnessResult, HostRuntimeCapabilityHarness};

/// Auto-approve stays OFF: the checklist declares only `DispatchCapability`
/// and must run without a gate even then.
pub(crate) fn plan_tools_profile() -> HarnessResult<ToolsProfile> {
    Ok(ToolsProfile {
        capability_ids: vec![CapabilityId::new(PLAN_UPDATE_CAPABILITY_ID)?],
        effect_kinds: vec![EffectKind::DispatchCapability],
        options: HostRuntimeHarnessOptions::new(MountView::default(), None),
        auto_approve_default: Some(false),
        ..ToolsProfile::new("reborn-e2e-plan-tools", "reborn-e2e-plan-user")?
    })
}

pub(crate) async fn plan_tools() -> HarnessResult<HostRuntimeCapabilityHarness> {
    plan_tools_profile()?.build().await
}
