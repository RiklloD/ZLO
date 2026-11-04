//! History search over the caller's past chats: the product half of the
//! `history.search` view and the `ironclaw.history.search` model tool.
//!
//! Transcript truth comes from `SessionThreadService` (the index, ranking
//! inputs and bounded reads); this module infers the mode from the request,
//! classifies thread origin from thread metadata (automation runs and
//! subagent working threads rank below the person's own chats), bounds every
//! answer, and shapes the wire DTOs. Orchestrator knowledge (delegation
//! parents and forks) is layered on by `orchestration::history`.
//! No model calls happen here.

use ironclaw_host_api::ids::ThreadId;
use ironclaw_product_contracts::history::{
    HISTORY_DEFAULT_READ_MESSAGES, HISTORY_DEFAULT_THREADS, HISTORY_DEFAULT_WINDOW,
    HISTORY_MAX_READ_MESSAGES, HISTORY_MAX_THREADS, HISTORY_MAX_WINDOW, HISTORY_MESSAGE_MAX_CHARS,
    HistoryHighlight, HistoryHit, HistoryMessage, HistoryRole, HistorySearchMode,
    HistorySearchRequest, HistorySearchResponse, HistoryThreadOrigin, HistoryThreadResult,
};
use ironclaw_threads::{
    HISTORY_SEARCH_MAX_HITS_PER_THREAD, ListThreadsForScopeRequest, MessageKind,
    SearchHitRole, SearchThreadMessagesRequest, SessionThreadError, SessionThreadRecord,
    SessionThreadService, ThreadHistoryRequest, ThreadMessageId, ThreadMessageRangeRequest,
    ThreadMessageRecord, ThreadScope, record_is_prepared_context_hidden,
    searchable_message_text,
};
use thiserror::Error;

use crate::automation_thread_metadata::thread_metadata_is_automation_trigger;

