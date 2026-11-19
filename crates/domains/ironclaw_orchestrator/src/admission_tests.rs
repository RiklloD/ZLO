//! Races on the per-thread admission path (`crate::admission`): runtime
//! doubles that block inside a steer or a turn start until the test lets
//! them go, so every interleaving below is forced, not timed.

use std::sync::Arc;
use std::time::Duration;

use ironclaw_host_api::ids::ThreadId;
use tokio::sync::Semaphore;

use crate::model::*;
use crate::profile_runtime::UserTurn;
use crate::runtime::*;
use crate::tests::{FakeRuntime, Harness, finish, harness_with, open_engine, scope, tid};

async fn queued(h: &Harness, thread: &ThreadId) -> Vec<String> {
    h.engine
        .store()
        .queue(thread, false)
        .await
        .unwrap()
        .into_iter()
        .map(|q| q.text)
        .collect()
}

async fn running(h: &Harness, name: &str, run: &str) -> ThreadRecord {
    let thread = tid(name);
    h.engine
        .ensure_thread(&thread, &scope(), None)
        .await
        .unwrap();
    h.engine
        .store()
        .set_turn_running(&thread, Some(run), now_ms())
        .await
        .unwrap();
    h.engine.store().thread(&thread).await.unwrap().unwrap()
}

/// A harness whose steers (or starts) wait on the returned gate.
async fn gated(reads: bool, steer: bool, start: bool) -> (Harness, Arc<Semaphore>) {
    let gate = Arc::new(Semaphore::new(0));
    let h = harness_with(FakeRuntime {
        reads,
        steer_gate: steer.then(|| gate.clone()),
        start_gate: start.then(|| gate.clone()),
        ..FakeRuntime::default()
    })
    .await;
    (h, gate)
}

async fn in_flight(h: &Harness) {
    tokio::time::timeout(Duration::from_secs(5), h.rt.entered.notified())
        .await
        .expect("the runtime call never started");
}

