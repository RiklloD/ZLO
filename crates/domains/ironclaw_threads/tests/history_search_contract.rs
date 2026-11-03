//! History search conformance: the same scenarios against the in-memory
//! reference service and the filesystem service on the in-memory and libSQL
//! backends (`fts5`). PostgreSQL (`tsvector`) serves the same
//! `Filter::FtsRanked` contract, which `ironclaw_filesystem`'s own
//! `db_root_filesystem_contract` suite pins across backends.

use std::sync::Arc;

use ironclaw_filesystem::{
    CasExpectation, Filter, InMemoryBackend, IndexKey, LibSqlRootFilesystem, Page, RootFilesystem,
    ScopedFilesystem,
};
use ironclaw_host_api::{
    ids::{AgentId, ProjectId, TenantId, ThreadId, UserId},
    mount::{MountGrant, MountPermissions, MountView},
    path::{MountAlias, ScopedPath, VirtualPath},
};
use ironclaw_threads::{
    AcceptInboundMessageRequest, AppendAssistantDraftRequest,
    AppendFinalizedAssistantMessageRequest, EnsureThreadRequest, FilesystemSessionThreadService,
    InMemorySessionThreadService, MessageContent, RedactMessageRequest, SearchHitRole,
    SearchThreadMessagesRequest, SearchThreadMessagesResponse, SessionThreadService, ThreadScope,
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

async fn new_thread(service: &dyn SessionThreadService, scope: &ThreadScope, id: &str) -> ThreadId {
    let thread_id = ThreadId::new(id).unwrap();
    service
        .ensure_thread(EnsureThreadRequest {
            scope: scope.clone(),
            thread_id: Some(thread_id.clone()),
            created_by_actor_id: "person".into(),
            title: None,
            metadata_json: None,
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

async fn search(
    service: &dyn SessionThreadService,
    scope: &ThreadScope,
    query: &str,
) -> SearchThreadMessagesResponse {
    service
        .search_thread_messages(SearchThreadMessagesRequest {
            scope: scope.clone(),
            query: query.into(),
            max_threads: 20,
            max_hits_per_thread: 5,
        })
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

    // Another user's transcript is invisible.
    let theirs = scope("bob");
    let bob = new_thread(other, &theirs, "thread-bob").await;
    say(other, &theirs, &bob, "staging deployment for bob only").await;

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

    // Search-as-you-type: a word prefix matches.
    assert_eq!(
        thread_ids(&search(service, &mine, "carbon").await),
        ["thread-pasta"]
    );

    // Covering more query terms ranks a thread first.
    let ranked = search(service, &mine, "staging carbonara deployment").await;
    assert_eq!(thread_ids(&ranked), ["thread-deploy", "thread-pasta"]);

    // Drafts and redacted text are never found; neither is another user.
    assert!(search(service, &mine, "zanzibar").await.threads.is_empty());
    assert!(search(service, &mine, "quokka").await.threads.is_empty());
    assert!(search(service, &mine, "bob").await.threads.is_empty());

    // Empty / punctuation-only queries match nothing.
    assert!(search(service, &mine, "  ?! ").await.threads.is_empty());
}

#[tokio::test]
async fn in_memory_reference_satisfies_the_search_contract() {
    let service = InMemorySessionThreadService::default();
    search_contract(&service, &service).await;
}

#[tokio::test]
async fn filesystem_in_memory_backend_satisfies_the_search_contract() {
    let backend = Arc::new(InMemoryBackend::new());
    let alice = FilesystemSessionThreadService::new(scoped_fs(Arc::clone(&backend), "alice"));
    let bob = FilesystemSessionThreadService::new(scoped_fs(backend, "bob"));
    search_contract(&alice, &bob).await;
}

#[tokio::test]
async fn filesystem_libsql_backend_satisfies_the_search_contract() {
    let (_dir, backend) = libsql_backend().await;
    let alice = FilesystemSessionThreadService::new(scoped_fs(Arc::clone(&backend), "alice"));
    let bob = FilesystemSessionThreadService::new(scoped_fs(backend, "bob"));
    search_contract(&alice, &bob).await;
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

async fn backfill_resumes_until_complete<F: RootFilesystem + 'static>(backend: Arc<F>) {
    let fs = scoped_fs(backend, "alice");
    let service = FilesystemSessionThreadService::new(Arc::clone(&fs));
    let mine = scope("alice");
    // More threads than one backfill step re-projects.
    for index in 0..45 {
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
    let first = search(&service, &mine, "walrus").await;
    assert!(
        !first.index_complete,
        "a large legacy scope backfills over several searches"
    );
    assert!(
        !first.threads.is_empty(),
        "already re-projected threads are found at once"
    );
    let second = search(&service, &mine, "walrus").await;
    assert!(second.index_complete);
    let all = service
        .search_thread_messages(SearchThreadMessagesRequest {
            scope: mine.clone(),
            query: "walrus".into(),
            max_threads: 50,
            max_hits_per_thread: 1,
        })
        .await
        .unwrap();
    assert_eq!(
        all.threads.len(),
        45,
        "every legacy thread is indexed after the backfill"
    );
    // The backfill re-projects; the stored transcript is unchanged.
    let history = service
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
async fn backfill_indexes_legacy_messages_in_bounded_resumable_steps_in_memory() {
    backfill_resumes_until_complete(Arc::new(InMemoryBackend::new())).await;
}

#[tokio::test]
async fn backfill_indexes_legacy_messages_in_bounded_resumable_steps_libsql() {
    let (_dir, backend) = libsql_backend().await;
    backfill_resumes_until_complete(backend).await;
}
