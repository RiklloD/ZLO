//! Caller-level coverage for the read-only capability batch fast path:
//! `CapabilityStage` skips the `BeforeSideEffect` checkpoint and widens
//! parallel fan-out only when every call is provably read-only.

use super::*;
use ironclaw_host_api::ids::CapabilityId;
use ironclaw_loop_contracts::LoopModelResponse;

fn read_file_id() -> CapabilityId {
    CapabilityId::new("builtin.read_file").expect("valid")
}

fn grep_id() -> CapabilityId {
    CapabilityId::new("builtin.grep").expect("valid")
}

fn descriptor(capability_id: CapabilityId) -> ironclaw_loop_contracts::CapabilityDescriptorView {
    ironclaw_loop_contracts::CapabilityDescriptorView {
        safe_name: capability_id.as_str().replace('.', "_"),
        capability_id,
        provider: None,
        runtime: ironclaw_host_api::runtime::RuntimeKind::FirstParty,
        safe_description: "read-only test capability".to_string(),
        description_trust: Default::default(),
        parameters_schema: serde_json::json!({"type":"object","properties":{}}),
    }
}

fn calls_for(capability_ids: &[CapabilityId]) -> LoopModelResponse {
    LoopModelResponse {
        chunks: Vec::new(),
        safe_reasoning_deltas: Vec::new(),
        output: ParentLoopOutput::CapabilityCalls(
            capability_ids
                .iter()
                .enumerate()
                .map(|(index, capability_id)| CapabilityCallCandidate {
                    activity_id: CapabilityActivityId::new(),
                    surface_version: surface_version(),
                    capability_id: capability_id.clone(),
                    input_ref: CapabilityInputRef::new(format!("input:read-only-{index}"))
                        .expect("valid"),
                    effective_capability_ids: vec![capability_id.clone()],
                    provider_replay: None,
                })
                .collect(),
        ),
        effective_model_profile_id: ironclaw_loop_contracts::ModelProfileId::new("model")
            .expect("valid"),
        usage: None,
    }
}

fn completed_outcomes(label: &str, count: usize) -> Vec<ironclaw_host_api::resolution::Resolution> {
    (0..count)
        .map(|index| {
            resolution::completed(
                LoopResultRef::new(format!("result:{label}-{index}")).expect("valid"),
                format!("{label} {index}"),
                ironclaw_loop_contracts::CapabilityProgress::MadeProgress,
                false,
                0,
                None,
                None,
            )
        })
        .collect()
}

fn read_only_host(model_responses: Vec<LoopModelResponse>, outcomes: usize) -> MockHost {
    MockHost::new(model_responses)
        .with_extra_capability_descriptors(vec![descriptor(read_file_id()), descriptor(grep_id())])
        .with_single_outcomes(completed_outcomes("read-only", outcomes))
}

fn count_kind(host: &MockHost, kind: LoopCheckpointKind) -> usize {
    host.checkpoint_kinds()
        .into_iter()
        .filter(|written| *written == kind)
        .count()
}

/// P2.1: a batch of pure reads writes no `BeforeSideEffect` checkpoint; the
/// newest durable checkpoint stays the resumable `BeforeModel`.
#[tokio::test]
async fn pure_read_only_batch_skips_the_before_side_effect_checkpoint() {
    let host = read_only_host(
        vec![calls_for(&[read_file_id(), grep_id()]), reply_response()],
        2,
    );
    let state = LoopExecutionState::initial_for_run(host.run_context());

    let exit = CanonicalAgentLoopExecutor
        .execute_family(&crate::families::default(), &host, state)
        .await
        .expect("execute");

    assert!(matches!(exit, LoopExit::Completed(_)));
    assert_eq!(host.single_invocations().len(), 2, "both reads dispatched");
    assert_eq!(
        count_kind(&host, LoopCheckpointKind::BeforeSideEffect),
        0,
        "a provably read-only batch must not write BeforeSideEffect; got {:?}",
        host.checkpoint_kinds()
    );
    // Durability is unchanged elsewhere: one BeforeModel per model call at the
    // default interval, and the results are still persisted.
    assert_eq!(count_kind(&host, LoopCheckpointKind::BeforeModel), 2);
    assert_eq!(host.appended_result_refs().len(), 2);
}

