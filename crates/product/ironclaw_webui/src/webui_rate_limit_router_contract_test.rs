//! Caller-level contract tests for PR #6592 review comments about
//! `enforce_rate_limit` ↔ `SseCapacity` refund behavior driven through the
//! real, fully-wired v2 router (real `stream_events`
//! handler, real `SseCapacity`) rather than a synthetic always-429
//! handler.
//!
//! Intentionally NOT under `crates/product/ironclaw_webui/tests/`: every test
//! here builds `RateLimitState` / `RouteLimit` / `ResolvedPolicy` literals
//! and calls `enforce_rate_limit` directly, all `pub(crate)`-only
//! internals of this module. Moving this file to `tests/` (an external,
//! separately-compiled crate) would force widening those internals to
//! `pub` just to serve this suite — the wrong tradeoff for middleware
//! plumbing nothing outside the crate needs to see. It stays a
//! caller-level contract test in an internal sibling module instead,
//! exercised via `cargo test -p ironclaw_webui --lib`.
use super::tests::caller;
use super::*;

use crate::webui_v2::{WebUiV2State, webui_v2_router};
use async_trait::async_trait;
use axum::body::{Body, to_bytes};
use axum::http::Request as HttpRequest;
use axum::middleware;
use ironclaw_product_contracts::surface::{
    ProductSurface, ProductSurfaceError, ProductSurfaceInvokeRequest, ProductSurfaceInvokeResponse,
    ProductSurfaceQueryPage, ProductSurfaceQueryRequest, ProductSurfaceStreamRequest,
    ProductSurfaceStreamResponse,
};
use tower::ServiceExt;

/// Minimal `ProductSurface` fake shared by the tests in this file. Only
/// `stream_events` needs a real body: the SSE capacity slot is reserved
/// synchronously at the top of the `stream_events`
/// handler before the surface is ever touched, so the other operations are
/// unreachable for these tests and panic loudly if that ever changes.
#[derive(Default)]
struct FakeServices;

#[async_trait]
impl ProductSurface for FakeServices {
    async fn invoke(
        &self,
        _caller: ProductSurfaceCaller,
        _request: ProductSurfaceInvokeRequest,
    ) -> Result<ProductSurfaceInvokeResponse, ProductSurfaceError> {
        unreachable!("test does not drive invoke")
    }

    async fn query(
        &self,
        _caller: ProductSurfaceCaller,
        _request: ProductSurfaceQueryRequest,
    ) -> Result<ProductSurfaceQueryPage, ProductSurfaceError> {
        unreachable!("test does not drive query")
    }

    async fn stream_events(
        &self,
        _caller: ProductSurfaceCaller,
        _request: ProductSurfaceStreamRequest,
    ) -> Result<ProductSurfaceStreamResponse, ProductSurfaceError> {
        // Returns instantly with an empty page — no backing projection
        // store is needed because these tests never drain the SSE body,
        // only the handshake status.
        Ok(ProductSurfaceStreamResponse {
            events: Vec::new(),
            next_cursor: None,
            subscription: None,
        })
    }
}

/// Build the real `webui_v2_router` (real handlers, real `SseCapacity`)
/// with `enforce_rate_limit` wired in front of it for the given `routes`,
/// exactly as `webui_serve::webui_v2_app_with_lifecycle` wires production
/// for the routes under test (`enforce_rate_limit` closest to the route
/// set). `sse_capacity_cap` bounds concurrent `SseCapacity` streams per
/// caller. Paths with no entry in `routes` simply fall through unrated —
/// `match_route` no-ops for them, same as an unknown path in production.
fn test_router(sse_capacity_cap: usize, routes: Vec<RouteLimit>) -> axum::Router {
    let shards = (0..SHARD_COUNT)
        .map(|_| Mutex::new(LruCache::new(RATE_LIMIT_PER_SHARD_CAPACITY)))
        .collect::<Vec<_>>();
    let rate_limit_state = RateLimitState {
        routes: Arc::new(routes),
        shards: Arc::new(shards),
        next_generation: Arc::new(AtomicU64::new(0)),
    };

    let services: Arc<dyn ProductSurface> = Arc::new(FakeServices);
    webui_v2_router(WebUiV2State::new(services, sse_capacity_cap)).route_layer(
        middleware::from_fn_with_state(rate_limit_state, enforce_rate_limit),
    )
}

