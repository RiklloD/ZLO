//! History search with the orchestrator: the `history.search` view
//! decorator, the `ironclaw.history.search` model tool, and the lineage they
//! share.
//!
//! The transcript service knows message text (canonical rows and the search
//! projections of CLI transcripts, see `history_ingest`); the engine knows how
//! threads relate: a delegated child (subagent) has a parent task, a fork has
//! a fork transfer from its source. Delegated children rank below the
//! person's own threads (inside the index query, before the answer is cut),
//! and forks collapse into the group of their lineage root. A CLI thread's
//! messages are read and scrolled from its runtime transcript.
//!
//! Answering is a pure read for both callers. Only the view starts indexing
//! (a bounded background catch-up of the canonical index and a one-time
//! sweep of the owner's CLI threads); the model tool never writes.

use std::collections::{HashMap, HashSet};
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
use ironclaw_orchestrator::model::ThreadRecord;
use ironclaw_orchestrator::profiles::ProfileOwner;
use ironclaw_product_contracts::history::{
    HISTORY_DEFAULT_READ_MESSAGES, HISTORY_DEFAULT_WINDOW, HISTORY_MAX_READ_MESSAGES,
    HISTORY_MAX_WINDOW, HISTORY_MESSAGE_MAX_CHARS, HISTORY_SEARCH_VIEW, HistorySearchMessage,
    HistorySearchMode, HistorySearchRequest, HistorySearchResponse, HistorySearchRole,
    HistoryThreadOrigin, HistoryThreadResult,
};
use ironclaw_product_contracts::surface::{
    ProductSurface, ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceErrorCode,
    ProductSurfaceInvokeRequest, ProductSurfaceInvokeResponse, ProductSurfaceQueryPage,
    ProductSurfaceQueryRequest, ProductSurfaceStreamRequest, ProductSurfaceStreamResponse,
    ProductSurfaceValidationCode,
};
use ironclaw_threads::{
    HISTORY_SEARCH_MAX_HITS_PER_THREAD, SessionThreadService, ThreadScope, visible_user_text,
};

use super::external::{segment_message_id, timeline_rows};
use super::history_ingest::{IngestOnCompletion, owner_swept, spawn_owner_sweep};
use crate::history_search::{
    DelegatedThreads, EngineKnowledge, HistorySearchError, caller_thread_scope, demote_non_own,
    run_history_search, spawn_index_catch_up,
};

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

/// Thread ids the engine records as delegated children of the owner.
async fn delegated_threads(engine: &Engine, owner: &ProfileOwner) -> DelegatedThreads {
    match engine.store().inbox_threads(owner).await {
        Ok(threads) => Arc::new(
            threads
                .into_iter()
                .filter(|t| t.parent_thread_id.is_some())
                .map(|t| t.thread_id.as_str().to_string())
                .collect::<HashSet<String>>(),
        ),
        Err(error) => {
            tracing::debug!(%error, "history search skips delegation tiers");
            DelegatedThreads::default()
        }
    }
}

/// The engine's provenance for the owner: delegated children and the runs
/// it started with its own text. A read failure leaves it empty (search
/// still answers; ranking and exclusion are refinements).
pub(crate) async fn engine_knowledge(engine: &Engine, owner: &ProfileOwner) -> EngineKnowledge {
    let scaffolding = match engine.store().scaffolding_runs(owner).await {
        Ok(runs) => Arc::new(runs),
        Err(error) => {
            tracing::debug!(%error, "history search skips scaffolding provenance");
            Arc::default()
        }
    };
    EngineKnowledge {
        delegated: delegated_threads(engine, owner).await,
        scaffolding,
    }
}

