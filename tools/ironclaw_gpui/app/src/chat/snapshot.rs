//! A SnapShot staged into the open thread's composer, next to any files
//! the paperclip staged (`crate::snapshot` builds it).

use super::ChatView;
use crate::snapshot::{self, Snapshot};
use crate::ui::*;

impl ChatView {
    pub(crate) fn stage_snapshot(
        &mut self,
        snap: Result<Snapshot, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let from = self.staged.len();
        let result =
            snap.and_then(|snap| snapshot::stage_into(&mut self.staged, snap, &self.limits, cx));
        self.error = result.err().map(Into::into);
        let new = self.staged.split_off(from);
        self.place(new, window, cx);
        cx.notify();
    }
}
