//! History search through the production decorator with a real engine and
//! a scripted CLI runtime: CLI threads (no session record, longer than the
//! timeline's history cap) become searchable by the background sweep and
//! after each completed turn, hits carry the id the thread timeline renders,
//! scaffolding never matches, CLI threads read from their runtime
//! transcript, and forks collapse into their lineage root.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use ironclaw_host_api::ids::{AgentId, TenantId, ThreadId, UserId};
use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::model::{
    Author, RuntimeId, ThreadRecord, TransferDelivery, TransferId, TransferKind, TransferRecord,
    TransferStatus, TransferStrategy, now_ms,
};
use ironclaw_orchestrator::runtime::{
    CreateThread, CreatedThread, HistoryMessage, HistoryRole, RuntimeCapabilities, RuntimeError,
    SteerHow, SteerOutcome, TurnAdmission, TurnEventSink, TurnInput, TurnOutcome, TurnRuntime,
};
use ironclaw_product_contracts::history::{
    HISTORY_SEARCH_VIEW, HistorySearchMode, HistorySearchResponse, HistoryThreadOrigin,
};
use ironclaw_product_contracts::surface::{
    ProductSurface, ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceInvokeRequest,
    ProductSurfaceInvokeResponse, ProductSurfaceQueryPage, ProductSurfaceQueryRequest,
    ProductSurfaceStreamRequest, ProductSurfaceStreamResponse,
};
use ironclaw_threads::{
    AcceptInboundMessageRequest, EnsureThreadRequest, InMemorySessionThreadService, MessageContent,
    SessionThreadService, ThreadScope,
};
use serde_json::{Value, json};

use super::history::wrap_surface;
use super::surface::{OrchestratedProductSurface, scope_of};

/// IronClaw's own surface stand-in: it holds no CLI thread.
struct Inner;

#[async_trait]
impl ProductSurface for Inner {
    async fn invoke(
        &self,
        _: ProductSurfaceCaller,
        _: ProductSurfaceInvokeRequest,
    ) -> Result<ProductSurfaceInvokeResponse, ProductSurfaceError> {
        Err(ProductSurfaceError::not_found())
    }
    async fn query(
        &self,
        _: ProductSurfaceCaller,
        _: ProductSurfaceQueryRequest,
    ) -> Result<ProductSurfaceQueryPage, ProductSurfaceError> {
        Err(ProductSurfaceError::not_found())
    }
    async fn stream_events(
        &self,
        _: ProductSurfaceCaller,
        _: ProductSurfaceStreamRequest,
    ) -> Result<ProductSurfaceStreamResponse, ProductSurfaceError> {
        Err(ProductSurfaceError::not_found())
    }
}

/// A CLI runtime whose sessions are scripted per thread; `history` honors
/// the "last `limit` messages" contract, as the real readers do.
#[derive(Default)]
struct ScriptedCli {
    history: Mutex<std::collections::HashMap<ThreadId, Vec<HistoryMessage>>>,
}

impl ScriptedCli {
    fn set(&self, thread: &ThreadId, messages: Vec<HistoryMessage>) {
        self.history
            .lock()
            .unwrap()
            .insert(thread.clone(), messages);
    }
}

#[async_trait]
impl TurnRuntime for ScriptedCli {
    fn id(&self) -> RuntimeId {
        RuntimeId::new("claude-cli").unwrap()
    }
    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities::default()
    }
    async fn create_thread(&self, _: CreateThread) -> Result<CreatedThread, RuntimeError> {
        Err(RuntimeError::Unsupported("create"))
    }
    async fn start_turn(
        &self,
        _: &ThreadRecord,
        _: TurnInput,
    ) -> Result<TurnAdmission, RuntimeError> {
        Err(RuntimeError::Unsupported("turns"))
    }
    async fn steer(
        &self,
        _: &ThreadRecord,
        _: &str,
        _: SteerHow,
    ) -> Result<SteerOutcome, RuntimeError> {
        Err(RuntimeError::Unsupported("steer"))
    }
    async fn cancel(&self, _: &ThreadRecord, _: &str) -> Result<(), RuntimeError> {
        Ok(())
    }
    async fn history(
        &self,
        thread: &ThreadRecord,
        limit: usize,
    ) -> Result<Vec<HistoryMessage>, RuntimeError> {
        let all = self
            .history
            .lock()
            .unwrap()
            .get(&thread.thread_id)
            .cloned()
            .unwrap_or_default();
        let skip = all.len().saturating_sub(limit);
        Ok(all.into_iter().skip(skip).collect())
    }
}

fn msg(role: HistoryRole, text: &str) -> HistoryMessage {
    HistoryMessage {
        role,
        text: text.into(),
        tool_calls: Vec::new(),
        run_id: None,
        hidden: false,
        at: None,
    }
}

