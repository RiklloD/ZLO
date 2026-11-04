//! Orchestrator knowledge layered on history search, and the
//! `ironclaw.history.search` model tool.
//!
//! The transcript service knows message text; the engine knows how threads
//! relate: a delegated child (subagent) has a parent task, a fork has a fork
//! transfer from its source. Delegated children rank below the person's own
//! threads, and forks collapse into the group of their lineage root so one
//! conversation does not fill the results several times.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_host_api::{
    ids::{InvocationId, ThreadId, UserId},
    resolution::Resolution,
    result_meta::FailureKind,
};
use ironclaw_loop_contracts::{
    AgentLoopHostError, AgentLoopHostErrorKind, CapabilityFailureDetail, CapabilityProgress,
    resolution,
};
use ironclaw_loop_host::{
    CapabilityResultWrite, DurablePersistence, SyntheticCapability, SyntheticCapabilityDescriptor,
    SyntheticCapabilityHandler, SyntheticCapabilityInvocation,
};
use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::profiles::ProfileOwner;
use ironclaw_product_contracts::history::{
    HISTORY_SEARCH_VIEW, HistorySearchMode, HistorySearchRequest, HistorySearchResponse,
    HistoryThreadOrigin, HistoryThreadResult,
};
use ironclaw_threads::{HISTORY_SEARCH_MAX_HITS_PER_THREAD, SessionThreadService, ThreadScope};

use crate::history_search::{HistorySearchError, demote_non_own, run_history_search};

/// Capability id and provider tool name of the history tool.
pub const HISTORY_SEARCH_CAPABILITY_ID: &str = "ironclaw.history.search";
pub const HISTORY_SEARCH_TOOL_NAME: &str = "history_search";
/// Fork hops followed to a lineage root; forks of forks are rare and short.
const MAX_FORK_DEPTH: usize = 16;

const DESCRIPTION: &str = include_str!("../../prompts/history_search_tool.md");

/// Annotate `response` with delegation and fork lineage from the engine:
/// delegated children become `Subagent` (ranked below own threads), and
/// threads sharing a fork root collapse into one group. Engine read failures
/// leave the transcript answer as it is — lineage only refines ranking.
pub(crate) async fn apply_lineage(
    engine: &Engine,
    owner: &ProfileOwner,
    response: &mut HistorySearchResponse,
) {
    if response.threads.is_empty() && response.thread.is_none() {
        return;
    }
    let parents: HashMap<String, Option<ThreadId>> = match engine.store().inbox_threads(owner).await
    {
        Ok(threads) => threads
            .into_iter()
            .map(|t| (t.thread_id.as_str().to_string(), t.parent_thread_id))
            .collect(),
        Err(error) => {
            tracing::debug!(%error, "history search skips delegation lineage");
            HashMap::new()
        }
    };
    let mut roots: HashMap<String, String> = HashMap::new();
    let mut targets: Vec<&mut HistoryThreadResult> = response.threads.iter_mut().collect();
    if let Some(thread) = response.thread.as_mut() {
        targets.push(thread);
    }
    for result in targets {
        if parents
            .get(&result.thread_id)
            .is_some_and(|parent| parent.is_some())
            && result.origin == HistoryThreadOrigin::Own
        {
            result.origin = HistoryThreadOrigin::Subagent;
        }
        let root = fork_root(engine, &result.thread_id).await;
        if root != result.thread_id {
            result.lineage_root_thread_id = Some(root.clone());
        }
        roots.insert(result.thread_id.clone(), root);
    }
    if response.mode == HistorySearchMode::Discover || response.mode == HistorySearchMode::Browse {
        collapse_lineage(&mut response.threads, &roots);
        demote_non_own(&mut response.threads);
    }
}

async fn fork_root(engine: &Engine, thread_id: &str) -> String {
    let Ok(mut at) = ThreadId::new(thread_id) else {
        return thread_id.to_string();
    };
    for _ in 0..MAX_FORK_DEPTH {
        match engine.store().fork_origin(&at).await {
            Ok(Some(transfer)) if transfer.source_thread != at => at = transfer.source_thread,
            Ok(_) => break,
            Err(error) => {
                tracing::debug!(%error, "history search stops at an unreadable fork link");
                break;
            }
        }
    }
    at.as_str().to_string()
}

