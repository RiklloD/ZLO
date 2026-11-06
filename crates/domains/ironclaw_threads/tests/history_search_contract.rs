//! History search conformance: the same scenarios against the in-memory
//! reference service and the filesystem service on the in-memory, libSQL
//! (`fts5`) and PostgreSQL (`tsvector`) backends. The PostgreSQL legs run
//! when `IRONCLAW_THREADS_HISTORY_POSTGRES_URL` names a server and skip with a
//! notice otherwise.

use std::sync::Arc;

use ironclaw_filesystem::{
    CasExpectation, Filter, InMemoryBackend, IndexKey, IsolatedPostgresDatabase,
    IsolatedPostgresProvisioner, LibSqlRootFilesystem, Page, PostgresRootFilesystem,
    PostgresUnreachable, RootFilesystem, ScopedFilesystem,
};
use ironclaw_host_api::{
    ids::{AgentId, ProjectId, TenantId, ThreadId, UserId},
    mount::{MountGrant, MountPermissions, MountView},
    path::{MountAlias, ScopedPath, VirtualPath},
};
use ironclaw_threads::{
    AcceptInboundMessageRequest, AppendAssistantDraftRequest,
    AppendFinalizedAssistantMessageRequest, EnsureThreadRequest, ExternalThreadSummary,
    ExternalTranscriptCursorRequest, ExternalTranscriptMessage, ExternalTranscriptSource,
    FilesystemSessionThreadService, InMemorySessionThreadService, MessageContent,
    RedactMessageRequest, SearchHitRole, SearchThreadMessagesRequest, SearchThreadMessagesResponse,
    SessionThreadRecord, SessionThreadService, ThreadMessageId, ThreadScope, ThreadSearchTiering,
    UpsertExternalTranscriptRequest,
};

fn scope(user: &str) -> ThreadScope {
    ThreadScope {
        tenant_id: TenantId::new("tenant-history").unwrap(),
        agent_id: AgentId::new("agent-history").unwrap(),
        project_id: Some(ProjectId::new("project-history").unwrap()),
        owner_user_id: Some(UserId::new(user).unwrap()),
        mission_id: None,
    }
}

fn scoped_fs<F: RootFilesystem>(backend: Arc<F>, user: &str) -> Arc<ScopedFilesystem<F>> {
    let mounts = MountView::new(vec![MountGrant::new(
        MountAlias::new("/threads").unwrap(),
        VirtualPath::new(format!("/tenants/tenant-history/users/{user}/threads")).unwrap(),
        MountPermissions::read_write_list_delete(),
    )])
    .unwrap();
    Arc::new(ScopedFilesystem::with_fixed_view(backend, mounts))
}

async fn libsql_backend() -> (tempfile::TempDir, Arc<LibSqlRootFilesystem>) {
    let dir = tempfile::tempdir().unwrap();
    let database = Arc::new(
        libsql::Builder::new_local(dir.path().join("threads.db"))
            .build()
            .await
            .unwrap(),
    );
    let backend = Arc::new(LibSqlRootFilesystem::new(database).unwrap());
    backend.run_migrations().await.unwrap();
    (dir, backend)
}

static POSTGRES: IsolatedPostgresProvisioner = IsolatedPostgresProvisioner::new(
    "postgres history search contract",
    "IRONCLAW_THREADS_HISTORY_POSTGRES_URL",
    "threads_history_isolated_",
    PostgresUnreachable::Skip,
);

async fn postgres_backend() -> Option<(IsolatedPostgresDatabase, Arc<PostgresRootFilesystem>)> {
    let db = POSTGRES.provision().await?;
    let manager = deadpool_postgres::Manager::new(db.config().clone(), tokio_postgres::NoTls);
    let pool = deadpool_postgres::Pool::builder(manager)
        .max_size(4)
        .build()
        .expect("postgres pool");
    let backend = Arc::new(PostgresRootFilesystem::new(pool));
    backend.run_migrations().await.expect("migrate postgres");
    Some((db, backend))
}

