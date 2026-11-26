//! Settings → Keys through the product surface: what a set, an import and a
//! remove leave in the operator store, and that the key never comes back out.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use ironclaw_host_api::ids::{ActivityId, TenantId, UserId};
use ironclaw_product_contracts::surface::{
    BoundProductSurface, ProductSurfaceErrorCode, ProductSurfaceStreamResponse,
};
use ironclaw_product_contracts::test_support::fakes::FakeOperatorSecretValueStore;

use super::*;

const KEY_ID: &str = "supermemory_api_key";
const GOOD: &str = "sm_good_key_never_shown";
const BAD: &str = "sm_bad_key_never_shown";
const OFFLINE: &str = "sm_offline_key_never_shown";

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

/// Accepts `GOOD`, rejects `BAD`, cannot reach the service for `OFFLINE`;
/// records every key it was asked about and every change notice.
#[derive(Default)]
struct Verifier {
    asked: Mutex<Vec<String>>,
    changed: AtomicUsize,
}

#[async_trait]
impl ServiceKeyVerifier for Verifier {
    async fn verify(
        &self,
        caller: &ProductSurfaceCaller,
        key: &SecretString,
    ) -> Result<(), ServiceKeyVerifyError> {
        assert_eq!(caller.user_id.as_str(), "alice", "verifies as the caller");
        let key = key.expose_secret().to_string();
        self.asked.lock().unwrap().push(key.clone());
        match key.as_str() {
            GOOD => Ok(()),
            BAD => Err(ServiceKeyVerifyError::Rejected),
            _ => Err(ServiceKeyVerifyError::Unreachable {
                reason: "timeout".to_string(),
            }),
        }
    }

    async fn key_changed(&self) {
        self.changed.fetch_add(1, Ordering::SeqCst);
    }
}