/// Merge groups that share a lineage root into the best-ranked one, keeping
/// order; the others' ids go to `related_thread_ids`.
fn collapse_lineage(threads: &mut Vec<HistoryThreadResult>, roots: &HashMap<String, String>) {
    let mut kept: Vec<HistoryThreadResult> = Vec::with_capacity(threads.len());
    let mut index_by_root: HashMap<String, usize> = HashMap::new();
    for result in threads.drain(..) {
        let root = roots
            .get(&result.thread_id)
            .cloned()
            .unwrap_or_else(|| result.thread_id.clone());
        match index_by_root.get(&root) {
            Some(&index) => {
                let group = &mut kept[index];
                group.related_thread_ids.push(result.thread_id.clone());
                group.related_thread_ids.extend(result.related_thread_ids);
                let room = HISTORY_SEARCH_MAX_HITS_PER_THREAD.saturating_sub(group.hits.len());
                group.hits.extend(result.hits.into_iter().take(room));
            }
            None => {
                index_by_root.insert(root, kept.len());
                kept.push(result);
            }
        }
    }
    *threads = kept;
}

/// The `history_search` model tool for one run's capability surface. It acts
/// on the run's acting user's own threads only; no model calls happen inside.
pub fn history_search_capability(
    threads: Arc<dyn SessionThreadService>,
    engine: Option<Arc<Engine>>,
    fallback_user_id: UserId,
) -> Result<SyntheticCapability, AgentLoopHostError> {
    Ok(SyntheticCapability::new(
        SyntheticCapabilityDescriptor::new(
            HISTORY_SEARCH_CAPABILITY_ID,
            HISTORY_SEARCH_TOOL_NAME,
            DESCRIPTION.trim(),
            history_search_input_schema(),
        )?,
        Arc::new(HistorySearchToolHandler {
            threads,
            engine,
            fallback_user_id,
        }),
    ))
}

pub fn history_search_input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "query": {"type": "string", "description": "Words to find in past chats. Omit to browse, scroll or read."},
            "thread_id": {"type": "string", "description": "Restrict to one thread; alone it reads that thread."},
            "around_message_id": {"type": "string", "description": "With thread_id: return the messages around this one."},
            "around_sequence": {"type": "integer", "minimum": 0},
            "limit": {"type": "integer", "minimum": 1, "description": "Threads (search/browse) or messages (read)."},
            "window": {"type": "integer", "minimum": 1, "description": "Messages on each side when scrolling."},
            "from_sequence": {"type": "integer", "minimum": 0, "description": "Read: continue from next_from_sequence."}
        }
    })
}

struct HistorySearchToolHandler {
    threads: Arc<dyn SessionThreadService>,
    engine: Option<Arc<Engine>>,
    fallback_user_id: UserId,
}

fn model_failure(kind: FailureKind, summary: &str) -> Resolution {
    resolution::failed(
        kind,
        summary.to_string(),
        CapabilityFailureDetail::Diagnostic {
            text: summary.to_string(),
        },
    )
}

#[async_trait]
impl SyntheticCapabilityHandler for HistorySearchToolHandler {
    fn validate_provider_arguments(
        &self,
        _arguments: &serde_json::Value,
    ) -> Result<(), AgentLoopHostError> {
        // Malformed arguments are a model-correctable failure answered by
        // `invoke`, never a turn-ending registration error.
        Ok(())
    }

