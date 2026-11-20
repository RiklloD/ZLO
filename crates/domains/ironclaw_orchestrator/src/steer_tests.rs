//! Steering (t3code semantics): read state, unread-steer recovery, Stop,
//! too-late and not-yet-ready steers, automatic input into a busy parent.

use ironclaw_host_api::ids::ThreadId;
use serde_json::json;

use crate::model::*;
use crate::profile_runtime::UserTurn;
use crate::runtime::*;
use crate::tests::{FakeRuntime, Harness, ctx, finish, harness, harness_with, scope, tid};

fn steers(texts: &[&str]) -> Vec<UnreadSteer> {
    texts
        .iter()
        .enumerate()
        .map(|(i, text)| UnreadSteer {
            id: format!("steer-{i}"),
            text: (*text).to_string(),
        })
        .collect()
}

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

async fn reading_harness() -> Harness {
    harness_with(FakeRuntime {
        reads: true,
        ..FakeRuntime::default()
    })
    .await
}

/// Wait (bounded) until the thread's queue holds `len` items: readiness
/// delivers deferred steers from a task of its own.
async fn until_queued(h: &Harness, thread: &ThreadId, len: usize) {
    for _ in 0..200 {
        if queued(h, thread).await.len() == len {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("queue never reached {len}: {:?}", queued(h, thread).await);
}

/// Distinct millisecond stamps for "sent before / queued after".
async fn tick_clock() {
    tokio::time::sleep(std::time::Duration::from_millis(3)).await;
}

/// A steer the runtime accepted but its agent never read is not lost: it is
/// queued and sent as the next turn.
#[tokio::test]
async fn unread_steers_are_queued_as_the_next_turn() {
    let h = harness().await;
    let a = running(&h, "a", "r1").await;
    h.engine
        .steers_unread(&a.thread_id, "r1", steers(&["also fix the docs"]))
        .await;
    finish(
        &h.engine,
        &a.thread_id,
        TurnOutcome::completed(Some("done".into())),
    )
    .await;
    h.engine.tick().await;
    assert_eq!(
        h.rt.starts_on(&a.thread_id),
        vec!["also fix the docs".to_string()]
    );
}

/// Unread steers go back AHEAD of messages queued after they were sent,
/// several in their send order (PLAN 5), whether the engine finds them in
/// its ledger at the turn end or the runtime reports them.
#[tokio::test]
async fn unread_steers_requeue_ahead_of_later_messages_in_send_order() {
    let h = reading_harness().await;
    let a = running(&h, "a", "r1").await;
    h.engine
        .queue_add(&a.thread_id, "queued before", Author::User)
        .await
        .unwrap();
    tick_clock().await;
    let first = h.engine.user_turn(&a, "steer one").await.unwrap();
    assert!(
        matches!(
            first,
            UserTurn::Steered {
                waiting_for_turn: false,
                steer_id: Some(_),
                ..
            }
        ),
        "{first:?}"
    );
    tick_clock().await;
    h.engine.user_turn(&a, "steer two").await.unwrap();
    tick_clock().await;
    h.engine
        .queue_add(&a.thread_id, "queued after", Author::User)
        .await
        .unwrap();

    // The ledger shows both waiting to be read.
    let ledger = h.engine.thread_steers(&a.thread_id).await.unwrap();
    assert_eq!(ledger.len(), 2);
    assert!(ledger.iter().all(|s| s.waiting_to_be_read()), "{ledger:?}");
    assert!(ledger.iter().all(|s| s.run_id.as_deref() == Some("r1")));

    // The second is reported by the runtime (its own report path), the
    // first is found unread at the turn end.
    let second = ledger[1].id.clone();
    h.engine
        .steers_unread(
            &a.thread_id,
            "r1",
            vec![UnreadSteer {
                id: second.as_str().into(),
                text: "steer two".into(),
            }],
        )
        .await;
    finish(&h.engine, &a.thread_id, TurnOutcome::completed(None)).await;
    assert_eq!(
        queued(&h, &a.thread_id).await,
        vec!["queued before", "steer one", "steer two", "queued after"]
    );
    let ledger = h.engine.thread_steers(&a.thread_id).await.unwrap();
    assert!(ledger.iter().all(|s| s.state == SteerState::Requeued));
    // A repeated report never queues it twice.
    h.engine
        .steers_unread(
            &a.thread_id,
            "r1",
            vec![UnreadSteer {
                id: second.as_str().into(),
                text: "steer two".into(),
            }],
        )
        .await;
    assert_eq!(queued(&h, &a.thread_id).await.len(), 4);
}

/// A steer the agent read is part of its run: nothing goes back to the
/// queue, and the ledger no longer shows "waiting to be read" (PLAN 2).
#[tokio::test]
async fn a_read_steer_is_not_requeued() {
    let h = reading_harness().await;
    let a = running(&h, "a", "r1").await;
    let UserTurn::Steered {
        steer_id: Some(id), ..
    } = h.engine.user_turn(&a, "use tabs").await.unwrap()
    else {
        panic!("not steered");
    };
    assert_eq!(
        h.rt.steers.lock().unwrap()[0].1,
        id,
        "the runtime gets the engine id"
    );
    h.engine
        .steer_updated(&a.thread_id, "r1", &id, SteerReadState::Read, now_ms())
        .await;
    finish(&h.engine, &a.thread_id, TurnOutcome::completed(None)).await;
    assert!(queued(&h, &a.thread_id).await.is_empty());
    let ledger = h.engine.thread_steers(&a.thread_id).await.unwrap();
    assert_eq!(ledger[0].state, SteerState::Read);
    assert!(!ledger[0].waiting_to_be_read());
}

/// A steer the runtime wrote with no word either way (the agent exited,
/// the connection ended) is reported unconfirmed: the ledger keeps the
/// "Steer delivery not confirmed" notice and it is never resent, on a
/// runtime that reports reads and on one whose steers start read.
#[tokio::test]
async fn an_unconfirmed_steer_is_a_notice_and_never_requeued() {
    for reads in [true, false] {
        let h = if reads {
            reading_harness().await
        } else {
            harness().await
        };
        let a = running(&h, "a", "r1").await;
        let UserTurn::Steered {
            steer_id: Some(id), ..
        } = h.engine.user_turn(&a, "maybe read").await.unwrap()
        else {
            panic!("not steered");
        };
        h.engine
            .steer_updated(
                &a.thread_id,
                "r1",
                &id,
                SteerReadState::Unconfirmed,
                now_ms(),
            )
            .await;
        finish(&h.engine, &a.thread_id, TurnOutcome::completed(None)).await;
        assert!(queued(&h, &a.thread_id).await.is_empty(), "reads={reads}");
        let ledger = h.engine.thread_steers(&a.thread_id).await.unwrap();
        assert_eq!(ledger[0].state, SteerState::Unconfirmed, "reads={reads}");
        assert!(ledger[0].unconfirmed_notice().contains("maybe read"));
    }
}

/// A runtime that does not report reads: a delivered steer counts as read
/// and is never resent.
#[tokio::test]
async fn a_steer_on_a_runtime_without_read_reports_counts_as_read() {
    let h = harness().await;
    let a = running(&h, "a", "r1").await;
    h.engine.user_turn(&a, "use tabs").await.unwrap();
    finish(&h.engine, &a.thread_id, TurnOutcome::completed(None)).await;
    assert!(queued(&h, &a.thread_id).await.is_empty());
    let ledger = h.engine.thread_steers(&a.thread_id).await.unwrap();
    assert_eq!(ledger[0].state, SteerState::Read);
}

/// After a Stop an unread steer is not resent: the thread keeps a "Steer
/// delivery not confirmed" notice with its text (PLAN 5), for steers in the
/// ledger and steers the runtime reports by its own ids.
#[tokio::test]
async fn after_a_stop_unread_steers_become_unconfirmed_notices() {
    let h = reading_harness().await;
    let a = running(&h, "a", "r1").await;
    h.engine.user_turn(&a, "first").await.unwrap();
    h.engine.on_user_stop(&a.thread_id).await.unwrap();
    h.engine
        .steers_unread(&a.thread_id, "r1", steers(&["second"]))
        .await;
    finish(
        &h.engine,
        &a.thread_id,
        TurnOutcome::ended(TurnEndStatus::Cancelled, None),
    )
    .await;
    h.engine.on_user_prompt(&a.thread_id).await.unwrap();
    h.engine.tick().await;
    assert!(queued(&h, &a.thread_id).await.is_empty(), "not requeued");
    assert!(h.rt.starts_on(&a.thread_id).is_empty());
    let ledger = h.engine.thread_steers(&a.thread_id).await.unwrap();
    let notes: Vec<_> = ledger
        .iter()
        .filter(|s| s.state == SteerState::Unconfirmed)
        .map(|s| s.unconfirmed_notice())
        .collect();
    assert_eq!(notes.len(), 2, "{ledger:?}");
    assert!(notes[0].contains("\"first\"") && notes[1].contains("\"second\""));
}

/// A new user send right after a turn ended places that turn's unread
/// steers before itself (PLAN 5).
#[tokio::test]
async fn a_new_send_runs_after_the_last_turns_unread_steers() {
    let h = reading_harness().await;
    let a = running(&h, "a", "r1").await;
    h.engine.user_turn(&a, "unread steer").await.unwrap();
    finish(&h.engine, &a.thread_id, TurnOutcome::completed(None)).await;
    let idle = h
        .engine
        .store()
        .thread(&a.thread_id)
        .await
        .unwrap()
        .unwrap();
    let turn = h.engine.user_turn(&idle, "new message").await.unwrap();
    assert!(matches!(turn, UserTurn::Queued { .. }), "{turn:?}");
    assert_eq!(
        h.rt.starts_on(&a.thread_id),
        vec!["unread steer".to_string()]
    );
    assert_eq!(queued(&h, &a.thread_id).await, vec!["new message"]);
}

/// A steer that is too late (the runtime no longer has the turn) starts the
/// next turn right away instead of waiting behind queued messages (PLAN 4).
#[tokio::test]
async fn a_too_late_steer_starts_the_next_turn_right_away() {
    let h = harness().await;
    let a = running(&h, "a", "r1").await;
    h.engine
        .queue_add(&a.thread_id, "older queued", Author::User)
        .await
        .unwrap();
    h.rt.script_steers(&[SteerOutcome::TurnEnded]);
    let engine = h.engine.clone();
    let thread = a.thread_id.clone();
    let ender = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        finish(&engine, &thread, TurnOutcome::completed(None)).await;
    });
    let turn = h.engine.user_turn(&a, "late words").await.unwrap();
    ender.await.unwrap();
    assert!(matches!(turn, UserTurn::Started { .. }), "{turn:?}");
    assert_eq!(h.rt.starts_on(&a.thread_id), vec!["late words".to_string()]);
    assert_eq!(queued(&h, &a.thread_id).await, vec!["older queued"]);

    // A plain refusal from a turn that ended meanwhile starts it too.
    let b = running(&h, "b", "r2").await;
    h.rt.script_steers(&[SteerOutcome::Rejected]);
    finish(&h.engine, &b.thread_id, TurnOutcome::completed(None)).await;
    let turn = h.engine.user_turn(&b, "after end").await.unwrap();
    assert!(matches!(turn, UserTurn::Started { .. }), "{turn:?}");
}

