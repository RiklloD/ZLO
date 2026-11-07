//! Thread titles for every surface that names a conversation: notifications,
//! system alerts, Memory > threads. The sidebar inbox supplies them, automation
//! runs included; a thread it does not list (older than its page) is fetched
//! once, on demand. (GPUI `titles.rs`; the GPUI global lives in
//! `WorkspaceState::titles` here.)

use std::collections::{HashMap, HashSet};

use ironclaw_gpui_client::inbox::InboxRow;
use rune::Tx;

use crate::jobs;
use crate::state::State;

#[derive(Default)]
pub struct Titles {
    known: HashMap<String, String>,
    pending: HashSet<String>,
}

pub fn get(s: &State, thread_id: &str) -> Option<String> {
    s.workspace.titles.known.get(thread_id).cloned()
}

/// Record the titles the inbox just returned.
pub fn set_rows(s: &mut State, rows: &[InboxRow]) {
    let titles = &mut s.workspace.titles;
    for row in rows {
        titles.known.insert(
            row.thread_id.clone(),
            crate::workspace::row_title(row).to_string(),
        );
    }
}

/// Fetch titles that are not known yet; the view re-runs when they land.
pub fn ensure(s: &mut State, thread_ids: impl IntoIterator<Item = String>) {
    for id in thread_ids {
        let titles = &mut s.workspace.titles;
        if !titles.known.contains_key(&id) && titles.pending.insert(id.clone()) {
            fetch(s, id, |_, _, _| {});
        }
    }
}

/// The title for one thread, fetched (and remembered) when not known yet,
/// handed to `done` on the UI thread. Falls back to "Conversation".
pub fn resolve(
    s: &mut State,
    thread_id: String,
    done: impl FnOnce(&mut State, String, &mut Tx) + Send + 'static,
) {
    if let Some(title) = get(s, &thread_id) {
        let handle = s.rt.handle.clone();
        handle.update(move |s, tx| done(s, title, tx));
        return;
    }
    s.workspace.titles.pending.insert(thread_id.clone());
    fetch(s, thread_id, done);
}

fn fetch(
    s: &State,
    thread_id: String,
    done: impl FnOnce(&mut State, String, &mut Tx) + Send + 'static,
) {
    let id = thread_id.clone();
    jobs::run(
        s,
        "titles",
        move |c| async move { c.thread_label(&id).await },
        move |s, label, tx| {
            let titles = &mut s.workspace.titles;
            titles.pending.remove(&thread_id);
            let title = match label {
                Ok(label) => {
                    titles.known.insert(thread_id, label.clone());
                    label
                }
                // silent-ok: the generic label stands in; the next view retries.
                Err(e) => {
                    tracing::debug!(thread = %thread_id, error = %e, "thread title fetch failed");
                    "Conversation".to_string()
                }
            };
            done(s, title, tx);
        },
    );
}
