//! Reborn integration test — mid-run queued-message steering (E-GATEWAY seam).
//!
//! Proves the queued-steering journey end-to-end through the real stack: a
//! message submitted while the thread's run is mid-model-call is accepted as
//! `DeferredBusy` and stored `Queued` (not rejected), the running loop drains
//! it as steering input, the transcript row flips `Queued` → `Submitted`, and
//! — the load-bearing assertion — the steering text reaches a subsequent
//! model request through the real prompt rebuild, shaping the final reply.
//!
//! The parked model call doubles as the end-of-turn race: the steering input
//! arrives while the run's FINAL scripted model call is in flight, so it is
//! only observable at the reply-only exit boundary. Without the follow-up
//! drain consuming `Steering` inputs (and without the drain-time ack that
//! makes the row model-visible before the next prompt build), the run would
//! complete with the message stranded or silently unseen.

use crate::reborn_support;

use std::time::Duration;

use ironclaw_product_contracts::inbound::ProductInboundAck;
use ironclaw_threads::MessageStatus;
use ironclaw_turns::TurnStatus;
use reborn_support::builder::RebornIntegrationHarness;
use reborn_support::reply::RebornScriptedReply;
use reborn_support::scripted_provider::ParkingModelGate;

/// The full mid-run steering journey:
///
/// 1. Turn A parks inside its (final) model call.
/// 2. Message B submitted on the busy thread → `DeferredBusy` ack, transcript
///    row `Queued` and bound to run A — never `RejectedBusy`.
/// 3. Release the park: run A's reply-only exit drains B as steering input,
///    forces one more model iteration, and completes.
/// 4. B's text appears in a model request (the real prompt rebuild carried
///    it), the final reply is the steered script entry, and B's transcript
///    row is `Submitted`, still bound to run A.
#[tokio::test]
async fn queued_steering_message_reaches_the_model_mid_run() {
    let gate = ParkingModelGate::new();
    let harness = RebornIntegrationHarness::test_default()
        .park_model(gate.clone())
        .script([
            RebornScriptedReply::text("first reply before steering"),
            RebornScriptedReply::text("steered reply"),
        ])
        .build()
        .await
        .expect("harness builds");

    let run_id = harness
        .submit_turn_async("start a long task")
        .await
        .expect("turn A submitted");
    tokio::time::timeout(Duration::from_secs(10), gate.wait_until_parked())
        .await
        .expect("model call parks before the timeout");

    // Submit B while A is mid-model-call: accepted and queued as steering.
    let ack = harness
        .submit_turn_ack("steer the task toward blue")
        .await
        .expect("busy submit does not error");
    let ProductInboundAck::DeferredBusy { active_run_id, .. } = ack else {
        panic!("expected DeferredBusy while the thread has an active run, got {ack:?}");
    };
    assert_eq!(
        active_run_id, run_id,
        "the deferred ack must name run A as the consuming run"
    );
    let queued = harness
        .user_message_record("steer the task toward blue")
        .await
        .expect("queued message persisted");
    assert_eq!(
        queued.status,
        MessageStatus::Queued,
        "the busy-thread message must be stored Queued, not RejectedBusy"
    );
    assert_eq!(
        queued.turn_run_id.as_deref(),
        Some(run_id.to_string().as_str()),
        "the Queued row must be bound to run A"
    );

    // Release the park: A's reply-only exit must drain B and keep iterating.
    gate.release();
    harness
        .wait_for_status(run_id, TurnStatus::Completed)
        .await
        .expect("run A completes after consuming the steering input");

    harness
        .assert_model_saw_user_message("steer the task toward blue")
        .await
        .expect("the steering text must reach a model request");
    harness
        .assert_reply_contains("steered reply")
        .await
        .expect("the final reply is the post-steering script entry");
    let consumed = harness
        .user_message_record("steer the task toward blue")
        .await
        .expect("steering message still in transcript");
    assert_eq!(
        consumed.status,
        MessageStatus::Submitted,
        "the consumed steering row must flip Queued → Submitted"
    );
    assert_eq!(
        consumed.turn_run_id.as_deref(),
        Some(run_id.to_string().as_str()),
        "the Submitted row keeps its binding to run A"
    );
}