    async fn invoke(
        &self,
        invocation: SyntheticCapabilityInvocation,
    ) -> Result<Resolution, AgentLoopHostError> {
        let input = if invocation.input.is_null() {
            serde_json::json!({})
        } else {
            invocation.input.clone()
        };
        let request: HistorySearchRequest = match serde_json::from_value(input) {
            Ok(request) => request,
            Err(error) => {
                tracing::debug!(%error, "history_search arguments rejected");
                return Ok(model_failure(
                    FailureKind::InputEncode,
                    "history_search arguments are invalid: use query, thread_id, around_message_id, around_sequence, limit, window, from_sequence",
                ));
            }
        };
        let run = &invocation.run_context;
        let user_id = run.acting_user_id(&self.fallback_user_id);
        let resource = run.scope.to_resource_scope();
        let Some(agent_id) = resource.agent_id.clone() else {
            return Err(AgentLoopHostError::new(
                AgentLoopHostErrorKind::Unavailable,
                "history search requires an agent-scoped run",
            ));
        };
        let scope = ThreadScope {
            tenant_id: resource.tenant_id.clone(),
            agent_id,
            project_id: resource.project_id.clone(),
            owner_user_id: Some(user_id.clone()),
            mission_id: None,
        };
        let mut response = match run_history_search(self.threads.as_ref(), &scope, &request).await {
            Ok(response) => response,
            Err(HistorySearchError::Invalid { field, reason }) => {
                return Ok(model_failure(
                    FailureKind::InputEncode,
                    &format!("history_search: {field} {reason}"),
                ));
            }
            Err(HistorySearchError::NotFound) => {
                return Ok(model_failure(
                    FailureKind::OperationFailed,
                    "history_search: no such thread or message in your history",
                ));
            }
            Err(HistorySearchError::Unavailable(error)) => {
                tracing::debug!(%error, "history_search storage unavailable");
                return Ok(model_failure(
                    FailureKind::OperationFailed,
                    "history_search: chat history is temporarily unavailable",
                ));
            }
        };
        if let Some(engine) = &self.engine {
            let owner = ProfileOwner {
                tenant_id: resource.tenant_id,
                user_id,
            };
            apply_lineage(engine, &owner, &mut response).await;
        }
        let summary = match response.mode {
            HistorySearchMode::Discover => {
                format!("history_search found {} thread(s)", response.threads.len())
            }
            HistorySearchMode::Browse => {
                format!("history_search listed {} thread(s)", response.threads.len())
            }
            HistorySearchMode::Scroll | HistorySearchMode::Read => {
                format!("history_search read {} message(s)", response.messages.len())
            }
        };
        let output = serde_json::to_value(&response).map_err(|error| {
            tracing::debug!(%error, "history_search output did not encode");
            AgentLoopHostError::new(
                AgentLoopHostErrorKind::Internal,
                "history search output did not encode",
            )
        })?;
        let write = invocation
            .result_writer
            .write_capability_result(CapabilityResultWrite {
                run_context: &invocation.run_context,
                input_ref: &invocation.request.input_ref,
                invocation_id: InvocationId::new(),
                capability_id: &invocation.request.capability_id,
                output,
                display_preview: None,
                durable_persistence: DurablePersistence::Persist,
            })
            .await?;
        Ok(resolution::completed(
            write.result_ref,
            summary,
            CapabilityProgress::MadeProgress,
            false,
            write.byte_len,
            write.output_digest,
            write.model_observation,
        ))
    }
}

