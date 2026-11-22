//! Concurrent `spawn_subagent` fan-out inside one capability batch.
//!
//! The agent loop routes any batch containing a spawn through
//! `invoke_capability_batch` (ordered), so the spawn port owns the fan-out:
//! consecutive spawns must run their child-creation work concurrently while
//! keeping input-ordered results, input-ordered slot reservation, the shared
//! batch gate, and reverse-order rollback on partial failure.

use std::{
    collections::HashMap,
    sync::atomic::{AtomicUsize, Ordering as AtomicOrdering},
    time::Duration,
};

use ironclaw_host_api::resolution::ToolVerdict;

use super::*;

const FANOUT_DEADLOCK_GUARD: Duration = Duration::from_secs(10);

/// Decodes a distinct task per input ref so each child can be traced back to
/// the invocation that spawned it.
struct PerInputSpawnInputCodec {
    args: HashMap<CapabilityInputRef, SpawnSubagentArgs>,
}

#[async_trait]
impl SpawnSubagentInputCodec for PerInputSpawnInputCodec {
    async fn decode(
        &self,
        _run_context: &LoopRunContext,
        input_ref: &CapabilityInputRef,
    ) -> Result<SpawnSubagentArgs, AgentLoopHostError> {
        self.args.get(input_ref).cloned().ok_or_else(|| {
            AgentLoopHostError::new(
                AgentLoopHostErrorKind::InvalidInvocation,
                "no args for input ref",
            )
        })
    }
}

/// Child-run submitter that proves concurrency: every submit waits on a
/// barrier sized to the expected fan-out, so a serialized batch never gets
/// past the first submit (the test's deadlock guard then fails it). After the
/// barrier it sleeps a per-task delay so completion order is the reverse of
/// input order, and it can fail one task to drive partial-failure rollback.
struct BarrierChildRuns {
    barrier: tokio::sync::Barrier,
    delays: HashMap<String, Duration>,
    fail_task: Option<String>,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    attempted: std::sync::Mutex<Vec<SubmitChildRunRequest>>,
    accepted: RecordingChildRuns,
}

impl BarrierChildRuns {
    fn new(parties: usize, delays: &[(&str, u64)], fail_task: Option<&str>) -> Self {
        Self {
            barrier: tokio::sync::Barrier::new(parties),
            delays: delays
                .iter()
                .map(|(task, millis)| ((*task).to_string(), Duration::from_millis(*millis)))
                .collect(),
            fail_task: fail_task.map(str::to_string),
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
            attempted: std::sync::Mutex::new(Vec::new()),
            accepted: RecordingChildRuns::default(),
        }
    }

    fn attempted(&self) -> Vec<SubmitChildRunRequest> {
        self.attempted.lock().unwrap().clone()
    }

    fn task_of_run(&self, run_id: TurnRunId) -> String {
        let request = self
            .accepted
            .requests()
            .into_iter()
            .find(|request| request.requested_run_id == Some(run_id))
            .expect("child run was accepted");
        request_task(&request)
    }
}

fn request_task(request: &SubmitChildRunRequest) -> String {
    let input = request.process_input.as_ref().expect("process input");
    serde_json::from_slice::<SubagentGoalRecord>(input.payload.as_bytes())
        .expect("goal record")
        .task
}

#[async_trait]
impl TurnSpawnTreePort for BarrierChildRuns {
    async fn submit_child_run(
        &self,
        request: SubmitChildRunRequest,
    ) -> Result<SubmitTurnResponse, TurnError> {
        let now = self.in_flight.fetch_add(1, AtomicOrdering::SeqCst) + 1;
        self.max_in_flight.fetch_max(now, AtomicOrdering::SeqCst);
        self.attempted.lock().unwrap().push(request.clone());
        self.barrier.wait().await;
        let task = request_task(&request);
        if let Some(delay) = self.delays.get(&task) {
            tokio::time::sleep(*delay).await;
        }
        self.in_flight.fetch_sub(1, AtomicOrdering::SeqCst);
        if self.fail_task.as_deref() == Some(task.as_str()) {
            return Err(TurnError::Unavailable {
                reason: "child submit failed".to_string(),
            });
        }
        self.accepted.submit_child_run(request).await
    }
}

