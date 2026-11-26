//! Merging the focused computer's inbox with every ready peer's inbox into one
//! sidebar list, with `(computer key, thread id)` identity ([`RowId`]) and
//! per-row action routing.

use std::collections::BTreeSet;

use zlo_client::inbox::{InboxAction, InboxProfile, InboxRow, InboxState, InboxStatus};

use super::{Inbox, mark, settle, title_of};
use crate::remote::peers::{self, PeerConn};
use crate::state::{Conn, Page, State};
use crate::workspace::active_id;

/// Identity of one row in the merged sidebar: its computer key (`this-mac`
/// or an `environment_id`) and its thread id on that computer.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RowId {
    pub computer: String,
    pub thread_id: String,
}

impl RowId {
    pub fn new(computer: impl Into<String>, thread_id: impl Into<String>) -> Self {
        Self {
            computer: computer.into(),
            thread_id: thread_id.into(),
        }
    }

    pub fn focused(s: &State, thread_id: impl Into<String>) -> Self {
        Self::new(s.onboarding.computer.key(), thread_id)
    }

    /// Stable element-key suffix (`{computer}:{thread_id}`), unique across
    /// computers even when two computers share a thread id.
    pub fn key(&self) -> String {
        format!("{}:{}", self.computer, self.thread_id)
    }
}

/// One row from the focused computer or a ready peer, with its owning
/// computer's metadata and profile catalog.
#[derive(Clone)]
pub(in crate::workspace) struct Entry<'a> {
    pub id: RowId,
    pub label: String,
    pub focused: bool,
    pub computer_ix: usize,
    pub row_ix: usize,
    pub row: &'a InboxRow,
    pub profiles: &'a [InboxProfile],
}

impl<'a> Entry<'a> {
    pub fn profile(&self) -> Option<&'a InboxProfile> {
        self.profiles
            .iter()
            .find(|p| p.profile_id == self.row.profile_id)
    }

    pub fn profile_name(&self) -> String {
        self.profile()
            .map_or_else(|| self.row.profile_id.clone(), |p| p.display_name.clone())
    }

    pub fn shown_state(&self, inbox: &Inbox) -> InboxState {
        if self.focused {
            inbox.shown_state(self.row)
        } else {
            self.row.state
        }
    }

    pub fn shown_status(&self, inbox: &Inbox) -> Option<InboxStatus> {
        if self.focused {
            inbox.shown_status(self.row)
        } else {
            self.row.status
        }
    }
}

/// More than one computer is connected right now (focused is Ready and at
/// least one peer is Ready). Computer tags appear only when this is true.
pub(in crate::workspace) fn multi_connected(s: &State) -> bool {
    matches!(s.conn, Conn::Ready) && peers::list(s).iter().any(|p| p.conn == PeerConn::Ready)
}

/// Whether any ready peer currently contributes rows to the sidebar.
pub(in crate::workspace) fn has_peer_rows(s: &State) -> bool {
    peers::list(s).iter().any(|p| {
        p.conn == PeerConn::Ready && p.inbox.as_ref().is_some_and(|list| !list.rows.is_empty())
    })
}

/// The open chat's row identity, if a chat is on screen.
pub(in crate::workspace) fn active_row(s: &State) -> Option<RowId> {
    (s.page == Page::Chat)
        .then(|| active_id(s))
        .flatten()
        .map(|id| RowId::focused(s, id))
}

/// Every row from the focused computer followed by every ready peer's rows.
pub(in crate::workspace) fn entries(s: &State) -> Vec<Entry<'_>> {
    let focused_key = s.onboarding.computer.key();
    let focused_label = s.onboarding.computer.label();
    let focused_list = &s.workspace.inbox.list;
    let mut out: Vec<Entry<'_>> = focused_list
        .rows
        .iter()
        .enumerate()
        .map(|(row_ix, row)| Entry {
            id: RowId::new(focused_key.clone(), row.thread_id.clone()),
            label: focused_label.clone(),
            focused: true,
            computer_ix: 0,
            row_ix,
            row,
            profiles: &focused_list.profiles,
        })
        .collect();
    for (peer_ix, peer) in peers::list(s).into_iter().enumerate() {
        if peer.conn != PeerConn::Ready {
            continue;
        }
        let Some(list) = peer.inbox.as_ref() else {
            continue;
        };
        let key = peer.computer.key();
        let label = peer.computer.label();
        for (row_ix, row) in list.rows.iter().enumerate() {
            out.push(Entry {
                id: RowId::new(key.clone(), row.thread_id.clone()),
                label: label.clone(),
                focused: false,
                computer_ix: peer_ix + 1,
                row_ix,
                row,
                profiles: &list.profiles,
            });
        }
    }
    out
}