/// Failures a caller can act on are `Invalid`/`NotFound`; storage faults are
/// `Unavailable` and keep their cause for server-side logs.
#[derive(Debug, Error)]
pub enum HistorySearchError {
    #[error("invalid {field}: {reason}")]
    Invalid {
        field: &'static str,
        reason: &'static str,
    },
    #[error("thread not found")]
    NotFound,
    #[error("history storage unavailable: {0}")]
    Unavailable(#[source] SessionThreadError),
}

impl HistorySearchError {
    fn from_thread(error: SessionThreadError) -> Self {
        match error {
            SessionThreadError::UnknownThread { .. } | SessionThreadError::UnknownMessage { .. } => {
                Self::NotFound
            }
            other => Self::Unavailable(other),
        }
    }
}

impl From<HistorySearchError> for ironclaw_product_contracts::surface::ProductSurfaceError {
    fn from(error: HistorySearchError) -> Self {
        use ironclaw_product_contracts::surface::{
            ProductSurfaceError, ProductSurfaceValidationCode,
        };
        match error {
            HistorySearchError::Invalid { field, .. } => {
                ProductSurfaceError::validation(field, ProductSurfaceValidationCode::InvalidValue)
            }
            HistorySearchError::NotFound => ProductSurfaceError::not_found(),
            HistorySearchError::Unavailable(source) => {
                tracing::debug!(error = %source, "history search storage unavailable");
                ProductSurfaceError::service_unavailable(true)
            }
        }
    }
}

/// The caller's own thread cell: the same scope the thread list uses.
pub fn caller_thread_scope(
    caller: &ironclaw_product_contracts::surface::ProductSurfaceCaller,
) -> Option<ThreadScope> {
    Some(ThreadScope {
        tenant_id: caller.tenant_id.clone(),
        agent_id: caller.agent_id.clone()?,
        project_id: caller.project_id.clone(),
        owner_user_id: Some(caller.user_id.clone()),
        mission_id: None,
    })
}

/// Answer one history request for `scope` (the caller's own thread cell).
pub async fn run_history_search(
    threads: &dyn SessionThreadService,
    scope: &ThreadScope,
    request: &HistorySearchRequest,
) -> Result<HistorySearchResponse, HistorySearchError> {
    match request.mode() {
        HistorySearchMode::Discover => discover(threads, scope, request).await,
        HistorySearchMode::Scroll => scroll(threads, scope, request).await,
        HistorySearchMode::Read => read(threads, scope, request).await,
        HistorySearchMode::Browse => browse(threads, scope, request).await,
    }
}

fn parse_thread_id(raw: &str) -> Result<ThreadId, HistorySearchError> {
    ThreadId::new(raw).map_err(|error| {
        tracing::debug!(%error, "history search rejected thread id");
        HistorySearchError::Invalid {
            field: "thread_id",
            reason: "not a thread id",
        }
    })
}

fn thread_limit(request: &HistorySearchRequest) -> usize {
    request
        .limit
        .unwrap_or(HISTORY_DEFAULT_THREADS)
        .clamp(1, HISTORY_MAX_THREADS) as usize
}

/// Where a thread came from, judged from its own metadata.
pub fn thread_origin(record: &SessionThreadRecord) -> HistoryThreadOrigin {
    let automation = record
        .metadata_json
        .as_deref()
        .map(thread_metadata_is_automation_trigger)
        .is_some_and(|parsed| parsed.unwrap_or(false)); // silent-ok: unparsable metadata classifies as the person's own thread
    if automation {
        HistoryThreadOrigin::Automation
    } else if record_is_prepared_context_hidden(record) {
        HistoryThreadOrigin::Subagent
    } else {
        HistoryThreadOrigin::Own
    }
}

fn thread_result(record: &SessionThreadRecord) -> HistoryThreadResult {
    HistoryThreadResult {
        thread_id: record.thread_id.as_str().to_string(),
        title: record.title.clone(),
        updated_at: record.updated_at.or(record.created_at),
        origin: thread_origin(record),
        lineage_root_thread_id: None,
        related_thread_ids: Vec::new(),
        hits: Vec::new(),
    }
}

fn role(kind: SearchHitRole) -> HistoryRole {
    match kind {
        SearchHitRole::User => HistoryRole::User,
        SearchHitRole::Assistant => HistoryRole::Assistant,
    }
}

/// The person's own threads first, then automation and subagent threads;
/// relevance (or recency) order is kept inside each tier.
pub fn demote_non_own(threads: &mut [HistoryThreadResult]) {
    threads.sort_by_key(|thread| thread.origin != HistoryThreadOrigin::Own);
}

async fn discover(
    threads: &dyn SessionThreadService,
    scope: &ThreadScope,
    request: &HistorySearchRequest,
) -> Result<HistorySearchResponse, HistorySearchError> {
    let query = request.query.clone().unwrap_or_default();
    let only = request.thread_id.as_deref().map(parse_thread_id).transpose()?;
    let limit = thread_limit(request);
    // Narrowing to one thread searches wider and filters, so that thread's
    // hits are not crowded out by better-ranked threads.
    let max_threads = if only.is_some() {
        ironclaw_threads::HISTORY_SEARCH_MAX_THREADS
    } else {
        limit
    };
    let found = threads
        .search_thread_messages(SearchThreadMessagesRequest {
            scope: scope.clone(),
            query: query.clone(),
            max_threads,
            max_hits_per_thread: HISTORY_SEARCH_MAX_HITS_PER_THREAD,
        })
        .await
        .map_err(HistorySearchError::from_thread)?;
    let mut results: Vec<HistoryThreadResult> = found
        .threads
        .into_iter()
        .filter(|group| only.as_ref().is_none_or(|id| &group.thread.thread_id == id))
        .map(|group| {
            let mut result = thread_result(&group.thread);
            result.hits = group
                .hits
                .into_iter()
                .map(|hit| HistoryHit {
                    thread_id: result.thread_id.clone(),
                    message_id: hit.message_id.to_string(),
                    sequence: hit.sequence,
                    role: role(hit.role),
                    created_at: hit.created_at,
                    snippet: hit.snippet,
                    highlights: hit
                        .highlights
                        .into_iter()
                        .map(|range| HistoryHighlight {
                            start: range.start,
                            end: range.end,
                        })
                        .collect(),
                })
                .collect();
            result
        })
        .collect();
    demote_non_own(&mut results);
    results.truncate(limit);
    Ok(HistorySearchResponse {
        mode: HistorySearchMode::Discover,
        query: Some(query),
        threads: results,
        thread: None,
        messages: Vec::new(),
        next_from_sequence: None,
        index_complete: found.index_complete,
    })
}

async fn browse(
    threads: &dyn SessionThreadService,
    scope: &ThreadScope,
    request: &HistorySearchRequest,
) -> Result<HistorySearchResponse, HistorySearchError> {
    let listed = threads
        .list_threads_for_scope(ListThreadsForScopeRequest {
            scope: scope.clone(),
            limit: Some(thread_limit(request) as u32),
            cursor: None,
        })
        .await
        .map_err(HistorySearchError::from_thread)?;
    let mut results: Vec<HistoryThreadResult> = listed.threads.iter().map(thread_result).collect();
    demote_non_own(&mut results);
    Ok(HistorySearchResponse {
        mode: HistorySearchMode::Browse,
        query: None,
        threads: results,
        thread: None,
        messages: Vec::new(),
        next_from_sequence: None,
        index_complete: true,
    })
}

fn history_message(record: &ThreadMessageRecord) -> Option<HistoryMessage> {
    let text = searchable_message_text(record)?;
    let role = match record.kind {
        MessageKind::User => HistoryRole::User,
        MessageKind::Assistant => HistoryRole::Assistant,
        _ => return None,
    };
    let truncated = text.chars().count() > HISTORY_MESSAGE_MAX_CHARS;
    let text = if truncated {
        let mut cut: String = text.chars().take(HISTORY_MESSAGE_MAX_CHARS).collect();
        cut.push('…');
        cut
    } else {
        text.to_string()
    };
    Some(HistoryMessage {
        message_id: record.message_id.to_string(),
        sequence: record.sequence,
        role,
        created_at: record.created_at,
        text,
        truncated,
    })
}

async fn scroll(
    threads: &dyn SessionThreadService,
    scope: &ThreadScope,
    request: &HistorySearchRequest,
) -> Result<HistorySearchResponse, HistorySearchError> {
    let thread_id = parse_thread_id(request.thread_id.as_deref().unwrap_or_default())?;
    let anchor = match (&request.around_message_id, request.around_sequence) {
        (Some(raw), _) => {
            let message_id = ThreadMessageId::parse(raw).map_err(|error| {
                tracing::debug!(%error, "history search rejected message id");
                HistorySearchError::Invalid {
                    field: "around_message_id",
                    reason: "not a message id",
                }
            })?;
            threads
                .read_thread_message(scope, &thread_id, message_id)
                .await
                .map_err(HistorySearchError::from_thread)?
                .ok_or(HistorySearchError::NotFound)?
                .sequence
        }
        (None, Some(sequence)) => sequence,
        (None, None) => {
            return Err(HistorySearchError::Invalid {
                field: "around_message_id",
                reason: "scroll needs an anchor",
            });
        }
    };
    let window = u64::from(
        request
            .window
            .unwrap_or(HISTORY_DEFAULT_WINDOW)
            .clamp(1, HISTORY_MAX_WINDOW),
    );
    // Sequences count every transcript row (tool results too), so read a
    // wider raw range and keep `window` conversation messages on each side.
    let raw_span = window.saturating_mul(4);
    let range = threads
        .list_thread_messages_range(ThreadMessageRangeRequest {
            scope: scope.clone(),
            thread_id: thread_id.clone(),
            after_sequence: anchor.saturating_sub(raw_span + 1),
            through_sequence: anchor.saturating_add(raw_span),
        })
        .await
        .map_err(HistorySearchError::from_thread)?;
    let visible: Vec<HistoryMessage> = range.messages.iter().filter_map(history_message).collect();
    let center = visible
        .iter()
        .position(|m| m.sequence >= anchor)
        .unwrap_or(visible.len());
    let start = center.saturating_sub(window as usize);
    let end = (center + window as usize + 1).min(visible.len());
    Ok(HistorySearchResponse {
        mode: HistorySearchMode::Scroll,
        query: None,
        threads: Vec::new(),
        thread: Some(thread_result(&range.thread)),
        messages: visible[start..end].to_vec(),
        next_from_sequence: None,
        index_complete: true,
    })
}

async fn read(
    threads: &dyn SessionThreadService,
    scope: &ThreadScope,
    request: &HistorySearchRequest,
) -> Result<HistorySearchResponse, HistorySearchError> {
    let thread_id = parse_thread_id(request.thread_id.as_deref().unwrap_or_default())?;
    let limit = request
        .limit
        .unwrap_or(HISTORY_DEFAULT_READ_MESSAGES)
        .clamp(1, HISTORY_MAX_READ_MESSAGES) as usize;
    let from = request.from_sequence.unwrap_or(0);
    let history = threads
        .list_thread_history(ThreadHistoryRequest {
            scope: scope.clone(),
            thread_id,
        })
        .await
        .map_err(HistorySearchError::from_thread)?;
    let mut visible = history
        .messages
        .iter()
        .filter(|m| m.sequence >= from)
        .filter_map(history_message);
    let messages: Vec<HistoryMessage> = visible.by_ref().take(limit).collect();
    let next_from_sequence = visible.next().map(|m| m.sequence);
    Ok(HistorySearchResponse {
        mode: HistorySearchMode::Read,
        query: None,
        threads: Vec::new(),
        thread: Some(thread_result(&history.thread)),
        messages,
        next_from_sequence,
        index_complete: true,
    })
}

#[cfg(test)]
mod tests {
    use ironclaw_host_api::ids::{AgentId, TenantId, UserId};
    use ironclaw_threads::{
        AcceptInboundMessageRequest, AppendFinalizedAssistantMessageRequest, EnsureThreadRequest,
        InMemorySessionThreadService, MessageContent,
    };