struct FanoutFixture {
    port: SubagentSpawnCapabilityPort,
    child_runs: Arc<BarrierChildRuns>,
    turn_store: Arc<StaticAgentTurnRuntime>,
    thread_service: Arc<InMemorySessionThreadService>,
    invocations: Vec<LoopRequest>,
}

async fn fanout_fixture(
    label: &str,
    tasks: &[&str],
    mode: SpawnSubagentMode,
    max_spawn_per_turn: u32,
    child_runs: BarrierChildRuns,
) -> FanoutFixture {
    let context = test_run_context_with_agent_actor(label).await;
    let turn_store = Arc::new(StaticAgentTurnRuntime::new(Some(turn_record(&context, 0))));
    let child_runs = Arc::new(child_runs);
    let thread_service = Arc::new(InMemorySessionThreadService::default());
    let input_refs = tasks
        .iter()
        .map(|task| CapabilityInputRef::new(format!("input:spawn-{task}")).unwrap())
        .collect::<Vec<_>>();
    let codec = PerInputSpawnInputCodec {
        args: input_refs
            .iter()
            .zip(tasks)
            .map(|(input_ref, task)| {
                (
                    input_ref.clone(),
                    SpawnSubagentArgs {
                        task: (*task).to_string(),
                        mode,
                        ..default_spawn_args()
                    },
                )
            })
            .collect(),
    };
    let deps = Arc::new(SubagentSpawnDeps {
        coordinator: Arc::new(StaticCoordinator),
        child_runs: child_runs.clone(),
        agent_turn_runtime: turn_store.clone(),
        thread_service: thread_service.clone(),
        await_edge_writer: Arc::new(InMemoryAwaitEdgeWriter),
        definition_resolver: Arc::new(StaticDefinitionResolver {
            resolved: Some(subagent_definition(false)),
            parent: None,
        }),
        spawn_input_codec: Arc::new(codec),
        result_writer: Arc::new(NoopResultWriter),
    });
    let port = SubagentSpawnCapabilityPort::new(
        Arc::new(AuthPassPort),
        context,
        CapabilityId::new(DEFAULT_SPAWN_SUBAGENT_CAPABILITY_ID).unwrap(),
        SubagentSpawnLimits {
            max_spawn_per_turn,
            ..SubagentSpawnLimits::default()
        },
        deps,
        Vec::new(),
    );
    let invocations = input_refs
        .into_iter()
        .map(|input_ref| {
            let activity_id = authorize_spawn_input_ref(&port, input_ref.clone());
            LoopRequest {
                input_ref,
                ..invocation_for_activity(DEFAULT_SPAWN_SUBAGENT_CAPABILITY_ID, activity_id)
            }
        })
        .collect();
    FanoutFixture {
        port,
        child_runs,
        turn_store,
        thread_service,
        invocations,
    }
}

async fn invoke_fanout(fixture: &FanoutFixture) -> Result<ResolutionBatch, AgentLoopHostError> {
    tokio::time::timeout(
        FANOUT_DEADLOCK_GUARD,
        fixture.port.invoke_capability_batch(LoopRequestBatch {
            invocations: fixture.invocations.clone(),
            // The agent loop always asks for this on the ordered path.
            stop_on_first_suspension: true,
        }),
    )
    .await
    .expect("spawn fan-out deadlocked: the batch serialized its child submits")
}

fn spawned_child_run(resolution: &Resolution) -> TurnRunId {
    match resolution {
        Resolution::Done(outcome) => match &outcome.verdict {
            ToolVerdict::ChildSpawned { child_run } => TurnRunId::from_uuid(child_run.as_uuid()),
            other => panic!("expected a spawned child, got {other:?}"),
        },
        other => panic!("expected a spawned child, got {other:?}"),
    }
}

