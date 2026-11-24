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
//! one observer cannot prove the turn actually reaches it. Scripted replies
//! report zero tokens unless they declare usage (`with_usage`); the token
//! scenarios here declare it so the token family moves by exact amounts.

use crate::reborn_support;

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
use reborn_support::reply::{RebornScriptedReply, ScriptedTokenUsage};
use reborn_support::scripted_provider::{
    SCRIPTED_FALLBACK_MODEL_NAME, SCRIPTED_MODEL_NAME, scripted_trace_llm,
};
use serde_json::json;
use tower::ServiceExt;

const HTTP_TOOL_URL: &str = "https://api.example.test/v1/items";
/// The vendor id the scripted provider reports in the token scenarios, so
/// the provider label is the provider's own, not the `unknown` fold.
const SCRIPTED_VENDOR: &str = "scripted-vendor";

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

fn tokens(scrape: &str, provider: &str, model: &str, kind: &str) -> u64 {
    sample(
        scrape,
        &format!(
            "ironclaw_llm_tokens_total{{provider=\"{provider}\",model=\"{model}\",kind=\"{kind}\"}}"
        ),
    )
}

/// Every `ironclaw_llm_tokens_total` sample line, for "nothing else moved"
/// assertions.
fn token_lines(scrape: &str) -> Vec<&str> {
    scrape
        .lines()
        .filter(|line| line.starts_with("ironclaw_llm_tokens_total{"))
        .collect()
}

