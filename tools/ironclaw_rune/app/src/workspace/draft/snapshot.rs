//! Where a SnapShot lands: the open thread's composer, or the new-thread
//! draft when no thread is open (or the draft is what is showing). (GPUI
//! `workspace/draft/snapshot.rs`.) The snapshot area (F) calls this.

use crate::snapshot::{self, Snapshot};
use crate::state::{Page, State};

pub fn stage_snapshot(s: &mut State, snap: Result<Snapshot, String>) {
    if super::super::in_settings(s) {
        super::super::back_to_threads(s);
    }
    if s.page == Page::Chat && s.chat.is_some() {
        crate::chat::stage_snapshot(s, snap);
        return;
    }
    super::open(s);
    let limits = s.session.as_ref().map(|s| s.attachments.clone());
    let Some(draft) = s.workspace.draft.as_mut() else {
        return;
    };
    let from = draft.staged.len();
    let result = match limits {
        Some(limits) => snap.and_then(|snap| {
            snapshot::stage_into(&mut s.snapshot, &mut draft.staged, snap, &limits)
        }),
        None => Err("ZLO is not connected to IronClaw yet.".into()),
    };
    draft.error = result.err();
    let new = draft.staged.split_off(from);
    super::place(s, new);
}
