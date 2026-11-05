//! Caller-level tests for the Memory page decorator: every view and command
//! goes through `ProductSurface::query`/`invoke` with typed descriptors, the
//! way the WebUI handlers call it.

use std::collections::HashMap;
use std::sync::Mutex;

use ironclaw_host_api::ids::{ActivityId, SecretHandle, TenantId, UserId};
use ironclaw_orchestrator::profiles::ProfileOwner;
use ironclaw_product_contracts::operator_secrets::{
    OperatorSecretValueStore, OperatorSecretValueStoreError,
};
use ironclaw_product_contracts::profiles::{ProfileSettings, ProfileSupermemory};
use ironclaw_product_contracts::surface::{
    BoundProductSurface, ProductSurfaceErrorKind, ProductSurfaceStreamResponse,
};
use ironclaw_supermemory::{StaticSupermemoryBindingSource, SupermemoryClient, SupermemoryConfig};
use secrecy::{ExposeSecret, SecretString};

use super::*;

const FAKE_KEY: &str = "sm_fake_key_never_shown";

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
        Ok(ProductSurfaceStreamResponse {
            events: Vec::new(),
            next_cursor: None,
            subscription: None,
        })
    }
}

/// An in-memory document store keyed by caller user, with hash-checked writes.
#[derive(Default)]
struct FakeDocuments {
    docs: Mutex<HashMap<(String, String), String>>,
    calls: Mutex<Vec<String>>,
}

fn hash(content: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(content.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

impl FakeDocuments {
    fn key(caller: &ProductSurfaceCaller, path: &str) -> (String, String) {
        (caller.user_id.as_str().to_string(), path.to_string())
    }
    fn document(path: &str, content: &str) -> MemoryDocument {
        MemoryDocument {
            path: path.to_string(),
            kind: memory_document_kind(path),
            content: content.to_string(),
            content_hash: hash(content),
            word_count: content.split_whitespace().count(),
            provenance: MemoryProvenance::default(),
        }
    }
}

#[async_trait]
impl MemoryDocumentsSource for FakeDocuments {
    async fn tree(
        &self,
        caller: &ProductSurfaceCaller,
    ) -> Result<Vec<MemoryDocumentEntry>, ProductSurfaceError> {
        Ok(self
            .docs
            .lock()
            .unwrap()
            .keys()
            .filter(|(user, _)| user == caller.user_id.as_str())
            .map(|(_, path)| MemoryDocumentEntry {
                path: path.clone(),
                kind: memory_document_kind(path),
            })
            .collect())
    }
    async fn read(
        &self,
        caller: &ProductSurfaceCaller,
        path: &str,
    ) -> Result<MemoryDocument, ProductSurfaceError> {
        self.calls.lock().unwrap().push(format!("read {path}"));
        self.docs
            .lock()
            .unwrap()
            .get(&Self::key(caller, path))
            .map(|content| Self::document(path, content))
            .ok_or_else(ProductSurfaceError::not_found)
    }
    async fn search(
        &self,
        caller: &ProductSurfaceCaller,
        query: &str,
        limit: usize,
    ) -> Result<Vec<MemorySearchHit>, ProductSurfaceError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("search {query} {limit}"));
        Ok(self
            .docs
            .lock()
            .unwrap()
            .iter()
            .filter(|((user, _), content)| {
                user == caller.user_id.as_str() && content.contains(query)
            })
            .map(|((_, path), content)| MemorySearchHit {
                path: path.clone(),
                snippet: content.clone(),
                score: 1.0,
            })
            .take(limit)
            .collect())
    }
    async fn write(
        &self,
        caller: &ProductSurfaceCaller,
        request: MemoryWriteRequest,
    ) -> Result<MemoryWriteResponse, ProductSurfaceError> {
        let key = Self::key(caller, &request.path);
        let mut docs = self.docs.lock().unwrap();
        let current = docs.get(&key).cloned();
        let matches = match (&current, &request.expected_content_hash) {
            (None, None) => true,
            (Some(content), Some(expected)) => &hash(content) == expected,
            _ => false,
        };
        if !matches {
            return Ok(MemoryWriteResponse {
                outcome: MemoryWriteOutcome::Conflict,
                document: current.map(|content| Self::document(&request.path, &content)),
            });
        }
        docs.insert(key, request.content.clone());
        Ok(MemoryWriteResponse {
            outcome: MemoryWriteOutcome::Written,
            document: Some(Self::document(&request.path, &request.content)),
        })
    }
    async fn archive(
        &self,
        caller: &ProductSurfaceCaller,
        request: MemoryArchiveRequest,
    ) -> Result<MemoryArchiveResponse, ProductSurfaceError> {
        let mut docs = self.docs.lock().unwrap();
        let key = Self::key(caller, &request.path);
        let Some(content) = docs.get(&key).cloned() else {
            return Err(ProductSurfaceError::not_found());
        };
        if hash(&content) != request.expected_content_hash {
            return Ok(MemoryArchiveResponse {
                outcome: MemoryArchiveOutcome::Conflict,
                archived_path: None,
            });
        }
        docs.remove(&key);
        let archived = format!("archive/t/{}", request.path);
        docs.insert(Self::key(caller, &archived), content);
        Ok(MemoryArchiveResponse {
            outcome: MemoryArchiveOutcome::Archived,
            archived_path: Some(archived),
        })
    }
}

