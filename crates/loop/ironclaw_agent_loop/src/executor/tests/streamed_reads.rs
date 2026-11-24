//! Caller-level coverage for overlapped streaming read-only pre-execution
//! (P2.1): a read the host announces mid-stream starts before the model call
//! returns, `CapabilityStage` reuses its result without dispatching again,
//! and anything not provably a pure read — or any attempt that fails,
//! truncates, or is cancelled — never leaves a speculative result behind.

use std::time::Duration;

use super::*;
use ironclaw_host_api::ids::CapabilityId;
use ironclaw_loop_contracts::LoopModelResponse;

fn id(raw: &str) -> CapabilityId {
    CapabilityId::new(raw).expect("valid")
}

fn descriptor(capability_id: CapabilityId) -> ironclaw_loop_contracts::CapabilityDescriptorView {
    ironclaw_loop_contracts::CapabilityDescriptorView {
        safe_name: capability_id.as_str().replace('.', "_"),
        capability_id,
        provider: None,
        runtime: ironclaw_host_api::runtime::RuntimeKind::FirstParty,
        safe_description: "streamed test capability".to_string(),
        description_trust: Default::default(),
        parameters_schema: serde_json::json!({"type":"object","properties":{}}),
    }
}

fn candidate(capability_id: &str, effective: &str, index: usize) -> CapabilityCallCandidate {
    CapabilityCallCandidate {
        activity_id: CapabilityActivityId::new(),
        surface_version: surface_version(),
        capability_id: id(capability_id),
        input_ref: CapabilityInputRef::new(format!("input:streamed-{index}")).expect("valid"),
        effective_capability_ids: vec![id(effective)],
        provider_replay: None,
    }
}

fn read(index: usize) -> CapabilityCallCandidate {
    candidate("builtin.read_file", "builtin.read_file", index)
}

fn calls(calls: Vec<CapabilityCallCandidate>) -> LoopModelResponse {
    LoopModelResponse {
        chunks: Vec::new(),
        safe_reasoning_deltas: Vec::new(),
        output: ParentLoopOutput::CapabilityCalls(calls),
        effective_model_profile_id: ironclaw_loop_contracts::ModelProfileId::new("model")
            .expect("valid"),
        usage: None,
    }
}

fn completed(count: usize) -> Vec<ironclaw_host_api::resolution::Resolution> {
    (0..count)
        .map(|index| {
            resolution::completed(
                LoopResultRef::new(format!("result:streamed-{index}")).expect("valid"),
                format!("streamed {index}"),
                ironclaw_loop_contracts::CapabilityProgress::MadeProgress,
                false,
                0,
                None,
                None,
            )
        })
        .collect()
}

fn host(model_responses: Vec<LoopModelResponse>, outcomes: usize) -> MockHost {
    MockHost::new(model_responses)
        .with_extra_capability_descriptors(vec![
            descriptor(id("builtin.read_file")),
            descriptor(id("builtin.grep")),
            descriptor(id("builtin.write_file")),
        ])
        .with_single_outcomes(completed(outcomes))
}

async fn run(host: &MockHost) -> Result<LoopExit, AgentLoopExecutorError> {
    let state = LoopExecutionState::initial_for_run(host.run_context());
    CanonicalAgentLoopExecutor
        .execute_family(&crate::families::default(), host, state)
        .await
}

/// A streamed read starts while the model call is still open (the mock holds
/// the response until the read entered the host), and the capability stage
/// reuses that result: one host invocation, no batch entry, one result.
#[tokio::test]
async fn streamed_read_starts_before_the_model_returns_and_is_reused() {
    let call = read(0);
    let host = host(vec![calls(vec![call.clone()]), reply_response()], 1)
        .with_streamed_capability_calls(vec![call.clone()], 1);

    let exit = run(&host).await.expect("execute");

    assert!(matches!(exit, LoopExit::Completed(_)), "{exit:?}");
    assert_eq!(
        host.streamed_overlap(),
        [true],
        "read overlapped the stream"
    );
    let invoked = host.single_invocations();
    assert_eq!(
        invoked.len(),
        1,
        "the streamed read is not dispatched twice"
    );
    assert_eq!(invoked[0].activity_id, call.activity_id);
    assert!(
        host.batch_invocations().is_empty(),
        "a reused lone read never enters the host batch path"
    );
    assert_eq!(host.appended_result_refs().len(), 1);
}

/// Only the streamed call is pre-executed; the rest of a read-only batch
/// dispatches normally and both results land, in batch order.
#[tokio::test]
async fn a_partially_streamed_read_batch_runs_only_the_remainder() {
    let first = read(0);
    let second = candidate("builtin.grep", "builtin.grep", 1);
    let host = host(
        vec![calls(vec![first.clone(), second.clone()]), reply_response()],
        2,
    )
    .with_streamed_capability_calls(vec![first.clone()], 1);

    let exit = run(&host).await.expect("execute");

    assert!(matches!(exit, LoopExit::Completed(_)), "{exit:?}");
    assert_eq!(host.streamed_overlap(), [true]);
    let invoked: Vec<_> = host
        .single_invocations()
        .into_iter()
        .map(|request| request.activity_id)
        .collect();
    assert_eq!(invoked, [first.activity_id, second.activity_id]);
    assert_eq!(host.appended_result_refs().len(), 2);
}