#[tokio::test]
async fn spawn_batch_fans_out_concurrently_and_keeps_input_order() {
    let tasks = ["alpha", "beta", "gamma"];
    // Completion order is the reverse of input order.
    let child_runs = BarrierChildRuns::new(3, &[("alpha", 40), ("beta", 20)], None);
    let fixture = fanout_fixture(
        "spawn-fanout-order",
        &tasks,
        SpawnSubagentMode::Background,
        SubagentSpawnLimits::default().max_spawn_per_turn,
        child_runs,
    )
    .await;

    let outcome = invoke_fanout(&fixture).await.expect("batch succeeds");

    assert!(!outcome.stopped_on_suspension);
    assert_eq!(
        fixture
            .child_runs
            .max_in_flight
            .load(AtomicOrdering::SeqCst),
        3,
        "all three child submits must be in flight at once"
    );
    let resolved_tasks = outcome
        .resolutions
        .iter()
        .map(|resolution| {
            fixture
                .child_runs
                .task_of_run(spawned_child_run(resolution))
        })
        .collect::<Vec<_>>();
    assert_eq!(resolved_tasks, tasks, "results stay in invocation order");
    assert_eq!(fixture.port.spawned_this_turn.load(Ordering::Acquire), 3);
    assert!(fixture.turn_store.cancels().is_empty());
}

#[tokio::test]
async fn blocking_spawn_batch_fans_out_onto_one_shared_gate_without_stopping() {
    let tasks = ["alpha", "beta", "gamma"];
    let child_runs = BarrierChildRuns::new(3, &[("alpha", 30)], None);
    let fixture = fanout_fixture(
        "spawn-fanout-blocking",
        &tasks,
        SpawnSubagentMode::Blocking,
        SubagentSpawnLimits::default().max_spawn_per_turn,
        child_runs,
    )
    .await;

    let outcome = invoke_fanout(&fixture).await.expect("batch succeeds");

    assert_eq!(outcome.resolutions.len(), 3);
    assert!(
        !outcome.stopped_on_suspension,
        "coalesced blocking spawns accrue on the batch gate instead of stopping"
    );
    let gates = outcome
        .resolutions
        .iter()
        .map(|resolution| match resolution {
            Resolution::Suspended(Suspension::DependentRun { waypoint, .. }) => waypoint
                .origin
                .as_ref()
                .map(|origin| origin.as_str().to_string())
                .expect("batch gate origin"),
            other => panic!("expected a dependent-run suspension, got {other:?}"),
        })
        .collect::<Vec<_>>();
    assert!(gates[0].starts_with("gate:subagent-batch-"));
    assert!(gates.iter().all(|gate| gate == &gates[0]));
    let groups = fixture
        .child_runs
        .accepted
        .requests()
        .into_iter()
        .map(|request| request.process_dependency.expect("dependency").group_ref)
        .collect::<Vec<_>>();
    assert!(
        groups
            .iter()
            .all(|group| group.as_deref() == Some(gates[0].as_str())),
        "every child joins the one shared await group"
    );
    assert_eq!(
        fixture
            .child_runs
            .max_in_flight
            .load(AtomicOrdering::SeqCst),
        3
    );
}

#[tokio::test]
async fn spawn_fanout_reserves_spawn_slots_in_input_order() {
    let tasks = ["alpha", "beta", "gamma"];
    // Only two children may ever reach submit; the barrier proves both run
    // concurrently and the third never gets a slot.
    let child_runs = BarrierChildRuns::new(2, &[("alpha", 30)], None);
    let fixture = fanout_fixture(
        "spawn-fanout-slots",
        &tasks,
        SpawnSubagentMode::Background,
        2,
        child_runs,
    )
    .await;

    let outcome = invoke_fanout(&fixture).await.expect("batch succeeds");

    assert_eq!(outcome.resolutions.len(), 3);
    assert_eq!(
        fixture
            .child_runs
            .task_of_run(spawned_child_run(&outcome.resolutions[0])),
        "alpha"
    );
    assert_eq!(
        fixture
            .child_runs
            .task_of_run(spawned_child_run(&outcome.resolutions[1])),
        "beta"
    );
    assert_eq!(
        denied_reason(outcome.resolutions[2].clone()),
        "fanout_cap_exceeded",
        "the slot cap applies in invocation order, never to whichever finishes first"
    );
    assert_eq!(fixture.child_runs.attempted().len(), 2);
    assert_eq!(fixture.port.spawned_this_turn.load(Ordering::Acquire), 2);

    // Regression: a cap rejection must not free a committed slot. The old
    // eager `then_some` guard released one per rejection, so this retry used
    // to be admitted as a third spawn past the cap of two.
    let retry = fixture.invocations[2].clone();
    let activity_id = authorize_spawn_input_ref(&fixture.port, retry.input_ref.clone());
    let retried = tokio::time::timeout(
        Duration::from_secs(2),
        fixture.port.invoke_capability(LoopRequest {
            activity_id,
            ..retry
        }),
    )
    .await
    .expect("retry was admitted past the spawn cap and reached child submit")
    .expect("retry resolves");
    assert_eq!(denied_reason(retried), "fanout_cap_exceeded");
    assert_eq!(fixture.port.spawned_this_turn.load(Ordering::Acquire), 2);
    assert_eq!(fixture.child_runs.attempted().len(), 2);
}

