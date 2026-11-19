//! Steers a runtime accepted into a turn but its agent never read
//! (`TurnEventSink::steers_unread`): queued, then sent as later turns.

use crate::model::now_ms;
use crate::runtime::*;
use crate::tests::{finish, harness, scope, tid};

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
        .steers_unread(&a, vec!["also fix the docs".into()])
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
        .steers_unread(&b, vec!["first".into(), "second".into()])
        .await;
    finish(
        &h.engine,
        &b,
        TurnOutcome::ended(TurnEndStatus::Cancelled, None),
    )
    .await;
    h.engine.tick().await;
    assert!(h.rt.starts_on(&b).is_empty(), "held by the Stop");
    let queued: Vec<String> = h
        .engine
        .store()
        .queue(&b, false)
        .await
        .unwrap()
        .into_iter()
        .map(|q| q.text)
        .collect();
    assert_eq!(queued, vec!["first".to_string(), "second".to_string()]);
}
