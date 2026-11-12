use std::sync::Arc;
use std::time::Duration;

use ironclaw_approvals::{ApprovalRequestStorePort, in_memory_backed_approval_request_store};
use ironclaw_event_log::{
    DurableEventLog, EventCursor, EventError, EventLogEntry, EventReplay, EventStreamKey,
    InMemoryDurableEventLog, ReadScope, RuntimeEvent,
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
use ironclaw_llm::provider_routing::{ProviderRouter, RoutedProviderSource};
use ironclaw_llm::testing::StubLlm;
use ironclaw_llm::{ChatMessage, CompletionRequest, FailoverProvider, LlmError, LlmProvider};

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
            "ironclaw_capability_invocations_total{capability_id=\"__other__\",outcome=\"succeeded\"}"
        )
        .as_deref(),
        Some("10"),
        "past the ceiling only the open capability label folds; the closed outcome is kept"
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
fn overflow_keeps_closed_outcome_and_kind_labels() {
    let metrics = OperatorMetrics::new();
    for index in 0..MAX_CAPABILITY_SERIES {
        metrics.record_runtime_event(&succeeded(&format!("ext.tool_{index}")));
    }
    metrics.record_runtime_event(&RuntimeEvent::capability_activity_failed(
        scope(),
        capability("ext.late_tool"),
        None,
        None,
        "operation_failed",
    ));
    let configured = |model: &'static str| LlmRequestLabels {
        provider: "configured",
        provider_configured: true,
        model,
    };
    for index in 0..MAX_LLM_SERIES {
        let model = format!("model-{index}");
        metrics.record_llm_request(
            LlmRequestLabels {
                provider: "configured",
                provider_configured: true,
                model: &model,
            },
            Ok(LlmTokenUsage::default()),
            Duration::from_millis(5),
        );
    }
    metrics.record_llm_request(
        configured("late-model"),
        Ok(LlmTokenUsage {
            input: 3,
            ..LlmTokenUsage::default()
        }),
        Duration::from_millis(5),
    );

    let rendered = metrics.render_prometheus();
    assert_eq!(
        line_value(
            &rendered,
            "ironclaw_capability_invocations_total{capability_id=\"__other__\",outcome=\"failed\"}"
        )
        .as_deref(),
        Some("1"),
        "{rendered}"
    );
    assert_eq!(
        line_value(
            &rendered,
            "ironclaw_llm_requests_total{provider=\"__other__\",model=\"__other__\",outcome=\"success\"}"
        )
        .as_deref(),
        Some("1"),
        "{rendered}"
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

/// A durable log that rejects the rows of one capability, as a backend
/// rejecting part of a batch does.
struct PartlyRejectingLog {
    inner: InMemoryDurableEventLog,
    rejected_capability: &'static str,
}

#[async_trait::async_trait]
impl DurableEventLog for PartlyRejectingLog {
    async fn append(&self, event: RuntimeEvent) -> Result<EventLogEntry<RuntimeEvent>, EventError> {
        if event.capability_id.as_str() == self.rejected_capability {
            return Err(EventError::Sink {
                reason: "rejected".to_string(),
            });
        }
        self.inner.append(event).await
    }

    async fn read_after_cursor(
        &self,
        stream: &EventStreamKey,
        filter: &ReadScope,
        after: Option<EventCursor>,
        limit: usize,
    ) -> Result<EventReplay<RuntimeEvent>, EventError> {
        self.inner
            .read_after_cursor(stream, filter, after, limit)
            .await
    }

    async fn head_cursor(
        &self,
        stream: &EventStreamKey,
        after: EventCursor,
    ) -> Result<EventCursor, EventError> {
        self.inner.head_cursor(stream, after).await
    }
}

#[tokio::test]
async fn durable_log_decorator_counts_only_committed_rows() {
    let metrics = Arc::new(OperatorMetrics::new());
    let log = MetricsDurableEventLog::new(
        Arc::new(PartlyRejectingLog {
            inner: InMemoryDurableEventLog::new(),
            rejected_capability: "builtin.rejected",
        }),
        Arc::clone(&metrics),
    );
    let results = log
        .append_batch(vec![
            succeeded("builtin.http"),
            succeeded("builtin.rejected"),
            succeeded("builtin.http"),
        ])
        .await;
    assert_eq!(
        results.iter().filter(|result| result.is_ok()).count(),
        2,
        "the inner per-row results are returned unchanged"
    );
    log.append(succeeded("builtin.http")).await.expect("append");
    assert!(log.append(succeeded("builtin.rejected")).await.is_err());

    let rendered = metrics.render_prometheus();
    assert_eq!(
        line_value(
            &rendered,
            "ironclaw_capability_invocations_total{capability_id=\"builtin.http\",outcome=\"succeeded\"}"
        )
        .as_deref(),
        Some("3"),
        "{rendered}"
    );
    assert!(
        !rendered.contains("builtin.rejected"),
        "rejected rows are not counted:\n{rendered}"
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
    // A capability-side rollback discards a gate that never became
    // user-actionable (and was never counted opened): not a resolution.
    let rolled_back = store
        .save_pending(scope.clone(), approval_request())
        .await
        .expect("save");
    store
        .discard_pending(&scope, rolled_back.request.id)
        .await
        .expect("discard");

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
        Some("1"),
        "a discarded rollback is not counted as a denial"
    );
    assert!(
        !rendered.contains("discarded"),
        "no discarded series:\n{rendered}"
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
    let failing_provider = label_value(&failing.provider_id());
    assert_eq!(
        line_value(
            &rendered,
            &format!(
                "ironclaw_llm_errors_total{{provider=\"{failing_provider}\",model=\"unknown\",kind=\"request_failed\"}}"
            )
        )
        .as_deref(),
        Some("1"),
        "a model never served mints no series; the error keeps its kind:\n{rendered}"
    );
    assert!(!rendered.contains("stub/failing-model"), "{rendered}");
    assert_eq!(
        line_value(
            &rendered,
            "ironclaw_llm_request_duration_seconds_count{outcome=\"success\"}"
        )
        .as_deref(),
        Some("1")
    );
    assert_eq!(
        line_value(
            &rendered,
            "ironclaw_llm_request_duration_seconds_count{outcome=\"error\"}"
        )
        .as_deref(),
        Some("1")
    );
}

#[test]
fn junk_model_names_on_failed_requests_cannot_claim_series() {
    let metrics = OperatorMetrics::new();
    for index in 0..(MAX_LLM_SERIES * 2) {
        let junk = format!("junk-{index}");
        metrics.record_llm_request(
            LlmRequestLabels {
                provider: "configured",
                provider_configured: true,
                model: &junk,
            },
            Err("invalid_request"),
            Duration::from_millis(1),
        );
        let routed_provider = format!("junk-provider-{index}");
        metrics.record_llm_request(
            LlmRequestLabels {
                provider: &routed_provider,
                provider_configured: false,
                model: &junk,
            },
            Err("model_not_available"),
            Duration::from_millis(1),
        );
    }
    metrics.record_llm_request(
        LlmRequestLabels {
            provider: "configured",
            provider_configured: true,
            model: "real-model",
        },
        Ok(LlmTokenUsage::default()),
        Duration::from_millis(1),
    );
    // Once served, the pair's later failures are attributed to it.
    metrics.record_llm_request(
        LlmRequestLabels {
            provider: "configured",
            provider_configured: true,
            model: "real-model",
        },
        Err("rate_limited"),
        Duration::from_millis(1),
    );

    let rendered = metrics.render_prometheus();
    assert!(!rendered.contains("junk"), "{rendered}");
    assert!(!rendered.contains("=\"__other__\""), "{rendered}");
    assert_eq!(
        line_value(
            &rendered,
            "ironclaw_llm_requests_total{provider=\"configured\",model=\"unknown\",outcome=\"error\"}"
        )
        .as_deref(),
        Some(&*(MAX_LLM_SERIES * 2).to_string())
    );
    assert_eq!(
        line_value(
            &rendered,
            "ironclaw_llm_requests_total{provider=\"unknown\",model=\"unknown\",outcome=\"error\"}"
        )
        .as_deref(),
        Some(&*(MAX_LLM_SERIES * 2).to_string())
    );
    assert_eq!(
        line_value(
            &rendered,
            "ironclaw_llm_requests_total{provider=\"configured\",model=\"real-model\",outcome=\"success\"}"
        )
        .as_deref(),
        Some("1")
    );
    assert_eq!(
        line_value(
            &rendered,
            "ironclaw_llm_errors_total{provider=\"configured\",model=\"real-model\",kind=\"rate_limited\"}"
        )
        .as_deref(),
        Some("1")
    );
}

/// Builds the routed provider for one configured id, as the operator's
/// provider catalog does.
struct OneRoutedProvider {
    id: &'static str,
    provider: Arc<dyn LlmProvider>,
}

#[async_trait::async_trait]
impl RoutedProviderSource for OneRoutedProvider {
    async fn provider(&self, provider_id: &str) -> Result<Arc<dyn LlmProvider>, LlmError> {
        if provider_id == self.id {
            Ok(Arc::clone(&self.provider))
        } else {
            Err(LlmError::ModelNotAvailable {
                provider: provider_id.to_string(),
                model: String::new(),
            })
        }
    }
}

/// Production's chain shape: metrics outermost over a `ProviderRouter` whose
/// primary is a failover set. A routed call is attributed to the provider it
/// named, and a call the failover served from its second provider to that
/// provider's model — not the primary's.
#[tokio::test]
async fn llm_labels_follow_the_routed_provider_and_the_failover_serving_model() {
    let metrics = Arc::new(OperatorMetrics::new());
    let failover = FailoverProvider::new(vec![
        Arc::new(StubLlm::failing("primary-model")),
        Arc::new(StubLlm::new("from fallback").with_model_name("fallback-model")),
    ])
    .expect("failover");
    let router = ProviderRouter::new(
        Arc::new(failover),
        Arc::new(OneRoutedProvider {
            id: "glm",
            provider: Arc::new(StubLlm::new("from glm").with_model_name("glm-default")),
        }),
    );
    let provider = MetricsLlmProvider::new(Arc::new(router), Arc::clone(&metrics));

    // Spawned, so the failover's task-bound serving evidence applies.
    let provider = Arc::new(provider);
    let task_provider = Arc::clone(&provider);
    tokio::spawn(async move {
        task_provider
            .complete(CompletionRequest::new(vec![ChatMessage::user("hi")]))
            .await
            .expect("fallback serves");
        task_provider
            .complete(
                CompletionRequest::new(vec![ChatMessage::user("hi")]).with_model("glm::glm-5.3"),
            )
            .await
            .expect("routed provider serves");
        task_provider
            .complete(
                CompletionRequest::new(vec![ChatMessage::user("hi")]).with_model("absent::model-x"),
            )
            .await
            .expect_err("unconfigured routed provider fails");
    })
    .await
    .expect("task completes");

    let rendered = metrics.render_prometheus();
    assert_eq!(
        line_value(
            &rendered,
            "ironclaw_llm_requests_total{provider=\"failover\",model=\"fallback-model\",outcome=\"success\"}"
        )
        .as_deref(),
        Some("1"),
        "the failover's serving model, not the primary's:\n{rendered}"
    );
    assert_eq!(
        line_value(
            &rendered,
            "ironclaw_llm_requests_total{provider=\"glm\",model=\"glm-5.3\",outcome=\"success\"}"
        )
        .as_deref(),
        Some("1"),
        "the routed provider, not the primary:\n{rendered}"
    );
    assert_eq!(
        line_value(
            &rendered,
            "ironclaw_llm_errors_total{provider=\"unknown\",model=\"unknown\",kind=\"model_not_available\"}"
        )
        .as_deref(),
        Some("1"),
        "a caller-named provider that never served mints no series:\n{rendered}"
    );
    assert!(!rendered.contains("primary-model"), "{rendered}");
    assert!(!rendered.contains("absent"), "{rendered}");
}