fn stream_events_route(max_requests: u32) -> RouteLimit {
    RouteLimit {
        route_id: "webui_v2.stream_events".into(),
        method: Method::GET,
        segments: parse_pattern("/api/webchat/v2/threads/{thread_id}/events"),
        policy: ResolvedPolicy::Limited {
            scope: RateLimitScope::PerCaller,
            max_requests,
            window: Duration::from_secs(60),
        },
    }
}

/// Caller-level contract test for the PR #6592 review comment "Missing
/// production test for refundable SSE capacity 429s". The two
/// `refund_test_app` tests in `webui_rate_limit_refund_test.rs` front
/// `enforce_rate_limit` with a synthetic handler that always 429s, and the
/// `SseCapacity` cap tests (`webui_v2::handlers` contract suite) inject the
/// caller `Extension` directly, bypassing this middleware entirely. Neither
/// proves that exhausting the SSE per-caller concurrency cap through the
/// real, fully-wired route — `enforce_rate_limit` in front of the real
/// `stream_events` handler and a real `SseCapacity` — actually refunds the
/// caller's rate-limit budget end to end. This test drives that exact
/// combination.
#[tokio::test]
async fn sse_capacity_429_through_real_stream_events_handler_is_refunded() {
    // Rate-limit budget deliberately smaller than the number of
    // SseCapacity rejections fired below: if those refundable 429s were
    // NOT actually refunded, `enforce_rate_limit` itself would start
    // rejecting before this test's final request, which would mask the
    // real assertion.
    let app = test_router(1, vec![stream_events_route(2)]);

    let alice = caller("tenant-alpha", "alice");
    let open_request = || {
        let mut request = HttpRequest::builder()
            .method(Method::GET)
            .uri("/api/webchat/v2/threads/thread-x/events")
            .body(Body::empty())
            .expect("request"); // safety: test-only helper in a #[cfg(test)] sibling module
        request.extensions_mut().insert(alice.clone());
        request
    };

    // First open succeeds and reserves the caller's only SseCapacity
    // slot. Hold the response alive so the slot stays reserved for the
    // rest of the test — the slot guard lives inside the SSE body.
    let held = app.clone().oneshot(open_request()).await.expect("oneshot");
    assert_eq!(held.status(), StatusCode::OK);

    // Fire more capacity-rejected opens than the rate-limit budget (2)
    // allows. Each must be `SseCapacity`'s own refundable 429 — a JSON
    // body `{"error":"rate_limited","kind":"busy",...}` from
    // `sse_capacity_rejected()` — not the middleware's own plain-text
    // "Rate limit exceeded" 429, which would mean the limiter itself
    // short-circuited before the real handler ran.
    for attempt in 0..5 {
        let rejected = app.clone().oneshot(open_request()).await.expect("oneshot");
        assert_eq!(
            rejected.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "attempt {attempt} must hit the real SseCapacity cap"
        );
        let body = to_bytes(rejected.into_body(), usize::MAX)
            .await
            .expect("read rejected body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(
            json["error"], "rate_limited",
            "attempt {attempt} must be SseCapacity's own 429 body, not the rate limiter's"
        );
        assert_eq!(json["kind"], "busy");
    }

    // Release the held slot; the `SseSlot` guard's Drop runs
    // synchronously, but yield once so any pending wakers settle.
    drop(held);
    tokio::task::yield_now().await;

    // A fresh open must succeed. If the five refundable SseCapacity
    // 429s above had actually drained the (max_requests = 2)
    // rate-limit budget, `enforce_rate_limit` would reject this
    // request itself before it ever reached the handler.
    let recovered = app.clone().oneshot(open_request()).await.expect("oneshot");
    assert_eq!(
        recovered.status(),
        StatusCode::OK,
        "refundable SseCapacity 429s through the real router must not have \
             consumed the caller's rate-limit budget"
    );
}

