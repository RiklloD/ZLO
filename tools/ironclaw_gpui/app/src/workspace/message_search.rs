//! Search across the text of past chats, shared by the Cmd+K menu ("In
//! messages") and the inbox search field (a snippet under a matching row).
//!
//! The backend owns the index (`GET /history/search`); this keeps one
//! debounced request in flight for the latest query and the answer for the
//! query it was asked with, so a stale answer never shows under a newer
//! query. While the backend is still indexing older chats
//! (`index_complete: false`) the answer says so and a bounded series of
//! follow-up requests refreshes it; a new query cancels them. Opening a hit
//! opens its thread scrolled to the matched message.

use std::time::Duration;

use ironclaw_gpui_client::history::{
    HistoryHit, HistorySearchRequest, HistoryThreadOrigin, HistoryThreadResult,
};

use super::Workspace;
use crate::design::Palette;
use crate::jobs;
use crate::ui::*;

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
/// index incomplete, and a bounded number of times.
fn follow_up_after(index_complete: bool, follow_ups: u32) -> Option<Duration> {
    (!index_complete && follow_ups < MAX_FOLLOW_UPS).then_some(FOLLOW_UP)
}

#[derive(Default)]
pub(super) struct MessageSearch {
    /// The query the current `results` answer.
    answered: String,
    /// The query of the request in flight or waiting out the debounce.
    asked: String,
    results: Vec<HistoryThreadResult>,
    /// The current answer covers every chat.
    complete: bool,
    /// Pending debounce or follow-up; replacing it cancels the old one.
    _debounce: Option<Task<()>>,
}

impl MessageSearch {
    /// Threads matching `query` by message text, best first; empty until the
    /// answer for exactly this query arrives.
    pub(super) fn results(&self, query: &str) -> &[HistoryThreadResult] {
        if normalized(query) == self.answered {
            &self.results
        } else {
            &[]
        }
    }

    /// Older chats are still being indexed for the answer to `query`.
    pub(super) fn indexing(&self, query: &str) -> bool {
        let query = normalized(query);
        query.chars().count() >= MIN_CHARS && (query != self.answered || !self.complete)
    }

    /// The best hit in `thread_id` for `query`, if the answer is current.
    pub(super) fn hit(&self, query: &str, thread_id: &str) -> Option<&HistoryHit> {
        self.results(query)
            .iter()
            .find(|t| {
                t.thread_id == thread_id || t.related_thread_ids.iter().any(|r| r == thread_id)
            })
            .and_then(|t| t.hits.first())
    }
}

fn normalized(query: &str) -> String {
    query.trim().to_lowercase()
}

impl Workspace {
    /// Ask for `query` after the debounce; a newer query replaces it.
    pub(super) fn search_messages(&mut self, query: &str, cx: &mut Context<Self>) {
        let query = normalized(query);
        let search = &mut self.message_search;
        if query == search.asked {
            return;
        }
        search.asked = query.clone();
        if query.chars().count() < MIN_CHARS {
            search._debounce = None;
            search.answered = query;
            search.results.clear();
            search.complete = true;
            return;
        }
        self.fetch_messages(query, DEBOUNCE, 0, cx);
    }

    /// One request for `query` after `delay`; `follow_ups` counts the ones
    /// already made for it while the index was incomplete.
    fn fetch_messages(
        &mut self,
        query: String,
        delay: Duration,
        follow_ups: u32,
        cx: &mut Context<Self>,
    ) {
        let Some(backend) = self.backend.clone() else {
            return;
        };
        self.message_search._debounce = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |this, cx| {
                if this.message_search.asked != query {
                    return;
                }
                let request = HistorySearchRequest {
                    query: Some(query.clone()),
                    limit: Some(THREADS),
                    ..Default::default()
                };
                jobs::run(
                    &backend,
                    cx,
                    move |c| async move { c.history_search(&request).await },
                    move |this, result, cx| {
                        // A newer query is waiting: this answer is stale.
                        if this.message_search.asked != query {
                            return;
                        }
                        match result {
                            Ok(found) => {
                                let complete = found.index_complete;
                                this.message_search.answered = query.clone();
                                this.message_search.results = found.threads;
                                this.message_search.complete = complete;
                                if let Some(after) = follow_up_after(complete, follow_ups) {
                                    this.fetch_messages(query, after, follow_ups + 1, cx);
                                }
                            }
                            // silent-ok: title search still works; the next keystroke retries.
                            Err(e) => tracing::debug!(error = %e, "message search failed"),
                        }
                        cx.notify();
                    },
                );
            });
        }));
    }

    /// Open `thread_id` scrolled to `message_id`.
    pub(super) fn open_thread_at(
        &mut self,
        thread_id: String,
        message_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_thread(thread_id.clone(), window, cx);
        if let Some((id, view)) = &self.active
            && *id == thread_id
        {
            view.update(cx, |chat, cx| chat.reveal_message(message_id, cx));
        }
    }
}

/// A hit's snippet with its matched words in full ink and bold.
pub(super) fn snippet_text(hit: &HistoryHit, cx: &App) -> StyledText {
    let p = *Palette::of(cx);
    let highlights: Vec<_> = hit
        .highlights
        .iter()
        .filter(|h| {
            h.start < h.end
                && h.end <= hit.snippet.len()
                && hit.snippet.is_char_boundary(h.start)
                && hit.snippet.is_char_boundary(h.end)
        })
        .map(|h| {
            (
                h.start..h.end,
                HighlightStyle {
                    color: Some(p.text),
                    font_weight: Some(FontWeight::SEMIBOLD),
                    ..Default::default()
                },
            )
        })
        .collect();
    StyledText::new(hit.snippet.clone()).with_highlights(highlights)
}

/// "Automation" / "Subagent" for threads that are not the person's own.
pub(super) fn origin_label(origin: HistoryThreadOrigin) -> Option<&'static str> {
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
    fn follow_ups_run_only_while_indexing_and_are_bounded() {
        assert_eq!(follow_up_after(false, 0), Some(FOLLOW_UP));
        assert_eq!(follow_up_after(false, MAX_FOLLOW_UPS - 1), Some(FOLLOW_UP));
        assert_eq!(follow_up_after(false, MAX_FOLLOW_UPS), None);
        assert_eq!(follow_up_after(true, 0), None);
    }

    #[test]
    fn indexing_shows_until_a_complete_answer_for_the_query() {
        let mut search = MessageSearch {
            answered: "deploy".into(),
            asked: "deploy".into(),
            results: Vec::new(),
            complete: false,
            _debounce: None,
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
            _debounce: None,
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
