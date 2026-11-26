//! The sidebar's grouping of the visible rows into shelves, and the
//! ⌘1..⌘9 jump order.

use std::cmp::Reverse;

use zlo_client::inbox::InboxState;

use super::merged::{self, Entry, RowId};
use super::subagents;
use crate::state::State;

/// The visible rows, grouped as the sidebar draws them, plus the ⌘1..⌘9
/// jump order. Shared by the view and the ⌘N shortcut.
pub(in crate::workspace) struct Shelves<'a> {
    pub(in crate::workspace) tree: subagents::Tree,
    pub(in crate::workspace) pinned: Vec<Entry<'a>>,
    pub(in crate::workspace) main: Vec<Entry<'a>>,
    pub(in crate::workspace) working: Vec<Entry<'a>>,
    pub(in crate::workspace) snoozed: Vec<Entry<'a>>,
    pub(in crate::workspace) settled: Vec<Entry<'a>>,
    /// Searching or filtering on a parked state unfolds it.
    pub(in crate::workspace) unfold: bool,
    pub(in crate::workspace) working_open: bool,
    /// Rows ⌘1..⌘9 open, in the order the cards show.
    pub(in crate::workspace) jump: Vec<RowId>,
}

pub(in crate::workspace) fn shelves(s: &State) -> Shelves<'_> {
    let inbox = &s.workspace.inbox;
    let query = inbox.query();
    let multi = merged::multi_connected(s);
    let merge_sort = merged::has_peer_rows(s);
    let side_chats: std::collections::HashSet<(&str, &str)> = s
        .prefs
        .side_chats
        .iter()
        .flat_map(|(k, children)| {
            let computer = k
                .split_once('~')
                .map_or(zlo_client::environments::THIS_MAC, |(c, _)| c);
            children.iter().map(move |child| (computer, child.as_str()))
        })
        .collect();
    let search = &s.workspace.message_search;
    let entries: Vec<Entry<'_>> = merged::entries(s)
        .into_iter()
        .filter(|e| {
            merged::matches_entry(s, e, &query, multi)
                // A message in the thread matches (focused computer's index):
                // the filters still apply.
                || (e.focused
                    && search.hit(&query, &e.row.thread_id).is_some()
                    && merged::matches_entry(s, e, "", multi))
        })
        // Side chats live under their parent on their own computer; a search
        // still finds them.
        .filter(|e| {
            !query.is_empty()
                || !side_chats.contains(&(e.id.computer.as_str(), e.row.thread_id.as_str()))
        })
        .collect();
    // Subagents fold under their parent on the same computer; the rest of the
    // list is the top-level threads only.
    let tree = subagents::Tree::new(entries.iter().map(|e| (&e.id, e.focused, e.row)));
    let entries: Vec<Entry<'_>> = entries
        .into_iter()
        .filter(|e| !tree.is_nested(&e.id))
        .collect();
    // Working threads sit on the Working shelf (folded by default), as in
    // t3code; `shown_state` moves a just-sent focused one there before the
    // backend confirms it. Working rows from every computer go into it.
    let in_state = |st: InboxState| -> Vec<Entry<'_>> {
        let mut out: Vec<Entry<'_>> = entries
            .iter()
            .filter(|e| e.shown_state(inbox) == st)
            .cloned()
            .collect();
        if merge_sort {
            if st == InboxState::Settled {
                out.sort_by_key(|e| Reverse(e.row.settled_at.unwrap_or(e.row.last_activity_at)));
            } else {
                out.sort_by_key(|e| Reverse(e.row.last_activity_at));
            }
        }
        out
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
        .map(|e| e.id.clone())
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

/// The row ⌘N (1-based `n`) opens.
pub(in crate::workspace) fn jump_target(s: &State, n: usize) -> Option<RowId> {
    shelves(s).jump.get(n.checked_sub(1)?).cloned()
}