/// Finding C4 (PR #6592 review): the test above only ever fires exactly
/// `sse_capacity::REJECTION_REFUND_LIMIT` (5) rejections, so nothing
/// end-to-end proves what happens *past* that burst — that further
/// capacity rejections genuinely drain `enforce_rate_limit`'s budget and
/// that, once the budget is gone, the caller gets the middleware's own
/// 429 rather than `SseCapacity`'s. This test saturates the cap and fires
/// past the refund burst, asserting all three phases: (1) refundable
/// capacity 429s that leave the budget untouched, (2) non-refundable
/// capacity 429s that drain it, (3) the middleware's own plain-text 429
/// once the budget is gone — proving the burst cutoff is not a free
/// 429 generator forever.
#[tokio::test]
async fn sse_capacity_429_burst_past_refund_limit_drains_budget_to_middleware_429() {
    // max_requests = 3: the initial successful open charges 1 (2 left).
    // The first 5 rejections (within REJECTION_REFUND_LIMIT) are
    // refundable and must not touch that remaining 2. Rejections 6 and 7
    // are past the burst limit and must each charge one unit, exhausting
    // the budget; rejection 8 must then be the middleware's own 429.
    let app = test_router(1, vec![stream_events_route(3)]);

    let alice = caller("tenant-alpha", "alice");
    let open_request = || {
        let mut request = HttpRequest::builder()
            .method(Method::GET)
            .uri("/api/webchat/v2/threads/thread-x/events")
            .body(Body::empty())
            .expect("request"); // safety: test-only helper in a #[cfg(test)] sibling module
        request.extensions_mut().insert(alice.clone());
        request
    };

    let held = app.clone().oneshot(open_request()).await.expect("oneshot");
    assert_eq!(held.status(), StatusCode::OK);

    async fn assert_sse_capacity_json_429(response: Response, attempt: u32) {
        assert_eq!(
            response.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "attempt {attempt} must be a 429"
        );
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read rejected body");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("json body");
        assert_eq!(
            json["error"], "rate_limited",
            "attempt {attempt} must be SseCapacity's own body, not the rate limiter's"
        );
        assert_eq!(json["kind"], "busy");
    }

    // Phase 1: attempts 1-5 are within REJECTION_REFUND_LIMIT — refundable,
    // budget stays at 2.
    for attempt in 1..=5 {
        let rejected = app.clone().oneshot(open_request()).await.expect("oneshot");
        assert_sse_capacity_json_429(rejected, attempt).await;
    }

    // Phase 2: attempts 6-7 are past the burst limit — still SseCapacity's
    // own JSON 429 (the handler still runs and still rejects on capacity),
    // but no longer marked refundable, so each drains one unit of the
    // (now down to 2) rate-limit budget.
    for attempt in 6..=7 {
        let rejected = app.clone().oneshot(open_request()).await.expect("oneshot");
        assert_sse_capacity_json_429(rejected, attempt).await;
    }

    // Phase 3: the budget is now fully spent (2 units drained by phase 2).
    // `enforce_rate_limit` must reject this request itself, before the
    // handler — and therefore SseCapacity — ever runs. That means the
    // exact plain-text body `enforce_rate_limit` returns on its own
    // rejection, not SseCapacity's JSON shape.
    let middleware_rejected = app.clone().oneshot(open_request()).await.expect("oneshot");
    assert_eq!(
        middleware_rejected.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "attempt 8 must still be a 429"
    );
    let retry_after = middleware_rejected
        .headers()
        .get(axum::http::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .expect("429 must expose a numeric retry delay");
    assert!(
        (1..=60).contains(&retry_after),
        "retry delay must be bounded by the configured 60-second window"
    );
    let body = to_bytes(middleware_rejected.into_body(), usize::MAX)
        .await
        .expect("read middleware-rejected body");
    assert_eq!(
        body.as_ref(),
        b"Rate limit exceeded. Try again shortly.",
        "once the budget is exhausted, the response must be enforce_rate_limit's own \
         plain-text 429 body, not SseCapacity's JSON body — proving the handler (and \
         therefore SseCapacity) was never reached for this attempt"
    );

    drop(held);
}
