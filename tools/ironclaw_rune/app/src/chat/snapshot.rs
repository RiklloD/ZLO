//! A SnapShot staged into the open thread's composer, next to any files the
//! paperclip staged (GPUI `chat/snapshot.rs`; `crate::snapshot` builds it).

use rune::Tx;

use crate::snapshot::{self, Snapshot};
use crate::state::State;

/// Stage `snap` (or say why it could not be taken) in `thread`'s composer.
/// Area F's snapshot flow calls this when a capture lands.
pub fn stage_snapshot(s: &mut State, thread: &str, snap: Result<Snapshot, String>, _tx: &mut Tx) {
    let Some(c) = super::chat_for(s, thread) else { return };
    let from = c.staged.len();
    let limits = c.limits.clone();
    let mut staged = std::mem::take(&mut c.staged);
    let result = snap.and_then(|snap| snapshot::stage_into(s, &mut staged, snap, &limits));
    let Some(c) = super::chat_for(s, thread) else { return };
    c.staged = staged;
    c.error = result.err();
    let new = c.staged.split_off(from);
    super::actions::place(s, thread, new);
}