/// Answer one history request. A pure read, shared by the view and the
/// model tool. `orch_scope` is the caller as the engine sees it (thread
/// ownership); `scope` is the caller's own transcript cell.
pub(crate) async fn answer_history(
    engine: Option<&Engine>,
    threads: &dyn SessionThreadService,
    scope: &ThreadScope,
    orch_scope: &ironclaw_orchestrator::model::ThreadScope,
    request: &HistorySearchRequest,
) -> Result<HistorySearchResponse, HistorySearchError> {
    // Every read below is write-free only once the scope is ready (its
    // migrations and index done by the background step); before that,
    // refuse rather than let a read path migrate.
    if !threads
        .message_search_ready(scope)
        .await
        .map_err(HistorySearchError::Unavailable)?
    {
        return Err(HistorySearchError::NotReady);
    }
    let Some(engine) = engine else {
        return run_history_search(threads, scope, request, &EngineKnowledge::default()).await;
    };
    let owner = ProfileOwner::from(orch_scope);
    let knowledge = engine_knowledge(engine, &owner).await;
    if matches!(
        request.mode(),
        HistorySearchMode::Scroll | HistorySearchMode::Read
    ) && let Some(raw) = request.thread_id.as_deref()
        && let Ok(id) = ironclaw_host_api::ids::ThreadId::new(raw)
        && let Ok(Some(thread)) = engine.external_thread(&id).await
        && engine.owned_thread(&id, orch_scope).await.is_ok()
    {
        let mut response = external_window(engine, &thread, request, &knowledge).await?;
        apply_lineage(engine, &owner, &mut response).await;
        return Ok(response);
    }
    let mut response = run_history_search(threads, scope, request, &knowledge).await?;
    apply_lineage(engine, &owner, &mut response).await;
    Ok(response)
}

/// Read or scroll a CLI thread from its runtime transcript: the same rows,
/// ids and visibility as its timeline and its search projections.
async fn external_window(
    engine: &Engine,
    thread: &ThreadRecord,
    request: &HistorySearchRequest,
    knowledge: &EngineKnowledge,
) -> Result<HistorySearchResponse, HistorySearchError> {
    let delegated = &knowledge.delegated;
    let unavailable = |error: ironclaw_orchestrator::error::OrchestratorError| {
        tracing::debug!(%error, "history search could not read a CLI transcript");
        HistorySearchError::Unavailable(ironclaw_threads::SessionThreadError::Backend(
            "runtime transcript unavailable".to_string(),
        ))
    };
    let segmented = !engine
        .segments(&thread.thread_id)
        .await
        .map_err(unavailable)?
        .is_empty();
    let mut messages: Vec<HistorySearchMessage> = Vec::new();
    for source in engine
        .external_transcript_sources(thread)
        .await
        .map_err(unavailable)?
    {
        for row in timeline_rows(&thread.thread_id, &source.messages) {
            if row.kind == "user"
                && knowledge.is_scaffolding(thread.thread_id.as_str(), row.run_id.as_deref())
            {
                continue;
            }
            let role = match row.kind {
                "user" if visible_user_text(&row.text).is_some() => HistorySearchRole::User,
                "assistant" => HistorySearchRole::Assistant,
                _ => continue,
            };
            let text = match role {
                HistorySearchRole::User => visible_user_text(&row.text).unwrap_or_default(),
                HistorySearchRole::Assistant => row.text.as_str(),
            }
            .trim();
            if text.is_empty() {
                continue;
            }
            let id = if segmented {
                segment_message_id(&thread.thread_id, source.segment as usize, row.message_id)
            } else {
                row.message_id
            };
            let truncated = text.chars().count() > HISTORY_MESSAGE_MAX_CHARS;
            let mut text: String = text.chars().take(HISTORY_MESSAGE_MAX_CHARS).collect();
            if truncated {
                text.push('…');
            }
            messages.push(HistorySearchMessage {
                message_id: id.to_string(),
                sequence: messages.len() as u64 + 1,
                role,
                created_at: row.at.and_then(chrono::DateTime::from_timestamp_millis),
                text,
                truncated,
            });
        }
    }
    let thread_result = HistoryThreadResult {
        thread_id: thread.thread_id.as_str().to_string(),
        title: thread.title.clone(),
        updated_at: chrono::DateTime::from_timestamp_millis(thread.updated_at),
        origin: if delegated.contains(thread.thread_id.as_str()) {
            HistoryThreadOrigin::Subagent
        } else {
            HistoryThreadOrigin::Own
        },
        lineage_root_thread_id: None,
        related_thread_ids: Vec::new(),
        hits: Vec::new(),
    };
    let (window, next_from_sequence) = if request.mode() == HistorySearchMode::Scroll {
        let anchor = match (&request.around_message_id, request.around_sequence) {
            (Some(id), _) => messages
                .iter()
                .position(|m| &m.message_id == id)
                .ok_or(HistorySearchError::NotFound)?,
            (None, Some(sequence)) => messages
                .iter()
                .position(|m| m.sequence >= sequence)
                .unwrap_or(messages.len().saturating_sub(1)),
            (None, None) => 0,
        };
        let w = request
            .window
            .unwrap_or(HISTORY_DEFAULT_WINDOW)
            .clamp(1, HISTORY_MAX_WINDOW) as usize;
        let start = anchor.saturating_sub(w);
        let end = (anchor + w + 1).min(messages.len());
        (messages[start..end].to_vec(), None)
    } else {
        let limit = request
            .limit
            .unwrap_or(HISTORY_DEFAULT_READ_MESSAGES)
            .clamp(1, HISTORY_MAX_READ_MESSAGES) as usize;
        let from = request.from_sequence.unwrap_or(0);
        let mut rest = messages.into_iter().filter(|m| m.sequence >= from);
        let window: Vec<HistorySearchMessage> = rest.by_ref().take(limit).collect();
        (window, rest.next().map(|m| m.sequence))
    };
    Ok(HistorySearchResponse {
        mode: request.mode(),
        query: None,
        threads: Vec::new(),
        thread: Some(thread_result),
        messages: window,
        next_from_sequence,
        index_complete: true,
    })
}

