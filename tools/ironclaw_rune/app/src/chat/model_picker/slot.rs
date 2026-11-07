//! How handlers and async replies find a model picker again: a chat's by
//! its thread id (the open chat and each side chat have their own, GPUI's
//! per-`ChatView` `ModelPicker`), the draft screen's and the automation
//! form's at a fixed place. Also which chat picker ⇧⌘M and Esc mean.

use std::sync::Arc;

use super::{ModelPickerState, PickerEvent};
use crate::state::State;

/// A picker that lives at a fixed place in the state (the draft screen's,
/// the automation form's): how handlers find it from the root, and where
/// its events go.
#[derive(Clone, Copy)]
pub struct FixedSlot {
    pub get: fn(&State) -> Option<&ModelPickerState>,
    pub get_mut: fn(&mut State) -> Option<&mut ModelPickerState>,
    pub event: fn(&mut State, PickerEvent),
}

/// How handlers and async replies find a picker again from the root state,
/// and where its events go. A chat's picker is found by its thread id
/// (`chat::chat_for`), so the open chat and each side chat have their own;
/// a reply for a chat that closed meanwhile finds nothing and is dropped.
#[derive(Clone)]
pub enum Slot {
    /// The picker of the chat showing this thread (main or side chat).
    Chat(Arc<str>),
    Fixed(FixedSlot),
}

impl Slot {
    /// The picker of the chat showing `thread_id`.
    pub fn chat(thread_id: &str) -> Self {
        Slot::Chat(Arc::from(thread_id))
    }

    pub fn get<'a>(&self, s: &'a State) -> Option<&'a ModelPickerState> {
        match self {
            Slot::Chat(id) => crate::chat::chat_ref(s, id).map(|c| &c.model_picker),
            Slot::Fixed(f) => (f.get)(s),
        }
    }

    pub fn get_mut<'a>(&self, s: &'a mut State) -> Option<&'a mut ModelPickerState> {
        match self {
            Slot::Chat(id) => crate::chat::chat_for(s, id).map(|c| &mut c.model_picker),
            Slot::Fixed(f) => (f.get_mut)(s),
        }
    }

    pub fn event(&self, s: &mut State, event: PickerEvent) {
        match self {
            Slot::Chat(id) => chat_event(s, id, event),
            Slot::Fixed(f) => (f.event)(s, event),
        }
    }

    /// An element key for this picker's pills: a side chat's sit beside
    /// the open chat's, and popovers anchor to the first key in tree order.
    pub(crate) fn key(&self, base: &str) -> String {
        match self {
            Slot::Chat(id) => format!("{base}:{id}"),
            Slot::Fixed(_) => base.to_string(),
        }
    }
}

/// The draft screen's picker, in `s.workspace.draft`. A failed pick shows
/// as the draft's error; a change needs nothing (the thread does not exist
/// yet, the pick applies when it is created).
pub const DRAFT: Slot = Slot::Fixed(FixedSlot {
    get: |s| s.workspace.draft.as_ref().map(|d| &d.picker),
    get_mut: |s| s.workspace.draft.as_mut().map(|d| &mut d.picker),
    event: |s, event| {
        if let (PickerEvent::Failed(message), Some(d)) = (event, s.workspace.draft.as_mut()) {
            d.error = Some(message);
        }
    },
});

/// The chat picker ⇧⌘M and Esc mean (GPUI `ChatView::focused_picker`): the
/// side chat's while its composer has focus or its picker is open, else
/// the open chat's.
pub fn focused_slot(s: &State) -> Option<Slot> {
    let main = s.chat.as_ref()?;
    if let Some(side) = main.side.chat.as_ref()
        && (side.state.composer.focused || side.state.model_picker.is_open())
    {
        return Some(Slot::chat(&side.thread_id));
    }
    Some(Slot::chat(&main.thread_id))
}

/// Esc: close whichever picker is open (the side chat's, the open chat's
/// or the draft's) and give its composer the keys back. True when one was.
pub fn close_open(s: &mut State) -> bool {
    let mut slots: Vec<Slot> = crate::chat::side::side_chat_ids(s)
        .iter()
        .map(|id| Slot::chat(id))
        .collect();
    slots.extend(s.chat.as_ref().map(|c| Slot::chat(&c.thread_id)));
    slots.push(DRAFT);
    for slot in slots {
        if let Some(p) = slot.get_mut(s)
            && p.is_open()
        {
            p.close();
            if let Slot::Chat(id) = &slot {
                crate::chat::focus_composer_of(s, id);
            }
            return true;
        }
    }
    false
}