async fn until_queued(h: &Harness, thread: &ThreadId, len: usize) {
    for _ in 0..300 {
        if queued(h, thread).await.len() == len {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("queue never reached {len}: {:?}", queued(h, thread).await);
}

fn send(
    h: &Harness,
    thread: &ThreadRecord,
    text: &'static str,
) -> tokio::task::JoinHandle<UserTurn> {
    let engine = h.engine.clone();
    let thread = thread.clone();
    tokio::spawn(async move { engine.user_turn(&thread, text).await.unwrap() })
}

fn steers_of(h: &Harness, thread: &ThreadId, text: &str) -> usize {
    h.rt.steered_on(thread)
        .iter()
        .filter(|t| *t == text)
        .count()
}

// ── 1: a turn ends while a steer is out ─────────────────────────────────

/// The run ends while the runtime still holds the steer, then it answers
/// "too late": the words start the next turn exactly once (the end did
/// not also requeue the in-flight steer).
#[tokio::test]
async fn a_turn_ending_during_an_in_flight_steer_delivers_it_once() {
    let (h, gate) = gated(true, true, false).await;
    let a = running(&h, "a", "r1").await;
    h.rt.script_steers(&[SteerOutcome::TurnEnded]);
    let sent = send(&h, &a, "words");
    in_flight(&h).await;
    finish(&h.engine, &a.thread_id, TurnOutcome::completed(None)).await;
    gate.add_permits(1);
    let turn = sent.await.unwrap();
    assert!(matches!(turn, UserTurn::Started { .. }), "{turn:?}");
    h.engine.tick().await;
    h.engine.tick().await;
    assert_eq!(h.rt.starts_on(&a.thread_id), vec!["words".to_string()]);
    assert!(queued(&h, &a.thread_id).await.is_empty());
}

/// Accepted just as the run ends: the steer settles after the end and is
/// recovered by the admission that settled it — queued once, run once.
#[tokio::test]
async fn a_steer_accepted_as_its_turn_ends_runs_next_once() {
    let (h, gate) = gated(true, true, false).await;
    let a = running(&h, "a", "r1").await;
    let sent = send(&h, &a, "words");
    in_flight(&h).await;
    finish(&h.engine, &a.thread_id, TurnOutcome::completed(None)).await;
    gate.add_permits(1);
    assert!(matches!(sent.await.unwrap(), UserTurn::Steered { .. }));
    assert_eq!(queued(&h, &a.thread_id).await, vec!["words"]);
    h.engine.tick().await;
    h.engine.tick().await;
    assert_eq!(h.rt.starts_on(&a.thread_id), vec!["words".to_string()]);
    assert!(queued(&h, &a.thread_id).await.is_empty());
}

/// A read report racing the recovery of the same steer never leaves a
/// read steer with a pending replay: one of them wins atomically.
#[tokio::test]
async fn a_read_report_racing_recovery_is_atomic() {
    for i in 0..10 {
        let (h, _gate) = gated(true, false, false).await;
        let a = running(&h, &format!("a{i}"), "r1").await;
        let UserTurn::Steered {
            steer_id: Some(id), ..
        } = h.engine.user_turn(&a, "words").await.unwrap()
        else {
            panic!("not steered");
        };
        h.engine
            .store()
            .set_turn_ended(&a.thread_id, Some("r1"), TurnState::Idle, None, now_ms())
            .await
            .unwrap();
        let (_, _) = tokio::join!(
            h.engine
                .steer_updated(&a.thread_id, "r1", &id, SteerReadState::Read, now_ms()),
            h.engine.recover_stranded(&a.thread_id),
        );
        let state = h
            .engine
            .store()
            .steer_record(&a.thread_id, &id)
            .await
            .unwrap()
            .unwrap()
            .state;
        let replays = queued(&h, &a.thread_id).await.len();
        assert!(
            (state == SteerState::Read && replays == 0)
                || (state == SteerState::Requeued && replays == 1),
            "{state:?} with {replays} queued"
        );
    }
}

// ── 2: one claim path ───────────────────────────────────────────────────

/// A promote racing the readiness delivery of the same deferred item:
/// the words reach the runtime once more, never twice.
#[tokio::test]
async fn a_promote_racing_readiness_sends_once() {
    let (h, gate) = gated(false, true, false).await;
    let a = running(&h, "a", "r1").await;
    h.rt.script_steers(&[SteerOutcome::NotReady]);
    gate.add_permits(1);
    let turn = h.engine.user_turn(&a, "x").await.unwrap();
    let UserTurn::Steered {
        queued_id: Some(item),
        ..
    } = turn
    else {
        panic!("not deferred: {turn:?}");
    };
    in_flight(&h).await; // the deferral's own call
    h.engine.turn_ready(&a.thread_id, Some("r1")).await;
    in_flight(&h).await;
    let engine = h.engine.clone();
    let thread = a.clone();
    let promote =
        tokio::spawn(async move { engine.queue_promote(&thread, &item, SteerHow::Steer).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    gate.add_permits(2);
    let promoted = promote.await.unwrap();
    until_queued(&h, &a.thread_id, 0).await;
    assert_eq!(
        steers_of(&h, &a.thread_id, "x"),
        2,
        "the deferral, then one delivery"
    );
    assert_eq!(promoted.unwrap_err().code(), "not_queued");
}

/// Two drains of one thread at once: while the head's start is in flight
/// the second claims nothing, so a later item can never start first.
#[tokio::test]
async fn concurrent_drains_claim_only_the_head() {
    let (h, gate) = gated(false, false, true).await;
    let a = tid("a");
    h.engine.ensure_thread(&a, &scope(), None).await.unwrap();
    h.engine.queue_add(&a, "first", Author::User).await.unwrap();
    let second = h
        .engine
        .queue_add(&a, "second", Author::User)
        .await
        .unwrap();
    let second = QueueItemId::new(second["queuedId"].as_str().unwrap()).unwrap();
    let (e1, e2) = (h.engine.clone(), h.engine.clone());
    let (t1, t2) = (a.clone(), a.clone());
    let d1 = tokio::spawn(async move { e1.drain_thread(&t1).await });
    in_flight(&h).await;
    let d2 = tokio::spawn(async move { e2.drain_thread(&t2).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let item = h
        .engine
        .store()
        .queue_item(&a, &second)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        item.status,
        QueueStatus::Pending,
        "claimed while the head was in flight"
    );
    gate.add_permits(2);
    d1.await.unwrap().unwrap();
    d2.await.unwrap().unwrap();
    assert_eq!(h.rt.starts_on(&a), vec!["first".to_string()]);
    assert_eq!(queued(&h, &a).await, vec!["second"]);
}

// ── 3: Stop against the run ─────────────────────────────────────────────

/// Stop while a steer is out, then the runtime answers "too late": the
/// words wait in the held queue instead of starting a turn after the Stop.
#[tokio::test]
async fn a_stop_during_a_too_late_steer_holds_the_words() {
    let (h, gate) = gated(true, true, false).await;
    let a = running(&h, "a", "r1").await;
    h.rt.script_steers(&[SteerOutcome::TurnEnded]);
    let sent = send(&h, &a, "words");
    in_flight(&h).await;
    h.engine.on_user_stop(&a.thread_id).await.unwrap();
    finish(
        &h.engine,
        &a.thread_id,
        TurnOutcome::ended(TurnEndStatus::Cancelled, None),
    )
    .await;
    gate.add_permits(1);
    assert!(matches!(sent.await.unwrap(), UserTurn::Queued { .. }));
    h.engine.tick().await;
    assert!(h.rt.starts_on(&a.thread_id).is_empty());
    assert_eq!(queued(&h, &a.thread_id).await, vec!["words"]);
    assert_eq!(
        h.engine.store().hold(&a.thread_id).await.unwrap(),
        Some(HoldReason::Stopped)
    );
}

/// A steer the runtime accepts after a Stop reached its run is never
/// resent: it becomes an unconfirmed notice.
#[tokio::test]
async fn a_steer_accepted_after_a_stop_is_not_resent() {
    let (h, gate) = gated(true, true, false).await;
    let a = running(&h, "a", "r1").await;
    let sent = send(&h, &a, "words");
    in_flight(&h).await;
    h.engine.on_user_stop(&a.thread_id).await.unwrap();
    gate.add_permits(1);
    sent.await.unwrap();
    finish(
        &h.engine,
        &a.thread_id,
        TurnOutcome::ended(TurnEndStatus::Cancelled, None),
    )
    .await;
    h.engine.on_user_prompt(&a.thread_id).await.unwrap();
    h.engine.tick().await;
    assert!(queued(&h, &a.thread_id).await.is_empty());
    assert!(h.rt.starts_on(&a.thread_id).is_empty());
    let ledger = h.engine.thread_steers(&a.thread_id).await.unwrap();
    assert_eq!(ledger[0].state, SteerState::Unconfirmed);
}

/// A send that reaches a run after Stop recorded against it does not
/// steer it: the words wait (held) for the person.
#[tokio::test]
async fn a_send_into_a_stopped_run_does_not_steer_it() {
    let (h, _gate) = gated(true, false, false).await;
    let a = running(&h, "a", "r1").await;
    h.engine.on_user_stop(&a.thread_id).await.unwrap();
    // An agent send (it does not release the hold) into the stopping run.
    let sent = h
        .engine
        .send(
            &crate::tests::ctx("caller"),
            &a.thread_id,
            "late",
            SendMode::Auto,
        )
        .await
        .unwrap();
    assert_eq!(sent["delivery"], serde_json::json!("queued"));
    assert!(h.rt.steered_on(&a.thread_id).is_empty());
}

// ── 4: crash and storage failures ───────────────────────────────────────

/// The backend dies while a deferred steer is out: the restarted engine
/// does not replay it (nobody knows whether the agent got it); it is kept
/// as an unconfirmed notice.
#[tokio::test]
async fn a_crash_mid_steer_is_not_replayed_at_boot() {
    let (h, gate) = gated(true, true, false).await;
    let a = running(&h, "a", "r1").await;
    h.rt.script_steers(&[SteerOutcome::NotReady]);
    gate.add_permits(1);
    h.engine.user_turn(&a, "x").await.unwrap();
    in_flight(&h).await; // the deferral's own call
    h.engine.turn_ready(&a.thread_id, Some("r1")).await;
    in_flight(&h).await;

    // A second backend over the same file: the first one is "dead".
    let reborn = open_engine(&h.db).await;
    let rt = Arc::new(FakeRuntime::default());
    reborn.register_runtime(rt.clone());
    reborn.tick().await;
    let thread = reborn.store().thread(&a.thread_id).await.unwrap().unwrap();
    reborn
        .turn_finished(
            &a.thread_id,
            thread.active_run_id.clone(),
            TurnOutcome::completed(None),
            now_ms(),
        )
        .await;
    reborn.tick().await;
    reborn.tick().await;
    assert!(
        rt.starts_on(&a.thread_id).is_empty(),
        "replayed after the crash"
    );
    let ledger = reborn.thread_steers(&a.thread_id).await.unwrap();
    assert!(
        ledger
            .iter()
            .any(|s| s.text == "x" && s.state == SteerState::Unconfirmed),
        "{ledger:?}"
    );
}

/// Recovery storage fails at the turn's end: the run still ends (it does
/// not stay "running"), and the recovery is retried once storage works.
#[tokio::test]
async fn a_failed_recovery_still_ends_the_run_and_is_retried() {
    let (h, _gate) = gated(true, false, false).await;
    let a = running(&h, "a", "r1").await;
    h.engine.user_turn(&a, "words").await.unwrap();
    let conn = h.db.connect().unwrap();
    for (name, event) in [("fail_up", "UPDATE"), ("fail_in", "INSERT")] {
        conn.execute(
            &format!(
                "CREATE TRIGGER {name} BEFORE {event} ON orch_queue WHEN NEW.kind = 'recovered_steer' \
                 BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END"
            ),
            (),
        )
        .await
        .unwrap();
    }
    finish(&h.engine, &a.thread_id, TurnOutcome::completed(None)).await;
    let thread = h
        .engine
        .store()
        .thread(&a.thread_id)
        .await
        .unwrap()
        .unwrap();
    assert!(!thread.is_running(), "an ended run stayed running");
    assert!(queued(&h, &a.thread_id).await.is_empty());
    conn.execute("DROP TRIGGER fail_up", ()).await.unwrap();
    conn.execute("DROP TRIGGER fail_in", ()).await.unwrap();
    h.engine.tick().await;
    assert_eq!(h.rt.starts_on(&a.thread_id), vec!["words".to_string()]);
}

// ── 5: readiness ────────────────────────────────────────────────────────

/// `turn_ready` returns without calling back into the runtime, even while
/// the runtime holds the session lock its steers need; delivery follows
/// once the lock is free.
#[tokio::test]
async fn turn_ready_never_calls_back_into_the_runtime() {
    let (h, _gate) = gated(false, false, false).await;
    let a = running(&h, "a", "r1").await;
    h.rt.script_steers(&[SteerOutcome::NotReady]);
    h.engine.user_turn(&a, "x").await.unwrap();
    let session = h.rt.session.clone();
    let held = session.lock().await;
    tokio::time::timeout(
        Duration::from_secs(1),
        h.engine.turn_ready(&a.thread_id, Some("r1")),
    )
    .await
    .expect("turn_ready waited on the runtime");
    drop(held);
    until_queued(&h, &a.thread_id, 0).await;
    assert_eq!(steers_of(&h, &a.thread_id, "x"), 2);
}

/// Readiness is per run: another run's readiness delivers nothing.
#[tokio::test]
async fn readiness_of_another_run_delivers_nothing() {
    let (h, _gate) = gated(false, false, false).await;
    let a = running(&h, "a", "r1").await;
    h.rt.script_steers(&[SteerOutcome::NotReady]);
    h.engine.user_turn(&a, "x").await.unwrap();
    h.engine.turn_ready(&a.thread_id, Some("other")).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(queued(&h, &a.thread_id).await, vec!["x"]);
    assert_eq!(steers_of(&h, &a.thread_id, "x"), 1);
}

// ── 6: order without timing ─────────────────────────────────────────────

/// No pauses: messages enqueued within one millisecond of the steer keep
/// their places around it (the monotonic `seq`, not timestamps).
#[tokio::test]
async fn same_millisecond_messages_keep_their_order() {
    let (h, _gate) = gated(true, false, false).await;
    for i in 0..10 {
        let a = running(&h, &format!("a{i}"), "r1").await;
        h.engine
            .queue_add(&a.thread_id, "before", Author::User)
            .await
            .unwrap();
        h.engine.user_turn(&a, "steer").await.unwrap();
        h.engine
            .queue_add(&a.thread_id, "after", Author::User)
            .await
            .unwrap();
        finish(&h.engine, &a.thread_id, TurnOutcome::completed(None)).await;
        assert_eq!(
            queued(&h, &a.thread_id).await,
            vec!["before", "steer", "after"]
        );
    }
}

/// A fresh send right after a turn ended runs behind the steer that turn
/// was still waiting to take.
#[tokio::test]
async fn a_send_after_a_turn_end_runs_behind_its_deferred_steer() {
    let (h, _gate) = gated(false, false, false).await;
    let a = running(&h, "a", "r1").await;
    h.rt.script_steers(&[SteerOutcome::NotReady]);
    h.engine.user_turn(&a, "early").await.unwrap();
    finish(&h.engine, &a.thread_id, TurnOutcome::completed(None)).await;
    let idle = h
        .engine
        .store()
        .thread(&a.thread_id)
        .await
        .unwrap()
        .unwrap();
    let turn = h.engine.user_turn(&idle, "new").await.unwrap();
    assert!(matches!(turn, UserTurn::Queued { .. }), "{turn:?}");
    assert_eq!(h.rt.starts_on(&a.thread_id), vec!["early".to_string()]);
    assert_eq!(queued(&h, &a.thread_id).await, vec!["new"]);
}

/// A tick while a too-late steer is being settled cannot start an older
/// queued message first: the late words start the next turn.
#[tokio::test]
async fn a_tick_cannot_overtake_a_too_late_steer() {
    let (h, gate) = gated(false, true, false).await;
    let a = running(&h, "a", "r1").await;
    h.engine
        .queue_add(&a.thread_id, "older", Author::User)
        .await
        .unwrap();
    h.rt.script_steers(&[SteerOutcome::TurnEnded]);
    let sent = send(&h, &a, "late");
    in_flight(&h).await;
    finish(&h.engine, &a.thread_id, TurnOutcome::completed(None)).await;
    h.engine.tick().await;
    assert!(
        h.rt.starts_on(&a.thread_id).is_empty(),
        "the tick overtook it"
    );
    gate.add_permits(1);
    assert!(matches!(sent.await.unwrap(), UserTurn::Started { .. }));
    assert_eq!(h.rt.starts_on(&a.thread_id), vec!["late".to_string()]);
    assert_eq!(queued(&h, &a.thread_id).await, vec!["older"]);
}

// ── 7: one delivery per client action ───────────────────────────────────

/// A retried submit (same client action id) answers what the first one
/// did and never reaches the runtime again.
#[tokio::test]
async fn a_retried_submit_reaches_the_runtime_once() {
    let (h, _gate) = gated(true, false, false).await;
    let a = running(&h, "a", "r1").await;
    let first = h
        .engine
        .user_turn_once(&a, "x", Some("act-1"))
        .await
        .unwrap();
    let again = h
        .engine
        .user_turn_once(&a, "x", Some("act-1"))
        .await
        .unwrap();
    assert_eq!(first, again);
    assert_eq!(steers_of(&h, &a.thread_id, "x"), 1);
}

// ── 8: schema upgrade from two openers ──────────────────────────────────

/// Two backends upgrading one pre-steering database at once both succeed
/// (the loser of a column add sees "duplicate column", which is fine).
#[tokio::test]
async fn two_openers_upgrade_one_old_database() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("o.db"))
            .build()
            .await
            .unwrap(),
    );
    db.connect()
        .unwrap()
        .execute(
            "CREATE TABLE orch_queue (id TEXT PRIMARY KEY, thread_id TEXT NOT NULL, position REAL NOT NULL, \
             text TEXT NOT NULL, status TEXT NOT NULL, created_by TEXT NOT NULL, created_at INTEGER NOT NULL, \
             updated_at INTEGER NOT NULL, sent_at INTEGER)",
            (),
        )
        .await
        .unwrap();
    let store = |db: &Arc<libsql::Database>| {
        crate::store::Store::new(Arc::new(
            ironclaw_libsql_runtime::LibSqlRuntime::new(Arc::clone(db)).unwrap(),
        ))
    };
    let (s1, s2) = (store(&db), store(&db));
    let rt = RuntimeId::new("fake").unwrap();
    let (r1, r2) = tokio::join!(s1.migrate(&rt), s2.migrate(&rt));
    r1.unwrap();
    r2.unwrap();
    // A column another opener added between the check and the add.
    s1.exec("ALTER TABLE orch_queue ADD COLUMN probe TEXT", vec![])
        .await
        .unwrap();
    s2.add_column_once("orch_queue", "probe", "TEXT")
        .await
        .unwrap();
    let duplicate = s2
        .exec("ALTER TABLE orch_queue ADD COLUMN probe TEXT", vec![])
        .await
        .unwrap_err();
    assert!(
        duplicate.to_string().contains("duplicate column"),
        "{duplicate}"
    );
}