fn caller(user: &str) -> ProductSurfaceCaller {
    ProductSurfaceCaller::new(
        TenantId::new("t1").unwrap(),
        UserId::new(user).unwrap(),
        None,
        None,
    )
}

fn bound(surface: &Arc<MemoryPageSurface>, user: &str) -> BoundProductSurface {
    BoundProductSurface::new(Arc::clone(surface) as Arc<dyn ProductSurface>, caller(user))
}

fn local_surface() -> (Arc<MemoryPageSurface>, Arc<FakeDocuments>) {
    let docs = Arc::new(FakeDocuments::default());
    let surface = Arc::new(MemoryPageSurface::new(
        Arc::new(Inner),
        Some(Arc::clone(&docs) as Arc<dyn MemoryDocumentsSource>),
        None,
        None,
    ));
    (surface, docs)
}

#[tokio::test]
async fn local_documents_round_trip_create_read_conflict_save_search_archive() {
    let (surface, docs) = local_surface();
    let alice = bound(&surface, "alice");

    let created = MEMORY_WRITE_COMMAND
        .invoke_on(
            &alice,
            MemoryWriteRequest {
                path: "notes/tea.md".to_string(),
                content: "likes green tea".to_string(),
                expected_content_hash: None,
            },
            ActivityId::new(),
        )
        .await
        .unwrap();
    assert_eq!(created.outcome, MemoryWriteOutcome::Written);

    let tree = MEMORY_TREE_VIEW
        .query_on(&alice, MemoryTreeRequest {}, None)
        .await
        .unwrap();
    assert!(tree.available);
    assert_eq!(tree.documents.len(), 1);
    assert_eq!(tree.documents[0].path, "notes/tea.md");

    let read = MEMORY_DOCUMENT_VIEW
        .query_on(
            &alice,
            MemoryDocumentRequest {
                path: "notes/tea.md".to_string(),
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(read.content, "likes green tea");

    // A stale hash is a conflict and changes nothing.
    let stale = MEMORY_WRITE_COMMAND
        .invoke_on(
            &alice,
            MemoryWriteRequest {
                path: "notes/tea.md".to_string(),
                content: "likes coffee".to_string(),
                expected_content_hash: Some(hash("something else")),
            },
            ActivityId::new(),
        )
        .await
        .unwrap();
    assert_eq!(stale.outcome, MemoryWriteOutcome::Conflict);
    assert_eq!(stale.document.unwrap().content, "likes green tea");

    let saved = MEMORY_WRITE_COMMAND
        .invoke_on(
            &alice,
            MemoryWriteRequest {
                path: "notes/tea.md".to_string(),
                content: "likes black tea".to_string(),
                expected_content_hash: Some(read.content_hash.clone()),
            },
            ActivityId::new(),
        )
        .await
        .unwrap();
    assert_eq!(saved.outcome, MemoryWriteOutcome::Written);

    let found = MEMORY_SEARCH_VIEW
        .query_on(
            &alice,
            MemorySearchRequest {
                query: "black".to_string(),
                limit: Some(500),
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(found.hits.len(), 1);
    assert!(
        docs.calls
            .lock()
            .unwrap()
            .contains(&format!("search black {MAX_MEMORY_SEARCH_LIMIT}")),
        "limit is clamped before the source"
    );

    let archived = MEMORY_ARCHIVE_COMMAND
        .invoke_on(
            &alice,
            MemoryArchiveRequest {
                path: "notes/tea.md".to_string(),
                expected_content_hash: saved.document.unwrap().content_hash,
            },
            ActivityId::new(),
        )
        .await
        .unwrap();
    assert_eq!(archived.outcome, MemoryArchiveOutcome::Archived);
    let tree = MEMORY_TREE_VIEW
        .query_on(&alice, MemoryTreeRequest {}, None)
        .await
        .unwrap();
    assert_eq!(tree.documents[0].kind, MemoryDocumentKind::Archived);

    // Another user sees none of it.
    let bob = bound(&surface, "bob");
    let tree = MEMORY_TREE_VIEW
        .query_on(&bob, MemoryTreeRequest {}, None)
        .await
        .unwrap();
    assert!(tree.documents.is_empty());
}

#[tokio::test]
async fn unsafe_paths_and_oversized_bodies_never_reach_the_source() {
    let (surface, docs) = local_surface();
    let alice = bound(&surface, "alice");
    for path in [
        "../secret.md",
        "/etc/passwd",
        "a//b.md",
        "a\\b.md",
        "a/\u{7}.md",
        "",
    ] {
        let error = MEMORY_DOCUMENT_VIEW
            .query_on(
                &alice,
                MemoryDocumentRequest {
                    path: path.to_string(),
                },
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind, ProductSurfaceErrorKind::Validation, "{path:?}");
    }
    let too_big = MEMORY_WRITE_COMMAND
        .invoke_on(
            &alice,
            MemoryWriteRequest {
                path: "big.md".to_string(),
                content: "x".repeat(MAX_MEMORY_DOCUMENT_BYTES + 1),
                expected_content_hash: None,
            },
            ActivityId::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(too_big.field.as_deref(), Some("content"));
    let into_archive = MEMORY_WRITE_COMMAND
        .invoke_on(
            &alice,
            MemoryWriteRequest {
                path: "archive/x/notes.md".to_string(),
                content: "edit".to_string(),
                expected_content_hash: None,
            },
            ActivityId::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(into_archive.field.as_deref(), Some("path"));
    let bad_hash = MEMORY_ARCHIVE_COMMAND
        .invoke_on(
            &alice,
            MemoryArchiveRequest {
                path: "a.md".to_string(),
                expected_content_hash: "nope".to_string(),
            },
            ActivityId::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(bad_hash.field.as_deref(), Some("expected_content_hash"));
    assert!(docs.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn without_a_document_provider_the_tree_says_unavailable_and_reads_fail_closed() {
    let surface = Arc::new(MemoryPageSurface::new(Arc::new(Inner), None, None, None));
    let alice = bound(&surface, "alice");
    let tree = MEMORY_TREE_VIEW
        .query_on(&alice, MemoryTreeRequest {}, None)
        .await
        .unwrap();
    assert!(!tree.available);
    let error = MEMORY_DOCUMENT_VIEW
        .query_on(
            &alice,
            MemoryDocumentRequest {
                path: "MEMORY.md".to_string(),
            },
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(error.kind, ProductSurfaceErrorKind::ServiceUnavailable);
    let status = SUPERMEMORY_STATUS_VIEW
        .query_on(&alice, SupermemoryStatusRequest::default(), None)
        .await
        .unwrap();
    assert!(!status.available);
    assert!(!status.active);
    // Other views pass through to the inner surface.
    let passthrough = alice
        .query(ProductSurfaceQueryRequest {
            view_id: "threads.list".to_string(),
            input: serde_json::json!({}),
            cursor: None,
            limit: None,
        })
        .await
        .unwrap_err();
    assert_eq!(passthrough.kind, ProductSurfaceErrorKind::NotFound);
}

#[derive(Default)]
struct Keys(Mutex<HashMap<String, String>>);

#[async_trait]
impl OperatorSecretValueStore for Keys {
    async fn put(
        &self,
        handle: &SecretHandle,
        value: SecretString,
    ) -> Result<(), OperatorSecretValueStoreError> {
        self.0.lock().unwrap().insert(
            handle.as_str().to_string(),
            value.expose_secret().to_string(),
        );
        Ok(())
    }
    async fn contains(&self, handle: &SecretHandle) -> Result<bool, OperatorSecretValueStoreError> {
        Ok(self.0.lock().unwrap().contains_key(handle.as_str()))
    }
    async fn handles(&self) -> Result<Vec<SecretHandle>, OperatorSecretValueStoreError> {
        Ok(Vec::new())
    }
    async fn read(
        &self,
        handle: &SecretHandle,
    ) -> Result<Option<SecretString>, OperatorSecretValueStoreError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .get(handle.as_str())
            .map(|value| SecretString::from(value.clone())))
    }
    async fn delete(&self, handle: &SecretHandle) -> Result<bool, OperatorSecretValueStoreError> {
        Ok(self.0.lock().unwrap().remove(handle.as_str()).is_some())
    }
}

#[tokio::test]
async fn supermemory_status_resolves_the_named_profile_binding_and_never_returns_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("o.db"))
            .build()
            .await
            .unwrap(),
    );
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(Arc::clone(&db)).unwrap());
    let engine = Engine::open_libsql(runtime, "ironclaw").await.unwrap();
    let owner = ProfileOwner {
        tenant_id: TenantId::new("t1").unwrap(),
        user_id: UserId::new("alice").unwrap(),
    };
    engine
        .store()
        .create_profile(
            &owner,
            &parse_profile_slug("work").unwrap(),
            &ProfileSettings {
                display_name: "Work".to_string(),
                supermemory: Some(ProfileSupermemory {
                    container: "work-memory".to_string(),
                    read_containers: vec!["career".to_string()],
                    instructions: None,
                }),
                ..ProfileSettings::default()
            },
        )
        .await
        .unwrap();

    let keys = Arc::new(Keys::default());
    keys.0.lock().unwrap().insert(
        ironclaw_supermemory::SUPERMEMORY_API_KEY_SECRET.to_string(),
        FAKE_KEY.to_string(),
    );
    let config = SupermemoryConfig::default();
    // The production client is never asked to send anything here: status
    // reads only the key's presence and the binding.
    let client = SupermemoryClient::production(&config, keys).unwrap();
    let default_binding = StaticSupermemoryBindingSource::new(Some(SupermemoryBinding {
        primary_container: "ironclaw".to_string(),
        read_containers: Vec::new(),
        instructions: None,
    }));
    let supermemory = Arc::new(Supermemory::new(client, Arc::new(default_binding), config));
    let surface = Arc::new(MemoryPageSurface::new(
        Arc::new(Inner),
        None,
        Some(supermemory),
        Some(Arc::clone(&engine)),
    ));
    let alice = bound(&surface, "alice");

    let status = SUPERMEMORY_STATUS_VIEW
        .query_on(
            &alice,
            SupermemoryStatusRequest {
                profile_id: Some("work".to_string()),
            },
            None,
        )
        .await
        .unwrap();
    assert!(status.available && status.key_configured && status.active);
    let binding = status.binding.clone().unwrap();
    assert_eq!(binding.primary_container, "work-memory");
    assert_eq!(binding.read_containers, vec!["career".to_string()]);
    assert_eq!(binding.origin, SupermemoryBindingOrigin::Profile);
    assert!(!serde_json::to_string(&status).unwrap().contains(FAKE_KEY));

    let default = SUPERMEMORY_STATUS_VIEW
        .query_on(&alice, SupermemoryStatusRequest::default(), None)
        .await
        .unwrap();
    let binding = default.binding.unwrap();
    assert_eq!(binding.primary_container, "ironclaw");
    assert_eq!(binding.origin, SupermemoryBindingOrigin::Default);

    // Another user's profile id resolves to nothing of alice's.
    let bob = bound(&surface, "bob");
    let missing = SUPERMEMORY_STATUS_VIEW
        .query_on(
            &bob,
            SupermemoryStatusRequest {
                profile_id: Some("work".to_string()),
            },
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(missing.kind, ProductSurfaceErrorKind::NotFound);
}