/// Look up one entry by `(computer_ix, row_ix)` during virtual row rendering.
pub(in crate::workspace) fn entry_at(
    s: &State,
    computer_ix: usize,
    row_ix: usize,
) -> Option<Entry<'_>> {
    if computer_ix == 0 {
        let list = &s.workspace.inbox.list;
        let row = list.rows.get(row_ix)?;
        return Some(Entry {
            id: RowId::focused(s, &row.thread_id),
            label: s.onboarding.computer.label(),
            focused: true,
            computer_ix: 0,
            row_ix,
            row,
            profiles: &list.profiles,
        });
    }
    let peer = peers::list(s).into_iter().nth(computer_ix - 1)?;
    if peer.conn != PeerConn::Ready {
        return None;
    }
    let list = peer.inbox.as_ref()?;
    let row = list.rows.get(row_ix)?;
    Some(Entry {
        id: RowId::new(peer.computer.key(), row.thread_id.clone()),
        label: peer.computer.label(),
        focused: false,
        computer_ix,
        row_ix,
        row,
        profiles: &list.profiles,
    })
}

/// Look up a row by its `(computer, thread_id)` identity.
pub(in crate::workspace) fn find_row<'a>(s: &'a State, id: &RowId) -> Option<&'a InboxRow> {
    if id.computer == s.onboarding.computer.key() {
        return s.workspace.inbox.row(&id.thread_id);
    }
    peers::get(s, &id.computer)
        .filter(|p| p.conn == PeerConn::Ready)
        .and_then(|p| p.inbox.as_ref())
        .and_then(|list| list.rows.iter().find(|r| r.thread_id == id.thread_id))
}

/// Whether `entry` matches the current sidebar filters and search `query`.
pub(in crate::workspace) fn matches_entry(
    s: &State,
    entry: &Entry<'_>,
    query: &str,
    multi: bool,
) -> bool {
    let inbox = &s.workspace.inbox;
    let row = entry.row;
    if inbox.profile.as_ref().is_some_and(|p| *p != row.profile_id)
        || inbox.state.is_some_and(|st| st != row.state)
        || (inbox.imported_only && row.imported.is_none())
        || inbox
            .model
            .as_ref()
            .is_some_and(|m| !row.engines.iter().any(|e| e.model.as_ref() == Some(m)))
    {
        return false;
    }
    if query.is_empty() {
        return true;
    }
    let profile = entry.profile().map(|p| p.display_name.to_lowercase());
    title_of(row).to_lowercase().contains(query)
        || profile.is_some_and(|p| p.contains(query))
        || (multi && entry.label.to_lowercase().contains(query))
        || row.engines.iter().any(|e| {
            e.model
                .as_deref()
                .is_some_and(|m| m.to_lowercase().contains(query))
        })
}

/// Distinct profiles across the focused computer and every ready peer.
pub(in crate::workspace) fn filter_profiles(s: &State) -> Vec<InboxProfile> {
    let mut out = s.workspace.inbox.list.profiles.clone();
    let mut seen: BTreeSet<String> = out.iter().map(|p| p.profile_id.clone()).collect();
    for peer in peers::list(s) {
        if peer.conn != PeerConn::Ready {
            continue;
        }
        let Some(list) = peer.inbox.as_ref() else {
            continue;
        };
        for p in &list.profiles {
            if seen.insert(p.profile_id.clone()) {
                out.push(p.clone());
            }
        }
    }
    out
}

/// Distinct models across the focused computer and every ready peer.
pub(in crate::workspace) fn filter_models(s: &State) -> Vec<String> {
    let mut set: BTreeSet<String> = s
        .workspace
        .inbox
        .list
        .rows
        .iter()
        .flat_map(|r| r.engines.iter().filter_map(|e| e.model.clone()))
        .collect();
    for peer in peers::list(s) {
        if peer.conn != PeerConn::Ready {
            continue;
        }
        if let Some(list) = peer.inbox.as_ref() {
            set.extend(
                list.rows
                    .iter()
                    .flat_map(|r| r.engines.iter().filter_map(|e| e.model.clone())),
            );
        }
    }
    set.into_iter().collect()
}

/// Whether any row on the focused computer or a ready peer is imported.
pub(in crate::workspace) fn any_imported(s: &State) -> bool {
    if s.workspace
        .inbox
        .list
        .rows
        .iter()
        .any(|r| r.imported.is_some())
    {
        return true;
    }
    peers::list(s).iter().any(|p| {
        p.conn == PeerConn::Ready
            && p.inbox
                .as_ref()
                .is_some_and(|l| l.rows.iter().any(|r| r.imported.is_some()))
    })
}