/// A steer the turn cannot take yet is held and delivered once the runtime
/// reports the turn ready, not turned into a later turn (PLAN 3); if the
/// turn ends first it runs as the next turn.
#[tokio::test]
async fn a_steer_before_the_turn_is_ready_is_delivered_when_it_is() {
    let h = harness().await;
    let a = running(&h, "a", "r1").await;
    h.rt.script_steers(&[SteerOutcome::NotReady, SteerOutcome::NotReady]);
    let turn = h.engine.user_turn(&a, "hold on").await.unwrap();
    let UserTurn::Steered {
        waiting_for_turn: true,
        queued_id: Some(queued_id),
        ..
    } = turn
    else {
        panic!("not deferred: {turn:?}");
    };
    let view = h.engine.queue_list(&a.thread_id).await.unwrap();
    assert_eq!(view["items"][0]["kind"], json!("deferred_steer"));
    // Still not ready on a tick: it keeps waiting.
    h.engine.tick().await;
    assert_eq!(queued(&h, &a.thread_id).await, vec!["hold on"]);
    // Ready: delivered into the same run.
    h.engine.turn_ready(&a.thread_id, Some("r1")).await;
    until_queued(&h, &a.thread_id, 0).await;
    assert_eq!(h.rt.steered_on(&a.thread_id).len(), 3);
    let item = h
        .engine
        .store()
        .queue_item(&a.thread_id, &queued_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(item.status, QueueStatus::Sent);
    assert!(h.rt.starts_on(&a.thread_id).is_empty());

    // The turn ends before it is ready: the message runs next.
    let b = running(&h, "b", "r2").await;
    h.rt.script_steers(&[SteerOutcome::NotReady]);
    h.engine.user_turn(&b, "too early").await.unwrap();
    finish(&h.engine, &b.thread_id, TurnOutcome::completed(None)).await;
    h.engine.tick().await;
    assert_eq!(h.rt.starts_on(&b.thread_id), vec!["too early".to_string()]);
}

/// `t3_queue promote` into a turn that is not ready yet defers the steer
/// instead of failing; an explicit `t3_thread send mode=steer` with no
/// steerable run stays an error, and `auto` steers when it can.
#[tokio::test]
async fn promote_and_agent_send_keep_t3code_semantics() {
    let h = harness().await;
    let a = running(&h, "a", "r1").await;
    let added = h
        .engine
        .queue_add(&a.thread_id, "promote me", Author::User)
        .await
        .unwrap();
    let id = QueueItemId::new(added["queuedId"].as_str().unwrap()).unwrap();
    h.rt.script_steers(&[SteerOutcome::NotReady]);
    let view = h
        .engine
        .queue_promote(&a, &id, SteerHow::Steer)
        .await
        .unwrap();
    assert_eq!(view["steerStatus"], json!("waiting_for_turn"));
    assert_eq!(view["items"][0]["kind"], json!("deferred_steer"));
    h.engine.turn_ready(&a.thread_id, Some("r1")).await;
    until_queued(&h, &a.thread_id, 0).await;

    // Agent sends.
    let idle = tid("idle");
    h.engine.ensure_thread(&idle, &scope(), None).await.unwrap();
    let err = h
        .engine
        .send(&ctx("caller"), &idle, "x", SendMode::Steer)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "no_active_run");
    let sent = h
        .engine
        .send(&ctx("caller"), &a.thread_id, "auto words", SendMode::Auto)
        .await
        .unwrap();
    assert_eq!(sent["delivery"], json!("steered"));
    h.rt.script_steers(&[SteerOutcome::NotReady]);
    let sent = h
        .engine
        .send(&ctx("caller"), &a.thread_id, "early", SendMode::Auto)
        .await
        .unwrap();
    assert_eq!(sent["status"], json!("steer_when_ready"));
    let started = h
        .engine
        .send(&ctx("caller"), &idle, "go", SendMode::Auto)
        .await
        .unwrap();
    assert_eq!(started["delivery"], json!("started"));
}