async fn new_thread(service: &dyn SessionThreadService, scope: &ThreadScope, id: &str) -> ThreadId {
    new_thread_with(service, scope, id, None).await
}

async fn new_thread_with(
    service: &dyn SessionThreadService,
    scope: &ThreadScope,
    id: &str,
    metadata_json: Option<String>,
) -> ThreadId {
    let thread_id = ThreadId::new(id).unwrap();
    service
        .ensure_thread(EnsureThreadRequest {
            scope: scope.clone(),
            thread_id: Some(thread_id.clone()),
            created_by_actor_id: "person".into(),
            title: None,
            metadata_json,
        })
        .await
        .unwrap();
    thread_id
}

async fn say(
    service: &dyn SessionThreadService,
    scope: &ThreadScope,
    thread: &ThreadId,
    text: &str,
) {
    service
        .accept_inbound_message(AcceptInboundMessageRequest {
            scope: scope.clone(),
            thread_id: thread.clone(),
            actor_id: "person".into(),
            source_binding_id: None,
            reply_target_binding_id: None,
            external_event_id: None,
            content: MessageContent::text(text),
        })
        .await
        .unwrap();
}

async fn reply(
    service: &dyn SessionThreadService,
    scope: &ThreadScope,
    thread: &ThreadId,
    run: &str,
    text: &str,
) -> ironclaw_threads::ThreadMessageRecord {
    service
        .append_finalized_assistant_message(AppendFinalizedAssistantMessageRequest {
            scope: scope.clone(),
            thread_id: thread.clone(),
            turn_run_id: run.into(),
            content: MessageContent::text(text),
        })
        .await
        .unwrap()
}

/// Run indexing steps until the scope reports complete.
async fn index_all(service: &dyn SessionThreadService, scope: &ThreadScope) {
    for _ in 0..500 {
        if service
            .advance_message_search_index(scope, 64)
            .await
            .unwrap()
            .complete
        {
            return;
        }
    }
    panic!("indexing did not complete");
}

fn request(scope: &ThreadScope, query: &str) -> SearchThreadMessagesRequest {
    SearchThreadMessagesRequest {
        max_threads: 20,
        ..SearchThreadMessagesRequest::new(scope.clone(), query)
    }
}

async fn search(
    service: &dyn SessionThreadService,
    scope: &ThreadScope,
    query: &str,
) -> SearchThreadMessagesResponse {
    service
        .search_thread_messages(request(scope, query))
        .await
        .unwrap()
}

fn thread_ids(response: &SearchThreadMessagesResponse) -> Vec<&str> {
    response
        .threads
        .iter()
        .map(|group| group.thread.thread_id.as_str())
        .collect()
}

/// Threads named `auto-*` are automation runs in these scenarios.
struct AutoLast;

impl ThreadSearchTiering for AutoLast {
    fn tier(&self, thread: &SessionThreadRecord) -> u8 {
        u8::from(thread.thread_id.as_str().starts_with("auto-"))
    }
}

fn source(segment: u32) -> ExternalTranscriptSource {
    ExternalTranscriptSource {
        segment,
        runtime_id: "claude".into(),
        session_id: Some("session-1".into()),
    }
}

fn cli_row(ordinal: u64, role: SearchHitRole, text: &str) -> ExternalTranscriptMessage {
    ExternalTranscriptMessage {
        ordinal,
        navigation_message_id: ThreadMessageId::new(),
        role,
        text: text.into(),
        created_at: None,
        run_id: None,
    }
}

async fn upsert_cli(
    service: &dyn SessionThreadService,
    scope: &ThreadScope,
    thread: &str,
    title: &str,
    rows: Vec<ExternalTranscriptMessage>,
) -> ironclaw_threads::ExternalTranscriptUpsert {
    service
        .upsert_external_transcript(UpsertExternalTranscriptRequest {
            scope: scope.clone(),
            thread: ExternalThreadSummary {
                thread_id: ThreadId::new(thread).unwrap(),
                title: Some(title.into()),
                created_at: None,
                updated_at: None,
            },
            source: source(0),
            messages: rows,
        })
        .await
        .unwrap()
}

