use std::sync::Arc;
use std::time::Duration;

use ironclaw_approvals::{ApprovalRequestStorePort, in_memory_backed_approval_request_store};
use ironclaw_event_log::{
    EventError, EventSink, InMemoryEventSink, NonBlockingEventSink, RuntimeEvent,
};
use ironclaw_host_api::action::Action;
use ironclaw_host_api::approval::ApprovalRequest;
use ironclaw_host_api::ids::{
    ApprovalRequestId, CapabilityId, CorrelationId, ExtensionId, InvocationId, TenantId, ThreadId,
    UserId,
};
use ironclaw_host_api::resource::{ResourceEstimate, ResourceScope};
use ironclaw_host_api::runtime::RuntimeKind;
use ironclaw_host_api::scope::Principal;
use ironclaw_llm::testing::StubLlm;
use ironclaw_llm::{ChatMessage, CompletionRequest, LlmProvider};

use super::*;

const SECRET_USER: &str = "user-must-not-leak";
const SECRET_THREAD: &str = "thread-must-not-leak";

fn scope() -> ResourceScope {
    ResourceScope {
        tenant_id: TenantId::new("tenant-must-not-leak").expect("tenant"),
        user_id: UserId::new(SECRET_USER).expect("user"),
        agent_id: None,
        project_id: None,
        mission_id: None,
        thread_id: Some(ThreadId::new(SECRET_THREAD).expect("thread")),
        invocation_id: InvocationId::new(),
    }
}

fn capability(id: &str) -> CapabilityId {
    CapabilityId::new(id).expect("capability id")
}

fn succeeded(id: &str) -> RuntimeEvent {
    RuntimeEvent::capability_activity_succeeded(
        scope(),
        capability(id),
        ExtensionId::new("builtin").expect("extension"),
        RuntimeKind::FirstParty,
        12,
    )
}

fn line_value(rendered: &str, series: &str) -> Option<String> {
    rendered
        .lines()
        .find(|line| line.starts_with(series) && line[series.len()..].starts_with(' '))
        .map(|line| line[series.len() + 1..].to_string())
}

#[test]
fn empty_registry_renders_every_family_header_and_a_zero_gate_counter() {
    let rendered = OperatorMetrics::new().render_prometheus();
    for family in [
        "ironclaw_turn_runs_total",
        "ironclaw_turn_run_duration_seconds",
        "ironclaw_model_calls_total",
        "ironclaw_capability_invocations_total",
        "ironclaw_llm_requests_total",
        "ironclaw_llm_tokens_total",
        "ironclaw_approval_gates_resolved_total",
    ] {
        assert!(
            rendered.contains(&format!("# TYPE {family} ")),
            "{family} missing from:\n{rendered}"
        );
    }
    assert_eq!(
        line_value(&rendered, "ironclaw_approval_gates_opened_total").as_deref(),
        Some("0")
    );
}

#[test]
fn turn_runs_fold_by_outcome_with_a_duration_histogram() {
    let metrics = OperatorMetrics::new();
    metrics.record_turn_run(
        TurnRunOutcome::Completed,
        Some(Duration::from_millis(1_500)),
    );
    metrics.record_turn_run(TurnRunOutcome::Failed, None);
    metrics.record_approval_gate_opened();

    let rendered = metrics.render_prometheus();
    assert_eq!(
        line_value(&rendered, "ironclaw_turn_runs_total{outcome=\"completed\"}").as_deref(),
        Some("1")
    );
    assert_eq!(
        line_value(&rendered, "ironclaw_turn_runs_total{outcome=\"failed\"}").as_deref(),
        Some("1")
    );
    // Only the completed run carried a duration; 1.5 s lands in the 2.5 s bucket.
    assert_eq!(
        line_value(
            &rendered,
            "ironclaw_turn_run_duration_seconds_bucket{outcome=\"completed\",le=\"1\"}"
        )
        .as_deref(),
        Some("0")
    );
    assert_eq!(
        line_value(
            &rendered,
            "ironclaw_turn_run_duration_seconds_bucket{outcome=\"completed\",le=\"2.5\"}"
        )
        .as_deref(),
        Some("1")
    );
    assert!(
        !rendered.contains("ironclaw_turn_run_duration_seconds_count{outcome=\"failed\"}"),
        "a run without an observed start contributes no duration sample"
    );
    assert_eq!(
        line_value(&rendered, "ironclaw_approval_gates_opened_total").as_deref(),
        Some("1")
    );
}