/// Answers `history.search` (everything else passes through) and keeps the
/// index current: every view request starts a bounded background catch-up
/// of the caller's canonical index and, once per process, a sweep of their
/// CLI threads; completed turns re-index their CLI thread.
pub struct HistorySearchSurface {
    inner: Arc<dyn ProductSurface>,
    engine: Arc<Engine>,
    threads: Arc<dyn SessionThreadService>,
}

/// Wrap `inner` so history search covers CLI threads. Without an engine the
/// inner surface answers the view itself (canonical transcripts only).
pub fn wrap_surface(
    inner: Arc<dyn ProductSurface>,
    engine: Option<Arc<Engine>>,
    threads: Arc<dyn SessionThreadService>,
) -> Arc<dyn ProductSurface> {
    let Some(engine) = engine else {
        return inner;
    };
    engine.add_turn_listener(Arc::new(IngestOnCompletion::new(
        Arc::downgrade(&engine),
        Arc::clone(&threads),
    )));
    Arc::new(HistorySearchSurface {
        inner,
        engine,
        threads,
    })
}

#[async_trait]
impl ProductSurface for HistorySearchSurface {
    async fn invoke(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSurfaceInvokeRequest,
    ) -> Result<ProductSurfaceInvokeResponse, ProductSurfaceError> {
        self.inner.invoke(caller, request).await
    }

    async fn query(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSurfaceQueryRequest,
    ) -> Result<ProductSurfaceQueryPage, ProductSurfaceError> {
        if request.view_id != HISTORY_SEARCH_VIEW.id {
            return self.inner.query(caller, request).await;
        }
        let history: HistorySearchRequest =
            serde_json::from_value(request.input).map_err(|error| {
                tracing::debug!(%error, "history search request rejected");
                ProductSurfaceError::validation("input", ProductSurfaceValidationCode::InvalidValue)
            })?;
        let scope = caller_thread_scope(&caller).ok_or_else(|| {
            ProductSurfaceError::from_status(ProductSurfaceErrorCode::InvalidRequest, 400, false)
        })?;
        let orch_scope = super::surface::scope_of(&caller);
        let owner = ProfileOwner::from(&orch_scope);
        spawn_index_catch_up(&self.threads, &scope);
        spawn_owner_sweep(&self.engine, &self.threads, owner.clone());
        let mut response = match answer_history(
            Some(&self.engine),
            self.threads.as_ref(),
            &scope,
            &orch_scope,
            &history,
        )
        .await
        {
            Ok(response) => response,
            // Indexing just started: an empty, incomplete answer; the
            // desktop asks again.
            Err(HistorySearchError::NotReady) => HistorySearchResponse {
                mode: history.mode(),
                query: history.query.clone(),
                threads: Vec::new(),
                thread: None,
                messages: Vec::new(),
                next_from_sequence: None,
                index_complete: false,
            },
            Err(error) => return Err(error.into()),
        };
        // Until this process has swept the owner's CLI threads, their older
        // messages may be missing: say so, and the desktop asks again.
        response.index_complete &= owner_swept(&self.engine, &owner);
        Ok(ProductSurfaceQueryPage {
            items: vec![
                serde_json::to_value(response).map_err(ProductSurfaceError::internal_from)?,
            ],
            next_cursor: None,
        })
    }

    async fn stream_events(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSurfaceStreamRequest,
    ) -> Result<ProductSurfaceStreamResponse, ProductSurfaceError> {
        self.inner.stream_events(caller, request).await
    }
}

