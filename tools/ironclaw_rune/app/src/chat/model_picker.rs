//! The model picker popover (GPUI `chat/model_picker.rs`). Area H.
//!
//! FOUNDATION STUB, replaced by its port (see PLAN.md for the owner).

#[derive(Default)]
pub struct ModelPickerState {}

use rune::{El, text};
use rune_kit::Kit;

use crate::state::State;

/// Where a standalone picker lives in the state tree (handlers reach it from
/// the root).
pub type PickerAt = fn(&mut State) -> Option<&mut StandalonePicker>;

// PORT-STUB(area H): GPUI `ModelPicker::new(backend, None, catalog_thread)`
// used outside a chat (the automation form): its own pick and effort, a
// profile to read defaults from, and a `Failed(message)` report the owner
// shows (GPUI `PickerEvent::Failed`).
#[derive(Default)]
pub struct StandalonePicker {
    /// Any thread, to read the per-user catalog through.
    pub catalog_thread: Option<String>,
    pub profile: String,
}

impl StandalonePicker {
    pub fn new(catalog_thread: Option<String>) -> Self {
        Self {
            catalog_thread,
            profile: String::new(),
        }
    }

    /// The picked (provider, model), if the user picked one.
    pub fn pending(&self) -> Option<(String, String)> {
        None
    }

    pub fn pending_effort(&self) -> Option<String> {
        None
    }
}

// PORT-STUB(area H): GPUI `ModelPicker::set_profile`.
pub fn set_profile(s: &mut State, at: PickerAt, profile: String) {
    if let Some(p) = at(s) {
        p.profile = profile;
    }
}

// PORT-STUB(area H): the picker's trigger and popover for a standalone
// picker; `layer` takes the popover.
pub fn standalone_view(
    s: &State,
    _picker: &StandalonePicker,
    _at: PickerAt,
    _k: &Kit,
    _layer: &mut Vec<El>,
) -> El {
    text("Model: profile default").color(s.palette.text_muted)
}

// PORT-STUB(area H): GPUI `chat::model_picker::pretty_model`, ported
// verbatim; `anthropic/claude-sonnet-4-5` → "Claude Sonnet 4.5".
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