fn owner() -> ProductSurfaceCaller {
    ProductSurfaceCaller::new(
        TenantId::new("t").unwrap(),
        UserId::new("owner").unwrap(),
        Some(AgentId::new("agent").unwrap()),
        None,
    )
}

fn transcript_scope() -> ThreadScope {
    crate::history_search::caller_thread_scope(&owner()).unwrap()
}

async fn engine() -> (tempfile::TempDir, Arc<Engine>) {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("o.db"))
            .build()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap());
    (dir, Engine::open_libsql(runtime, "ironclaw").await.unwrap())
}

async fn cli_thread(engine: &Engine, id: &str, title: &str) -> ThreadId {
    let thread = ThreadId::new(id).unwrap();
    let mut record = ThreadRecord::new(
        thread.clone(),
        scope_of(&owner()),
        RuntimeId::new("claude-cli").unwrap(),
    );
    record.title = Some(title.into());
    engine.store().ensure_thread(&record).await.unwrap();
    thread
}

async fn history(surface: &dyn ProductSurface, input: Value) -> HistorySearchResponse {
    let page = surface
        .query(
            owner(),
            ProductSurfaceQueryRequest {
                view_id: HISTORY_SEARCH_VIEW.id.into(),
                input,
                cursor: None,
                limit: None,
            },
        )
        .await
        .unwrap();
    serde_json::from_value(page.items[0].clone()).unwrap()
}