/// `history_search` for CLI runs (Claude, Codex) on the loopback MCP route,
/// beside the orchestrator tools. Same pure-read answer as the model tool,
/// as the session thread's owner; never indexes.
struct HistoryBridgeTool {
    engine: std::sync::Weak<Engine>,
    threads: Arc<dyn SessionThreadService>,
}

fn bridge_result(text: String, is_error: bool) -> serde_json::Value {
    serde_json::json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

#[async_trait]
impl ironclaw_cli_providers::bridge_tools::BridgeToolProvider for HistoryBridgeTool {
    fn tools(&self) -> Vec<serde_json::Value> {
        vec![serde_json::json!({
            "name": HISTORY_SEARCH_TOOL_NAME,
            "description": DESCRIPTION.trim(),
            "inputSchema": history_search_input_schema(),
        })]
    }

    fn handles(&self, name: &str) -> bool {
        name == HISTORY_SEARCH_TOOL_NAME
    }

    async fn call(
        &self,
        thread: &ThreadRecord,
        _name: &str,
        arguments: &serde_json::Value,
    ) -> serde_json::Value {
        let Some(engine) = self.engine.upgrade() else {
            return bridge_result("history_search is unavailable".into(), true);
        };
        let request: HistorySearchRequest = match serde_json::from_value(arguments.clone()) {
            Ok(request) => request,
            Err(error) => {
                tracing::debug!(%error, "history_search bridge arguments rejected");
                return bridge_result("history_search arguments are invalid".into(), true);
            }
        };
        let Some(scope) = super::history_ingest::projection_scope(&thread.scope) else {
            return bridge_result("history_search needs an agent-scoped thread".into(), true);
        };
        match answer_history(
            Some(&engine),
            self.threads.as_ref(),
            &scope,
            &thread.scope,
            &request,
        )
        .await
        {
            Ok(response) => match serde_json::to_string(&response) {
                Ok(text) => bridge_result(text, false),
                Err(error) => {
                    tracing::debug!(%error, "history_search bridge output did not encode");
                    bridge_result("history_search failed".into(), true)
                }
            },
            Err(HistorySearchError::Invalid { field, reason }) => {
                bridge_result(format!("history_search: {field} {reason}"), true)
            }
            Err(HistorySearchError::NotFound) => {
                bridge_result("history_search: no such thread or message".into(), true)
            }
            Err(HistorySearchError::NotReady) => bridge_result(
                "history_search: chat history is still being indexed; try again in a moment".into(),
                true,
            ),
            Err(HistorySearchError::Unavailable(error)) => {
                tracing::debug!(%error, "history_search bridge storage unavailable");
                bridge_result(
                    "history_search: history is temporarily unavailable".into(),
                    true,
                )
            }
        }
    }
}

/// Offer `history_search` to CLI runs through the bridge's MCP route.
pub fn register_history_bridge_tool(
    bridge: &ironclaw_cli_providers::bridge::CliBridge,
    engine: &Arc<Engine>,
    threads: Arc<dyn SessionThreadService>,
) {
    bridge.set_tool_provider(Arc::new(HistoryBridgeTool {
        engine: Arc::downgrade(engine),
        threads,
    }));
}

/// The `history_search` model tool for one run's capability surface. It acts
/// on the run's acting user's own threads only; no model calls happen inside.
///
/// Mediation exception (`.claude/rules/tools.md`, "Direct access
/// exceptions"): like the other synthetic loop tools it is not dispatched
/// through `CapabilityHost`. It is a pure read of the acting user's own
/// transcripts through the typed `SessionThreadService` facade and the
/// engine's read APIs — no write, no indexing, no network, no secret — so
/// no authorization, approval, resource or runtime obligation is skipped.
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
        let orch_scope = ironclaw_orchestrator::model::ThreadScope {
            tenant_id: resource.tenant_id.clone(),
            user_id: user_id.clone(),
            agent_id: resource.agent_id.clone(),
            project_id: resource.project_id.clone(),
        };
        let response = match answer_history(
            self.engine.as_deref(),
            self.threads.as_ref(),
            &scope,
            &orch_scope,
            &request,
        )
        .await
        {
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
            Err(HistorySearchError::NotReady) => {
                return Ok(model_failure(
                    FailureKind::OperationFailed,
                    "history_search: chat history is still being indexed; try again in a moment",
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