    use super::*;
    use crate::automation_thread_metadata::automation_trigger_thread_metadata_json;

    fn scope() -> ThreadScope {
        ThreadScope {
            tenant_id: TenantId::new("t1").unwrap(),
            agent_id: AgentId::new("a1").unwrap(),
            project_id: None,
            owner_user_id: Some(UserId::new("u1").unwrap()),
            mission_id: None,
        }
    }

    async fn thread(
        service: &InMemorySessionThreadService,
        id: &str,
        metadata_json: Option<String>,
        exchanges: &[(&str, &str)],
    ) {
        let thread_id = ThreadId::new(id).unwrap();
        service
            .ensure_thread(EnsureThreadRequest {
                scope: scope(),
                thread_id: Some(thread_id.clone()),
                created_by_actor_id: "u1".into(),
                title: None,
                metadata_json,
            })
            .await
            .unwrap();
        for (n, (ask, answer)) in exchanges.iter().enumerate() {
            service
                .accept_inbound_message(AcceptInboundMessageRequest {
                    scope: scope(),
                    thread_id: thread_id.clone(),
                    actor_id: "u1".into(),
                    source_binding_id: None,
                    reply_target_binding_id: None,
                    external_event_id: None,
                    content: MessageContent::text(*ask),
                })
                .await
                .unwrap();
            service
                .append_finalized_assistant_message(AppendFinalizedAssistantMessageRequest {
                    scope: scope(),
                    thread_id: thread_id.clone(),
                    turn_run_id: format!("{id}-run-{n}"),
                    content: MessageContent::text(*answer),
                })
                .await
                .unwrap();
        }
    }

