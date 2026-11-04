//! Goal checks through the engine: a completed turn arms the check, the
//! tick gates and runs it through the host runner, and the result meets the
//! goal or queues a follow-up on the thread's durable queue.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use ironclaw_host_api::ids::ThreadId;
use ironclaw_product_contracts::inbox::{
    GoalCheckAction, GoalCheckRecord, GoalCheckSpec, GoalCheckStatus, GoalStatus,
};

use super::*;
use crate::engine::Engine;
use crate::model::{QueueStatus, now_ms};
use crate::runtime::{TurnEndStatus, TurnOutcome};
use crate::tests::{finish, harness, open_engine, scope, tid};

struct FakeRunner {
    gate: Mutex<GoalCheckGate>,
    results: Mutex<VecDeque<std::result::Result<GoalCheckOutput, String>>>,
    runs: Mutex<Vec<GoalCheckRequest>>,
    /// Held until released, to change the check while a run is in flight.
    gate_run: tokio::sync::Semaphore,
}

impl FakeRunner {
    fn new(gate: GoalCheckGate) -> Arc<Self> {
        Arc::new(Self {
            gate: Mutex::new(gate),
            results: Mutex::new(VecDeque::new()),
            runs: Mutex::new(Vec::new()),
            gate_run: tokio::sync::Semaphore::new(1_000),
        })
    }
    fn answer(&self, exit_code: i64, output: &str) {
        self.results.lock().unwrap().push_back(Ok(GoalCheckOutput {
            exit_code,
            output: output.into(),
        }));
    }
    fn runs(&self) -> Vec<String> {
        self.runs
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.command.clone())
            .collect()
    }
}

#[async_trait]
impl GoalCheckRunner for FakeRunner {
    async fn gate(&self, _thread: &ThreadRecord) -> GoalCheckGate {
        self.gate.lock().unwrap().clone()
    }
    async fn run(&self, request: GoalCheckRequest) -> std::result::Result<GoalCheckOutput, String> {
        self.runs.lock().unwrap().push(request);
        let _permit = self.gate_run.acquire().await.map_err(|e| e.to_string())?;
        self.results
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err("no scripted result".into()))
    }
}

fn spec(command: &str, max_attempts: u32) -> GoalCheckSpec {
    GoalCheckSpec {
        command: command.into(),
        max_attempts: Some(max_attempts),
        timeout_secs: Some(30),
    }
}

async fn check(engine: &Engine, thread: &ThreadId) -> GoalCheckRecord {
    engine
        .store()
        .goal(thread)
        .await
        .unwrap()
        .unwrap()
        .check
        .unwrap()
}