/// A chat's reaction to its picker (GPUI `ChatView`'s `PickerEvent`
/// subscription); `id` is the chat's thread, main or side.
fn chat_event(s: &mut State, id: &str, event: PickerEvent) {
    match event {
        PickerEvent::Changed(change) => {
            // A fork opens the new thread; a switch (same thread, new
            // runtime) moves the live stream and gains the timeline marker;
            // an in-place change only gains the marker.
            crate::chat::picker_changed(s, id, change.fork_thread_id, &change.moved);
        }
        PickerEvent::Failed(message) => crate::chat::report_error(s, id, message),
    }
}

#[cfg(test)]
mod tests {
    use super::{Slot, close_open, focused_slot};
    use crate::chat::fixtures::{close_side_chat, test_state, with_side_chat};
    use crate::chat::model_picker::{PickerEvent, fixture};
    use crate::chat::{chat_ref, side};

    fn side_open(s: &crate::state::State) -> bool {
        side::side_chat(s, "sc2").is_some_and(|c| c.model_picker.is_open())
    }

    fn main_open(s: &crate::state::State) -> bool {
        s.chat.as_ref().is_some_and(|c| c.model_picker.is_open())
    }

    #[test]
    fn a_side_chats_slot_reads_and_writes_that_side_chat() {
        let mut s = test_state();
        with_side_chat(&mut s, "sc2");
        if let Some(c) = side::side_chat_mut(&mut s, "sc2") {
            c.model_picker = fixture();
            c.model_picker.thread_id = Some("sc2".into());
        }
        let slot = Slot::chat("sc2");
        assert_eq!(
            slot.get(&s).and_then(|p| p.thread_id.clone()).as_deref(),
            Some("sc2")
        );
        assert_eq!(
            slot.get(&s).map(|p| p.model_label()).as_deref(),
            Some("Claude Opus 5.5")
        );
        if let Some(p) = slot.get_mut(&mut s) {
            p.toggle();
        }
        assert!(side_open(&s));
        assert!(!main_open(&s), "the open chat's picker is its own");
        // The pills and popover anchor to keys of their own.
        assert_eq!(slot.key("model-pill"), "model-pill:sc2");
        assert_ne!(slot.key("model-pill"), Slot::chat("t1").key("model-pill"));
    }

    #[test]
    fn esc_and_the_shortcut_mean_the_focused_chats_picker() {
        let mut s = test_state();
        with_side_chat(&mut s, "sc2");
        assert!(matches!(focused_slot(&s), Some(Slot::Chat(id)) if &*id == "t1"));
        if let Some(c) = side::side_chat_mut(&mut s, "sc2") {
            c.composer.focused = true;
        }
        assert!(matches!(focused_slot(&s), Some(Slot::Chat(id)) if &*id == "sc2"));
        if let Some(c) = side::side_chat_mut(&mut s, "sc2") {
            c.composer.focused = false;
            c.model_picker.toggle();
        }
        // Open, it stays the one Esc closes; its composer gets the keys back.
        assert!(matches!(focused_slot(&s), Some(Slot::Chat(id)) if &*id == "sc2"));
        assert!(close_open(&mut s));
        assert!(!side_open(&s));
        assert!(side::side_chat(&s, "sc2").is_some_and(|c| c.composer.focused));
        assert!(!close_open(&mut s));
    }

    #[test]
    fn a_reply_for_a_closed_side_chat_goes_nowhere() {
        let mut s = test_state();
        with_side_chat(&mut s, "sc2");
        let slot = Slot::chat("sc2");
        close_side_chat(&mut s);
        assert!(slot.get_mut(&mut s).is_none());
        slot.event(&mut s, PickerEvent::Failed("route refused".into()));
        assert!(
            chat_ref(&s, "t1").is_some_and(|c| c.error.is_none()),
            "not the open chat's error"
        );
        // While it is open, its failure lands in its own error line.
        with_side_chat(&mut s, "sc2");
        slot.event(&mut s, PickerEvent::Failed("route refused".into()));
        assert_eq!(
            chat_ref(&s, "sc2").and_then(|c| c.error.clone()).as_deref(),
            Some("route refused")
        );
        assert!(chat_ref(&s, "t1").is_some_and(|c| c.error.is_none()));
    }
}
