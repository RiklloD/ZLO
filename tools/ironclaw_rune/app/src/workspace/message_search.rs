//! Search across the text of past chats, shared by the ⌘K menu ("In
//! messages") and the inbox search field (a snippet under a matching row).
//! (GPUI `workspace/message_search.rs`.)
//!
//! The backend owns the index (`GET /history/search`); this keeps one
//! debounced request in flight for the latest query and the answer for the
//! query it was asked with, so a stale answer never shows under a newer
//! query. While the backend is still indexing older chats
//! (`index_complete: false`) the answer says so and a bounded series of
//! follow-up requests refreshes it; a new query cancels them. Opening a hit
//! opens its thread scrolled to the matched message.
//!
//! GPUI's debounce task is a deadline here (`pending`), run by
//! `workspace::poll`.

use std::time::{Duration, Instant};

use ironclaw_gpui_client::history::{
    HistoryHit, HistorySearchRequest, HistoryThreadOrigin, HistoryThreadResult,
};
use rune::{El, Span, TextStyle, rich};

use crate::design::Palette;
use crate::jobs;
use crate::state::State;

/// Typing settles for this long before a request goes out.
const DEBOUNCE: Duration = Duration::from_millis(180);
/// Shorter queries match too much to be useful.
const MIN_CHARS: usize = 2;
/// Threads asked for per search.
const THREADS: u32 = 20;
/// Pause between follow-ups while the index is incomplete.
const FOLLOW_UP: Duration = Duration::from_millis(1_500);
/// Follow-ups per query (about a minute), then the last answer stands.
const MAX_FOLLOW_UPS: u32 = 40;

/// When to ask again for the same query: only while the backend reports the
/// index incomplete, a search surface still shows the query, and a bounded
/// number of times.
fn follow_up_after(index_complete: bool, follow_ups: u32, wanted: bool) -> Option<Duration> {
    (wanted && !index_complete && follow_ups < MAX_FOLLOW_UPS).then_some(FOLLOW_UP)
}

#[derive(Default)]
pub struct MessageSearch {
    /// The query the current `results` answer.
    answered: String,
    /// The query of the request in flight or waiting out the debounce.
    asked: String,
    results: Vec<HistoryThreadResult>,
    /// The current answer covers every chat.
    complete: bool,
    /// The pending debounce or follow-up: when, which query, and how many
    /// follow-ups came before it. Replacing it cancels the old one.
    pending: Option<(Instant, String, u32)>,
}

impl MessageSearch {
    /// Threads matching `query` by message text, best first; empty until the
    /// answer for exactly this query arrives.
    pub fn results(&self, query: &str) -> &[HistoryThreadResult] {
        if normalized(query) == self.answered {
            &self.results
        } else {
            &[]
        }
    }

    /// Older chats are still being indexed for the answer to `query`.
    pub fn indexing(&self, query: &str) -> bool {
        let query = normalized(query);
        query.chars().count() >= MIN_CHARS && (query != self.answered || !self.complete)
    }

    /// The best hit in `thread_id` for `query`, if the answer is current.
    pub fn hit(&self, query: &str, thread_id: &str) -> Option<&HistoryHit> {
        self.results(query)
            .iter()
            .find(|t| {
                t.thread_id == thread_id || t.related_thread_ids.iter().any(|r| r == thread_id)
            })
            .and_then(|t| t.hits.first())
    }

    /// A scene's answer for `query`.
    pub fn fixture(&mut self, query: &str, results: Vec<HistoryThreadResult>) {
        self.asked = normalized(query);
        self.answered = self.asked.clone();
        self.results = results;
        self.complete = true;
    }

    /// When `poll` should look again.
    pub(super) fn due(&self) -> Option<Instant> {
        self.pending.as_ref().map(|(at, ..)| *at)
    }
}

fn normalized(query: &str) -> String {
    query.trim().to_lowercase()
}

/// Whether an open search surface (⌘K, the inbox field) shows `query`.
fn query_wanted(s: &State, query: &str) -> bool {
    let palette = s
        .workspace
        .command
        .as_ref()
        .map(|menu| normalized(&menu.input.text));
    let inbox = normalized(&s.workspace.inbox.search.text);
    palette.as_deref() == Some(query) || inbox == query
}

/// Ask for `query` after the debounce; a newer query replaces it.
pub(super) fn search(s: &mut State, query: &str) {
    let query = normalized(query);
    let search = &mut s.workspace.message_search;
    if query == search.asked {
        return;
    }
    search.asked = query.clone();
    if query.chars().count() < MIN_CHARS {
        search.pending = None;
        search.answered = query;
        search.results.clear();
        search.complete = true;
        return;
    }
    search.pending = Some((Instant::now() + DEBOUNCE, query, 0));
}

