//! The sidebar's grouping of the visible rows into shelves, and the
//! ⌘1..⌘9 jump order.

use ironclaw_gpui_client::inbox::{InboxRow, InboxState};

use super::subagents;
use crate::state::State;

/// The visible rows, grouped as the sidebar draws them, plus the ⌘1..⌘9
/// jump order. Shared by the view and the ⌘N shortcut.
pub(in crate::workspace) struct Shelves<'a> {
    pub(in crate::workspace) tree: subagents::Tree,
    pub(in crate::workspace) pinned: Vec<&'a InboxRow>,
    pub(in crate::workspace) main: Vec<&'a InboxRow>,
    pub(in crate::workspace) working: Vec<&'a InboxRow>,
    pub(in crate::workspace) snoozed: Vec<&'a InboxRow>,
    pub(in crate::workspace) settled: Vec<&'a InboxRow>,
    /// Searching or filtering on a parked state unfolds it.
    pub(in crate::workspace) unfold: bool,
    pub(in crate::workspace) working_open: bool,
    /// Thread ids ⌘1..⌘9 open, in the order the cards show.
    pub(in crate::workspace) jump: Vec<String>,
}

pub(in crate::workspace) fn shelves(s: &State) -> Shelves<'_> {
    let inbox = &s.workspace.inbox;
    let query = inbox.query();
    let side_chats: std::collections::HashSet<&str> = s
        .prefs
        .side_chats
        .values()
        .flatten()
        .map(String::as_str)
        .collect();
    let search = &s.workspace.message_search;
    let rows: Vec<&InboxRow> = inbox
        .list
        .rows
        .iter()
        .filter(|r| {
            inbox.matches(r, &query)
                // A message in the thread matches: the filters still apply.
                || (search.hit(&query, &r.thread_id).is_some() && inbox.matches(r, ""))
        })
        // Side chats live under their parent; a search still finds them.
        .filter(|r| !query.is_empty() || !side_chats.contains(r.thread_id.as_str()))
        .collect();
    // Subagents fold under their parent; the rest of the list is the
    // top-level threads only.
    let tree = subagents::Tree::new(rows.iter().copied());
    let rows: Vec<&InboxRow> = rows
        .into_iter()
        .filter(|r| !tree.is_nested(&r.thread_id))
        .collect();
    let in_state = |st: InboxState| -> Vec<&InboxRow> {
        rows.iter().copied().filter(|r| r.state == st).collect()
    };
    let unfold = !query.is_empty() || inbox.state.is_some();
    let working_open = inbox.show_working || unfold;
    let (pinned, main, working) = (
        in_state(InboxState::Pinned),
        in_state(InboxState::Inbox),
        in_state(InboxState::Working),
    );
    // ⌘1..⌘9 follow the full cards top to bottom, Working last when its
    // shelf is open, as t3code numbers its visible threads.
    let jump = pinned
        .iter()
        .chain(main.iter())
        .chain(working.iter().filter(|_| working_open))
        .take(9)
        .map(|r| r.thread_id.clone())
        .collect();
    Shelves {
        tree,
        snoozed: in_state(InboxState::Snoozed),
        settled: in_state(InboxState::Settled),
        pinned,
        main,
        working,
        unfold,
        working_open,
        jump,
    }
}

/// The thread ⌘N (1-based `n`) opens.
pub(in crate::workspace) fn jump_target(s: &State, n: usize) -> Option<String> {
    shelves(s).jump.get(n.checked_sub(1)?).cloned()
}
