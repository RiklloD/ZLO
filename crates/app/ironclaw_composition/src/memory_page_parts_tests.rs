//! The native Memory-page source on both filesystem backends: create, read
//! with provenance, conflict-aware save, search, archive (never delete), and
//! caller isolation. Plus the hook inventory's flag handling.

use std::sync::Arc;

use ironclaw_filesystem::{InMemoryBackend, LibSqlRootFilesystem, RootFilesystem};
use ironclaw_host_api::ids::{TenantId, UserId};
use ironclaw_product_contracts::memory_page::MemoryDocumentKind;

use super::*;
use crate::observability::hooks::HooksActivationConfig;
use crate::observability::hooks::catalog::{MEMORY_UPKEEP_HOOK_ID, hook_catalog};

fn caller(user: &str) -> ProductSurfaceCaller {
    ProductSurfaceCaller::new(
        TenantId::new("t1").unwrap(),
        UserId::new(user).unwrap(),
        None,
        None,
    )
}

fn write(path: &str, content: &str, expected: Option<String>) -> MemoryWriteRequest {
    MemoryWriteRequest {
        path: path.to_string(),
        content: content.to_string(),
        expected_content_hash: expected,
    }
}

async fn round_trip(filesystem: Arc<dyn RootFilesystem>) {
    let source = NativeMemoryDocuments::new(filesystem);
    let alice = caller("alice");

    let created = source
        .write(&alice, write("USER.md", "Name: Alice", None))
        .await
        .unwrap();
    assert_eq!(created.outcome, MemoryWriteOutcome::Written);
    let document = created.document.unwrap();
    assert_eq!(document.kind, MemoryDocumentKind::Identity);
    assert_eq!(document.provenance.version_count, 0);

    // Creating over an existing document is a conflict, not an overwrite.
    let again = source
        .write(&alice, write("USER.md", "Name: Mallory", None))
        .await
        .unwrap();
    assert_eq!(again.outcome, MemoryWriteOutcome::Conflict);
    assert_eq!(again.document.unwrap().content, "Name: Alice");

    // A stale hash is a conflict; the current hash saves and keeps a version.
    let stale = source
        .write(
            &alice,
            write(
                "USER.md",
                "Name: Bob",
                Some(ironclaw_memory::content_sha256("other")),
            ),
        )
        .await
        .unwrap();
    assert_eq!(stale.outcome, MemoryWriteOutcome::Conflict);
    let saved = source
        .write(
            &alice,
            write(
                "USER.md",
                "Name: Alice Liddell",
                Some(document.content_hash.clone()),
            ),
        )
        .await
        .unwrap();
    assert_eq!(saved.outcome, MemoryWriteOutcome::Written);
    let saved = saved.document.unwrap();
    assert_eq!(saved.content, "Name: Alice Liddell");
    assert_eq!(saved.provenance.version_count, 1);

    source
        .write(
            &alice,
            write("notes/pets.md", "owns a cat named Dinah", None),
        )
        .await
        .unwrap();
    let hits = source.search(&alice, "Dinah", 5).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, "notes/pets.md");

    let pets = source.read(&alice, "notes/pets.md").await.unwrap();
    let archived = source
        .archive(
            &alice,
            MemoryArchiveRequest {
                path: "notes/pets.md".to_string(),
                expected_content_hash: pets.content_hash,
            },
        )
        .await
        .unwrap();
    assert_eq!(archived.outcome, MemoryArchiveOutcome::Archived);
    let archived_path = archived.archived_path.unwrap();
    let tree = source.tree(&alice).await.unwrap();
    assert!(
        tree.iter()
            .any(|entry| entry.path == archived_path && entry.kind == MemoryDocumentKind::Archived)
    );
    assert!(!tree.iter().any(|entry| entry.path == "notes/pets.md"));
    assert_eq!(
        source.read(&alice, &archived_path).await.unwrap().content,
        "owns a cat named Dinah"
    );
    assert!(source.search(&alice, "Dinah", 5).await.unwrap().is_empty());
    let missing = source.read(&alice, "notes/pets.md").await.unwrap_err();
    assert_eq!(missing.status_code, 404);

    // Bob sees none of alice's memory.
    let bob = caller("bob");
    assert!(source.tree(&bob).await.unwrap().is_empty());
    assert_eq!(
        source.read(&bob, "USER.md").await.unwrap_err().status_code,
        404
    );
}

#[tokio::test]
async fn native_documents_round_trip_in_memory() {
    round_trip(Arc::new(InMemoryBackend::new())).await;
}

#[tokio::test]
async fn native_documents_round_trip_on_libsql() {
    let storage = tempfile::tempdir().unwrap();
    let database = Arc::new(
        libsql::Builder::new_local(storage.path().join("memory.db"))
            .build()
            .await
            .unwrap(),
    );
    let backend = LibSqlRootFilesystem::new(database).unwrap();
    backend.run_migrations().await.unwrap();
    round_trip(Arc::new(backend)).await;
}

#[tokio::test]
async fn hook_catalog_reports_flags_and_the_memory_upkeep_hook() {
    let off = hook_catalog(HooksActivationConfig::disabled(), None, false);
    assert!(!off.framework_enabled);
    assert!(!off.toggle_supported);
    let upkeep = off
        .hooks
        .iter()
        .find(|hook| hook.id == MEMORY_UPKEEP_HOOK_ID)
        .unwrap();
    assert!(!upkeep.enabled);
    assert_eq!(upkeep.points, vec!["after_turn".to_string()]);

    let on = StaticHookCatalog(hook_catalog(
        HooksActivationConfig::enabled().with_third_party_enabled(true),
        None,
        true,
    ))
    .catalog()
    .await;
    assert!(on.framework_enabled && on.third_party_enabled);
    assert!(
        on.hooks
            .iter()
            .any(|hook| hook.id == MEMORY_UPKEEP_HOOK_ID && hook.enabled)
    );
}
