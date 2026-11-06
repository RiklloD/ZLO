//! Scenario: "no, do X instead". A gated `builtin.write_file` raises a real
//! `BlockedApproval` gate; the person rejects it WITH feedback. The run
//! resumes, the call is not re-dispatched, and the model's next request
//! carries the feedback as the rejected call's result, so it adapts (here:
//! writes to the place the person asked for) instead of stopping.
//!
//! Real path: gate fires → real `ApprovalResolver::deny` carrying the
//! feedback (no lease) → `coordinator.resume_turn` with
//! `GateResumeDisposition::Denied { feedback }` → the executor surfaces a
//! `GateDeclined` result with the feedback → the model's next call is
//! admitted on its own gate.

use super::reborn_support::group::{HarnessResult, RebornIntegrationGroup};
use super::reborn_support::reply::RebornScriptedReply;
use ironclaw_turns::TurnStatus;
use serde_json::json;

pub async fn run(g: &RebornIntegrationGroup) -> HarnessResult<()> {
    let h = g
        .thread("conv-reject-feedback")
        .script([
            RebornScriptedReply::tool_call(
                "builtin.write_file",
                json!({"path": "/workspace/rejected.txt", "content": "should not persist"}),
            ),
            RebornScriptedReply::text("ok, I will put it in drafts instead"),
        ])
        .build()
        .await?;

    let (run_id, gate_ref) = h.submit_turn_until_blocked("write the file").await?;
    h.deny_gate_with_feedback(run_id, &gate_ref, "Put it in /workspace/drafts instead.")
        .await?;
    h.wait_for_status(run_id, TurnStatus::Completed).await?;

    // Feedback never turns the denial into a dispatch.
    h.assert_workspace_file_absent("rejected.txt").await?;
    // The person's words reached the model as the rejected call's result.
    h.assert_model_saw_text("The user rejected this call: Put it in /workspace/drafts instead.")
        .await?;
    Ok(())
}

/// The same rejection through the REAL product request path
/// (`ProductResolveGateRequest` -> ingress validation and redaction ->
/// `ApprovalInteractionService` -> durable denial -> resume), with ordinary
/// credential vocabulary in the feedback and a credential value the ingress
/// must strip. Runs in the real-gate-dispatch group.
pub async fn via_product_request(g: &RebornIntegrationGroup) -> HarnessResult<()> {
    let h = g
        .thread("conv-reject-feedback-product")
        .script([
            RebornScriptedReply::tool_call(
                "builtin.write_file",
                json!({"path": "/workspace/rejected-product.txt", "content": "should not persist"}),
            ),
            RebornScriptedReply::text("ok, using the password manager entry instead"),
        ])
        .build()
        .await?;

    let (run_id, gate_ref) = h.submit_turn_until_blocked("write the file").await?;
    h.reject_gate_via_product_request(
        run_id,
        &gate_ref,
        "Use the password manager entry, not password: hunter2; write it to /workspace/drafts instead.",
    )
    .await?;
    h.wait_for_status(run_id, TurnStatus::Completed).await?;

    // The decision is a durable denial and the call never ran.
    if h.approval_status(&gate_ref).await? != ironclaw_approvals::ApprovalStatus::Denied {
        return Err("the rejection must be a durable denial".into());
    }
    h.assert_workspace_file_absent("rejected-product.txt")
        .await?;
    // The person's words — "password" included — reached the model as the
    // call's result; the credential value they pasted did not.
    h.assert_model_saw_text("The user rejected this call: Use the password manager entry")
        .await?;
    h.assert_model_saw_text("write it to /workspace/drafts instead.")
        .await?;
    h.assert_model_never_saw_text("hunter2").await?;
    // The model gateway redacts its own input too, so the persisted, replayable
    // transcript is the real evidence: it keeps the words and was written
    // from the ingress-redacted feedback, never the pasted value.
    h.assert_conversation_history_contains("Use the password manager entry")
        .await?;
    h.assert_conversation_history_lacks("hunter2").await?;
    Ok(())
}
