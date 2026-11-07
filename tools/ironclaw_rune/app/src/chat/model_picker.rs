//! The model picker popover (GPUI `chat/model_picker.rs`). Area H.
//!
//! FOUNDATION STUB, replaced by its port (see PLAN.md for the owner).

#[derive(Default)]
pub struct ModelPickerState {}

use rune::{Cx, El, el};
use rune_kit::Kit;

use crate::state::State;

// PORT-STUB(area H): ⇧⌘M — toggle the composer's model picker: the open
// thread's on `Page::Chat`, the draft's (`s.workspace.draft.picker`) on
// `Page::Draft`.
pub fn open(_s: &mut State) {}

// PORT-STUB(area H): Esc — close whichever model or thinking picker is open
// (chat or draft); true when one was.
pub fn close_open(_s: &mut State) -> bool {
    false
}

// PORT-STUB(area H): the new-thread screen's picker follows its profile
// (GPUI `ModelPicker::set_profile`); `catalog` is any thread id to read the
// model catalog through.
pub fn draft_set_profile(_s: &mut State, _profile: String, _catalog: Option<String>) {}

// PORT-STUB(area H): the picked (provider, model) not yet applied (GPUI
// `ModelPicker::pending`).
pub fn pending(_picker: &ModelPickerState) -> Option<(String, String)> {
    None
}

// PORT-STUB(area H): the picked effort not yet applied (GPUI
// `ModelPicker::pending_effort`).
pub fn pending_effort(_picker: &ModelPickerState) -> Option<String> {
    None
}

// PORT-STUB(area H): the draft composer's model pill and its popover (in
// `layer`), reading `s.workspace.draft.picker`.
pub fn draft_pill(_s: &State, _cx: &mut Cx<State>, _k: &Kit, _layer: &mut Vec<El>) -> El {
    el()
}
