//! Where a SnapShot lands: the open thread's composer, or the new-thread
//! draft when no thread is open (or the draft is what is showing).

use crate::snapshot::{self, Snapshot};
use crate::ui::*;
use crate::workspace::{Page, Workspace};

impl Workspace {
    pub(crate) fn stage_snapshot(
        &mut self,
        snap: Result<Snapshot, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.in_settings() {
            self.back_to_threads(window, cx);
        }
        if self.page == Page::Chat
            && let Some((_, chat)) = self.active.clone()
        {
            chat.update(cx, |chat, cx| chat.stage_snapshot(snap, cx));
            cx.notify();
            return;
        }
        self.open_draft(window, cx);
        let limits = self.session.as_ref().map(|s| s.attachments.clone());
        let Some(draft) = self.draft.as_mut() else {
            return;
        };
        let result = match limits {
            Some(limits) => {
                snap.and_then(|snap| snapshot::stage_into(&mut draft.staged, snap, &limits, cx))
            }
            None => Err("Numoh is not connected to IronClaw yet.".into()),
        };
        draft.error = result.err().map(Into::into);
        cx.notify();
    }
}