    async fn ask(
        service: &InMemorySessionThreadService,
        request: HistorySearchRequest,
    ) -> Result<HistorySearchResponse, HistorySearchError> {
        run_history_search(service, &scope(), &request).await
    }

    #[tokio::test]
    async fn discover_ranks_automation_runs_below_the_persons_own_chats() {
        let service = InMemorySessionThreadService::default();
        // The automation thread matches more terms, and still ranks second.
        thread(
            &service,
            "auto",
            Some(automation_trigger_thread_metadata_json("brief")),
            &[("morning invoice report", "invoice report sent")],
        )
        .await;
        thread(&service, "mine", None, &[("where is the invoice?", "in billing")]).await;
        let found = ask(
            &service,
            HistorySearchRequest {
                query: Some("invoice report".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(found.mode, HistorySearchMode::Discover);
        let order: Vec<(&str, HistoryThreadOrigin)> = found
            .threads
            .iter()
            .map(|t| (t.thread_id.as_str(), t.origin))
            .collect();
        assert_eq!(
            order,
            [
                ("mine", HistoryThreadOrigin::Own),
                ("auto", HistoryThreadOrigin::Automation)
            ]
        );
        let hit = &found.threads[0].hits[0];
        assert_eq!(hit.thread_id, "mine");
        let h = hit.highlights[0];
        assert_eq!(&hit.snippet[h.start..h.end], "invoice");
    }

    #[tokio::test]
    async fn scroll_returns_a_window_of_conversation_messages_around_a_hit() {
        let service = InMemorySessionThreadService::default();
        let exchanges: Vec<(String, String)> = (0..10)
            .map(|n| (format!("question {n}"), format!("answer {n}")))
            .collect();
        let pairs: Vec<(&str, &str)> = exchanges
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        thread(&service, "long", None, &pairs).await;
        let found = ask(
            &service,
            HistorySearchRequest {
                query: Some("question 5".into()),
                thread_id: Some("long".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let anchor = found.threads[0]
            .hits
            .iter()
            .find(|h| h.snippet == "question 5")
            .unwrap()
            .clone();
        let window = ask(
            &service,
            HistorySearchRequest {
                thread_id: Some("long".into()),
                around_message_id: Some(anchor.message_id.clone()),
                window: Some(2),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(window.mode, HistorySearchMode::Scroll);
        let texts: Vec<&str> = window.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(
            texts,
            ["question 4", "answer 4", "question 5", "answer 5", "question 6"]
        );
    }

    #[tokio::test]
    async fn read_is_bounded_per_message_and_pages_forward() {
        let service = InMemorySessionThreadService::default();
        let huge = "x".repeat(HISTORY_MESSAGE_MAX_CHARS + 50);
        thread(&service, "big", None, &[(huge.as_str(), "ok"), ("next", "done")]).await;
        let first = ask(
            &service,
            HistorySearchRequest {
                thread_id: Some("big".into()),
                limit: Some(3),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(first.mode, HistorySearchMode::Read);
        assert_eq!(first.messages.len(), 3);
        assert!(first.messages[0].truncated);
        assert_eq!(
            first.messages[0].text.chars().count(),
            HISTORY_MESSAGE_MAX_CHARS + 1
        );
        let rest = ask(
            &service,
            HistorySearchRequest {
                thread_id: Some("big".into()),
                from_sequence: first.next_from_sequence,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let texts: Vec<&str> = rest.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, ["done"]);
        assert_eq!(rest.next_from_sequence, None);
    }

    #[tokio::test]
    async fn browse_lists_recent_threads_and_bad_ids_are_correctable() {
        let service = InMemorySessionThreadService::default();
        thread(&service, "one", None, &[("hello", "hi")]).await;
        let browsed = ask(&service, HistorySearchRequest::default()).await.unwrap();
        assert_eq!(browsed.mode, HistorySearchMode::Browse);
        assert_eq!(browsed.threads[0].thread_id, "one");
        let missing = ask(
            &service,
            HistorySearchRequest {
                thread_id: Some("nope".into()),
                ..Default::default()
            },
        )
        .await;
        assert!(matches!(missing, Err(HistorySearchError::NotFound)));
        let bad = ask(
            &service,
            HistorySearchRequest {
                thread_id: Some("one".into()),
                around_message_id: Some("not-a-uuid".into()),
                ..Default::default()
            },
        )
        .await;
        assert!(matches!(
            bad,
            Err(HistorySearchError::Invalid {
                field: "around_message_id",
                ..
            })
        ));
    }
}
