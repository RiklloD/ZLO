//! Schedule routes forward the body to `orchestrator.schedule_mutate`
//! keeping an absent `signature` (keep the stored one) apart from
//! `"signature": null` (turn signing off) and a value.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use ironclaw_host_api::ids::{TenantId, UserId};
use ironclaw_product_contracts::orchestration::ORCHESTRATOR_SCHEDULE_MUTATE_COMMAND_ID;
use ironclaw_product_contracts::surface::{
    ProductSurface, ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceInvokeRequest,
    ProductSurfaceInvokeResponse, ProductSurfaceQueryPage, ProductSurfaceQueryRequest,
    ProductSurfaceStreamRequest, ProductSurfaceStreamResponse,
};
use ironclaw_webui::webui_v2::{
    DEFAULT_SSE_MAX_CONCURRENT_PER_CALLER, WebUiV2Capabilities, WebUiV2State, webui_v2_router,
};
use serde_json::{Value, json};
use tower::ServiceExt;

#[derive(Default)]
struct Recording {
    invokes: Mutex<Vec<(String, Value)>>,
}

#[async_trait]
impl ProductSurface for Recording {
    async fn invoke(
        &self,
        _: ProductSurfaceCaller,
        request: ProductSurfaceInvokeRequest,
    ) -> Result<ProductSurfaceInvokeResponse, ProductSurfaceError> {
        self.invokes
            .lock()
            .unwrap()
            .push((request.operation_id.as_str().to_string(), request.input));
        Ok(ProductSurfaceInvokeResponse {
            output: json!({"ok": true}),
        })
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

fn router(surface: Arc<Recording>) -> Router {
    let caller = ProductSurfaceCaller::new(
        TenantId::new("t").unwrap(),
        UserId::new("u").unwrap(),
        None,
        None,
    );
    webui_v2_router(WebUiV2State::new(
        surface,
        DEFAULT_SSE_MAX_CONCURRENT_PER_CALLER,
    ))
    .layer(axum::Extension(caller))
    .layer(axum::Extension(WebUiV2Capabilities::default()))
}

async fn update(router: &Router, body: Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/webchat/v2/schedules/s1")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
}

#[tokio::test]
async fn signed_to_unsigned_update_forwards_an_explicit_null() {
    let surface = Arc::new(Recording::default());
    let router = router(Arc::clone(&surface));

    update(&router, json!({"title": "t", "signature": null})).await;
    update(&router, json!({"title": "t"})).await;
    update(
        &router,
        json!({"signature": {"header": "x-zlo-signature", "generate": true}}),
    )
    .await;

    let calls = surface.invokes.lock().unwrap().clone();
    assert_eq!(calls.len(), 3);
    assert!(
        calls
            .iter()
            .all(|(op, _)| op == ORCHESTRATOR_SCHEDULE_MUTATE_COMMAND_ID)
    );
    let off = calls[0].1.as_object().unwrap();
    assert_eq!(off.get("signature"), Some(&Value::Null), "{off:?}");
    let kept = calls[1].1.as_object().unwrap();
    assert!(!kept.contains_key("signature"), "{kept:?}");
    assert_eq!(calls[2].1["signature"]["generate"], true);
}
