//! Screen snapshots sent to the agent (global hotkey, capture, AX tree).
//!
//! FOUNDATION STUB. The snapshot port (PLAN.md, area F) replaces this.

use rune::Tx;
use serde::{Deserialize, Serialize};

use crate::state::State;

#[derive(Default)]
pub struct SnapshotState {
    // PORT-STUB(area F): Settings → General → SnapShots reads and edits these
    // (GPUI `SnapshotPrefs::of/update`, saved as `snapshots.json`).
    pub prefs: SnapshotPrefs,
}

pub fn poll(_s: &mut State, _tx: &mut Tx) -> bool {
    false
}

// PORT-STUB(area F): GPUI `snapshot::FALLBACKS`, the fallback combos
// Settings offers, as `hotkey` strings.
pub const FALLBACKS: [(&str, &str); 4] = [
    ("none", "Off"),
    ("ctrl-alt-s", "⌃⌥S"),
    ("cmd-shift-2", "⇧⌘2"),
    ("ctrl-alt-cmd-s", "⌃⌥⌘S"),
];

// PORT-STUB(area F): GPUI `snapshot::SnapshotPrefs`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SnapshotPrefs {
    pub enabled: bool,
    pub include_text: bool,
    pub fallback: String,
}

impl Default for SnapshotPrefs {
    fn default() -> Self {
        Self {
            enabled: false,
            include_text: true,
            fallback: "ctrl-alt-s".into(),
        }
    }
}

// PORT-STUB(area F): GPUI `SnapshotPrefs::update` (save, re-arm the trigger).
pub fn update_prefs(s: &mut State, edit: impl FnOnce(&mut SnapshotPrefs)) {
    edit(&mut s.snapshot.prefs);
}

// PORT-STUB(area F): GPUI `snapshot::warning`, why the fallback shortcut is
// not working, if it is not.
pub fn warning(_s: &State) -> Option<String> {
    None
}

// PORT-STUB(area F): GPUI `settings/snapshots.rs::ask` via `permissions`:
// ask macOS for Screen Recording (`screen`) and Accessibility (`text`)
// where missing.
pub fn ask_permissions(_screen: bool, _text: bool) {}
