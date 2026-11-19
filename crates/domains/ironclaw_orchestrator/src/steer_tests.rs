//! Steers a runtime accepted into a turn but its agent never read
//! (`TurnEventSink::steers_unread`): queued, then sent as later turns.

use crate::model::now_ms;
use crate::runtime::*;
use crate::tests::{finish, harness, scope, tid};

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

async fn queued(
    h: &crate::tests::Harness,
    thread: &ironclaw_host_api::ids::ThreadId,
) -> Vec<String> {
    h.engine
        .store()
        .queue(thread, false)
        .await
        .unwrap()
        .into_iter()
        .map(|q| q.text)
        .collect()
}

/// A steer the runtime accepted but its agent never read is not lost: it is
/// queued and sent as the next turn, after a Stop only once released.
#[tokio::test]
async fn unread_steers_are_queued_as_the_next_turn() {
    let h = harness().await;
    let a = tid("a");
    h.engine.ensure_thread(&a, &scope(), None).await.unwrap();
    h.engine
        .store()
        .set_turn_running(&a, Some("r1"), now_ms())
        .await
        .unwrap();
    h.engine
        .steers_unread(&a, "r1", steers(&["also fix the docs"]))
        .await;
    finish(&h.engine, &a, TurnOutcome::completed(Some("done".into()))).await;
    h.engine.tick().await;
    assert_eq!(h.rt.starts_on(&a), vec!["also fix the docs".to_string()]);

    // Stopped: the steer waits in the queue behind the hold.
    let b = tid("b");
    h.engine.ensure_thread(&b, &scope(), None).await.unwrap();
    h.engine
        .store()
        .set_turn_running(&b, Some("r2"), now_ms())
        .await
        .unwrap();
    h.engine.on_user_stop(&b).await.unwrap();
    h.engine
        .steers_unread(&b, "r2", steers(&["first", "second"]))
        .await;
    finish(
        &h.engine,
        &b,
        TurnOutcome::ended(TurnEndStatus::Cancelled, None),
    )
    .await;
    h.engine.tick().await;
    assert!(h.rt.starts_on(&b).is_empty(), "held by the Stop");
    assert_eq!(
        queued(&h, &b).await,
        vec!["first".to_string(), "second".to_string()]
    );
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
