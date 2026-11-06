//! The Memory page's provider operations — flat listing, history, and archive
//! (the user's "remove") — on both filesystem backends the deployment uses.
//! Archive keeps the body under `archive/`, unindexed, and removes the live
//! path only while it still matches the hash the user read.

use std::sync::Arc;

use ironclaw_filesystem::{InMemoryBackend, LibSqlRootFilesystem, RootFilesystem};
use ironclaw_host_api::{
    ids::{InvocationId, TenantId, UserId},
    resource::ResourceScope,
};
use ironclaw_memory::{
    MemoryInvocation, MemoryServiceReadRequest, MemoryServiceSearchRequest,
    MemoryServiceWriteRequest, MemoryWriteStatus, content_bytes_sha256,
};
use ironclaw_memory_native::NativeMemoryService;

fn invocation(user: &str) -> MemoryInvocation {
    MemoryInvocation {
        scope: ResourceScope {
            tenant_id: TenantId::new("tenant-memory-page").unwrap(),
            user_id: UserId::new(user).unwrap(),
            agent_id: None,
            project_id: None,
            mission_id: None,
            thread_id: None,
            invocation_id: InvocationId::new(),
        },
        correlation_id: ironclaw_host_api::ids::CorrelationId::new(),
    }
}

fn full_write(path: &str, content: &str, expected: Option<String>) -> MemoryServiceWriteRequest {
    MemoryServiceWriteRequest {
        target: path.to_string(),
        content: content.to_string(),
        append: false,
        expected_content_hash: expected,
        old_string: None,
        new_string: None,
        replace_all: false,
        metadata: None,
        timezone: None,
    }
}

async fn read(service: &NativeMemoryService, user: &str, path: &str) -> Option<String> {
    service
        .read(
            invocation(user),
            MemoryServiceReadRequest {
                path: path.to_string(),
            },
        )
        .await
        .ok()
        .map(|response| response.content)
}

