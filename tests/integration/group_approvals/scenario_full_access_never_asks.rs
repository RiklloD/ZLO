//! "Full access" asks for nothing, `ask_each_time` included, while a per-tool
//! `disabled` still refuses (the gate's step 2b in
//! `ironclaw_approvals::profile_gate`). Full access here is the global
//! auto-approve switch ON for a thread whose profile sets no approval mode —
//! the production default.
//!
//! The rest of this group pins "Ask first", where the same `ask_each_time`
//! override parks the run (`scenario_ask_each_time_resumes_once.rs`); this
//! scenario is the other side of that switch, at the same tier. It runs in
//! its own group because it turns auto-approve on.

use super::reborn_support::group::{HarnessResult, RebornIntegrationGroup};
use super::reborn_support::reply::RebornScriptedReply;
use ironclaw_approvals::CapabilityPermissionOverride;
use ironclaw_host_api::ids::CapabilityId;
use ironclaw_turns::TurnStatus;
use serde_json::json;

pub async fn run(g: &RebornIntegrationGroup) -> HarnessResult<()> {
    let h = g
        .thread("conv-full-access")
        .script([
            RebornScriptedReply::tool_call(
                "builtin.write_file",
                json!({"path": "/workspace/full-access.txt", "content": "full access payload"}),
            ),
            RebornScriptedReply::text("written without asking"),
            RebornScriptedReply::tool_call(
                "builtin.read_file",
                json!({"path": "/workspace/full-access.txt"}),
            ),
            RebornScriptedReply::text("the read was refused"),
        ])
        .build()
        .await?;

    // Full access: the global switch on, before the first dispatch.
    h.enable_auto_approve().await?;
    let capabilities = g
        .capability_harness()
        .ok_or("live_approvals always uses HostRuntime")?;
    capabilities
        .set_tool_permission_override_for_test(
            &CapabilityId::new("builtin.write_file")?,
            h.binding.tenant_id.clone(),
            h.binding.actor_user_id.clone(),
            CapabilityPermissionOverride::AskEachTime,
        )
        .await?;
    capabilities
        .set_tool_permission_override_for_test(
            &CapabilityId::new("builtin.read_file")?,
            h.binding.tenant_id.clone(),
            h.binding.actor_user_id.clone(),
            CapabilityPermissionOverride::Disabled,
        )
        .await?;

    // The ask-each-time write runs without parking: the run goes straight to
    // Completed (a parked run never gets there) and the file is on disk.
    let write_run = h.submit_turn_async("write the full-access file").await?;
    h.wait_for_status(write_run, TurnStatus::Completed).await?;
    h.assert_workspace_file_contains("full-access.txt", "full access payload")
        .await?;

    // The disabled read is still refused: `disabled` keeps it off the run's
    // surface, so the host rejects the call before dispatch and tells the
    // model nothing ran.
    h.submit_turn("read the full-access file").await?;
    h.assert_tool_not_invoked("builtin.read_file").await?;
    h.assert_model_saw_text("None of this response's tool calls were executed")
        .await?;
    Ok(())
}
