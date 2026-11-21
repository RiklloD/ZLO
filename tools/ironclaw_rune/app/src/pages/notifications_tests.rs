//! The person's archives and reads show on the click and are taken back,
//! with motion, when the gateway refuses them; listings older than a
//! change never overwrite it.

use super::*;
use crate::pages::motion_b_tests::{at_rest, frames, live, scene, settle};

fn page(s: &State, without: Option<&str>) -> ProductListNotificationsResponse {
    let notifications: Vec<_> = s
        .pages
        .notifications
        .items
        .iter()
        .filter(|n| Some(n.id.as_str()) != without)
        .cloned()
        .collect();
    ProductListNotificationsResponse {
        unread_count: notifications.iter().filter(|n| n.read_at.is_none()).count(),
        notifications,
        next_cursor: None,
    }
}

fn asked(s: &State, change: &Change) -> jobs::Generation {
    s.pages.notifications.pending[change].asked
}

fn shown_ids(s: &State) -> Vec<String> {
    s.pages
        .notifications
        .shown()
        .map(|n| n.id.clone())
        .collect()
}

#[test]
fn a_refused_archive_brings_the_row_back_with_motion() {
    let mut rt = scene("notifications");
    crate::pages::motion_b_tests::click_in(&mut rt, "notification-n1", "archive");
    frames(&mut rt, 2);
    assert!(
        live(&rt, "notification-n1").is_none(),
        "folds away on the click"
    );
    assert_eq!(
        rt.state.pages.notifications.items.len(),
        4,
        "records untouched"
    );
    let change = Change::Archive("n1".into());
    let at = asked(&rt.state, &change);
    rt.update(|s, tx| change_landed(s, &change, at, Err("refused".into()), tx));
    frames(&mut rt, 2);
    let row = live(&rt, "notification-n1").expect("the row is back");
    assert!(
        row.opacity < 0.99,
        "it comes back with motion: {}",
        row.opacity
    );
    assert_eq!(
        rt.state.pages.notifications.error.as_deref(),
        Some("refused")
    );
    settle(&mut rt);
    assert!(at_rest(&mut rt));
}

#[test]
fn a_refused_mark_all_read_brings_the_unread_dots_back() {
    let mut rt = scene("notifications");
    let dot = |rt: &Runtime| {
        rt.elements().into_iter().any(|e| {
            !e.exiting && e.key.as_deref() == Some("unread") && e.path.contains("notification-n1")
        })
    };
    assert!(dot(&rt));
    crate::pages::motion_b_tests::click(&mut rt, "mark-all-read");
    settle(&mut rt);
    assert!(!dot(&rt), "shown read on the click");
    assert!(
        rt.state.pages.notifications.items[0].read_at.is_none(),
        "the record stays unread until the gateway says so"
    );
    let at = asked(&rt.state, &Change::ReadAll);
    rt.update(|s, tx| change_landed(s, &Change::ReadAll, at, Err("refused".into()), tx));
    settle(&mut rt);
    assert!(dot(&rt), "unread again");
    assert!(at_rest(&mut rt));
}

type Runtime = rune::Runtime<State>;

#[test]
fn a_listing_older_than_an_archive_never_brings_the_row_back() {
    let mut s = crate::chat::fixtures::test_state();
    super::fixture(&mut s);
    s.fixture = true;
    // A listing goes out, then the person archives n1.
    refresh(&mut s);
    let old = s.pages.notifications.list_gen;
    archive(&mut s, "n1".into());
    let stale = page(&s, None);
    list_landed(&mut s, old, Ok(stale));
    assert!(
        !shown_ids(&s).contains(&"n1".to_string()),
        "the old answer is dropped"
    );

    // A listing asked after the archive but before the gateway applied it
    // still shows n1: the archive in flight keeps it hidden.
    refresh(&mut s);
    let newer = s.pages.notifications.list_gen;
    let with_n1 = page(&s, None);
    list_landed(&mut s, newer, Ok(with_n1));
    assert!(!shown_ids(&s).contains(&"n1".to_string()));

    // The gateway confirms; the listing after it no longer has n1 and takes
    // over from the pending entry.
    let change = Change::Archive("n1".into());
    let at = asked(&s, &change);
    let mut tx = Tx::default();
    change_landed(&mut s, &change, at, Ok(()), &mut tx);
    let confirming = s.pages.notifications.list_gen;
    let without = page(&s, Some("n1"));
    list_landed(&mut s, confirming, Ok(without));
    assert!(
        s.pages.notifications.pending.is_empty(),
        "the listing carries it now"
    );
    assert_eq!(shown_ids(&s), ["n2", "n3", "n4"]);
}

#[test]
fn a_late_refusal_of_an_earlier_change_leaves_a_newer_one_alone() {
    let mut s = crate::chat::fixtures::test_state();
    super::fixture(&mut s);
    s.fixture = true;
    archive(&mut s, "n1".into());
    let change = Change::Archive("n1".into());
    let first = asked(&s, &change);
    archive(&mut s, "n1".into());
    let mut tx = Tx::default();
    change_landed(&mut s, &change, first, Err("refused".into()), &mut tx);
    assert!(
        !shown_ids(&s).contains(&"n1".to_string()),
        "the second archive still owns the row"
    );
}