/// Ask until the index reports complete and `thread` is among the hits.
async fn search_until(
    surface: &dyn ProductSurface,
    query: &str,
    thread: &str,
) -> HistorySearchResponse {
    for _ in 0..400 {
        let found = history(surface, json!({"query": query})).await;
        if found.index_complete && found.threads.iter().any(|t| t.thread_id == thread) {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("{query} never found in {thread}");
}

#[tokio::test]
async fn cli_threads_become_searchable_and_navigable() {
    let (_dir, engine) = engine().await;
    let cli = Arc::new(ScriptedCli::default());
    engine.register_runtime(cli.clone());
    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());

    // A CLI-only thread (no session record) longer than the timeline's
    // history cap, whose oldest message carries the word.
    let long = cli_thread(&engine, "cli-long", "Long terminal session").await;
    let mut long_history = vec![msg(HistoryRole::User, "the porcupine migration plan")];
    long_history.extend((0..2_100).map(|n| msg(HistoryRole::Assistant, &format!("step {n}"))));
    cli.set(&long, long_history);
    // A short one, with engine scaffolding the person never typed.
    let short = cli_thread(&engine, "cli-short", "Terminal").await;
    cli.set(
        &short,
        vec![
            msg(HistoryRole::User, "rename the heron module"),
            msg(HistoryRole::Assistant, "renamed heron to egret"),
            msg(
                HistoryRole::User,
                "Delegated work finished. Use these results to continue.\n\nkestrel",
            ),
        ],
    );

    let orchestrated: Arc<dyn ProductSurface> = Arc::new(OrchestratedProductSurface::new(
        Arc::new(Inner),
        Arc::clone(&engine),
        None,
    ));
    let surface = wrap_surface(
        Arc::clone(&orchestrated),
        Some(Arc::clone(&engine)),
        Arc::clone(&threads),
    );

    // The first search starts the sweep and says the index is incomplete.
    let first = history(surface.as_ref(), json!({"query": "porcupine"})).await;
    assert!(!first.index_complete);

    let found = search_until(surface.as_ref(), "porcupine", "cli-long").await;
    assert_eq!(
        found.threads[0].title.as_deref(),
        Some("Long terminal session")
    );

    // A hit carries the id the thread's own timeline renders for it.
    let hit = search_until(surface.as_ref(), "heron", "cli-short")
        .await
        .threads[0]
        .hits[0]
        .clone();
    let timeline = orchestrated
        .query(
            owner(),
            ProductSurfaceQueryRequest {
                view_id: crate::reborn_services::TIMELINE_VIEW.id.to_string(),
                input: json!({"thread_id": "cli-short", "limit": 50}),
                cursor: None,
                limit: None,
            },
        )
        .await
        .unwrap();
    let rendered: Vec<String> = timeline.items[0]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["message_id"].as_str().unwrap().to_string())
        .collect();
    assert!(
        rendered.contains(&hit.message_id),
        "hit opens a rendered row"
    );

    // Scaffolding never matches.
    assert!(
        history(surface.as_ref(), json!({"query": "kestrel"}))
            .await
            .threads
            .is_empty()
    );

    // A CLI thread reads from its runtime transcript, visible rows only.
    let read = history(surface.as_ref(), json!({"thread_id": "cli-short"})).await;
    assert_eq!(read.mode, HistorySearchMode::Read);
    let texts: Vec<&str> = read.messages.iter().map(|m| m.text.as_str()).collect();
    assert_eq!(texts, ["rename the heron module", "renamed heron to egret"]);
    let scroll = history(
        surface.as_ref(),
        json!({"thread_id": "cli-short", "around_message_id": hit.message_id, "window": 1}),
    )
    .await;
    assert_eq!(scroll.mode, HistorySearchMode::Scroll);
    assert!(
        scroll
            .messages
            .iter()
            .any(|m| m.message_id == hit.message_id)
    );

    // A CLI run reaches the same search through the bridge's MCP route,
    // as its session thread.
    let bridge = ironclaw_cli_providers::bridge::CliBridge::new(Duration::from_secs(10));
    super::history::register_history_bridge_tool(&bridge, &engine, Arc::clone(&threads));
    let token = bridge.mint_token(
        ironclaw_cli_providers::bridge::SessionGrant {
            thread_id: short.clone(),
            run_id: "r".into(),
        },
        Duration::from_secs(60),
    );
    let body = json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call",
        "params": {"name": "history_search", "arguments": {"query": "heron"}}});
    let answer = ironclaw_cli_providers::bridge::serve_mcp(
        &bridge,
        &engine,
        true,
        Some(&format!("Bearer {token}")),
        body.to_string().as_bytes(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(answer["result"]["isError"], false);
    let text = answer["result"]["content"][0]["text"].as_str().unwrap();
    let via_mcp: HistorySearchResponse = serde_json::from_str(text).unwrap();
    assert_eq!(via_mcp.threads[0].thread_id, "cli-short");

    // A completed turn re-indexes the thread.
    let mut grown = cli.history.lock().unwrap().get(&short).cloned().unwrap();
    grown.push(msg(HistoryRole::Assistant, "the toucan follow-up landed"));
    cli.set(&short, grown);
    engine
        .store()
        .set_turn_running(&short, Some("run-2"), now_ms())
        .await
        .unwrap();
    engine
        .turn_finished(
            &short,
            Some("run-2".into()),
            TurnOutcome::completed(None),
            now_ms(),
        )
        .await;
    search_until(surface.as_ref(), "toucan", "cli-short").await;
}

#[tokio::test]
async fn forks_collapse_into_their_lineage_root() {
    let (_dir, engine) = engine().await;
    let threads: Arc<dyn SessionThreadService> = Arc::new(InMemorySessionThreadService::default());
    let scope = transcript_scope();
    for id in ["root", "fork", "other"] {
        let thread = ThreadId::new(id).unwrap();
        threads
            .ensure_thread(EnsureThreadRequest {
                scope: scope.clone(),
                thread_id: Some(thread.clone()),
                created_by_actor_id: "owner".into(),
                title: None,
                metadata_json: None,
            })
            .await
            .unwrap();
        threads
            .accept_inbound_message(AcceptInboundMessageRequest {
                scope: scope.clone(),
                thread_id: thread,
                actor_id: "owner".into(),
                source_binding_id: None,
                reply_target_binding_id: None,
                external_event_id: None,
                content: MessageContent::text(format!("deploy notes from {id}")),
            })
            .await
            .unwrap();
    }
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
    let surface = wrap_surface(Arc::new(Inner), Some(engine), threads);
    let found = history(surface.as_ref(), json!({"query": "deploy"})).await;
    assert_eq!(
        found.threads.len(),
        2,
        "the fork and its root are one result"
    );
    let forked = found
        .threads
        .iter()
        .find(|t| t.thread_id == "fork" || t.thread_id == "root")
        .unwrap();
    assert_eq!(forked.origin, HistoryThreadOrigin::Own);
    assert_eq!(forked.related_thread_ids.len(), 1);
}

/// The scaffolding rule in `ironclaw_threads` must recognize exactly what the
/// orchestrator generates (it cannot name that crate, so this pins it).
#[test]
fn visibility_rule_recognizes_every_orchestrator_scaffolding_form() {
    use ironclaw_orchestrator::handoff::{delta_context, portable_context};
    use ironclaw_threads::visible_user_text;

    let messages = vec![msg(HistoryRole::User, "earlier question")];
    let portable = portable_context(&messages, "Title", "src", "claude", "fork", 10_000);
    assert_eq!(visible_user_text(&portable), None);
    assert_eq!(
        visible_user_text(&format!("{portable}\n\n---\n\nmy typed text")),
        Some("my typed text")
    );
    let delta = delta_context(&messages, "Fork", "f", 0, 10_000);
    assert_eq!(visible_user_text(&delta), None);
    assert_eq!(
        visible_user_text(ironclaw_orchestrator::restart::CONTINUE_PROMPT),
        None
    );
    assert_eq!(
        visible_user_text("Delegated work finished. Use these results to continue.\n\nresults"),
        None
    );
}

/// Ingestion re-keys segment rows exactly as the stitched timeline does.
#[test]
fn segment_ids_match_the_stitched_timeline() {
    let thread = ThreadId::new("t").unwrap();
    let base = super::external::timeline_message_id(&thread, 3);
    let old = json!(base).to_string();
    let stitched = uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_OID,
        format!("{thread}:2:{old}").as_bytes(),
    );
    assert_eq!(
        super::external::segment_message_id(&thread, 2, base),
        stitched
    );
}

