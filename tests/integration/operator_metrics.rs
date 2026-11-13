//! Operator metrics over whole real turns: scripted turns run through the
//! harness with production's metrics observers built by production's own
//! helpers (the turn-event fan-out, `metered_runtime_event_sink` under the
//! durable event log, `metered_model_provider` outermost on the provider
//! chain, the metered approval resolver), recording into a test registry;
//! then the real operator scrape route is read and the counters asserted.
//! Deleting a production call site of those helpers is caught at the
//! composition tier (`runtime/tests/core.rs`, the operator-metrics pins).
//!
//! A new bin because no existing scenario composes the metrics registry: the
//! run families are folded from committed turn lifecycle events, the
//! model-call and capability families from the durable runtime events, and
//! the LLM families from the provider responses — a crate-tier test of any
//! one observer cannot prove the turn actually reaches it. (Scripted replies
//! report zero tokens, so the token family is pinned at the decorator tier.)

#[allow(dead_code)]
#[path = "support/mod.rs"]
mod reborn_support;
#[allow(dead_code)]
#[path = "../support/mod.rs"]
mod support;

use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use ironclaw_operator::OperatorMetrics;
use ironclaw_product_contracts::inbound::{ApprovalDecision, ProductInboundAck};
use ironclaw_turns::TurnStatus;
use ironclaw_webui::webui_v2::WebUiV2Capabilities;
use ironclaw_webui::{OPERATOR_METRICS_PATH, operator_metrics_route_mount};
use reborn_support::builder::RebornIntegrationHarness;
use reborn_support::group::RebornIntegrationGroup;
use reborn_support::reply::RebornScriptedReply;
use serde_json::json;
use tower::ServiceExt;

const HTTP_TOOL_URL: &str = "https://api.example.test/v1/items";

/// Read the scrape through the real route; the operator capability stands in
/// for the bearer middleware (its 401/403 gate is pinned at the WebUI crate
/// tier, `operator_observability_routes.rs`).
async fn scrape(metrics: &Arc<OperatorMetrics>) -> String {
    let router = operator_metrics_route_mount(Arc::clone(metrics) as _)
        .router
        .layer(axum::Extension(WebUiV2Capabilities {
            operator_webui_config: true,
        }));
    let response = router
        .oneshot(
            Request::builder()
                .uri(OPERATOR_METRICS_PATH)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("scrape responds");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("scrape body");
    String::from_utf8(bytes.to_vec()).expect("utf-8 exposition")
}

fn sample(scrape: &str, series: &str) -> u64 {
    scrape
        .lines()
        .find_map(|line| line.strip_prefix(series)?.strip_prefix(' '))
        .and_then(|value| value.parse().ok())
        .unwrap_or(0)
}

#[test]
fn a_tool_call_turn_moves_the_operator_scrape() {
    // The HTTP tool turn needs the same stack headroom `tool_call.rs` gives it.
    std::thread::Builder::new()
        .name("operator_metrics".to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio test runtime")
                .block_on(a_tool_call_turn_moves_the_operator_scrape_impl());
        })
        .expect("spawn test thread")
        .join()
        .expect("test thread completes");
}