#[tokio::test]
async fn spawn_fanout_rolls_back_every_sibling_in_reverse_order_when_one_fails() {
    let tasks = ["alpha", "beta", "gamma"];
    let child_runs = BarrierChildRuns::new(3, &[("alpha", 20)], Some("beta"));
    let fixture = fanout_fixture(
        "spawn-fanout-rollback",
        &tasks,
        SpawnSubagentMode::Blocking,
        SubagentSpawnLimits::default().max_spawn_per_turn,
        child_runs,
    )
    .await;
    let actor = fixture.port.run_context.actor.clone().unwrap();

    let error = invoke_fanout(&fixture)
        .await
        .expect_err("a failed child submit fails the batch");

    assert_eq!(error.kind, AgentLoopHostErrorKind::Unavailable);
    let attempted = fixture.child_runs.attempted();
    assert_eq!(attempted.len(), 3, "all three spawns ran concurrently");
    let run_of = |task: &str| {
        attempted
            .iter()
            .find(|request| request_task(request) == task)
            .and_then(|request| request.requested_run_id)
            .expect("attempted child run")
    };
    // The failed child cleaned itself up; its accepted siblings are
    // cancelled in reverse invocation order.
    let cancelled = fixture
        .turn_store
        .cancels()
        .into_iter()
        .map(|cancel| cancel.run_id)
        .collect::<Vec<_>>();
    assert_eq!(cancelled, vec![run_of("gamma"), run_of("alpha")]);
    assert_eq!(fixture.turn_store.releases.lock().unwrap().len(), 2);
    assert_eq!(
        fixture.port.spawned_this_turn.load(Ordering::Acquire),
        0,
        "every spawn slot is released after rollback"
    );
    for request in &attempted {
        let read = fixture
            .thread_service
            .read_thread(ThreadHistoryRequest {
                scope: ThreadScope {
                    tenant_id: request.child_scope.tenant_id.clone(),
                    agent_id: request.child_scope.agent_id.clone().unwrap(),
                    project_id: request.child_scope.project_id.clone(),
                    owner_user_id: Some(actor.user_id.clone()),
                    mission_id: None,
                },
                thread_id: request.child_scope.thread_id.clone(),
            })
            .await;
        assert!(
            matches!(read, Err(SessionThreadError::UnknownThread { .. })),
            "rolled-back child threads are deleted"
        );
    }
}

#[tokio::test]
async fn spawn_fanout_still_runs_non_spawn_segments_in_order() {
    let tasks = ["alpha", "beta"];
    let child_runs = BarrierChildRuns::new(2, &[], None);
    let mut fixture = fanout_fixture(
        "spawn-fanout-mixed",
        &tasks,
        SpawnSubagentMode::Background,
        SubagentSpawnLimits::default().max_spawn_per_turn,
        child_runs,
    )
    .await;
    fixture.invocations.insert(0, invocation("regular.before"));
    fixture.invocations.push(invocation("regular.after"));

    let outcome = invoke_fanout(&fixture).await.expect("batch succeeds");

    assert_eq!(outcome.resolutions.len(), 4);
    assert!(matches!(outcome.resolutions[0], Resolution::Done(_)));
    assert_eq!(
        fixture
            .child_runs
            .task_of_run(spawned_child_run(&outcome.resolutions[1])),
        "alpha"
    );
    assert_eq!(
        fixture
            .child_runs
            .task_of_run(spawned_child_run(&outcome.resolutions[2])),
        "beta"
    );
    assert!(matches!(outcome.resolutions[3], Resolution::Done(_)));
}