/// The behavioral contract every backend must satisfy.
async fn search_contract(service: &dyn SessionThreadService, other: &dyn SessionThreadService) {
    let mine = scope("alice");
    let deploy = new_thread(service, &mine, "thread-deploy").await;
    say(
        service,
        &mine,
        &deploy,
        "How do we roll out the staging deployment?",
    )
    .await;
    reply(
        service,
        &mine,
        &deploy,
        "run-1",
        "Use the blue green rollout; the deployment script lives in ops.",
    )
    .await;
    let pasta = new_thread(service, &mine, "thread-pasta").await;
    say(service, &mine, &pasta, "Give me a carbonara recipe").await;
    reply(
        service,
        &mine,
        &pasta,
        "run-2",
        "Guanciale, eggs, pecorino and pepper.",
    )
    .await;
    // A draft is not finished text and is never searchable.
    service
        .append_assistant_draft(AppendAssistantDraftRequest {
            scope: mine.clone(),
            thread_id: pasta.clone(),
            turn_run_id: "run-3".into(),
            content: MessageContent::text("zanzibar draft text"),
        })
        .await
        .unwrap();
    // A redacted reply leaves the index.
    let secret = reply(
        service,
        &mine,
        &pasta,
        "run-4",
        "the quokka password is hunter2",
    )
    .await;
    service
        .redact_message(RedactMessageRequest {
            scope: mine.clone(),
            thread_id: pasta.clone(),
            message_id: secret.message_id,
            redaction_ref: "redaction-1".into(),
        })
        .await
        .unwrap();
    // Engine scaffolding the person never typed (finding: handoffs, merged
    // forks, delegation completions, restart prompts) is never searchable;
    // text typed after carried context is.
    let scaffold = new_thread(service, &mine, "thread-scaffold").await;
    say(
        service,
        &mine,
        &scaffold,
        "Context handoff (fork). Source thread: \"x\" (id). The earlier conversation: \
         narwhal plans\n\n[End of context handoff]\n\n---\n\nwhat about the axolotl?",
    )
    .await;
    say(
        service,
        &mine,
        &scaffold,
        "Merged context from fork \"f\" (id). okapi findings\n\n[End of merged fork context]",
    )
    .await;
    say(
        service,
        &mine,
        &scaffold,
        "Delegated work finished. Use these results to continue.\n\nwombat result",
    )
    .await;
    say(service, &mine, &scaffold, "Continue where you left off.").await;

    // Another user's transcript is invisible.
    let theirs = scope("bob");
    let bob = new_thread(other, &theirs, "thread-bob").await;
    say(other, &theirs, &bob, "staging deployment for bob only").await;

    index_all(service, &mine).await;
    index_all(other, &theirs).await;

    // Word match, grouped by thread, both roles, snippets highlighted.
    let found = search(service, &mine, "deployment").await;
    assert!(found.index_complete);
    assert_eq!(thread_ids(&found), ["thread-deploy"]);
    let group = &found.threads[0];
    assert_eq!(group.hits.len(), 2);
    let roles: Vec<SearchHitRole> = group.hits.iter().map(|hit| hit.role).collect();
    assert!(roles.contains(&SearchHitRole::User));
    assert!(roles.contains(&SearchHitRole::Assistant));
    for hit in &group.hits {
        assert!(!hit.highlights.is_empty(), "every hit highlights its match");
        for range in &hit.highlights {
            assert_eq!(hit.snippet[range.clone()].to_lowercase(), "deployment");
        }
    }

    // Search-as-you-type: word prefixes match, from two characters.
    assert_eq!(
        thread_ids(&search(service, &mine, "carbon").await),
        ["thread-pasta"]
    );
    assert_eq!(
        thread_ids(&search(service, &mine, "gu").await),
        ["thread-pasta"]
    );

    // Covering more query terms ranks a thread first.
    let ranked = search(service, &mine, "staging carbonara deployment").await;
    assert_eq!(thread_ids(&ranked), ["thread-deploy", "thread-pasta"]);

    // Drafts and redacted text are never found; neither is another user.
    assert!(search(service, &mine, "zanzibar").await.threads.is_empty());
    assert!(search(service, &mine, "quokka").await.threads.is_empty());
    assert!(search(service, &mine, "bob").await.threads.is_empty());

    // Scaffolding is not indexed; the typed tail of a handoff is.
    for hidden in [
        "narwhal",
        "okapi",
        "wombat",
        "handoff",
        "delegated",
        "continue",
    ] {
        assert!(
            search(service, &mine, hidden).await.threads.is_empty(),
            "engine scaffolding matched {hidden}"
        );
    }
    let typed = search(service, &mine, "axolotl").await;
    assert_eq!(thread_ids(&typed), ["thread-scaffold"]);
    assert_eq!(typed.threads[0].hits[0].snippet, "what about the axolotl?");

    // Empty / punctuation-only queries match nothing.
    assert!(search(service, &mine, "  ?! ").await.threads.is_empty());
}