/// A pending request came due: send it. True when the view should re-run.
pub(super) fn poll(s: &mut State) -> bool {
    let due =
        matches!(&s.workspace.message_search.pending, Some((at, ..)) if *at <= Instant::now());
    if !due {
        return false;
    }
    let Some((_, query, follow_ups)) = s.workspace.message_search.pending.take() else {
        return false;
    };
    if s.workspace.message_search.asked != query {
        return false;
    }
    // Nothing shows this query any more (the palette closed, the field
    // changed): stop asking.
    if follow_ups > 0 && !query_wanted(s, &query) {
        s.workspace.message_search.asked.clear();
        return false;
    }
    let request = HistorySearchRequest {
        query: Some(query.clone()),
        limit: Some(THREADS),
        ..Default::default()
    };
    jobs::run(
        s,
        "message search",
        move |c| async move { c.history_search(&request).await },
        move |s, result, _| {
            // A newer query is waiting: this answer is stale.
            if s.workspace.message_search.asked != query {
                return;
            }
            match result {
                Ok(found) => {
                    let complete = found.index_complete;
                    let wanted = query_wanted(s, &query);
                    let search = &mut s.workspace.message_search;
                    search.answered = query.clone();
                    search.results = found.threads;
                    search.complete = complete;
                    if let Some(after) = follow_up_after(complete, follow_ups, wanted) {
                        search.pending = Some((Instant::now() + after, query, follow_ups + 1));
                    }
                }
                // silent-ok: title search still works; the next keystroke retries.
                Err(e) => tracing::debug!(error = %e, "message search failed"),
            }
        },
    );
    false
}

/// A hit's snippet with its matched words in full ink and bold.
pub fn snippet_text(p: &Palette, hit: &HistoryHit, size: f32) -> El {
    let spans: Vec<Span> = hit
        .highlights
        .iter()
        .filter(|h| {
            h.start < h.end
                && h.end <= hit.snippet.len()
                && hit.snippet.is_char_boundary(h.start)
                && hit.snippet.is_char_boundary(h.end)
        })
        .map(|h| Span::new(h.start, h.end).weight(600.).color(p.text))
        .collect();
    rich(hit.snippet.clone(), spans)
        .font(TextStyle::new(size, 400.))
        .color(p.text_muted)
}

/// "Automation" / "Subagent" for threads that are not the person's own.
pub fn origin_label(origin: HistoryThreadOrigin) -> Option<&'static str> {
    match origin {
        HistoryThreadOrigin::Own => None,
        HistoryThreadOrigin::Automation => Some("Automation"),
        HistoryThreadOrigin::Subagent => Some("Subagent"),
    }
}

#[cfg(test)]
mod tests {
    use ironclaw_gpui_client::history::HistorySearchRole;

    use super::*;

    fn result(id: &str) -> HistoryThreadResult {
        HistoryThreadResult {
            thread_id: id.into(),
            title: None,
            updated_at: None,
            origin: HistoryThreadOrigin::Own,
            lineage_root_thread_id: None,
            related_thread_ids: vec!["fork".into()],
            hits: vec![HistoryHit {
                thread_id: id.into(),
                message_id: "m".into(),
                sequence: 1,
                role: HistorySearchRole::User,
                created_at: None,
                snippet: "deploy it".into(),
                highlights: Vec::new(),
            }],
        }
    }

    #[test]
    fn follow_ups_run_only_while_indexing_shown_and_bounded() {
        assert_eq!(follow_up_after(false, 0, true), Some(FOLLOW_UP));
        assert_eq!(
            follow_up_after(false, MAX_FOLLOW_UPS - 1, true),
            Some(FOLLOW_UP)
        );
        assert_eq!(follow_up_after(false, MAX_FOLLOW_UPS, true), None);
        assert_eq!(follow_up_after(true, 0, true), None);
        // No search surface shows the query (palette closed): no polling.
        assert_eq!(follow_up_after(false, 0, false), None);
    }

    #[test]
    fn indexing_shows_until_a_complete_answer_for_the_query() {
        let mut search = MessageSearch {
            answered: "deploy".into(),
            asked: "deploy".into(),
            ..Default::default()
        };
        assert!(search.indexing("deploy"));
        assert!(
            search.indexing("other"),
            "an unanswered query is still pending"
        );
        assert!(!search.indexing("d"), "too short to search");
        search.complete = true;
        assert!(!search.indexing("Deploy "));
    }

    #[test]
    fn answers_show_only_under_the_query_they_answer() {
        let search = MessageSearch {
            answered: "deploy".into(),
            asked: "deploy".into(),
            results: vec![result("t")],
            complete: true,
            pending: None,
        };
        assert_eq!(search.results(" Deploy ").len(), 1);
        assert!(search.results("deploys").is_empty());
        assert!(search.hit("deploy", "t").is_some());
        assert!(
            search.hit("deploy", "fork").is_some(),
            "a collapsed fork finds its group"
        );
        assert!(search.hit("deploy", "other").is_none());
    }
}