/// Wait for the spawned run to be recorded.
async fn settled(engine: &Engine, thread: &ThreadId) -> GoalCheckRecord {
    for _ in 0..200 {
        let c = check(engine, thread).await;
        if !matches!(
            c.status,
            GoalCheckStatus::Running | GoalCheckStatus::Pending
        ) {
            return c;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("goal check never settled");
}

/// One completed turn on `thread`.
async fn complete_turn(engine: &Engine, thread: &ThreadId) {
    engine
        .store()
        .set_turn_running(thread, Some(&format!("run-{}", now_ms())), now_ms())
        .await
        .unwrap();
    finish(engine, thread, TurnOutcome::completed(None)).await;
}

async fn goal_with_check(engine: &Engine, thread: &ThreadId, command: &str, max: u32) {
    engine.ensure_thread(thread, &scope(), None).await.unwrap();
    engine
        .store()
        .set_goal(thread, Some("tests pass"), None, now_ms())
        .await
        .unwrap();
    let goal = engine
        .store()
        .set_goal_check(thread, Some(&spec(command, max)), now_ms())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(goal.check.unwrap().status, GoalCheckStatus::NotRun);
}

fn system_items(items: &[crate::model::QueueItem]) -> Vec<String> {
    items
        .iter()
        .filter(|i| i.created_by == Author::System)
        .map(|i| i.text.clone())
        .collect()
}

#[tokio::test]
async fn passing_check_meets_the_goal_after_a_completed_turn() {
    let h = harness().await;
    let runner = FakeRunner::new(GoalCheckGate::Allowed);
    h.engine.set_goal_check_runner(runner.clone());
    let a = tid("a");
    goal_with_check(&h.engine, &a, "cargo test -p foo", 5).await;

    h.engine.tick().await;
    assert!(runner.runs().is_empty(), "nothing runs before a turn ends");

    // A failed or cancelled turn does not arm it.
    h.engine
        .store()
        .set_turn_running(&a, Some("r0"), now_ms())
        .await
        .unwrap();
    finish(
        &h.engine,
        &a,
        TurnOutcome::ended(TurnEndStatus::Cancelled, None),
    )
    .await;
    h.engine.tick().await;
    assert!(runner.runs().is_empty(), "a cancelled turn arms no check");

    runner.answer(0, "test result: ok. 3 passed");
    complete_turn(&h.engine, &a).await;
    assert_eq!(check(&h.engine, &a).await.status, GoalCheckStatus::Pending);
    h.engine.tick().await;
    let c = settled(&h.engine, &a).await;
    assert_eq!(c.status, GoalCheckStatus::Passed);
    assert_eq!(c.exit_code, Some(0));
    assert_eq!(c.output_tail.as_deref(), Some("test result: ok. 3 passed"));
    let goal = h.engine.store().goal(&a).await.unwrap().unwrap();
    assert_eq!(
        goal.status,
        GoalStatus::Done,
        "a passing check meets the goal"
    );
    assert_eq!(runner.runs(), vec!["cargo test -p foo".to_string()]);
    let request = runner.runs.lock().unwrap()[0].clone();
    assert_eq!(request.thread.thread_id, a);
    assert_eq!(request.timeout, Duration::from_secs(30));
    assert!(
        h.engine.store().queue(&a, true).await.unwrap().is_empty(),
        "a pass queues nothing"
    );

    // A met goal is not checked again.
    complete_turn(&h.engine, &a).await;
    h.engine.tick().await;
    assert_eq!(runner.runs().len(), 1);
}

#[tokio::test]
async fn failing_check_queues_a_bounded_follow_up_until_attempts_run_out() {
    let h = harness().await;
    let runner = FakeRunner::new(GoalCheckGate::Allowed);
    h.engine.set_goal_check_runner(runner.clone());
    let a = tid("a");
    goal_with_check(&h.engine, &a, "npm run lint", 2).await;

    let long: String = (1..=200)
        .map(|n| format!("line {n}: error"))
        .collect::<Vec<_>>()
        .join("\n");
    runner.answer(1, &long);
    complete_turn(&h.engine, &a).await;
    h.engine.tick().await;
    let c = settled(&h.engine, &a).await;
    assert_eq!(
        (c.status, c.attempts, c.exit_code),
        (GoalCheckStatus::Failed, 1, Some(1))
    );
    let tail = c.output_tail.unwrap();
    assert!(tail.ends_with("line 200: error"));
    assert!(!tail.contains("line 1: error\n"), "the tail is bounded");
    assert!(tail.lines().count() <= 40);

    let queued = system_items(&h.engine.store().queue(&a, false).await.unwrap());
    assert_eq!(queued.len(), 1);
    assert!(
        queued[0].starts_with("Goal check failed (attempt 1/2): `npm run lint` exited 1."),
        "{}",
        queued[0]
    );
    assert!(queued[0].contains("line 200: error"));
    assert!(!queued[0].contains("line 100: error\n"));

    // The follow-up drains into a turn through the existing queue path.
    h.engine.tick().await;
    assert_eq!(h.rt.starts_on(&a), vec![queued[0].clone()]);

    // That turn ends; the check runs again and fails a second (last) time.
    runner.answer(2, "still broken");
    finish(&h.engine, &a, TurnOutcome::completed(None)).await;
    h.engine.tick().await;
    let c = settled(&h.engine, &a).await;
    assert_eq!(
        (c.status, c.attempts, c.exit_code),
        (GoalCheckStatus::NeedsYou, 2, Some(2))
    );
    assert_eq!(c.output_tail.as_deref(), Some("still broken"));
    let all = h.engine.store().queue(&a, true).await.unwrap();
    assert_eq!(
        system_items(&all).len(),
        1,
        "the last attempt hands over to the person, not the agent"
    );
    assert!(all.iter().all(|i| i.status == QueueStatus::Sent));
    assert_eq!(
        h.engine.store().goal(&a).await.unwrap().unwrap().status,
        GoalStatus::Active
    );

    // Nothing more runs on its own.
    complete_turn(&h.engine, &a).await;
    h.engine.tick().await;
    assert_eq!(runner.runs().len(), 2);

    // "Run check now" starts over.
    runner.answer(0, "ok");
    h.engine
        .store()
        .goal_check_action(&a, GoalCheckAction::RunNow, now_ms())
        .await
        .unwrap();
    assert_eq!(check(&h.engine, &a).await.attempts, 0);
    h.engine.tick().await;
    assert_eq!(settled(&h.engine, &a).await.status, GoalCheckStatus::Passed);
}

#[tokio::test]
async fn ask_first_waits_for_approval_of_each_command() {
    let h = harness().await;
    let runner = FakeRunner::new(GoalCheckGate::AskFirst);
    h.engine.set_goal_check_runner(runner.clone());
    let a = tid("a");
    goal_with_check(&h.engine, &a, "cargo test", 5).await;

    complete_turn(&h.engine, &a).await;
    h.engine.tick().await;
    assert_eq!(
        check(&h.engine, &a).await.status,
        GoalCheckStatus::AwaitingApproval
    );
    assert!(
        runner.runs().is_empty(),
        "no run before the person approves"
    );
    // Another turn does not slip past the gate.
    complete_turn(&h.engine, &a).await;
    h.engine.tick().await;
    assert!(runner.runs().is_empty());

    // Approving ("Run check now") runs it; later turns run it without asking.
    runner.answer(1, "fail");
    h.engine
        .store()
        .goal_check_action(&a, GoalCheckAction::RunNow, now_ms())
        .await
        .unwrap();
    h.engine.tick().await;
    assert_eq!(settled(&h.engine, &a).await.status, GoalCheckStatus::Failed);
    // The follow-up turn runs first; when it ends the check runs again.
    h.engine.tick().await;
    assert_eq!(h.rt.starts_on(&a).len(), 1);
    runner.answer(1, "fail again");
    finish(&h.engine, &a, TurnOutcome::completed(None)).await;
    h.engine.tick().await;
    assert_eq!(settled(&h.engine, &a).await.attempts, 2);
    assert_eq!(runner.runs().len(), 2);

    // A different command is a new approval.
    h.engine
        .store()
        .set_goal_check(&a, Some(&spec("rm -rf target && cargo test", 5)), now_ms())
        .await
        .unwrap();
    h.engine.tick().await; // the second failure's follow-up turn starts
    finish(&h.engine, &a, TurnOutcome::completed(None)).await;
    h.engine.tick().await;
    assert_eq!(
        check(&h.engine, &a).await.status,
        GoalCheckStatus::AwaitingApproval
    );
    assert_eq!(runner.runs().len(), 2);
}

#[tokio::test]
async fn ask_each_time_spends_the_approval_on_one_run() {
    let h = harness().await;
    let runner = FakeRunner::new(GoalCheckGate::AskEachRun);
    h.engine.set_goal_check_runner(runner.clone());
    let a = tid("a");
    goal_with_check(&h.engine, &a, "cargo test", 5).await;
    runner.answer(1, "fail");
    h.engine
        .store()
        .goal_check_action(&a, GoalCheckAction::RunNow, now_ms())
        .await
        .unwrap();
    h.engine.tick().await;
    assert_eq!(settled(&h.engine, &a).await.status, GoalCheckStatus::Failed);
    h.engine.tick().await; // the follow-up turn
    finish(&h.engine, &a, TurnOutcome::completed(None)).await;
    h.engine.tick().await;
    assert_eq!(
        check(&h.engine, &a).await.status,
        GoalCheckStatus::AwaitingApproval,
        "the same command asks again"
    );
    assert_eq!(runner.runs().len(), 1);
}

#[tokio::test]
async fn refused_checks_need_the_person_and_never_run() {
    let h = harness().await;
    let runner = FakeRunner::new(GoalCheckGate::Refused(
        "shell commands are switched off for this profile".into(),
    ));
    h.engine.set_goal_check_runner(runner.clone());
    let a = tid("a");
    goal_with_check(&h.engine, &a, "make check", 5).await;
    complete_turn(&h.engine, &a).await;
    h.engine.tick().await;
    let c = check(&h.engine, &a).await;
    assert_eq!(c.status, GoalCheckStatus::NeedsYou);
    assert_eq!(
        c.output_tail.as_deref(),
        Some("shell commands are switched off for this profile")
    );
    assert!(runner.runs().is_empty());
    assert!(h.engine.store().queue(&a, true).await.unwrap().is_empty());
}

#[tokio::test]
async fn held_threads_wait_and_stale_results_are_dropped() {
    let h = harness().await;
    let runner = FakeRunner::new(GoalCheckGate::Allowed);
    h.engine.set_goal_check_runner(runner.clone());
    let a = tid("a");
    goal_with_check(&h.engine, &a, "cargo test", 5).await;
    complete_turn(&h.engine, &a).await;
    h.engine.queue_hold(&a, true).await.unwrap();
    h.engine.tick().await;
    assert!(runner.runs().is_empty(), "a held thread is not checked");
    h.engine.queue_hold(&a, false).await.unwrap();

    // The check is replaced while its run is in flight: the old result is
    // not recorded and queues nothing.
    let permits = runner.gate_run.available_permits();
    runner.gate_run.forget_permits(permits);
    runner.answer(1, "old failure");
    h.engine.tick().await;
    for _ in 0..200 {
        if !runner.runs().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(check(&h.engine, &a).await.status, GoalCheckStatus::Running);
    h.engine
        .store()
        .set_goal_check(&a, Some(&spec("cargo test --all", 5)), now_ms())
        .await
        .unwrap();
    runner.gate_run.add_permits(1);
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let c = check(&h.engine, &a).await;
    assert_eq!((c.status, c.attempts), (GoalCheckStatus::NotRun, 0));
    assert!(h.engine.store().queue(&a, true).await.unwrap().is_empty());
}

#[tokio::test]
async fn runs_cut_by_a_restart_run_again_and_old_goals_load_without_a_check() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("o.db"))
            .build()
            .await
            .unwrap(),
    );
    {
        // An `orch_goals` table from before checks, with a goal in it.
        let conn = db.connect().unwrap();
        conn.execute_batch(crate::goals::GOALS_SCHEMA)
            .await
            .unwrap();
        conn.execute(
            "INSERT INTO orch_goals (thread_id, objective, status, created_at, updated_at) \
             VALUES ('old', 'ship it', 'active', 1, 1)",
            (),
        )
        .await
        .unwrap();
    }
    let engine = open_engine(&db).await;
    let old = engine.store().goal(&tid("old")).await.unwrap().unwrap();
    assert_eq!(
        (old.objective.as_str(), old.status, old.check),
        ("ship it", GoalStatus::Active, None)
    );
    // An old goal gains a check in place.
    engine
        .store()
        .set_goal_check(&tid("old"), Some(&spec("true", 5)), now_ms())
        .await
        .unwrap();

    // A run in flight when the process died.
    let a = tid("a");
    goal_with_check(&engine, &a, "cargo test", 5).await;
    complete_turn(&engine, &a).await;
    assert!(
        engine
            .store()
            .move_goal_check(
                &a,
                "cargo test",
                GoalCheckStatus::Pending,
                GoalCheckStatus::Running,
                now_ms()
            )
            .await
            .unwrap()
    );
    drop(engine);
    let engine = open_engine(&db).await;
    let rt = Arc::new(crate::tests::FakeRuntime::default());
    engine.register_runtime(rt);
    let runner = FakeRunner::new(GoalCheckGate::Allowed);
    engine.set_goal_check_runner(runner.clone());
    runner.answer(0, "ok");
    engine.tick().await; // boot requeues it, then the same tick runs it
    assert_eq!(settled(&engine, &a).await.status, GoalCheckStatus::Passed);
}