struct Found(Option<&'static str>);

#[async_trait]
impl ServiceKeyImportSource for Found {
    async fn find(&self, env_var: &str) -> Option<FoundServiceKey> {
        assert_eq!(env_var, "SUPERMEMORY_API_KEY");
        self.0.map(|value| FoundServiceKey {
            value: SecretString::from(value.to_string()),
            source: "Hermes (2 profiles)".to_string(),
        })
    }
}

struct Fixture {
    surface: BoundProductSurface,
    store: Arc<FakeOperatorSecretValueStore>,
    verifier: Arc<Verifier>,
}

fn fixture(found: Option<&'static str>) -> Fixture {
    let store = Arc::new(FakeOperatorSecretValueStore::new());
    let verifier = Arc::new(Verifier::default());
    let surface = Arc::new(ServiceKeysSurface::new(
        Arc::new(Inner),
        vec![ServiceKey {
            descriptor: ServiceKeyDescriptor {
                id: KEY_ID.to_string(),
                label: "Supermemory".to_string(),
                purpose: "Cloud memory.".to_string(),
                docs_url: None,
                import_env_var: Some("SUPERMEMORY_API_KEY".to_string()),
            },
            verifier: Some(Arc::clone(&verifier) as Arc<dyn ServiceKeyVerifier>),
        }],
        Arc::clone(&store) as Arc<dyn OperatorSecretValueStore>,
        vec![Arc::new(Found(found))],
    ));
    let caller = ProductSurfaceCaller::new(
        TenantId::new("t1").unwrap(),
        UserId::new("alice").unwrap(),
        None,
        None,
    );
    Fixture {
        surface: BoundProductSurface::new(surface as Arc<dyn ProductSurface>, caller),
        store,
        verifier,
    }
}

impl Fixture {
    async fn stored(&self) -> Option<String> {
        self.store
            .read(&SecretHandle::new(KEY_ID).unwrap())
            .await
            .unwrap()
            .map(|value| value.expose_secret().to_string())
    }

    async fn list(&self) -> ServiceKeysListResponse {
        SERVICE_KEYS_VIEW
            .query_on(&self.surface, ServiceKeysListRequest::default(), None)
            .await
            .unwrap()
    }

    async fn set(&self, value: &str) -> Result<ServiceKeySetResponse, ProductSurfaceError> {
        SERVICE_KEY_SET_COMMAND
            .invoke_on(
                &self.surface,
                ServiceKeySetRequest {
                    id: KEY_ID.to_string(),
                    value: value.to_string(),
                },
                ActivityId::new(),
            )
            .await
    }
}

fn no_key_in(value: &impl Serialize) {
    let text = serde_json::to_string(value).unwrap();
    for key in [GOOD, BAD, OFFLINE] {
        assert!(!text.contains(key), "a key leaked into {text}");
    }
}

#[tokio::test]
async fn a_verified_key_is_stored_read_back_and_never_returned() {
    let fx = fixture(None);
    let before = fx.list().await;
    assert!(!before.keys[0].stored);

    let response = fx.set(&format!("  {GOOD}\n")).await.unwrap();
    assert_eq!(response.check, ServiceKeyCheck::Verified);
    assert!(response.stored, "read back from the store");
    assert_eq!(fx.stored().await.as_deref(), Some(GOOD), "stored trimmed");
    assert_eq!(fx.verifier.changed.load(Ordering::SeqCst), 1, "cache told");
    no_key_in(&response);

    let after = fx.list().await;
    assert!(after.keys[0].stored);
    no_key_in(&after);
}

#[tokio::test]
async fn a_rejected_key_is_never_stored() {
    let fx = fixture(None);
    fx.set(GOOD).await.unwrap();
    let error = fx.set(BAD).await.unwrap_err();
    assert_eq!(error.code, ProductSurfaceErrorCode::InvalidRequest);
    assert_eq!(error.field.as_deref(), Some("value"));
    no_key_in(&error);
    assert_eq!(fx.stored().await.as_deref(), Some(GOOD), "old key kept");
}

#[tokio::test]
async fn an_unreachable_service_stores_the_key_marked_unverified() {
    let fx = fixture(None);
    let response = fx.set(OFFLINE).await.unwrap();
    assert_eq!(response.check, ServiceKeyCheck::Unverified);
    assert!(response.stored);
    assert_eq!(fx.stored().await.as_deref(), Some(OFFLINE));
}

#[tokio::test]
async fn blank_spaced_and_unknown_inputs_are_refused_before_any_call() {
    let fx = fixture(None);
    for value in ["", "   ", "sm a b"] {
        let error = fx.set(value).await.unwrap_err();
        assert_eq!(error.field.as_deref(), Some("value"), "{value:?}");
    }
    let unknown = SERVICE_KEY_SET_COMMAND
        .invoke_on(
            &fx.surface,
            ServiceKeySetRequest {
                id: "llm_provider_openai_api_key".to_string(),
                value: GOOD.to_string(),
            },
            ActivityId::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(unknown.field.as_deref(), Some("id"), "only declared keys");
    assert!(fx.verifier.asked.lock().unwrap().is_empty());
    assert!(fx.stored().await.is_none());
}

#[tokio::test]
async fn import_stores_the_found_copy_and_names_its_source() {
    let fx = fixture(Some(GOOD));
    let listed = fx.list().await;
    assert_eq!(
        listed.keys[0].import_from.as_deref(),
        Some("Hermes (2 profiles)")
    );

    let response = SERVICE_KEY_IMPORT_COMMAND
        .invoke_on(
            &fx.surface,
            ServiceKeyIdRequest {
                id: KEY_ID.to_string(),
            },
            ActivityId::new(),
        )
        .await
        .unwrap();
    assert_eq!(response.check, ServiceKeyCheck::Verified);
    assert_eq!(response.source.as_deref(), Some("Hermes (2 profiles)"));
    assert_eq!(fx.stored().await.as_deref(), Some(GOOD));
    no_key_in(&response);

    // The same copy is stored now: nothing left to offer.
    assert!(fx.list().await.keys[0].import_from.is_none());
}

#[tokio::test]
async fn import_without_a_found_copy_is_not_found_and_a_bad_copy_is_refused() {
    let fx = fixture(None);
    let request = ServiceKeyIdRequest {
        id: KEY_ID.to_string(),
    };
    let missing = SERVICE_KEY_IMPORT_COMMAND
        .invoke_on(&fx.surface, request.clone(), ActivityId::new())
        .await
        .unwrap_err();
    assert_eq!(missing.code, ProductSurfaceErrorCode::NotFound);

    let fx = fixture(Some(BAD));
    let refused = SERVICE_KEY_IMPORT_COMMAND
        .invoke_on(&fx.surface, request, ActivityId::new())
        .await
        .unwrap_err();
    assert_eq!(refused.field.as_deref(), Some("value"));
    assert!(fx.stored().await.is_none());
}

#[tokio::test]
async fn remove_deletes_reads_back_and_tells_the_verifier() {
    let fx = fixture(None);
    fx.set(GOOD).await.unwrap();
    let response = SERVICE_KEY_REMOVE_COMMAND
        .invoke_on(
            &fx.surface,
            ServiceKeyIdRequest {
                id: KEY_ID.to_string(),
            },
            ActivityId::new(),
        )
        .await
        .unwrap();
    assert!(!response.stored);
    assert!(fx.stored().await.is_none());
    assert_eq!(fx.verifier.changed.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn other_operations_pass_through() {
    let fx = fixture(None);
    let error = ironclaw_product_contracts::memory_page::MEMORY_TREE_VIEW
        .query_on(
            &fx.surface,
            ironclaw_product_contracts::memory_page::MemoryTreeRequest::default(),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, ProductSurfaceErrorCode::NotFound);
}