/// Skipping `BeforeSideEffect` is what lets `BeforeModel` batching work across
/// consecutive read-only exploration turns: the previous durable checkpoint is
/// resumable, so a non-flush iteration may skip its `BeforeModel` write.
/// (Two calls per turn keep dispatch on the parallel single-invocation path
/// the scripted outcomes feed.)
#[tokio::test]
async fn consecutive_read_only_turns_batch_before_model_checkpoints() {
    let read_only = read_only_host(
        vec![
            calls_for(&[read_file_id(), grep_id()]),
            calls_for(&[grep_id(), read_file_id()]),
            reply_response(),
        ],
        4,
    )
    .with_before_model_checkpoint_interval(3);
    let state = LoopExecutionState::initial_for_run(read_only.run_context());
    let exit = CanonicalAgentLoopExecutor
        .execute_family(&crate::families::default(), &read_only, state)
        .await
        .expect("execute");
    assert!(matches!(exit, LoopExit::Completed(_)));
    assert_eq!(
        read_only
            .checkpoint_kinds()
            .into_iter()
            .filter(|kind| *kind != LoopCheckpointKind::Final)
            .collect::<Vec<_>>(),
        vec![LoopCheckpointKind::BeforeModel],
        "only iteration 0 flushes BeforeModel when every tool turn is read-only"
    );

    // The same shape with a side-effecting capability keeps the per-iteration
    // flush the BeforeSideEffect marker forces.
    let side_effecting = MockHost::new(vec![
        calls_for(&[capability_id(), capability_id()]),
        calls_for(&[capability_id(), capability_id()]),
        reply_response(),
    ])
    .with_single_outcomes(completed_outcomes("side-effect", 4))
    .with_before_model_checkpoint_interval(3);
    let state = LoopExecutionState::initial_for_run(side_effecting.run_context());
    let exit = CanonicalAgentLoopExecutor
        .execute_family(&crate::families::default(), &side_effecting, state)
        .await
        .expect("execute");
    assert!(matches!(exit, LoopExit::Completed(_)));
    assert_eq!(
        count_kind(&side_effecting, LoopCheckpointKind::BeforeSideEffect),
        2
    );
    assert_eq!(
        count_kind(&side_effecting, LoopCheckpointKind::BeforeModel),
        3
    );
}

/// Fail-closed: one call the loop cannot prove read-only keeps the marker for
/// the whole batch.
#[tokio::test]
async fn batch_with_any_side_effecting_or_unknown_call_keeps_before_side_effect() {
    let host = read_only_host(
        vec![
            calls_for(&[read_file_id(), capability_id(), grep_id()]),
            reply_response(),
        ],
        3,
    );
    let state = LoopExecutionState::initial_for_run(host.run_context());

    let exit = CanonicalAgentLoopExecutor
        .execute_family(&crate::families::default(), &host, state)
        .await
        .expect("execute");

    assert!(matches!(exit, LoopExit::Completed(_)));
    assert_eq!(
        count_kind(&host, LoopCheckpointKind::BeforeSideEffect),
        1,
        "a mixed batch must keep its BeforeSideEffect checkpoint"
    );
    let kinds = host.checkpoint_kinds();
    let side_effect_at = kinds
        .iter()
        .position(|kind| *kind == LoopCheckpointKind::BeforeSideEffect)
        .expect("BeforeSideEffect written");
    assert_eq!(
        kinds[..side_effect_at],
        [LoopCheckpointKind::BeforeModel],
        "BeforeSideEffect precedes dispatch, right after the model checkpoint"
    );
}

async fn max_concurrency_for(capability_ids: Vec<CapabilityId>) -> (usize, usize) {
    let call_count = capability_ids.len();
    let host = read_only_host(
        vec![calls_for(&capability_ids), reply_response()],
        call_count,
    )
    .with_single_invoke_delays(vec![std::time::Duration::from_millis(10); call_count]);
    let state = LoopExecutionState::initial_for_run(host.run_context());
    let run_host = host.clone();
    let executor = tokio::spawn(async move {
        CanonicalAgentLoopExecutor
            .execute_family(&crate::families::default(), &run_host, state)
            .await
    });
    // Let the scheduler fill its whole launch window under paused time before
    // any scripted sleep can complete.
    while host.single_invocations().is_empty() && !executor.is_finished() {
        tokio::task::yield_now().await;
    }
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    for _ in 0..call_count {
        tokio::time::advance(std::time::Duration::from_millis(10)).await;
    }
    let exit = executor
        .await
        .expect("executor task must not panic")
        .expect("execute");
    assert!(matches!(exit, LoopExit::Completed(_)));
    (
        host.max_concurrent_single_invocations(),
        host.single_invocations().len(),
    )
}

/// P2.1: a pure read batch fans out up to 12 concurrent invocations.
#[tokio::test(start_paused = true)]
async fn read_only_batch_runs_up_to_twelve_calls_concurrently() {
    let mut ids = vec![read_file_id(); 7];
    ids.extend(vec![grep_id(); 7]);
    let (max_concurrent, launched) = max_concurrency_for(ids).await;
    assert_eq!(max_concurrent, 12);
    assert_eq!(launched, 14, "the window refills until every call ran");
}

/// A batch that is not provably read-only keeps the 4-wide window.
#[tokio::test(start_paused = true)]
async fn mixed_batch_keeps_the_four_call_parallel_window() {
    let mut ids = vec![read_file_id(); 13];
    ids.push(capability_id());
    let (max_concurrent, launched) = max_concurrency_for(ids).await;
    assert_eq!(max_concurrent, 4);
    assert_eq!(launched, 14);
}