/// A thread restriction selects candidates inside the thread, so its one
/// weak match is found however many other threads match better; tiering
/// ranks automation runs below own threads even when they saturate.
async fn ranking_contract(service: &dyn SessionThreadService) {
    let mine = scope("alice");
    for n in 0..60 {
        let auto = new_thread(service, &mine, &format!("auto-{n:02}")).await;
        say(
            service,
            &mine,
            &auto,
            "invoice invoice report summary for the morning",
        )
        .await;
    }
    let own = new_thread(service, &mine, "thread-own").await;
    say(service, &mine, &own, "where did the invoice go").await;
    index_all(service, &mine).await;

    let only = service
        .search_thread_messages(SearchThreadMessagesRequest {
            thread_id: Some(own.clone()),
            max_threads: 5,
            ..SearchThreadMessagesRequest::new(mine.clone(), "invoice report summary")
        })
        .await
        .unwrap();
    assert_eq!(thread_ids(&only), ["thread-own"]);

    let tiered = service
        .search_thread_messages(SearchThreadMessagesRequest {
            max_threads: 10,
            tiering: Some(Arc::new(AutoLast)),
            ..SearchThreadMessagesRequest::new(mine.clone(), "invoice report summary")
        })
        .await
        .unwrap();
    assert_eq!(tiered.threads.len(), 10);
    assert_eq!(tiered.threads[0].thread.thread_id.as_str(), "thread-own");
}

/// CLI transcripts: projections are searchable, idempotent, cursor-monotonic,
/// visibility-filtered, narrowable to a thread, and a CLI-only thread with no
/// session record still groups under its summary title.
async fn external_contract(service: &dyn SessionThreadService) {
    let mine = scope("alice");
    let first = upsert_cli(
        service,
        &mine,
        "cli-only",
        "Terminal session",
        vec![
            cli_row(0, SearchHitRole::User, "refactor the flamingo parser"),
            cli_row(
                1,
                SearchHitRole::Assistant,
                "the flamingo parser now streams",
            ),
            cli_row(
                2,
                SearchHitRole::User,
                "Delegated work finished. Use these results to continue.\n\nmarmoset",
            ),
        ],
    )
    .await;
    assert_eq!(first.written, 3);
    assert_eq!(first.ingested_through, 3);
    // Re-ingesting the same rows writes nothing; an older batch never moves
    // the cursor back.
    let again = upsert_cli(
        service,
        &mine,
        "cli-only",
        "Terminal session",
        vec![cli_row(
            0,
            SearchHitRole::User,
            "refactor the flamingo parser",
        )],
    )
    .await;
    assert_eq!(again.written, 1, "a fresh navigation id is a changed row");
    assert_eq!(again.ingested_through, 3);
    let cursor = service
        .external_transcript_cursor(ExternalTranscriptCursorRequest {
            scope: mine.clone(),
            thread_id: ThreadId::new("cli-only").unwrap(),
            source: source(0),
        })
        .await
        .unwrap();
    assert_eq!(cursor, 3);
    let row = cli_row(5, SearchHitRole::Assistant, "pelican deploy notes");
    upsert_cli(
        service,
        &mine,
        "cli-only",
        "Terminal session",
        vec![row.clone()],
    )
    .await;
    let unchanged = upsert_cli(service, &mine, "cli-only", "Terminal session", vec![row]).await;
    assert_eq!((unchanged.written, unchanged.unchanged), (0, 1));

    index_all(service, &mine).await;
    let found = search(service, &mine, "flamingo").await;
    assert_eq!(thread_ids(&found), ["cli-only"]);
    assert_eq!(
        found.threads[0].thread.title.as_deref(),
        Some("Terminal session")
    );
    assert_eq!(found.threads[0].hits.len(), 2);
    assert!(search(service, &mine, "marmoset").await.threads.is_empty());
    let narrowed = service
        .search_thread_messages(SearchThreadMessagesRequest {
            thread_id: Some(ThreadId::new("cli-only").unwrap()),
            ..SearchThreadMessagesRequest::new(mine.clone(), "pelican")
        })
        .await
        .unwrap();
    assert_eq!(thread_ids(&narrowed), ["cli-only"]);
    // Another user never sees them.
    assert!(
        search(service, &scope("bob"), "flamingo")
            .await
            .threads
            .is_empty()
    );
}

