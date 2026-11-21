//! The Memory page's selection pill glides to the document the person
//! opened (the reply to their click lands with motion) and keeps its
//! identity; the page draws nothing once it lands.

use crate::pages::motion_b_tests::{at_rest, between, frames, live, scene, settle};

#[test]
fn the_document_pill_glides_to_the_opened_document() {
    let mut rt = scene("memory");
    let before = live(&rt, "memory-doc-pill").expect("MEMORY.md is open");
    // The reply to the person's click on USER.md (a fixture sends no
    // request): `open_document`'s reply opens it in an animated commit.
    rt.update(|s, tx| {
        tx.animated();
        let m = &mut s.pages.memory;
        if let Some(open) = m.open.as_mut() {
            open.document.path = "SOUL.md".into();
        }
    });
    frames(&mut rt, 3);
    let mid = live(&rt, "memory-doc-pill").expect("one pill");
    assert_eq!(mid.path, before.path, "the same pill moves");
    assert!(mid.target[1] > before.rect[1] + 1., "SOUL.md is lower");
    assert!(
        between(mid.rect[1], before.rect[1], mid.target[1]),
        "seen gliding: {} between {} and {}",
        mid.rect[1],
        before.rect[1],
        mid.target[1]
    );
    settle(&mut rt);
    let row = live(&rt, "memory-doc-SOUL.md").expect("row");
    let end = live(&rt, "memory-doc-pill").expect("pill");
    assert!(
        (end.rect[1] - row.rect[1]).abs() < 0.5,
        "it lands on its row"
    );
    assert!(
        (end.rect[3] - row.rect[3]).abs() < 0.5,
        "as tall as the row"
    );
    assert!(at_rest(&mut rt));
}