fn total_tokens(scrape: &str) -> u64 {
    token_lines(scrape)
        .iter()
        .filter_map(|line| line.rsplit(' ').next()?.parse::<u64>().ok())
        .sum()
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
        .with_scripted_provider_id(SCRIPTED_VENDOR)
        .script([
            // First attempt fails argument validation (no `url`): a
            // model-visible `invalid_input` failure the model recovers from.
            RebornScriptedReply::tool_call("builtin.http", json!({}))
                .with_usage(ScriptedTokenUsage::new(100, 20)),
            RebornScriptedReply::tool_call("builtin.http", json!({"url": HTTP_TOOL_URL}))
                .with_usage(ScriptedTokenUsage::new(140, 22).with_cache(90, 0)),
            RebornScriptedReply::text("fetched")
                .with_usage(ScriptedTokenUsage::new(180, 9).with_cache(120, 0)),
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
    // Every model call of the turn reports its own usage, and the token
    // family is their exact sum under the serving provider/model — each
    // call counted once, the cache kinds as reported.
    assert_eq!(
        sample(
            &after,
            &format!(
                "ironclaw_llm_requests_total{{provider=\"{SCRIPTED_VENDOR}\",model=\"{SCRIPTED_MODEL_NAME}\",outcome=\"success\"}}"
            )
        ),
        3,
        "three provider requests:\n{after}"
    );
    for (kind, expected) in [("input", 420), ("output", 51), ("cache_read", 210)] {
        assert_eq!(
            tokens(&after, SCRIPTED_VENDOR, SCRIPTED_MODEL_NAME, kind),
            expected,
            "{kind} tokens are the sum over the turn's three model calls:\n{after}"
        );
    }
    assert_eq!(
        token_lines(&after).len(),
        3,
        "no cache_write series when no call reported one, and no other provider/model:\n{after}"
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

/// A plain chat turn whose one model call declares usage moves each token
/// kind by exactly that amount, labelled with the provider that served it and
/// the model it served — and moves nothing else in the token family.
#[tokio::test]
async fn a_chat_turn_moves_the_token_counters_by_its_declared_usage() {
    let metrics = Arc::new(OperatorMetrics::new());
    let h = RebornIntegrationHarness::test_default()
        .with_operator_metrics_for_test(Arc::clone(&metrics))
        .with_scripted_provider_id(SCRIPTED_VENDOR)
        .script([RebornScriptedReply::text("hello back")
            .with_usage(ScriptedTokenUsage::new(120, 30).with_cache(80, 10))])
        .build()
        .await
        .expect("harness builds");
    h.submit_turn("hello").await.expect("turn completes");
    h.assert_reply_contains("hello back")
        .await
        .expect("reply finalized");

    let after = scrape(&metrics).await;
    for (kind, expected) in [
        ("input", 120),
        ("output", 30),
        ("cache_read", 80),
        ("cache_write", 10),
    ] {
        assert_eq!(
            tokens(&after, SCRIPTED_VENDOR, SCRIPTED_MODEL_NAME, kind),
            expected,
            "{kind} tokens:\n{after}"
        );
    }
    assert_eq!(total_tokens(&after), 240, "nothing else counted:\n{after}");
}

/// Usage rides only on a provider *response*: `LlmError` has no usage field,
/// so a call that fails at the provider counts an error and zero tokens. A
/// response the loop rejects (here: truncated at the length limit, then
/// recovered) was still served and billed, so its reported usage counts — the
/// metric is provider-reported spend, not useful output.
#[tokio::test]
async fn a_rejected_response_counts_its_usage_and_a_provider_error_counts_none() {
    // Provider error: the request fails before any response exists.
    let metrics = Arc::new(OperatorMetrics::new());
    let failing = RebornIntegrationHarness::test_default()
        .with_operator_metrics_for_test(Arc::clone(&metrics))
        .fail_model()
        .build()
        .await
        .expect("harness builds");
    let run_id = failing
        .submit_turn_async("do something")
        .await
        .expect("turn submitted");
    failing
        .wait_for_status(run_id, TurnStatus::Failed)
        .await
        .expect("a non-retryable provider error fails the run");
    let after_error = scrape(&metrics).await;
    assert_eq!(
        sample(
            &after_error,
            "ironclaw_llm_errors_total{provider=\"unknown\",model=\"unknown\",kind=\"context_length_exceeded\"}"
        ),
        2,
        "both failed requests (the call and the loop's one recovery attempt) are counted, on model=\"unknown\" because that model was never served:\n{after_error}"
    );
    assert!(
        token_lines(&after_error).is_empty(),
        "a provider error reports no usage, so no token series exists:\n{after_error}"
    );
    assert!(
        !after_error.contains(SCRIPTED_MODEL_NAME),
        "a failure mints no series for the model it asked for:\n{after_error}"
    );

    // Rejected response: served with usage (11 in / 7 out, finish=length),
    // discarded by the loop, then a recovered reply with its own usage.
    let metrics = Arc::new(OperatorMetrics::new());
    let truncated = RebornIntegrationHarness::test_default()
        .with_operator_metrics_for_test(Arc::clone(&metrics))
        .with_scripted_provider_id(SCRIPTED_VENDOR)
        .output_truncated_model_times(1)
        .script([
            RebornScriptedReply::text("complete answer after truncation")
                .with_usage(ScriptedTokenUsage::new(40, 5)),
        ])
        .build()
        .await
        .expect("harness builds");
    truncated
        .submit_turn("give a complete answer")
        .await
        .expect("run survives a truncated provider response");
    truncated
        .assert_reply_contains("complete answer after truncation")
        .await
        .expect("recovered reply finalized");
    let after = scrape(&metrics).await;
    assert_eq!(
        tokens(&after, SCRIPTED_VENDOR, SCRIPTED_MODEL_NAME, "input"),
        51,
        "the truncated response's 11 input tokens count with the recovery's 40:\n{after}"
    );
    assert_eq!(
        tokens(&after, SCRIPTED_VENDOR, SCRIPTED_MODEL_NAME, "output"),
        12,
        "the truncated response's 7 output tokens count with the recovery's 5:\n{after}"
    );
    assert_eq!(total_tokens(&after), 63, "{after}");
}

/// Failover: the primary is unavailable, the turn is served from the
/// configured fallback. The fallback's usage is counted exactly once, under
/// the fallback model that served it; the primary's failure adds no tokens.
#[tokio::test]
async fn failover_counts_the_serving_fallbacks_usage_once() {
    let metrics = Arc::new(OperatorMetrics::new());
    let h = RebornIntegrationHarness::test_default()
        .with_operator_metrics_for_test(Arc::clone(&metrics))
        .with_scripted_provider_id(SCRIPTED_VENDOR)
        .advance_fallback_after_unavailable()
        .script([RebornScriptedReply::text("fallback recovered the turn")
            .with_usage(ScriptedTokenUsage::new(60, 9).with_cache(30, 0))])
        .build()
        .await
        .expect("harness builds");
    h.submit_turn("recover through the configured provider fallback")
        .await
        .expect("turn completes through the fallback");
    h.assert_ordered_fallback_vendor_calls()
        .await
        .expect("primary is called once and the fallback serves");

    let after = scrape(&metrics).await;
    let fallback_tokens: Vec<&str> = token_lines(&after);
    assert_eq!(
        total_tokens(&after),
        99,
        "the fallback's usage is counted once:\n{after}"
    );
    assert!(
        fallback_tokens.iter().all(|line| line.contains(&format!(
            "provider=\"failover\",model=\"{SCRIPTED_FALLBACK_MODEL_NAME}\""
        ))),
        "every token sample is attributed to the fallback model that served it:\n{after}"
    );
    for (kind, expected) in [("input", 60), ("output", 9), ("cache_read", 30)] {
        assert_eq!(
            fallback_tokens
                .iter()
                .filter(|line| line.contains(&format!("kind=\"{kind}\"")))
                .filter_map(|line| line.rsplit(' ').next()?.parse::<u64>().ok())
                .sum::<u64>(),
            expected,
            "{kind}:\n{after}"
        );
    }
}

/// The series ceiling holds for tokens too: once other traffic has filled the
/// LLM families, a new provider/model's token samples fold into
/// `provider/model="__other__"` with the closed `kind` label kept, and each
/// folded sample is counted in the overflow family. The folded series still
/// carries the turn's exact token counts.
#[tokio::test]
async fn token_series_fold_into_other_past_the_series_ceiling() {
    const PREFILLED_MODELS: usize = 128;
    let metrics = Arc::new(OperatorMetrics::new());

    // Other traffic: 128 served provider/model pairs, each reporting all four
    // token kinds — the request family's 128-series and the token family's
    // 512-series ceilings, both exactly full. Recorded through the production
    // decorator over the same scripted vendor seam.
    let prefill_vendor = Arc::new(
        scripted_trace_llm((0..PREFILLED_MODELS).map(|_| {
            RebornScriptedReply::text("prefill")
                .with_usage(ScriptedTokenUsage::new(1, 1).with_cache(1, 1))
        }))
        .with_provider_id("prefill-vendor"),
    );
    let prefill = ironclaw_operator::MetricsLlmProvider::new(prefill_vendor, Arc::clone(&metrics));
    for index in 0..PREFILLED_MODELS {
        ironclaw_llm::LlmProvider::complete(
            &prefill,
            ironclaw_llm::CompletionRequest::new(vec![ironclaw_llm::ChatMessage::user("fill")])
                .with_model(format!("prefill-model-{index}")),
        )
        .await
        .expect("prefill call is served");
    }
    let saturated = scrape(&metrics).await;
    assert_eq!(
        token_lines(&saturated).len(),
        PREFILLED_MODELS * 4,
        "{saturated}"
    );
    assert!(
        !saturated.contains("=\"__other__\""),
        "nothing folded yet:\n{saturated}"
    );

    let h = RebornIntegrationHarness::test_default()
        .with_operator_metrics_for_test(Arc::clone(&metrics))
        .with_scripted_provider_id(SCRIPTED_VENDOR)
        .script([RebornScriptedReply::text("folded")
            .with_usage(ScriptedTokenUsage::new(70, 8).with_cache(50, 4))])
        .build()
        .await
        .expect("harness builds");
    h.submit_turn("hello").await.expect("turn completes");

    let after = scrape(&metrics).await;
    for (kind, expected) in [
        ("input", 70),
        ("output", 8),
        ("cache_read", 50),
        ("cache_write", 4),
    ] {
        assert_eq!(
            tokens(&after, "__other__", "__other__", kind),
            expected,
            "the turn's {kind} tokens fold with their kind kept:\n{after}"
        );
    }
    assert!(
        !after.contains(SCRIPTED_VENDOR) && !after.contains(SCRIPTED_MODEL_NAME),
        "the new provider/model mints no series past the ceiling:\n{after}"
    );
    assert_eq!(
        token_lines(&after).len(),
        PREFILLED_MODELS * 4 + 4,
        "only the four __other__ kind series were added:\n{after}"
    );
    assert_eq!(
        sample(
            &after,
            "ironclaw_metrics_series_overflow_total{family=\"llm_tokens\"}"
        ),
        4,
        "one folded sample per reported token kind:\n{after}"
    );
    assert_eq!(
        sample(
            &after,
            "ironclaw_metrics_series_overflow_total{family=\"llm_requests\"}"
        ),
        1,
        "the request itself folded too:\n{after}"
    );
}

/// `LLM_RESPONSE_CACHE_ENABLED=true`: two identical non-streaming model calls
/// through the production provider chain, the production metering decorator
/// outermost, and the production model gateway. The provider serves the first;
/// the response cache replays the second. Tokens are counted once, the replay
/// is counted as one cache hit, and the replayed usage reaches the loop with
/// its full counts (context accounting needs them) marked as a cache replay —
/// the provenance the budget accountant reads to charge nothing for it. The
/// budget half is pinned where production sends non-streaming calls — the
/// guarded system-inference port with the production accountant, in
/// `ironclaw_loop_host`'s
/// `llm_gateway.rs::response_cache_replay_is_budgeted_once_and_keeps_its_context_counts`
/// (the interactive loop streams, and the cache never serves a streaming call).
#[tokio::test]
async fn a_response_cache_hit_is_counted_as_a_hit_not_as_tokens() {
    use ironclaw_host_api::resource::ModelUsageSource;
    use ironclaw_loop_contracts::ModelProfileId;
    use ironclaw_loop_host::{
        HostManagedModelGateway, HostManagedModelMessage, HostManagedModelMessageRole,
        HostManagedModelRequest, LlmModelProfilePolicy, LlmProviderModelGateway,
    };
    use ironclaw_turns::{LoopMessageRef, TurnId, TurnRunId};

    let metrics = Arc::new(OperatorMetrics::new());
    let session_root = tempfile::tempdir().expect("session dir");
    let session = ironclaw_llm::create_session_manager(ironclaw_llm::SessionConfig {
        session_path: session_root.path().join("cache.session.json"),
        ..ironclaw_llm::SessionConfig::default()
    })
    .await;
    let mut config = ironclaw_llm::testing::nearai_test_config(SCRIPTED_MODEL_NAME);
    config.response_cache_enabled = true;
    // A second scripted step only a cache miss could consume.
    let vendor = Arc::new(
        scripted_trace_llm([
            RebornScriptedReply::text("cached answer")
                .with_usage(ScriptedTokenUsage::new(90, 12).with_cache(40, 0)),
            RebornScriptedReply::text("a second provider call")
                .with_usage(ScriptedTokenUsage::new(1, 1)),
        ])
        .with_provider_id(SCRIPTED_VENDOR),
    );
    let chain = ironclaw_llm::testing::provider_chain_over(
        Arc::clone(&vendor) as Arc<dyn ironclaw_llm::LlmProvider>,
        &config,
        session,
    )
    .await
    .expect("production provider chain builds");
    let metered = ironclaw_composition::test_support::operator_metered_model_provider_for_test(
        chain,
        Arc::clone(&metrics),
    );
    let profile = ModelProfileId::new("interactive_model").expect("profile id");
    let gateway = LlmProviderModelGateway::new(
        metered,
        LlmModelProfilePolicy::new().allow_model_profile(profile.clone(), None),
    );
    let message = |role, content: &str, reference: &str| HostManagedModelMessage {
        role,
        content: content.to_string(),
        content_ref: LoopMessageRef::new(reference).expect("message ref"),
        tool_result_provider_call: None,
        tool_result_content: None,
        image_parts: Vec::new(),
    };
    let request = HostManagedModelRequest {
        model_profile_id: profile,
        messages: vec![
            message(
                HostManagedModelMessageRole::System,
                "system instructions",
                "msg:11111111-1111-1111-1111-111111111111",
            ),
            message(
                HostManagedModelMessageRole::User,
                "the same question twice",
                "msg:22222222-2222-2222-2222-222222222222",
            ),
        ],
        surface_version: None,
        fallback_index: 0,
        resolved_model_route: None,
        run_id: TurnRunId::new(),
        turn_id: TurnId::new(),
        thread_id: None,
        tool_choice: None,
        response_format: None,
    };

    let served = gateway
        .stream_model(request.clone())
        .await
        .expect("provider serves the first call");
    let replayed = gateway
        .stream_model(request)
        .await
        .expect("the cache replays the second call");
    assert_eq!(
        vendor.calls(),
        1,
        "the second call never reached the provider"
    );
    let served_usage = served.usage.expect("served usage");
    let replayed_usage = replayed.usage.expect("replayed usage");
    assert_eq!(served_usage.usage_source, ModelUsageSource::Provider);
    assert_eq!(
        replayed_usage.usage_source,
        ModelUsageSource::ResponseCache,
        "the loop is told no provider billed the replay"
    );
    assert_eq!(
        (
            replayed_usage.input_tokens,
            replayed_usage.output_tokens,
            replayed_usage.cache_read_input_tokens
        ),
        (90, 12, 40),
        "the replay keeps its counts for context accounting"
    );

    let after = scrape(&metrics).await;
    for (kind, expected) in [("input", 90), ("output", 12), ("cache_read", 40)] {
        assert_eq!(
            tokens(&after, SCRIPTED_VENDOR, SCRIPTED_MODEL_NAME, kind),
            expected,
            "{kind} tokens are counted once, for the provider call:\n{after}"
        );
    }
    assert_eq!(total_tokens(&after), 142, "{after}");
    assert_eq!(
        sample(
            &after,
            &format!(
                "ironclaw_llm_response_cache_hits_total{{provider=\"{SCRIPTED_VENDOR}\",model=\"{SCRIPTED_MODEL_NAME}\"}}"
            )
        ),
        1,
        "one cache hit:\n{after}"
    );
    assert_eq!(
        sample(
            &after,
            &format!(
                "ironclaw_llm_requests_total{{provider=\"{SCRIPTED_VENDOR}\",model=\"{SCRIPTED_MODEL_NAME}\",outcome=\"success\"}}"
            )
        ),
        2,
        "both requests were answered:\n{after}"
    );
}