#[test]
fn runtime_events_fold_into_models_and_capabilities_without_scope_ids() {
    let metrics = OperatorMetrics::new();
    metrics.record_runtime_event(&RuntimeEvent::model_completed_with_duration(
        scope(),
        capability("model.call"),
        200,
    ));
    metrics.record_runtime_event(&succeeded("builtin.http"));
    metrics.record_runtime_event(&RuntimeEvent::capability_activity_failed(
        scope(),
        capability("builtin.http"),
        None,
        None,
        "operation_failed",
    ));
    // Events the registry does not fold — loop terminals included, since runs
    // come from the turn kernel — are ignored, not mis-counted.
    metrics.record_runtime_event(&RuntimeEvent::loop_completed(
        scope(),
        capability("loop.run"),
    ));
    metrics.record_runtime_event(&RuntimeEvent::assistant_reply_finalized(
        scope(),
        capability("assistant.reply"),
    ));

    let rendered = metrics.render_prometheus();
    assert_eq!(
        line_value(
            &rendered,
            "ironclaw_model_calls_total{outcome=\"completed\"}"
        )
        .as_deref(),
        Some("1")
    );
    assert_eq!(
        line_value(
            &rendered,
            "ironclaw_model_call_duration_seconds_bucket{outcome=\"completed\",le=\"0.25\"}"
        )
        .as_deref(),
        Some("1")
    );
    assert!(
        !rendered.contains("ironclaw_turn_runs_total{"),
        "loop-terminal runtime events are not a run source:\n{rendered}"
    );
    assert_eq!(
        line_value(
            &rendered,
            "ironclaw_capability_invocations_total{capability_id=\"builtin.http\",outcome=\"succeeded\"}"
        )
        .as_deref(),
        Some("1")
    );
    assert_eq!(
        line_value(
            &rendered,
            "ironclaw_capability_failures_total{capability_id=\"builtin.http\",reason=\"operation_failed\"}"
        )
        .as_deref(),
        Some("1")
    );
    for leaked in [SECRET_USER, SECRET_THREAD, "tenant-must-not-leak"] {
        assert!(
            !rendered.contains(leaked),
            "{leaked} leaked into:\n{rendered}"
        );
    }
}

#[test]
fn capability_series_are_bounded_and_overflow_is_counted() {
    let metrics = OperatorMetrics::new();
    for index in 0..(MAX_CAPABILITY_SERIES + 10) {
        metrics.record_runtime_event(&succeeded(&format!("ext.tool_{index}")));
    }
    let rendered = metrics.render_prometheus();
    let series = rendered
        .lines()
        .filter(|line| line.starts_with("ironclaw_capability_invocations_total{"))
        .count();
    assert_eq!(
        series,
        MAX_CAPABILITY_SERIES + 1,
        "ceiling plus one overflow series"
    );
    assert_eq!(
        line_value(
            &rendered,
            "ironclaw_capability_invocations_total{capability_id=\"__other__\",outcome=\"__other__\"}"
        )
        .as_deref(),
        Some("10")
    );
    assert_eq!(
        line_value(
            &rendered,
            "ironclaw_metrics_series_overflow_total{family=\"capability_invocations\"}"
        )
        .as_deref(),
        Some("10")
    );
}

#[test]
fn label_values_cannot_carry_exposition_syntax() {
    assert_eq!(label_value("a\"b\nc{d}"), "a_b_c_d_");
    assert_eq!(label_value("   "), "unknown");
    assert_eq!(
        label_value(&"é".repeat(200)).chars().count(),
        MAX_LABEL_VALUE_CHARS
    );
}

/// A sink whose accept fails, to prove a rejected event is never counted.
struct RejectingSink;

#[async_trait::async_trait]
impl EventSink for RejectingSink {
    async fn emit(&self, _event: RuntimeEvent) -> Result<(), EventError> {
        Err(EventError::Sink {
            reason: "full".to_string(),
        })
    }
}

impl NonBlockingEventSink for RejectingSink {
    fn try_emit(&self, _event: RuntimeEvent) -> Result<(), EventError> {
        Err(EventError::Sink {
            reason: "full".to_string(),
        })
    }
}