async fn archive_round_trip(filesystem: Arc<dyn RootFilesystem>) {
    let service = NativeMemoryService::from_filesystem(filesystem, None);
    let user = "alice";
    service
        .write(
            invocation(user),
            full_write("notes/wrong.md", "zebra sightings are wrong", None),
        )
        .await
        .unwrap();
    // A second save keeps the first body as a version.
    let first_hash = content_bytes_sha256(b"zebra sightings are wrong");
    let rewrite = service
        .write(
            invocation(user),
            full_write(
                "notes/wrong.md",
                "zebra sightings are still wrong",
                Some(first_hash.clone()),
            ),
        )
        .await
        .unwrap();
    assert_eq!(rewrite.status, MemoryWriteStatus::Written);
    let history = service
        .history(invocation(user), "notes/wrong.md")
        .await
        .unwrap();
    assert_eq!(history.version_count, 1);
    assert!(history.last_changed_by.is_some());

    // A stale hash archives nothing.
    let stale = service
        .archive(invocation(user), "notes/wrong.md", &first_hash)
        .await
        .unwrap();
    assert_eq!(stale, None);
    assert!(read(&service, user, "notes/wrong.md").await.is_some());

    let current_hash = content_bytes_sha256(b"zebra sightings are still wrong");
    let archived = service
        .archive(invocation(user), "notes/wrong.md", &current_hash)
        .await
        .unwrap()
        .expect("archived");
    assert!(archived.starts_with("archive/"));
    assert!(archived.ends_with("/notes/wrong.md"));

    // The live path is gone, the body is kept under archive/, and it is no
    // longer searchable.
    assert_eq!(read(&service, user, "notes/wrong.md").await, None);
    // Only the owner's page reads archives; the agent's read tool does not.
    assert_eq!(read(&service, user, &archived).await, None);
    assert_eq!(
        service
            .owner_read(invocation(user), &archived)
            .await
            .unwrap()
            .content,
        "zebra sightings are still wrong"
    );
    let paths = service.list_paths(invocation(user)).await.unwrap();
    assert!(paths.contains(&archived));
    assert!(!paths.iter().any(|path| path == "notes/wrong.md"));
    let hits = service
        .search(
            invocation(user),
            MemoryServiceSearchRequest {
                query: "zebra".to_string(),
                limit: 5,
            },
        )
        .await
        .unwrap();
    assert!(
        hits.results.is_empty(),
        "archived documents are not recalled: {:?}",
        hits.results
    );

    // Archive refuses archived documents and other users cannot see either.
    let hash = content_bytes_sha256(b"zebra sightings are still wrong");
    assert!(
        service
            .archive(invocation(user), &archived, &hash)
            .await
            .is_err()
    );
    assert!(
        service
            .list_paths(invocation("bob"))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn archive_keeps_the_body_and_removes_the_live_path_in_memory() {
    archive_round_trip(Arc::new(InMemoryBackend::new())).await;
}

#[tokio::test]
async fn archive_keeps_the_body_and_removes_the_live_path_on_libsql() {
    let storage = tempfile::tempdir().unwrap();
    let database = Arc::new(
        libsql::Builder::new_local(storage.path().join("memory.db"))
            .build()
            .await
            .unwrap(),
    );
    let backend = LibSqlRootFilesystem::new(database).unwrap();
    backend.run_migrations().await.unwrap();
    archive_round_trip(Arc::new(backend)).await;
}

// --- Review findings: archive is not reachable or reversible through the
// agent's tool surface, and retrieval never serves a removed document. ---

use ironclaw_memory::{
    MemoryContextProfileId, MemoryService, MemoryServiceContextRequest, MemoryServiceTreeRequest,
};

async fn archived_fixture(filesystem: Arc<dyn RootFilesystem>) -> (NativeMemoryService, String) {
    let service = NativeMemoryService::from_filesystem(filesystem, None);
    service
        .write(
            invocation("alice"),
            full_write("notes/old.md", "quokka habits are wrong", None),
        )
        .await
        .unwrap();
    let archived = service
        .archive(
            invocation("alice"),
            "notes/old.md",
            &content_bytes_sha256(b"quokka habits are wrong"),
        )
        .await
        .unwrap()
        .unwrap();
    (service, archived)
}

async fn agent_cannot_reach_or_revive_archives(filesystem: Arc<dyn RootFilesystem>) {
    let (service, archived) = archived_fixture(filesystem).await;

    // The agent's read and tree tools do not see archive/.
    assert!(
        service
            .read(
                invocation("alice"),
                MemoryServiceReadRequest {
                    path: archived.clone(),
                },
            )
            .await
            .is_err()
    );
    let tree = service
        .tree(
            invocation("alice"),
            MemoryServiceTreeRequest {
                path: String::new(),
                depth: 10,
            },
        )
        .await
        .unwrap();
    assert!(
        !serde_json::to_string(&tree.entries)
            .unwrap()
            .contains("archive"),
        "tree hides archive/: {:?}",
        tree.entries
    );

    // Archive bodies are immutable: no rewrite, append, patch, or metadata
    // change (re-enabling indexing) is accepted.
    let mut revive = full_write(&archived, "quokka habits are wrong", None);
    revive.metadata = Some(ironclaw_memory::DocumentMetadata {
        skip_indexing: Some(false),
        ..Default::default()
    });
    assert!(service.write(invocation("alice"), revive).await.is_err());
    let mut append = full_write(&archived, "more", None);
    append.append = true;
    assert!(service.write(invocation("alice"), append).await.is_err());
    let mut patch = full_write(&archived, "", None);
    patch.old_string = Some("wrong".to_string());
    patch.new_string = Some("right".to_string());
    assert!(service.write(invocation("alice"), patch).await.is_err());
    assert!(
        service
            .write(invocation("alice"), full_write("archive/new.md", "x", None))
            .await
            .is_err(),
        "nothing new can be written under archive/"
    );

    // Recall and search never serve archived text.
    let lane = service
        .read_long_term(
            invocation("alice"),
            MemoryServiceContextRequest {
                query: "quokka habits".to_string(),
                max_snippets: 10,
                context_profile_id: MemoryContextProfileId::new("default").unwrap(),
            },
        )
        .await
        .unwrap();
    assert!(lane.iter().all(|snippet| !snippet.text.contains("quokka")));

    // The user's own page still reads it.
    assert_eq!(
        service
            .owner_read(invocation("alice"), &archived)
            .await
            .unwrap()
            .content,
        "quokka habits are wrong"
    );
}

#[tokio::test]
async fn agent_cannot_reach_or_revive_archives_in_memory() {
    agent_cannot_reach_or_revive_archives(Arc::new(InMemoryBackend::new())).await;
}

#[tokio::test]
async fn agent_cannot_reach_or_revive_archives_on_libsql() {
    let (_storage, backend) = libsql_backend().await;
    agent_cannot_reach_or_revive_archives(backend).await;
}

async fn libsql_backend() -> (tempfile::TempDir, Arc<dyn RootFilesystem>) {
    let storage = tempfile::tempdir().unwrap();
    let database = Arc::new(
        libsql::Builder::new_local(storage.path().join("memory.db"))
            .build()
            .await
            .unwrap(),
    );
    let backend = LibSqlRootFilesystem::new(database).unwrap();
    backend.run_migrations().await.unwrap();
    (storage, Arc::new(backend))
}

/// A removal interrupted after the body went (search chunks still on disk)
/// must not surface the removed text: retrieval only serves chunks whose live
/// document still exists.
async fn interrupted_removal_leaves_nothing_searchable(filesystem: Arc<dyn RootFilesystem>) {
    let service = NativeMemoryService::from_filesystem(Arc::clone(&filesystem), None);
    service
        .write(
            invocation("alice"),
            full_write("notes/gone.md", "wombat burrows deep", None),
        )
        .await
        .unwrap();
    let path = ironclaw_memory::MemoryDocumentPath::new_with_agent(
        "tenant-memory-page",
        "alice",
        None,
        None,
        "notes/gone.md",
    )
    .unwrap();
    // Simulate a crash after the body delete: only the body record goes.
    filesystem
        .delete(&path.virtual_path().unwrap())
        .await
        .unwrap();
    let hits = service
        .search(
            invocation("alice"),
            MemoryServiceSearchRequest {
                query: "wombat".to_string(),
                limit: 5,
            },
        )
        .await
        .unwrap();
    assert!(hits.results.is_empty(), "{:?}", hits.results);

    // Recreating the path serves only the new body.
    service
        .write(
            invocation("alice"),
            full_write("notes/gone.md", "wombat tunnels are new", None),
        )
        .await
        .unwrap();
    let hits = service
        .search(
            invocation("alice"),
            MemoryServiceSearchRequest {
                query: "wombat".to_string(),
                limit: 5,
            },
        )
        .await
        .unwrap();
    assert_eq!(hits.results.len(), 1);
    assert!(hits.results[0].content.contains("new"));
}

#[tokio::test]
async fn interrupted_removal_leaves_nothing_searchable_in_memory() {
    interrupted_removal_leaves_nothing_searchable(Arc::new(InMemoryBackend::new())).await;
}

#[tokio::test]
async fn interrupted_removal_leaves_nothing_searchable_on_libsql() {
    let (_storage, backend) = libsql_backend().await;
    interrupted_removal_leaves_nothing_searchable(backend).await;
}

/// A document recreated right after an archive keeps its own metadata and
/// search entries: the removal is one version-checked delete of the body and
/// never touches what a later write created.
async fn recreate_after_archive_keeps_new_index(filesystem: Arc<dyn RootFilesystem>) {
    let (service, _) = archived_fixture(filesystem).await;
    service
        .write(
            invocation("alice"),
            full_write("notes/old.md", "quokka facts, corrected", None),
        )
        .await
        .unwrap();
    let hits = service
        .search(
            invocation("alice"),
            MemoryServiceSearchRequest {
                query: "quokka".to_string(),
                limit: 5,
            },
        )
        .await
        .unwrap();
    assert_eq!(hits.results.len(), 1, "{:?}", hits.results);
    assert_eq!(hits.results[0].path, "notes/old.md");
    assert!(hits.results[0].content.contains("corrected"));
}

#[tokio::test]
async fn recreate_after_archive_keeps_new_index_in_memory() {
    recreate_after_archive_keeps_new_index(Arc::new(InMemoryBackend::new())).await;
}

#[tokio::test]
async fn recreate_after_archive_keeps_new_index_on_libsql() {
    let (_storage, backend) = libsql_backend().await;
    recreate_after_archive_keeps_new_index(backend).await;
}

/// The page's save is literal (no `memory`/`daily_log` alias resolution) and a
/// create is an atomic absent-CAS: it never overwrites an existing document,
/// and of two racing creates exactly one wins.
async fn owner_write_is_literal_and_creates_atomically(filesystem: Arc<dyn RootFilesystem>) {
    use ironclaw_memory_native::OwnerWriteOutcome;
    let service = Arc::new(NativeMemoryService::from_filesystem(filesystem, None));
    service
        .write(
            invocation("alice"),
            full_write("MEMORY.md", "standing facts", None),
        )
        .await
        .unwrap();
    // `memory` is the agent tool's alias for MEMORY.md; the page writes the
    // literal path and leaves MEMORY.md alone.
    let outcome = service
        .owner_write(invocation("alice"), "memory", "a literal file", None)
        .await
        .unwrap();
    assert_eq!(outcome, OwnerWriteOutcome::Written);
    assert_eq!(
        service
            .owner_read(invocation("alice"), "MEMORY.md")
            .await
            .unwrap()
            .content,
        "standing facts"
    );
    // Creating over an existing document is a conflict.
    assert_eq!(
        service
            .owner_write(invocation("alice"), "MEMORY.md", "clobber", None)
            .await
            .unwrap(),
        OwnerWriteOutcome::Conflict
    );
    // Archive and thread scratch are refused.
    assert!(
        service
            .owner_write(invocation("alice"), "archive/x.md", "x", None)
            .await
            .is_err()
    );

    // Two concurrent creates of one new path: exactly one is written.
    let (a, b) = tokio::join!(
        {
            let service = Arc::clone(&service);
            async move {
                service
                    .owner_write(invocation("alice"), "notes/race.md", "first", None)
                    .await
            }
        },
        {
            let service = Arc::clone(&service);
            async move {
                service
                    .owner_write(invocation("alice"), "notes/race.md", "second", None)
                    .await
            }
        }
    );
    let outcomes = [a.unwrap(), b.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| **outcome == OwnerWriteOutcome::Written)
            .count(),
        1,
        "{outcomes:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_write_is_literal_and_creates_atomically_in_memory() {
    owner_write_is_literal_and_creates_atomically(Arc::new(InMemoryBackend::new())).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owner_write_is_literal_and_creates_atomically_on_libsql() {
    let (_storage, backend) = libsql_backend().await;
    owner_write_is_literal_and_creates_atomically(backend).await;
}
