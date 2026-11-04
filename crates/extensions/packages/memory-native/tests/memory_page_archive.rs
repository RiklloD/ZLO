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
    assert_eq!(
        read(&service, user, &archived).await.as_deref(),
        Some("zebra sightings are still wrong")
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