/// Every mode of the model tool's answer (`answer_history`) is a pure read:
/// before the scope is ready it refuses without touching storage writes,
/// and once ready it answers from a fresh service over a backend that
/// rejects every write.
#[tokio::test]
async fn tool_answers_are_pure_reads_ready_or_not() {
    use ironclaw_filesystem::{
        Fault, FaultInjecting, FilesystemOperation as Op, InMemoryBackend, ScopedFilesystem,
    };
    use ironclaw_host_api::mount::{MountGrant, MountPermissions, MountView};
    use ironclaw_host_api::path::{MountAlias, VirtualPath};
    use ironclaw_threads::FilesystemSessionThreadService;

    const WRITES: [Op; 7] = [
        Op::WriteFile,
        Op::Delete,
        Op::EnsureIndex,
        Op::BeginTxn,
        Op::Append,
        Op::ReserveSeq,
        Op::CreateSubtreeAtomic,
    ];
    let guarded = Arc::new(FaultInjecting::new(InMemoryBackend::new()));
    let service = || {
        let mounts = MountView::new(vec![MountGrant::new(
            MountAlias::new("/threads").unwrap(),
            VirtualPath::new("/tenants/t/users/owner/threads").unwrap(),
            MountPermissions::read_write_list_delete(),
        )])
        .unwrap();
        Arc::new(FilesystemSessionThreadService::new(Arc::new(
            ScopedFilesystem::with_fixed_view(Arc::clone(&guarded), mounts),
        )))
    };
    let writes = || WRITES.iter().map(|op| guarded.count(*op)).sum::<usize>();
    let scope = transcript_scope();
    let orch_scope = scope_of(&owner());
    let writer = service();
    let thread = ThreadId::new("thread-tool").unwrap();
    writer
        .ensure_thread(EnsureThreadRequest {
            scope: scope.clone(),
            thread_id: Some(thread.clone()),
            created_by_actor_id: "owner".into(),
            title: None,
            metadata_json: None,
        })
        .await
        .unwrap();
    let accepted = writer
        .accept_inbound_message(AcceptInboundMessageRequest {
            scope: scope.clone(),
            thread_id: thread.clone(),
            actor_id: "owner".into(),
            source_binding_id: None,
            reply_target_binding_id: None,
            external_event_id: None,
            content: MessageContent::text("the albatross route"),
        })
        .await
        .unwrap();
    let modes = [
        json!({"query": "albatross"}),
        json!({"thread_id": "thread-tool"}),
        json!({"thread_id": "thread-tool", "around_message_id": accepted.message_id.to_string()}),
        json!({}),
    ];
    let ask = |svc: Arc<FilesystemSessionThreadService<_>>, input: Value| {
        let (scope, orch_scope) = (scope.clone(), orch_scope.clone());
        async move {
            let request = serde_json::from_value(input).unwrap();
            super::history::answer_history(None, svc.as_ref(), &scope, &orch_scope, &request).await
        }
    };

    // Not ready: every mode refuses, and nothing is written.
    let before = writes();
    for input in &modes {
        let answer = ask(service(), input.clone()).await;
        assert!(
            matches!(
                answer,
                Err(crate::history_search::HistorySearchError::NotReady)
            ),
            "{input}"
        );
    }
    assert_eq!(writes(), before, "an unready answer wrote");

    // The background step makes the scope ready (writes allowed here).
    while !writer
        .advance_message_search_index(&scope, 100)
        .await
        .unwrap()
        .complete
    {}
    for op in WRITES {
        guarded.add_fault(Fault::on(op));
    }
    let before = writes();
    for input in &modes {
        let answer = ask(service(), input.clone()).await;
        assert!(answer.is_ok(), "{input}: {answer:?}");
    }
    let found = ask(service(), modes[0].clone()).await.unwrap();
    assert_eq!(found.threads[0].thread_id, "thread-tool");
    assert_eq!(writes(), before, "a ready answer wrote");
}