/// Writes, exec, the dispatch bridge (even targeting a reader), unknown
/// capabilities, a read outside the visible surface, and any read on a host
/// that requires ordered entry are never started mid-stream.
#[tokio::test]
async fn non_read_only_streamed_calls_are_never_pre_executed() {
    let cases = [
        (
            candidate("builtin.write_file", "builtin.write_file", 0),
            false,
        ),
        (candidate("builtin.shell", "builtin.shell", 0), false),
        (
            candidate("ironclaw.tool_call", "builtin.read_file", 0),
            false,
        ),
        (
            candidate("custom.unknown_tool", "custom.unknown_tool", 0),
            false,
        ),
        (
            candidate("builtin.read_file", "builtin.write_file", 0),
            false,
        ),
        (candidate("builtin.list_dir", "builtin.list_dir", 0), false),
        (read(0), true),
    ];
    for (streamed, ordered_host) in cases {
        let mut host = host(vec![reply_response()], 0)
            .with_streamed_capability_calls(vec![streamed.clone()], 1);
        if ordered_host {
            host = host.requiring_ordered_batch_invocation();
        }

        let exit = run(&host).await.expect("execute");

        assert!(matches!(exit, LoopExit::Completed(_)), "{exit:?}");
        assert_eq!(
            host.streamed_overlap(),
            [false],
            "{} must not start mid-stream",
            streamed.capability_id.as_str()
        );
        assert!(host.single_invocations().is_empty());
        assert!(host.appended_result_refs().is_empty());
    }
}

/// A stream that fails after announcing a read aborts the in-flight read:
/// it entered the host but never finished, no result is appended, and the
/// retried model call starts clean (the retry here replies, so nothing of
/// the failed attempt survives into the completed run).
#[tokio::test(start_paused = true)]
async fn a_failed_stream_aborts_the_in_flight_read() {
    let host = host(vec![reply_response()], 1)
        .with_single_invoke_delays(vec![Duration::from_secs(30)])
        .with_streamed_capability_calls(vec![read(0)], 1)
        .with_model_errors(vec![AgentLoopHostError::new(
            AgentLoopHostErrorKind::Internal,
            "model provider failed internally",
        )]);

    let exit = run(&host).await.expect("execute");

    assert!(matches!(exit, LoopExit::Completed(_)), "{exit:?}");
    assert_eq!(
        host.model_requests().len(),
        2,
        "the failed call was retried"
    );
    assert_eq!(host.streamed_overlap(), [true], "the read had started");
    assert_eq!(host.single_invocations().len(), 1);
    assert_eq!(
        host.finished_single_invocations(),
        0,
        "the read was aborted"
    );
    assert!(host.appended_result_refs().is_empty());
}

/// A response that is not a capability batch (a truncated `Length` stop or
/// a plain reply) discards the streamed read instead of waiting for it.
#[tokio::test]
async fn a_non_tool_response_discards_the_in_flight_read() {
    let host = host(vec![reply_response()], 1)
        .with_single_invoke_delays(vec![Duration::from_secs(30)])
        .with_streamed_capability_calls(vec![read(0)], 1);

    let exit = run(&host).await.expect("execute");

    assert!(matches!(exit, LoopExit::Completed(_)), "{exit:?}");
    assert_eq!(host.streamed_overlap(), [true]);
    assert_eq!(
        host.finished_single_invocations(),
        0,
        "the read was aborted"
    );
    assert!(host.appended_result_refs().is_empty());
}

/// A final batch that mixes in a side-effecting call is not read-only: the
/// streamed read is aborted and the whole batch dispatches normally, so the
/// read's result comes from the normal path only.
#[tokio::test]
async fn a_mixed_final_batch_discards_the_speculative_read() {
    let streamed = read(0);
    let write = candidate("builtin.write_file", "builtin.write_file", 1);
    let host = host(
        vec![
            calls(vec![streamed.clone(), write.clone()]),
            reply_response(),
        ],
        3,
    )
    .with_single_invoke_delays(vec![Duration::from_secs(30)])
    .with_streamed_capability_calls(vec![streamed.clone()], 1);

    let exit = run(&host).await.expect("execute");

    assert!(matches!(exit, LoopExit::Completed(_)), "{exit:?}");
    assert_eq!(host.streamed_overlap(), [true]);
    let invoked: Vec<_> = host
        .single_invocations()
        .into_iter()
        .map(|request| request.activity_id)
        .collect();
    assert_eq!(
        invoked,
        [
            streamed.activity_id,
            streamed.activity_id,
            write.activity_id
        ],
        "the read re-dispatches with the batch"
    );
    assert_eq!(
        host.finished_single_invocations(),
        2,
        "only the normal-path invocations finished"
    );
    assert_eq!(host.appended_result_refs().len(), 2);
    assert!(
        host.checkpoint_kinds()
            .contains(&LoopCheckpointKind::BeforeSideEffect),
        "the mixed batch keeps its side-effect checkpoint"
    );
}

/// Stop while the loop waits on a confirmed streamed read aborts it and ends
/// the run cancelled, without appending its result.
#[tokio::test]
async fn cancellation_while_settling_aborts_the_streamed_read() {
    let call = read(0);
    let host = host(vec![calls(vec![call.clone()])], 1)
        .with_single_invoke_delays(vec![Duration::from_secs(30)])
        .with_streamed_capability_calls(vec![call], 1)
        .cancel_after_model_response();

    let exit = run(&host).await.expect("execute");

    assert!(matches!(exit, LoopExit::Cancelled(_)), "{exit:?}");
    assert_eq!(
        host.finished_single_invocations(),
        0,
        "the read was aborted"
    );
    assert!(host.appended_result_refs().is_empty());
}