#[tokio::test]
async fn check_specs_are_validated_and_clearing_the_goal_drops_the_check() {
    let h = harness().await;
    let a = tid("a");
    let store = h.engine.store();
    assert!(
        store
            .set_goal_check(&a, Some(&spec("x", 5)), 1)
            .await
            .is_err(),
        "no goal, no check"
    );
    store.set_goal(&a, Some("g"), None, 1).await.unwrap();
    for bad in [spec(" ", 5), spec("a\nb", 5), spec("ok", 0), spec("ok", 51)] {
        assert!(store.set_goal_check(&a, Some(&bad), 2).await.is_err());
    }
    let too_slow = GoalCheckSpec {
        timeout_secs: Some(601),
        ..spec("ok", 5)
    };
    assert!(store.set_goal_check(&a, Some(&too_slow), 2).await.is_err());
    let defaults = GoalCheckSpec {
        command: " cargo test ".into(),
        max_attempts: None,
        timeout_secs: None,
    };
    let c = store
        .set_goal_check(&a, Some(&defaults), 3)
        .await
        .unwrap()
        .unwrap()
        .check
        .unwrap();
    assert_eq!(
        (c.command.as_str(), c.max_attempts, c.timeout_secs),
        ("cargo test", 5, 120)
    );
    // Editing the objective keeps the check; stop retrying sticks.
    store.set_goal(&a, Some("g2"), None, 4).await.unwrap();
    store
        .goal_check_action(&a, GoalCheckAction::StopRetrying, 5)
        .await
        .unwrap();
    assert!(!store.arm_goal_check(&a, 6).await.unwrap());
    assert_eq!(check(&h.engine, &a).await.status, GoalCheckStatus::Stopped);
    // Removing it, and a goal set again after clearing, start without one.
    store.set_goal_check(&a, None, 7).await.unwrap();
    assert_eq!(store.goal(&a).await.unwrap().unwrap().check, None);
    store
        .set_goal_check(&a, Some(&spec("ok", 5)), 8)
        .await
        .unwrap();
    store.set_goal(&a, None, None, 9).await.unwrap();
    store.set_goal(&a, Some("new"), None, 10).await.unwrap();
    assert_eq!(store.goal(&a).await.unwrap().unwrap().check, None);
}

#[test]
fn tails_are_bounded_on_character_boundaries() {
    let wide = "é".repeat(5_000);
    let tail = output_tail(&wide);
    assert!(tail.starts_with('…'));
    assert_eq!(tail.chars().count(), 3_001);
    assert_eq!(output_tail("a\nb\n\n"), "a\nb");
    let text = failure_follow_up("make", 2, 3, 5, "");
    assert!(text.starts_with("Goal check failed (attempt 3/5): `make` exited 2."));
    assert!(text.contains("(no output)"));
}
