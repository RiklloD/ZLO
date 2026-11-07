//! The model picker popover (GPUI `chat/model_picker.rs`). Area H.
//!
//! FOUNDATION STUB, replaced by its port (see PLAN.md for the owner).

#[derive(Default)]
pub struct ModelPickerState {}

// ---- Called by the chat (area A). PORT-STUB(area H): replace with the port
// of `chat/model_picker.rs`; keep the signatures. A change reports back
// through `crate::chat::picker_changed`, a failure through
// `crate::chat::report_error`.

use rune::El;
use rune_kit::Kit;

use crate::chat::ChatState;
use crate::state::State;

/// PORT-STUB(area H): icon-only pills (a side chat's narrow composer).
pub fn set_compact(_picker: &mut ModelPickerState, _compact: bool) {}

/// PORT-STUB(area H): the chat opened: load the thread's model.
pub fn opened(_s: &mut State, _id: &str) {}

/// PORT-STUB(area H): the composer's model pill (and its panel in `layer`).
pub fn pill(_s: &State, _chat: &ChatState, _k: &Kit, _layer: &mut Vec<El>) -> El {
    rune::el()
}

/// "claude-opus-5-5" → "Claude Opus 5.5", for the switch marker.
pub fn pretty_model(id: &str) -> String {
    let tail = id.rsplit('/').next().unwrap_or(id);
    let mut out: Vec<String> = Vec::new();
    for part in tail.split(['-', '_']) {
        if part.is_empty() {
            continue;
        }
        let numeric = part.chars().all(|c| c.is_ascii_digit() || c == '.');
        match out.last_mut() {
            Some(last) if numeric && last.chars().all(|c| c.is_ascii_digit() || c == '.') => {
                last.push('.');
                last.push_str(part);
            }
            _ if part.eq_ignore_ascii_case("gpt") => out.push("GPT".into()),
            _ => {
                let mut chars = part.chars();
                let first = chars.next().map(|c| c.to_ascii_uppercase());
                out.push(first.into_iter().chain(chars).collect());
            }
        }
    }
    if out.is_empty() {
        id.to_string()
    } else {
        out.join(" ")
    }
}