/// A delegated result steers a busy parent whose runtime steers without
/// cancelling tool calls (PLAN 8); on a runtime that would cancel them it
/// waits for the parent's turn to end, and never restarts it.
#[tokio::test]
async fn child_results_steer_a_busy_parent_only_on_gentle_runtimes() {
    for gentle in [true, false] {
        let h = harness_with(FakeRuntime {
            gentle,
            ..FakeRuntime::default()
        })
        .await;
        let child = h
            .engine
            .delegate(
                &ctx("root"),
                serde_json::from_value(json!({"task": "look it up"})).unwrap(),
            )
            .await
            .unwrap();
        let child_thread = tid(child["childThreadId"].as_str().unwrap());
        let root = running(&h, "root", "parent-run").await;
        finish(
            &h.engine,
            &child_thread,
            TurnOutcome::completed(Some("found it".into())),
        )
        .await;
        h.engine.tick().await;
        let steered = h.rt.steered_on(&root.thread_id);
        assert!(
            h.rt.starts_on(&root.thread_id).is_empty(),
            "never a new turn mid-run"
        );
        if gentle {
            assert_eq!(steered.len(), 1);
            assert!(steered[0].contains("found it"), "{steered:?}");
            // Delivered once: the parent's next idle tick starts nothing.
            finish(&h.engine, &root.thread_id, TurnOutcome::completed(None)).await;
            h.engine.tick().await;
            assert!(h.rt.starts_on(&root.thread_id).is_empty());
        } else {
            assert!(steered.is_empty(), "{steered:?}");
            finish(&h.engine, &root.thread_id, TurnOutcome::completed(None)).await;
            h.engine.tick().await;
            let starts = h.rt.starts_on(&root.thread_id);
            assert_eq!(starts.len(), 1);
            assert!(starts[0].contains("found it"));
        }
    }
}

