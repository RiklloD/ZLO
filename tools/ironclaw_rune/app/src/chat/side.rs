//! The chat's right pane and work panel: activity, agents, browser, changes, console, git, history, outputs, preview, project, routines, session (GPUI `chat/side.rs`, `chat/side/*`). Area S.
//!
//! FOUNDATION STUB, replaced by its port (see PLAN.md for the owner).

#[derive(Default)]
pub struct SideState {}

// ---- Called by the chat (area A). PORT-STUB(area S): replace the bodies
// with the port of `chat/side.rs` and `chat/side/*`; keep the signatures.
// `id` is the chat's thread id (the open chat, or a side chat in its pane).

use std::path::PathBuf;

use ironclaw_gpui_client::mentions::LineRange;
use rune::{El, Tx};
use rune_kit::Kit;

use crate::chat::ChatState;
use crate::state::State;

/// PORT-STUB(area S): a side chat drawn in the open chat's pane, by thread
/// id (it holds a `ChatState` from `crate::chat::new_side_chat`).
pub fn side_chat_mut<'a>(_s: &'a mut State, _id: &str) -> Option<&'a mut ChatState> {
    None
}

/// PORT-STUB(area S): read-only [`side_chat_mut`].
pub fn side_chat<'a>(_s: &'a State, _id: &str) -> Option<&'a ChatState> {
    None
}

/// PORT-STUB(area S): the thread ids of the side chats now open.
pub fn side_chat_ids(_s: &State) -> Vec<String> {
    Vec::new()
}

/// PORT-STUB(area S): `side.folder`, the thread's project folder.
pub fn folder(_s: &State, _id: &str) -> Option<PathBuf> {
    None
}

/// PORT-STUB(area S): `side_set_folder`: `adopt` when a thread without a
/// folder of its own takes the one its mentions came from.
pub fn set_folder(_s: &mut State, _id: &str, _root: PathBuf, _adopt: bool) {}

/// PORT-STUB(area S): `side_init`, once per opened (non-side) chat.
pub fn init(_s: &mut State, _id: &str) {}

/// PORT-STUB(area S): `side_turn_started`: the working tree as it stands
/// now, for the turn's card.
pub fn turn_started(_s: &mut State, _id: &str) {}

/// PORT-STUB(area S): `side_turn_ended`.
pub fn turn_ended(_s: &mut State, _id: &str) {}

/// PORT-STUB(area S): `open_link`: a link in a reply previews in the pane;
/// `external` (⌘ held) opens it outside the app. Until the pane lands, web
/// links open in the browser and paths in their default app.
pub fn open_link(_s: &mut State, _id: &str, href: &str, _external: bool, tx: &mut Tx) {
    if href.starts_with("http://") || href.starts_with("https://") || href.starts_with("mailto:") {
        if let Err(e) = tx.open_url(href) {
            tracing::warn!(error = ?e, "couldn't open the link");
        }
    } else {
        let path = href.strip_prefix("file://").unwrap_or(href).replace("%20", " ");
        crate::chat::actions::open_with_system(std::path::Path::new(&path));
    }
}

/// PORT-STUB(area S): `changes_selection`, lines picked in the Changes tab,
/// offered as an `@` selection.
pub fn changes_selection(_s: &State, _id: &str) -> Option<(PathBuf, LineRange)> {
    None
}

/// PORT-STUB(area S): `render_docks`, the right pane and work panel beside
/// the chat column.
pub fn docks(_s: &State, _chat: &ChatState, _cx: &mut rune::Cx<State>, _k: &Kit, _layer: &mut Vec<El>) -> Option<El> {
    None
}
