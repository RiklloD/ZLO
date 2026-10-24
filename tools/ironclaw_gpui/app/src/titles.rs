//! Thread titles for every surface that names a conversation: notifications,
//! system alerts, Memory > threads. The sidebar inbox supplies them, automation
//! runs included; a thread it does not list (older than its page) is fetched
//! once, on demand.

use std::collections::{HashMap, HashSet};

use ironclaw_gpui_client::Backend;
use ironclaw_gpui_client::inbox::InboxRow;

use crate::jobs::flatten;
use crate::ui::*;

#[derive(Default)]
struct ThreadTitles {
    known: HashMap<String, String>,
    pending: HashSet<String>,
}

impl Global for ThreadTitles {}

pub fn get(cx: &App, thread_id: &str) -> Option<String> {
    cx.try_global::<ThreadTitles>()?
        .known
        .get(thread_id)
        .cloned()
}

/// Record the titles the inbox just returned.
pub fn set_rows(rows: &[InboxRow], cx: &mut App) {
    let titles = cx.default_global::<ThreadTitles>();
    for row in rows {
        titles.known.insert(
            row.thread_id.clone(),
            crate::workspace::row_title(row).to_string(),
        );
    }
}

/// Fetch titles that are not known yet; windows repaint when they land.
pub fn ensure(thread_ids: impl IntoIterator<Item = String>, backend: &Backend, cx: &mut App) {
    let titles = cx.default_global::<ThreadTitles>();
    let mut missing = Vec::new();
    for id in thread_ids {
        if !titles.known.contains_key(&id) && titles.pending.insert(id.clone()) {
            missing.push(id);
        }
    }
    for id in missing {
        let fetch = resolve(id, backend, cx);
        cx.spawn(async move |cx| {
            fetch.await;
            cx.update(|cx| cx.refresh_windows());
        })
        .detach();
    }
}

/// The title for one thread, fetched (and remembered) when not known yet.
/// Falls back to "Conversation".
pub fn resolve(thread_id: String, backend: &Backend, cx: &mut App) -> Task<String> {
    if let Some(title) = get(cx, &thread_id) {
        return Task::ready(title);
    }
    let job = {
        let id = thread_id.clone();
        backend.spawn(move |c| async move { c.thread_label(&id).await })
    };
    cx.spawn(async move |cx| {
        let label = flatten(job.await);
        cx.update(|cx| {
            let titles = cx.default_global::<ThreadTitles>();
            titles.pending.remove(&thread_id);
            match label {
                Ok(label) => {
                    titles.known.insert(thread_id, label.clone());
                    label
                }
                // silent-ok: the generic label stands in; the next view retries.
                Err(e) => {
                    tracing::debug!(thread = %thread_id, error = %e, "thread title fetch failed");
                    "Conversation".to_string()
                }
            }
        })
    })
}