/// The queue write fails for the second steer of a batch (every retry):
/// the first is queued, the second comes back to the runtime as
/// undelivered instead of vanishing, and reporting the same batch again
/// (a duplicate or late recovery) never queues the first twice.
#[tokio::test]
async fn a_steer_that_cannot_be_queued_is_reported_back_and_recovery_is_idempotent() {
    let h = harness().await;
    let a = tid("a");
    h.engine.ensure_thread(&a, &scope(), None).await.unwrap();
    h.db.connect()
        .unwrap()
        .execute(
            "CREATE TRIGGER fail_second BEFORE INSERT ON orch_queue WHEN NEW.text = 'second' \
             BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END",
            (),
        )
        .await
        .unwrap();
    let batch = steers(&["first", "second"]);
    let recovery = h.engine.steers_unread(&a, "r1", batch.clone()).await;
    assert_eq!(recovery.undelivered, vec![batch[1].clone()]);
    assert_eq!(queued(&h, &a).await, vec!["first".to_string()]);

    let again = h.engine.steers_unread(&a, "r1", batch.clone()).await;
    assert_eq!(again.undelivered, vec![batch[1].clone()]);
    assert_eq!(
        queued(&h, &a).await,
        vec!["first".to_string()],
        "not queued twice"
    );

    // The runtime's outcome names what was lost.
    let outcome =
        TurnOutcome::completed(Some("done".into())).with_undelivered_steers(&recovery.undelivered);
    assert_eq!(outcome.status, TurnEndStatus::Failed);
    let error = outcome.error.unwrap();
    assert!(
        error.contains("Not delivered") && error.contains("\"second\""),
        "{error}"
    );
}

/// A runtime can report a run's end before its admission returns (Stop
/// while the turn prepares): the admission bookkeeping that follows must
/// not mark the ended run running again or leave live work behind.
#[tokio::test]
async fn a_run_reported_ended_before_admission_returns_stays_ended() {
    let h = harness().await;
    let a = tid("a");
    h.engine.ensure_thread(&a, &scope(), None).await.unwrap();
    *h.rt.next_run.lock().unwrap() = Some("early".into());
    h.engine
        .turn_finished(
            &a,
            Some("early".into()),
            TurnOutcome::completed(None),
            now_ms(),
        )
        .await;
    h.engine.steers_unread(&a, "r0", steers(&["go"])).await;
    h.engine.tick().await;
    assert_eq!(h.rt.starts_on(&a), vec!["go".to_string()]);
    let thread = h.engine.store().thread(&a).await.unwrap().unwrap();
    assert!(!thread.is_running(), "{:?}", thread.turn_state);
    assert!(
        h.engine
            .store()
            .live_work()
            .await
            .unwrap()
            .iter()
            .all(|w| w.thread_id != a),
        "live work left for an ended run"
    );
}