async fn a_tool_call_turn_moves_the_operator_scrape_impl() {
    let metrics = Arc::new(OperatorMetrics::new());
    let before = scrape(&metrics).await;
    assert_eq!(
        sample(&before, "ironclaw_turn_runs_total{outcome=\"completed\"}"),
        0
    );

    let h = RebornIntegrationHarness::test_default()
        .with_durable_capability_io_builtin_http_tools()
        .with_operator_metrics_for_test(Arc::clone(&metrics))
        .script([
            // First attempt fails argument validation (no `url`): a
            // model-visible `invalid_input` failure the model recovers from.
            RebornScriptedReply::tool_call("builtin.http", json!({})),
            RebornScriptedReply::tool_call("builtin.http", json!({"url": HTTP_TOOL_URL})),
            RebornScriptedReply::text("fetched"),
        ])
        .build()
        .await
        .expect("harness builds");
    h.submit_turn("fetch items").await.expect("turn completes");
    h.assert_reply_contains("fetched")
        .await
        .expect("final reply finalized");
    // Runtime events are counted when the write-behind sink's drain commits
    // them, not when producers hand them over: drain before scraping.
    h.flush_durable_events()
        .await
        .expect("durable events flush");

    // Lifecycle sinks are best-effort after the terminal commit; give them a
    // moment rather than racing the status read.
    let mut after = scrape(&metrics).await;
    for _ in 0..50 {
        if sample(&after, "ironclaw_turn_runs_total{outcome=\"completed\"}") > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        after = scrape(&metrics).await;
    }

    assert_eq!(
        sample(&after, "ironclaw_turn_runs_total{outcome=\"completed\"}"),
        1,
        "one completed run:\n{after}"
    );
    assert_eq!(
        sample(
            &after,
            "ironclaw_turn_run_duration_seconds_count{outcome=\"completed\"}"
        ),
        1,
        "the run's duration is folded from its lifecycle transitions:\n{after}"
    );
    assert_eq!(
        sample(
            &after,
            "ironclaw_capability_invocations_total{capability_id=\"builtin.http\",outcome=\"succeeded\"}"
        ),
        1,
        "the tool call is counted by capability id:\n{after}"
    );
    assert_eq!(
        sample(
            &after,
            "ironclaw_capability_invocations_total{capability_id=\"builtin.http\",outcome=\"failed\"}"
        ),
        1,
        "a call that fails argument validation is a failed invocation:\n{after}"
    );
    assert_eq!(
        sample(
            &after,
            "ironclaw_capability_failures_total{capability_id=\"builtin.http\",reason=\"invalid_input\"}"
        ),
        1,
        "...counted under the invalid_input reason:\n{after}"
    );
    assert!(
        sample(&after, "ironclaw_model_calls_total{outcome=\"completed\"}") >= 3,
        "an invalid call, a retried call and the reply are three model calls:\n{after}"
    );
    let llm_requests: u64 = after
        .lines()
        .filter(|line| {
            line.starts_with("ironclaw_llm_requests_total{") && line.contains("outcome=\"success\"")
        })
        .filter_map(|line| line.rsplit(' ').next()?.parse::<u64>().ok())
        .sum();
    assert!(
        llm_requests >= 2,
        "provider requests counted by provider/model:\n{after}"
    );
    assert_eq!(
        sample(&after, "ironclaw_approval_gates_opened_total"),
        0,
        "an ungated tool call opens no approval gate:\n{after}"
    );

    // Bounded labels: no tenant, user, thread, or run identity in the scrape.
    for identity in [
        h.binding.tenant_id.as_str(),
        h.binding.actor_user_id.as_str(),
        h.binding.thread_id.as_str(),
    ] {
        assert!(
            !after.contains(identity),
            "scope identity {identity} leaked into the scrape:\n{after}"
        );
    }
}

/// An approval-gated tool call: the turn blocks on a real gate (opened), the
/// user approves it through the real `ApprovalInteractionService`
/// (production's metered resolver), and the run completes (resolved).
#[tokio::test]
async fn an_approved_gate_moves_the_opened_and_resolved_counters() {
    let metrics = Arc::new(OperatorMetrics::new());
    let group = RebornIntegrationGroup::builder()
        .with_real_gate_dispatch_services()
        .with_operator_metrics_for_test(Arc::clone(&metrics))
        .live_approvals()
        .await
        .expect("live-approvals group builds");
    let h = group
        .thread("conv-operator-metrics-approval")
        .script([
            RebornScriptedReply::tool_call(
                "builtin.write_file",
                json!({"path": "/workspace/metrics_approved.txt", "content": "approved"}),
            ),
            RebornScriptedReply::text("written after approval"),
        ])
        .build()
        .await
        .expect("thread builds");

    let (run_id, gate_ref) = h
        .submit_turn_until_blocked("write the metrics file")
        .await
        .expect("blocks on a real approval gate");
    let mut blocked = scrape(&metrics).await;
    for _ in 0..50 {
        if sample(&blocked, "ironclaw_approval_gates_opened_total") > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        blocked = scrape(&metrics).await;
    }
    assert_eq!(
        sample(&blocked, "ironclaw_approval_gates_opened_total"),
        1,
        "the committed Blocked transition opens one gate:\n{blocked}"
    );
    assert_eq!(
        sample(
            &blocked,
            "ironclaw_approval_gates_resolved_total{resolution=\"approved\"}"
        ),
        0,
        "{blocked}"
    );

    let ack = h
        .submit_approval_resolution(&gate_ref, ApprovalDecision::ApproveOnce)
        .await
        .expect("resolution submits");
    assert!(
        matches!(ack, ProductInboundAck::Accepted { .. }),
        "expected Accepted, got {ack:?}"
    );
    h.wait_for_status(run_id, TurnStatus::Completed)
        .await
        .expect("approved run completes");
    h.assert_workspace_file_contains("metrics_approved.txt", "approved")
        .await
        .expect("the approved write re-dispatched");

    let after = scrape(&metrics).await;
    assert_eq!(
        sample(
            &after,
            "ironclaw_approval_gates_resolved_total{resolution=\"approved\"}"
        ),
        1,
        "the approval resolution is counted:\n{after}"
    );
    assert_eq!(
        sample(&after, "ironclaw_approval_gates_opened_total"),
        1,
        "resuming the run opens no second gate:\n{after}"
    );
    assert_eq!(
        sample(
            &after,
            "ironclaw_approval_gates_resolved_total{resolution=\"denied\"}"
        ),
        0,
        "{after}"
    );
}