async fn all_contracts(make: impl Fn(&str) -> Arc<dyn SessionThreadService>) {
    search_contract(make("alice").as_ref(), make("bob").as_ref()).await;
}

#[tokio::test]
async fn history_search_in_memory_reference_satisfies_the_search_contract() {
    let service = InMemorySessionThreadService::default();
    search_contract(&service, &service).await;
    let ranking = InMemorySessionThreadService::default();
    ranking_contract(&ranking).await;
    let external = InMemorySessionThreadService::default();
    external_contract(&external).await;
}

fn filesystem_service<F: RootFilesystem + 'static>(
    backend: &Arc<F>,
    user: &str,
) -> Arc<dyn SessionThreadService> {
    Arc::new(FilesystemSessionThreadService::new(scoped_fs(
        Arc::clone(backend),
        user,
    )))
}

#[tokio::test]
async fn history_search_filesystem_in_memory_backend_satisfies_the_search_contract() {
    let backend = Arc::new(InMemoryBackend::new());
    all_contracts(|user| filesystem_service(&backend, user)).await;
    let ranking = Arc::new(InMemoryBackend::new());
    ranking_contract(filesystem_service(&ranking, "alice").as_ref()).await;
    let external = Arc::new(InMemoryBackend::new());
    external_contract(filesystem_service(&external, "alice").as_ref()).await;
}

#[tokio::test]
async fn history_search_filesystem_libsql_backend_satisfies_the_search_contract() {
    let (_dir, backend) = libsql_backend().await;
    all_contracts(|user| filesystem_service(&backend, user)).await;
    let (_ranking_dir, ranking) = libsql_backend().await;
    ranking_contract(filesystem_service(&ranking, "alice").as_ref()).await;
    let (_external_dir, external) = libsql_backend().await;
    external_contract(filesystem_service(&external, "alice").as_ref()).await;
}

#[tokio::test]
async fn history_search_filesystem_postgres_backend_satisfies_the_search_contract() {
    let Some((db, backend)) = postgres_backend().await else {
        return;
    };
    all_contracts(|user| filesystem_service(&backend, user)).await;
    ranking_contract(filesystem_service(&backend, "carol").as_ref()).await;
    external_contract(filesystem_service(&backend, "dave").as_ref()).await;
    drop(backend);
    db.cleanup().await;
}