/// Whether any row on the focused computer or a ready peer is actively working.
pub(in crate::workspace) fn any_working(s: &State) -> bool {
    if s.workspace.inbox.any_working() {
        return true;
    }
    peers::list(s).iter().any(|p| {
        p.conn == PeerConn::Ready
            && p.inbox.as_ref().is_some_and(|l| {
                l.rows
                    .iter()
                    .any(|r| r.status == Some(InboxStatus::Working))
            })
    })
}

/// Open a row on its own computer: directly when focused, via
/// `peers::focus_thread` (instant handover when ready) for a peer.
pub(in crate::workspace) fn open_row(s: &mut State, id: &RowId) {
    if id.computer == s.onboarding.computer.key() {
        crate::workspace::open_thread(s, id.thread_id.clone());
    } else {
        peers::focus_thread(s, &id.computer, id.thread_id.clone());
    }
}

/// Send a mark (pin, unpin, snooze, wake, unsettle) to the row's own computer.
pub(in crate::workspace) fn mark_row(
    s: &mut State,
    id: RowId,
    action: InboxAction,
    until: Option<i64>,
) {
    s.workspace.inbox.snooze_menu = None;
    if id.computer == s.onboarding.computer.key() {
        mark(s, id.thread_id, action, until);
        return;
    }
    let computer = id.computer;
    let thread_id = id.thread_id;
    let done_computer = computer.clone();
    peers::run(
        s,
        &computer,
        "peer inbox mark",
        move |c| async move { c.inbox_mutate(&thread_id, action, until).await },
        move |s, result, tx| match result {
            Ok(list) => {
                tx.animated();
                peers::apply_inbox(s, &done_computer, list, tx);
            }
            // silent-ok: an older gateway has no marks; the row stays as it was.
            Err(e) => tracing::debug!(
                computer = %done_computer,
                error = %e,
                "peer inbox mark failed"
            ),
        },
    );
}

/// Settle a row on its own computer.
pub(in crate::workspace) fn settle_row(s: &mut State, id: RowId) {
    if id.computer == s.onboarding.computer.key() {
        settle(s, id.thread_id);
        return;
    }
    let computer = id.computer;
    let thread_id = id.thread_id;
    let done_computer = computer.clone();
    peers::run(
        s,
        &computer,
        "peer inbox settle",
        move |c| async move { c.inbox_mutate(&thread_id, InboxAction::Settle, None).await },
        move |s, result, tx| match result {
            Ok(list) => {
                tx.animated();
                peers::apply_inbox(s, &done_computer, list, tx);
            }
            Err(e) => tracing::warn!(
                computer = %done_computer,
                error = %e,
                "could not settle peer thread"
            ),
        },
    );
}

/// Delete a row on its own computer.
pub(in crate::workspace) fn delete_row(s: &mut State, id: RowId) {
    if id.computer == s.onboarding.computer.key() {
        crate::workspace::delete_thread(s, id.thread_id);
        return;
    }
    let computer = id.computer;
    let thread_id = id.thread_id;
    let req_id = thread_id.clone();
    let done_computer = computer.clone();
    peers::run(
        s,
        &computer,
        "peer delete thread",
        move |c| async move {
            if let Err(e) = c.clean_thread_worktree(&req_id).await {
                tracing::debug!(error = %e, "peer worktree not cleaned before delete");
            }
            c.delete_thread(&req_id).await
        },
        move |s, result, tx| {
            if result.is_err() {
                return;
            }
            tx.animated();
            peers::remove_row(s, &done_computer, &thread_id);
            peers::refresh_one(s, &done_computer);
        },
    );
}

/// Quiet one-line status for the sidebar's foot when any peer is connecting,
/// asleep or failed. `None` when there are no peers or every peer is ready.
pub(in crate::workspace) fn peer_status_line(s: &State) -> Option<String> {
    let parts: Vec<String> = peers::list(s)
        .into_iter()
        .filter_map(|peer| {
            let label = peer.computer.label();
            match &peer.conn {
                PeerConn::Ready => None,
                PeerConn::Asleep => Some(format!("{label} is asleep")),
                PeerConn::Connecting => Some(format!("Connecting to {label}…")),
                PeerConn::Failed(_) => Some(format!("{label} isn't connected")),
            }
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

#[cfg(test)]
#[path = "merged_tests.rs"]
mod tests;