/// The decorator's half of the `history.search` view: the inner surface
/// answers from transcripts, then lineage refines the ranking.
pub(crate) fn is_history_view(view_id: &str) -> bool {
    view_id == HISTORY_SEARCH_VIEW.id
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(id: &str, origin: HistoryThreadOrigin) -> HistoryThreadResult {
        HistoryThreadResult {
            thread_id: id.into(),
            title: None,
            updated_at: None,
            origin,
            lineage_root_thread_id: None,
            related_thread_ids: Vec::new(),
            hits: Vec::new(),
        }
    }

    #[test]
    fn forks_collapse_into_the_best_ranked_member_of_their_lineage() {
        let mut threads = vec![
            result("fork-b", HistoryThreadOrigin::Own),
            result("other", HistoryThreadOrigin::Own),
            result("root", HistoryThreadOrigin::Own),
        ];
        let roots = HashMap::from([
            ("fork-b".to_string(), "root".to_string()),
            ("other".to_string(), "other".to_string()),
            ("root".to_string(), "root".to_string()),
        ]);
        collapse_lineage(&mut threads, &roots);
        let ids: Vec<&str> = threads.iter().map(|t| t.thread_id.as_str()).collect();
        assert_eq!(ids, ["fork-b", "other"]);
        assert_eq!(threads[0].related_thread_ids, ["root"]);
    }

    /// The undecorated surface answers the view from "transcripts": a fork,
    /// an unrelated thread, and the fork's source, in that rank order.
    struct Inner;

    #[async_trait]
    impl ironclaw_product_contracts::surface::ProductSurface for Inner {
        async fn invoke(
            &self,
            _caller: ironclaw_product_contracts::surface::ProductSurfaceCaller,
            _request: ironclaw_product_contracts::surface::ProductSurfaceInvokeRequest,
        ) -> Result<
            ironclaw_product_contracts::surface::ProductSurfaceInvokeResponse,
            ironclaw_product_contracts::surface::ProductSurfaceError,
        > {
            Err(ironclaw_product_contracts::surface::ProductSurfaceError::not_found())
        }

        async fn query(
            &self,
            _caller: ironclaw_product_contracts::surface::ProductSurfaceCaller,
            request: ironclaw_product_contracts::surface::ProductSurfaceQueryRequest,
        ) -> Result<
            ironclaw_product_contracts::surface::ProductSurfaceQueryPage,
            ironclaw_product_contracts::surface::ProductSurfaceError,
        > {
            assert_eq!(request.view_id, HISTORY_SEARCH_VIEW.id);
            let response = HistorySearchResponse {
                mode: HistorySearchMode::Discover,
                query: Some("deploy".into()),
                threads: vec![
                    result("fork", HistoryThreadOrigin::Own),
                    result("other", HistoryThreadOrigin::Own),
                    result("root", HistoryThreadOrigin::Own),
                ],
                thread: None,
                messages: Vec::new(),
                next_from_sequence: None,
                index_complete: true,
            };
            Ok(ironclaw_product_contracts::surface::ProductSurfaceQueryPage {
                items: vec![serde_json::to_value(response).unwrap()],
                next_cursor: None,
            })
        }

        async fn stream_events(
            &self,
            _caller: ironclaw_product_contracts::surface::ProductSurfaceCaller,
            _request: ironclaw_product_contracts::surface::ProductSurfaceStreamRequest,
        ) -> Result<
            ironclaw_product_contracts::surface::ProductSurfaceStreamResponse,
            ironclaw_product_contracts::surface::ProductSurfaceError,
        > {
            Err(ironclaw_product_contracts::surface::ProductSurfaceError::not_found())
        }
    }

    #[tokio::test]
    async fn the_decorated_view_collapses_forks_into_their_lineage_root() {
        use ironclaw_host_api::ids::TenantId;
        use ironclaw_orchestrator::model::{
            Author, TransferDelivery, TransferId, TransferKind, TransferRecord, TransferStatus,
            TransferStrategy, now_ms,
        };
        use ironclaw_product_contracts::surface::{ProductSurface, ProductSurfaceCaller};

        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            libsql::Builder::new_local(dir.path().join("o.db"))
                .build()
                .await
                .unwrap(),
        );
        let runtime =
            Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(Arc::clone(&db)).unwrap());
        let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
        engine
            .store()
            .insert_transfer(&TransferRecord {
                id: TransferId::generate(),
                kind: TransferKind::Fork,
                source_thread: ThreadId::new("root").unwrap(),
                target_thread: Some(ThreadId::new("fork").unwrap()),
                source_point: None,
                base_point: None,
                strategy: TransferStrategy::PortableContext,
                status: TransferStatus::Consumed,
                delivery: TransferDelivery::NextMessage,
                handoff: None,
                created_by: Author::User,
                created_at: now_ms(),
                consumed_at: Some(now_ms()),
            })
            .await
            .unwrap();
        let surface =
            crate::orchestration::OrchestratedProductSurface::new(Arc::new(Inner), engine, None);
        let page = surface
            .query(
                ProductSurfaceCaller::new(
                    TenantId::new("t1").unwrap(),
                    UserId::new("u1").unwrap(),
                    None,
                    None,
                ),
                ironclaw_product_contracts::surface::ProductSurfaceQueryRequest {
                    view_id: HISTORY_SEARCH_VIEW.id.into(),
                    input: serde_json::json!({"query": "deploy"}),
                    cursor: None,
                    limit: None,
                },
            )
            .await
            .unwrap();
        let response: HistorySearchResponse =
            serde_json::from_value(page.items[0].clone()).unwrap();
        let ids: Vec<&str> = response.threads.iter().map(|t| t.thread_id.as_str()).collect();
        assert_eq!(ids, ["fork", "other"], "the fork and its root are one result");
        assert_eq!(response.threads[0].lineage_root_thread_id.as_deref(), Some("root"));
        assert_eq!(response.threads[0].related_thread_ids, ["root"]);
    }

    #[test]
    fn schema_matches_the_request_fields() {
        let schema = history_search_input_schema();
        let props = schema["properties"].as_object().map(|p| p.len());
        assert_eq!(props, Some(7));
        let all = serde_json::json!({
            "query": "q", "thread_id": "t", "around_message_id": "m", "around_sequence": 1,
            "limit": 1, "window": 1, "from_sequence": 1
        });
        assert!(serde_json::from_value::<HistorySearchRequest>(all).is_ok());
    }
}