/// Strip the `search_text` projection from every message row, as on a scope
/// written before history search existed. Bodies are left untouched.
async fn strip_search_projection<F: RootFilesystem>(
    fs: &ScopedFilesystem<F>,
    scope: &ThreadScope,
    thread: &ThreadId,
) {
    let resource_scope = scope.to_resource_scope();
    let prefix = ScopedPath::new(format!(
        "/threads/agents/agent-history/projects/project-history/owners/{}/threads/{}/messages",
        scope.owner_user_id.as_ref().unwrap().as_str(),
        thread.as_str()
    ))
    .unwrap();
    let key = IndexKey::new("search_text").unwrap();
    let rows = fs
        .query(
            &resource_scope,
            &prefix,
            &Filter::All,
            Page::new(0, Page::MAX_LIMIT),
        )
        .await
        .unwrap();
    assert!(!rows.is_empty());
    for row in rows {
        let mut entry = row.entry.clone();
        entry.indexed.remove(&key);
        let name = row.path.as_str().rsplit('/').next().unwrap().to_string();
        let path = ScopedPath::new(format!("{}/{name}", prefix.as_str())).unwrap();
        fs.put(
            &resource_scope,
            &path,
            entry,
            CasExpectation::Version(row.version),
        )
        .await
        .unwrap();
    }
}

/// Legacy messages are indexed by bounded steps that resume from a durable
/// cursor: a huge thread takes several steps, a new service instance (a
/// restart) carries on where the last one stopped, overlapping steps never
/// fail or move the cursor back, and search itself writes nothing.
async fn backfill_contract<F: RootFilesystem + 'static>(backend: Arc<F>) {
    let fs = scoped_fs(Arc::clone(&backend), "alice");
    let service = FilesystemSessionThreadService::new(Arc::clone(&fs));
    let mine = scope("alice");
    for index in 0..12 {
        let thread = new_thread(&service, &mine, &format!("thread-{index:03}")).await;
        say(
            &service,
            &mine,
            &thread,
            &format!("legacy walrus note {index}"),
        )
        .await;
        strip_search_projection(&fs, &mine, &thread).await;
    }
    let huge = new_thread(&service, &mine, "thread-huge").await;
    for n in 0..150 {
        say(&service, &mine, &huge, &format!("bulk line {n} iguana")).await;
    }
    strip_search_projection(&fs, &mine, &huge).await;

    // Search before indexing: nothing indexed yet, reported incomplete, and
    // repeated searches still do not index.
    for _ in 0..2 {
        let early = search(&service, &mine, "walrus").await;
        assert!(!early.index_complete);
    }

    // One bounded step does not finish ~160 messages.
    let first = service
        .advance_message_search_index(&mine, 40)
        .await
        .unwrap();
    assert!(!first.complete);
    assert_eq!(first.scanned, 40);

    // Overlapping steps: both succeed; progress only moves forward.
    let (a, b) = tokio::join!(
        service.advance_message_search_index(&mine, 40),
        service.advance_message_search_index(&mine, 40),
    );
    a.unwrap();
    b.unwrap();

    // A restart: a fresh service over the same storage resumes.
    let restarted = FilesystemSessionThreadService::new(Arc::clone(&fs));
    let mut steps = 0;
    while !restarted
        .advance_message_search_index(&mine, 40)
        .await
        .unwrap()
        .complete
    {
        steps += 1;
        assert!(steps < 20, "indexing must finish in bounded steps");
    }
    let all = restarted
        .search_thread_messages(SearchThreadMessagesRequest {
            max_threads: 50,
            max_hits_per_thread: 1,
            ..SearchThreadMessagesRequest::new(mine.clone(), "walrus")
        })
        .await
        .unwrap();
    assert!(all.index_complete);
    assert_eq!(all.threads.len(), 12, "every legacy thread is indexed");
    let huge_hits = search(&restarted, &mine, "iguana").await;
    assert_eq!(thread_ids(&huge_hits), ["thread-huge"]);
    // The stored transcript is unchanged.
    let history = restarted
        .list_thread_history(ironclaw_threads::ThreadHistoryRequest {
            scope: mine.clone(),
            thread_id: ThreadId::new("thread-007").unwrap(),
        })
        .await
        .unwrap();
    assert_eq!(
        history.messages[0].content.as_deref(),
        Some("legacy walrus note 7")
    );
}

#[tokio::test]
async fn history_search_backfill_is_bounded_resumable_and_race_safe_in_memory() {
    backfill_contract(Arc::new(InMemoryBackend::new())).await;
}