#[tokio::test]
async fn event_sink_decorator_forwards_then_counts_only_accepted_events() {
    let metrics = Arc::new(OperatorMetrics::new());
    let inner = Arc::new(InMemoryEventSink::new());
    let sink = MetricsEventSink::new(inner.clone(), Arc::clone(&metrics));
    sink.try_emit(succeeded("builtin.http")).expect("accepted");
    sink.emit(succeeded("builtin.http"))
        .await
        .expect("accepted");
    assert_eq!(
        inner.events().len(),
        2,
        "every event reaches the durable sink"
    );

    let rejecting = MetricsEventSink::new(Arc::new(RejectingSink), Arc::clone(&metrics));
    assert!(rejecting.try_emit(succeeded("builtin.http")).is_err());
    assert!(rejecting.emit(succeeded("builtin.http")).await.is_err());

    assert_eq!(
        line_value(
            &metrics.render_prometheus(),
            "ironclaw_capability_invocations_total{capability_id=\"builtin.http\",outcome=\"succeeded\"}"
        )
        .as_deref(),
        Some("2"),
        "rejected events are not counted"
    );
}

fn approval_request() -> ApprovalRequest {
    ApprovalRequest {
        id: ApprovalRequestId::new(),
        correlation_id: CorrelationId::new(),
        requested_by: Principal::Extension(ExtensionId::new("caller").expect("extension")),
        action: Box::new(Action::Dispatch {
            capability: capability("echo.say"),
            estimated_resources: ResourceEstimate::default(),
        }),
        invocation_fingerprint: None,
        reason: "metrics test".to_string(),
        reusable_scope: None,
    }
}

#[tokio::test]
async fn approval_store_decorator_counts_committed_resolutions_only() {
    let metrics = Arc::new(OperatorMetrics::new());
    let store = MetricsApprovalRequestStore::new(
        Arc::new(in_memory_backed_approval_request_store()),
        Arc::clone(&metrics),
    );
    let scope = scope();
    let approved = store
        .save_pending(scope.clone(), approval_request())
        .await
        .expect("save");
    let denied = store
        .save_pending(scope.clone(), approval_request())
        .await
        .expect("save");
    store
        .approve(&scope, approved.request.id)
        .await
        .expect("approve");
    store.deny(&scope, denied.request.id).await.expect("deny");
    // A second resolution of a decided request fails in the store and must
    // not count as another resolution.
    assert!(store.deny(&scope, denied.request.id).await.is_err());

    let rendered = metrics.render_prometheus();
    assert_eq!(
        line_value(&rendered, "ironclaw_approval_gates_opened_total").as_deref(),
        Some("0"),
        "opened is counted from the turn Blocked transition, not the store"
    );
    assert_eq!(
        line_value(
            &rendered,
            "ironclaw_approval_gates_resolved_total{resolution=\"approved\"}"
        )
        .as_deref(),
        Some("1")
    );
    assert_eq!(
        line_value(
            &rendered,
            "ironclaw_approval_gates_resolved_total{resolution=\"denied\"}"
        )
        .as_deref(),
        Some("1")
    );
}

#[tokio::test]
async fn llm_decorator_records_provider_model_tokens_and_error_kind() {
    let metrics = Arc::new(OperatorMetrics::new());
    let ok = MetricsLlmProvider::new(
        Arc::new(StubLlm::new("hello").with_model_name("stub/ok-model")),
        Arc::clone(&metrics),
    );
    ok.complete(CompletionRequest::new(vec![ChatMessage::user("hi")]))
        .await
        .expect("stub completes");
    let failing = MetricsLlmProvider::new(
        Arc::new(StubLlm::failing("stub/failing-model")),
        Arc::clone(&metrics),
    );
    assert!(
        failing
            .complete(CompletionRequest::new(vec![ChatMessage::user("hi")]))
            .await
            .is_err()
    );

    let rendered = metrics.render_prometheus();
    let ok_provider = ok.provider_id();
    assert_eq!(
        line_value(
            &rendered,
            &format!(
                "ironclaw_llm_requests_total{{provider=\"{}\",model=\"stub/ok-model\",outcome=\"success\"}}",
                label_value(&ok_provider)
            )
        )
        .as_deref(),
        Some("1"),
        "{rendered}"
    );
    assert!(
        rendered
            .lines()
            .any(|line| line.starts_with("ironclaw_llm_tokens_total{")
                && line.contains("model=\"stub/ok-model\"")),
        "token usage recorded:\n{rendered}"
    );
    assert!(
        rendered
            .lines()
            .any(|line| line.starts_with("ironclaw_llm_errors_total{")
                && line.contains("model=\"stub/failing-model\"")),
        "error recorded with its kind:\n{rendered}"
    );
    assert_eq!(
        line_value(&rendered, "ironclaw_llm_request_duration_seconds_count").as_deref(),
        Some("2")
    );
}