/// The durable steering guarantee across a restart (review: High): a message
/// queued behind a run parked on a durable approval gate survives a full
/// planned-runtime restart over the same LibSQL store — the resumed run drains
/// the persisted input, the row flips `Queued` → `Submitted`, and the steering
/// text reaches the post-restart model request.
#[tokio::test]
async fn queued_steering_survives_runtime_restart() {
    use reborn_support::builder::StorageMode;
    use reborn_support::group::RebornIntegrationGroup;

    let group = RebornIntegrationGroup::builder()
        .storage(StorageMode::LibSql)
        .live_approvals()
        .await
        .expect("durable live-approvals group builds");
    let before_restart = group
        .thread("steering-restart")
        .script([
            RebornScriptedReply::tool_call(
                "builtin.write_file",
                serde_json::json!({
                    "path": "/workspace/steering-restart.txt",
                    "content": "written across restart"
                }),
            ),
            RebornScriptedReply::text("unreachable on the old runtime"),
        ])
        .build()
        .await
        .expect("pre-restart thread builds");
    let (run_id, gate_ref) = before_restart
        .submit_turn_until_blocked("start gated work")
        .await
        .expect("run parks on the durable approval gate");

    // Busy submit while parked: accepted, stored Queued, durably enqueued.
    let ack = before_restart
        .submit_turn_ack("steer the gated work toward blue")
        .await
        .expect("busy submit does not error");
    assert!(
        matches!(ack, ProductInboundAck::DeferredBusy { .. }),
        "expected DeferredBusy while parked on the gate, got {ack:?}"
    );
    let queued = before_restart
        .user_message_record("steer the gated work toward blue")
        .await
        .expect("queued row persisted");
    assert_eq!(queued.status, MessageStatus::Queued);

    // Restart the whole planned runtime over the same durable store.
    drop(before_restart);
    let restarted_group = group
        .restart_planned_runtime()
        .await
        .expect("planned runtime restarts over durable state");
    let after_restart = restarted_group
        .thread("steering-restart")
        .script([RebornScriptedReply::text("steered reply after restart")])
        .build()
        .await
        .expect("post-restart thread rebuilds the scope gateway");

    after_restart
        .approve_gate(run_id, &gate_ref)
        .await
        .expect("approval resolves after restart");
    after_restart
        .wait_for_status(run_id, TurnStatus::Completed)
        .await
        .expect("resumed run completes after draining the persisted steering input");

    after_restart
        .assert_model_saw_user_message("steer the gated work toward blue")
        .await
        .expect("the persisted steering text must reach a post-restart model request");
    let consumed = after_restart
        .user_message_record("steer the gated work toward blue")
        .await
        .expect("steering row still in transcript");
    assert_eq!(
        consumed.status,
        MessageStatus::Submitted,
        "the resumed run must flip the persisted row Queued → Submitted"
    );
}

/// t3code: "a message sent while approvals are pending redirects the agent".
/// A message sent while the run waits on an approval is steered into it AND
/// declines the pending approval, so the run continues with the message
/// right away — nobody has to answer the approval first, the gated call
/// never runs, and the message's text reaches the model exactly once (as the
/// person's message, not repeated as decline feedback).
#[tokio::test]
async fn steer_during_an_approval_declines_it_and_the_run_continues_with_it() {
    use ironclaw_approvals::ApprovalStatus;
    use reborn_support::group::RebornIntegrationGroup;

    let group = RebornIntegrationGroup::builder()
        .with_real_gate_dispatch_services()
        .live_approvals()
        .await
        .expect("live-approvals group builds");
    let h = group
        .thread("steer-declines-approval")
        .script([
            RebornScriptedReply::tool_call(
                "builtin.write_file",
                serde_json::json!({
                    "path": "/workspace/should_not_exist.txt",
                    "content": "never written"
                }),
            ),
            RebornScriptedReply::text("redirected reply"),
        ])
        .build()
        .await
        .expect("thread builds");
    let (run_id, gate_ref) = h
        .submit_turn_until_blocked("write the file")
        .await
        .expect("run parks on the approval");

    let ack = h
        .submit_turn_ack("no, write nothing and just say hello")
        .await
        .expect("busy submit does not error");
    let ProductInboundAck::DeferredBusy { active_run_id, .. } = ack else {
        panic!("a message during an approval must steer the run, got {ack:?}");
    };
    assert_eq!(active_run_id, run_id);

    h.wait_for_status(run_id, TurnStatus::Completed)
        .await
        .expect("the run continues without anyone answering the approval");
    assert!(
        matches!(
            h.approval_status(&gate_ref).await.expect("approval status"),
            ApprovalStatus::Denied
        ),
        "the pending approval is declined by the steer"
    );
    h.assert_workspace_file_absent("should_not_exist.txt")
        .await
        .expect("the declined call never ran");
    h.assert_model_saw_user_message("no, write nothing and just say hello")
        .await
        .expect("the steer reaches the model as the person's message");
    h.assert_model_saw_text("the person sent a new message instead")
        .await
        .expect("the declined call tells the model why, without repeating the message");
    h.assert_reply_contains("redirected reply")
        .await
        .expect("the run answers after the steer");
    let row = h
        .user_message_record("no, write nothing and just say hello")
        .await
        .expect("steer row");
    assert_eq!(row.status, MessageStatus::Submitted);
    assert_eq!(
        row.turn_run_id.as_deref(),
        Some(run_id.to_string().as_str()),
        "the steer is part of the same run"
    );
}