#[tokio::test]
async fn history_search_backfill_is_bounded_resumable_and_race_safe_libsql() {
    let (_dir, backend) = libsql_backend().await;
    backfill_contract(backend).await;
}

#[tokio::test]
async fn history_search_backfill_is_bounded_resumable_and_race_safe_postgres() {
    let Some((db, backend)) = postgres_backend().await else {
        return;
    };
    backfill_contract(Arc::clone(&backend)).await;
    drop(backend);
    db.cleanup().await;
}

const WRITE_OPS: [ironclaw_filesystem::FilesystemOperation; 7] = [
    ironclaw_filesystem::FilesystemOperation::WriteFile,
    ironclaw_filesystem::FilesystemOperation::Delete,
    ironclaw_filesystem::FilesystemOperation::EnsureIndex,
    ironclaw_filesystem::FilesystemOperation::BeginTxn,
    ironclaw_filesystem::FilesystemOperation::Append,
    ironclaw_filesystem::FilesystemOperation::ReserveSeq,
    ironclaw_filesystem::FilesystemOperation::CreateSubtreeAtomic,
];

/// Once a scope is ready, every read history search uses is a pure read: a
/// fresh service (empty in-process caches, as after a restart) over a
/// backend that rejects every write still answers all of them. Before the
/// scope is ready, `message_search_ready` itself is a pure read that says no.
async fn ready_reads_never_write<F: RootFilesystem + 'static>(inner: F) {
    // Every write fails once the faults are armed; until then the same
    // backend takes the setup and the indexing.
    let guarded = Arc::new(ironclaw_filesystem::FaultInjecting::new(inner));
    let writer = FilesystemSessionThreadService::new(scoped_fs(Arc::clone(&guarded), "alice"));
    let mine = scope("alice");
    let thread = new_thread(&writer, &mine, "thread-read").await;
    say(&writer, &mine, &thread, "the gannet colony survey").await;
    let answer = reply(&writer, &mine, &thread, "run-1", "gannets nest on cliffs").await;

    let arm = || {
        for op in WRITE_OPS {
            guarded.add_fault(ironclaw_filesystem::Fault::on(op));
        }
    };
    let writes = || WRITE_OPS.iter().map(|op| guarded.count(*op)).sum::<usize>();
    let fresh = || FilesystemSessionThreadService::new(scoped_fs(Arc::clone(&guarded), "alice"));

    let before = writes();
    assert!(!fresh().message_search_ready(&mine).await.unwrap());
    assert_eq!(writes(), before, "readiness is a pure read");

    index_all(&writer, &mine).await;
    arm();
    let before = writes();
    let reader = fresh();
    assert!(reader.message_search_ready(&mine).await.unwrap());
    let found = search(&reader, &mine, "gannet").await;
    assert_eq!(thread_ids(&found), ["thread-read"]);
    reader
        .list_thread_history(ironclaw_threads::ThreadHistoryRequest {
            scope: mine.clone(),
            thread_id: thread.clone(),
        })
        .await
        .unwrap();
    reader
        .read_thread_message(&mine, &thread, answer.message_id)
        .await
        .unwrap();
    reader
        .list_thread_messages_range(ironclaw_threads::ThreadMessageRangeRequest {
            scope: mine.clone(),
            thread_id: thread.clone(),
            after_sequence: 0,
            through_sequence: 10,
        })
        .await
        .unwrap();
    reader
        .list_threads_for_scope(ironclaw_threads::ListThreadsForScopeRequest {
            scope: mine.clone(),
            limit: Some(10),
            cursor: None,
        })
        .await
        .unwrap();
    assert_eq!(writes(), before, "a ready read attempted a write");
}

#[tokio::test]
async fn history_search_ready_reads_never_write_in_memory() {
    ready_reads_never_write(InMemoryBackend::new()).await;
}

#[tokio::test]
async fn history_search_ready_reads_never_write_libsql() {
    let (_dir, backend) = libsql_backend().await;
    let backend = Arc::try_unwrap(backend).unwrap_or_else(|_| panic!("sole owner"));
    ready_reads_never_write(backend).await;
}