/// Production-wired handoff: a steer is queued while the run's final model
/// call is in flight, and the run's queue is settled before the loop can
/// drain it (the schedule the runner's pre-terminal settlement creates). The
/// steer must start the next turn — the real coordinator, loop, and model —
/// and a replayed settlement must not deliver it again: one successor run,
/// one answer to the steer.
#[tokio::test]
async fn a_steer_the_run_never_read_starts_the_next_turn_exactly_once() {
    use ironclaw_loop_host::RunQueueEnding;

    let gate = ParkingModelGate::new();
    let harness = RebornIntegrationHarness::test_default()
        .park_model(gate.clone())
        .script([
            RebornScriptedReply::text("first run reply"),
            RebornScriptedReply::text("successor answers the steer"),
        ])
        .build()
        .await
        .expect("harness builds");
    let run_a = harness
        .submit_turn_async("start a long task")
        .await
        .expect("turn A submitted");
    tokio::time::timeout(Duration::from_secs(10), gate.wait_until_parked())
        .await
        .expect("model call parks");

    let ack = harness
        .submit_turn_ack("and also check the tests")
        .await
        .expect("busy submit");
    assert!(
        matches!(ack, ProductInboundAck::DeferredBusy { .. }),
        "{ack:?}"
    );

    // Force the handoff: settle A's queue before its loop drains again.
    let reconcile = harness.input_queue_reconcile_for_test();
    reconcile
        .settle_unconsumed(run_a, RunQueueEnding::Ended)
        .await
        .expect("settle");
    let (dispatch, store) = harness.steer_dispatch_for_test();
    let recorded = store.pending().await.expect("handoffs");
    assert_eq!(recorded.len(), 1, "the unread steer is handed off durably");

    gate.release();
    harness
        .wait_for_status(run_a, TurnStatus::Completed)
        .await
        .expect("run A completes");

    // The dispatcher starts the successor once A released the thread.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let successor = loop {
        let row = harness
            .user_message_record("and also check the tests")
            .await
            .expect("steer row");
        if row.status == MessageStatus::Submitted
            && row.turn_run_id.as_deref() != Some(run_a.to_string().as_str())
        {
            break row.turn_run_id.expect("successor run id");
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the unread steer must start the next turn; row: {row:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let successor_run = ironclaw_turns::TurnRunId::from_uuid(
        uuid::Uuid::parse_str(&successor).expect("successor uuid"),
    );
    harness
        .wait_for_status(successor_run, TurnStatus::Completed)
        .await
        .expect("the successor completes");
    harness
        .assert_model_saw_user_message("and also check the tests")
        .await
        .expect("the steer reached the model");
    harness
        .assert_reply_contains("successor answers the steer")
        .await
        .expect("the successor answered");

    // Replay the settlement: nothing is delivered again.
    for handoff in recorded {
        store.record(handoff).await.expect("replay record");
    }
    dispatch.sweep().await;
    reconcile
        .settle_unconsumed(run_a, RunQueueEnding::Ended)
        .await
        .expect("replayed settle");
    dispatch.sweep().await;
    let row = harness
        .user_message_record("and also check the tests")
        .await
        .expect("steer row");
    assert_eq!(row.status, MessageStatus::Submitted);
    assert_eq!(
        row.turn_run_id.as_deref(),
        Some(successor.as_str()),
        "still delivered to the one successor"
    );
    assert!(store.pending().await.expect("handoffs").is_empty());
}
