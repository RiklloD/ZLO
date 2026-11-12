//! Operator metrics: a process-scoped registry rendered as Prometheus text.
//!
//! The registry invents no state. Every sample is folded from a fact another
//! owner already produced:
//!
//! - **turn runs and approval gates opened** — committed turn lifecycle
//!   transitions (`Submitted` … `Completed`/`Failed`/`Cancelled`, `Blocked`
//!   on an approval gate), handed in by the composition-side turn-event
//!   adapter through [`OperatorMetrics::record_turn_run`] and
//!   [`OperatorMetrics::record_approval_gate_opened`] (this crate may not
//!   name `ironclaw_turns`);
//! - **model calls, capability invocations** — the redacted
//!   [`RuntimeEvent`]s the runtime's write-behind event sink commits to the
//!   durable event log, observed by [`MetricsDurableEventLog`] beneath that
//!   sink's drain: an event counts only once its row is durably appended, so
//!   events the bounded queue drops under overload, or the backend rejects,
//!   are never counted;
//! - **approval gates resolved** — the durable approval-request decisions
//!   (`approve` / `deny`), observed by [`MetricsApprovalRequestStore`] around
//!   the approval resolver's store. A pending request *discarded* during a
//!   capability-side rollback is not a resolution: the rollback runs before
//!   the invocation blocks, so the turn never commits the `Blocked`
//!   transition that counts a gate opened, and counting the discard would
//!   make resolved exceed opened. A gate whose run ends (cancelled, failed)
//!   while still pending stays in `opened - resolved`;
//! - **LLM requests, tokens, errors** — the provider responses the model
//!   gateway receives, observed by [`MetricsLlmProvider`] at the outermost
//!   position of the provider chain, labelled from the request's own route
//!   evidence after the call (see that type).
//!
//! Label cardinality is bounded by construction: no label carries a tenant,
//! user, thread, run, or invocation id, every label value is reduced to a
//! short closed alphabet, and each labelled family holds at most a fixed
//! number of series. Past the ceiling only the *open-vocabulary* labels
//! (capability id, provider, model, failure reason) fold into `__other__` —
//! closed ones (`outcome`, token `kind`, error `kind`) are kept — and every
//! folded sample is counted in `ironclaw_metrics_series_overflow_total`. A
//! model or provider label is only ever minted by a successful response: a
//! failed request is attributed to `model="unknown"` unless that
//! provider/model pair has already been served, so caller-chosen model names
//! the provider rejects cannot claim series slots.
//!
//! Like the operator log ring, the production registry is process-global
//! ([`operator_metrics`]): Prometheus scrapes a process, and the runtime's
//! sinks and the scrape route are assembled in different places. Tests build
//! their own [`OperatorMetrics::new`] instance.

mod decorators;
mod llm;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::Duration;

use ironclaw_event_log::{RuntimeEvent, RuntimeEventKind};
use ironclaw_product_contracts::operator_service::OperatorMetricsService;

pub use decorators::{MetricsApprovalRequestStore, MetricsDurableEventLog};
pub use llm::MetricsLlmProvider;

/// Series ceiling for the per-capability families.
const MAX_CAPABILITY_SERIES: usize = 256;
/// Series ceiling for the per-provider/model families.
const MAX_LLM_SERIES: usize = 128;
/// Longest label value kept; longer values are cut on a char boundary.
const MAX_LABEL_VALUE_CHARS: usize = 96;
const OVERFLOW_LABEL_VALUE: &str = "__other__";
const UNKNOWN_LABEL_VALUE: &str = "unknown";

/// Upper bounds (seconds) shared by every duration histogram.
const DURATION_BUCKETS_SECONDS: [f64; 14] = [
    0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0,
];

static OPERATOR_METRICS: LazyLock<Arc<OperatorMetrics>> =
    LazyLock::new(|| Arc::new(OperatorMetrics::new()));

/// The process-wide registry the production runtime records into and the
/// operator scrape route renders.
pub fn operator_metrics() -> Arc<OperatorMetrics> {
    Arc::clone(&OPERATOR_METRICS)
}

/// How a turn run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnRunOutcome {
    Completed,
    Failed,
    Cancelled,
}

impl TurnRunOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// How a user-actionable approval gate left the pending state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ApprovalResolution {
    Approved,
    Denied,
}

impl ApprovalResolution {
    fn as_str(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Denied => "denied",
        }
    }
}

/// Which labels of a family are open-vocabulary (folded into `__other__`
/// past the series ceiling) and which are closed (always kept).
const OUTCOME_ONLY: &[bool] = &[false];
const OPEN_THEN_CLOSED: &[bool] = &[true, false];
const OPEN_OPEN: &[bool] = &[true, true];
const OPEN_OPEN_CLOSED: &[bool] = &[true, true, false];

/// The labels one provider request is attributed to.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LlmRequestLabels<'a> {
    pub(crate) provider: &'a str,
    /// `true` when `provider` is the configured chain's own id, `false` when
    /// it came from the request (a routed `provider::model`).
    pub(crate) provider_configured: bool,
    pub(crate) model: &'a str,
}

/// Token counts one successful provider response reported.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct LlmTokenUsage {
    pub(crate) input: u64,
    pub(crate) output: u64,
    pub(crate) cache_read: u64,
    pub(crate) cache_write: u64,
}

#[derive(Debug, Default)]
pub struct OperatorMetrics {
    state: Mutex<MetricsState>,
}

#[derive(Debug, Default)]
struct MetricsState {
    runs: LabeledCounter,
    run_duration: LabeledHistogram,
    model_calls: LabeledCounter,
    model_call_duration: LabeledHistogram,
    capability_invocations: LabeledCounter,
    capability_failures: LabeledCounter,
    llm_requests: LabeledCounter,
    llm_errors: LabeledCounter,
    llm_tokens: LabeledCounter,
    llm_request_duration: LabeledHistogram,
    approvals_opened: u64,
    approvals_resolved: LabeledCounter,
    overflow: BTreeMap<&'static str, u64>,
}

impl OperatorMetrics {
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> MutexGuard<'_, MetricsState> {
        // A panic while holding the lock can only have left a counter
        // half-incremented; the registry stays usable.
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// One turn run reached a committed terminal state. `duration` is the
    /// time from the run's first observed lifecycle transition (normally
    /// `Submitted`) to the terminal one, when the start was observed.
    pub fn record_turn_run(&self, outcome: TurnRunOutcome, duration: Option<Duration>) {
        let mut state = self.state();
        let state = &mut *state;
        state.runs.increment(
            &mut state.overflow,
            "runs",
            vec![outcome.as_str().into()],
            OUTCOME_ONLY,
            1,
            3,
        );
        if let Some(duration) = duration {
            state
                .run_duration
                .observe(vec![outcome.as_str().into()], duration);
        }
    }

    /// A turn run committed a `Blocked` transition on an approval gate.
    pub fn record_approval_gate_opened(&self) {
        self.state().approvals_opened += 1;
    }

    /// Fold one runtime event the durable event log committed.
    pub fn record_runtime_event(&self, event: &RuntimeEvent) {
        let duration = event.duration_ms.map(Duration::from_millis);
        let mut state = self.state();
        let state = &mut *state;
        match event.kind {
            RuntimeEventKind::ModelCompleted | RuntimeEventKind::ModelFailed => {
                let outcome = if event.kind == RuntimeEventKind::ModelCompleted {
                    "completed"
                } else {
                    "failed"
                };
                state.model_calls.increment(
                    &mut state.overflow,
                    "model_calls",
                    vec![outcome.into()],
                    OUTCOME_ONLY,
                    1,
                    2,
                );
                if let Some(duration) = duration {
                    state
                        .model_call_duration
                        .observe(vec![outcome.into()], duration);
                }
            }
            RuntimeEventKind::CapabilityActivitySucceeded
            | RuntimeEventKind::CapabilityActivityFailed => {
                let capability = label_value(event.capability_id.as_str());
                let failed = event.kind == RuntimeEventKind::CapabilityActivityFailed;
                let outcome = if failed { "failed" } else { "succeeded" };
                state.capability_invocations.increment(
                    &mut state.overflow,
                    "capability_invocations",
                    vec![capability.clone(), outcome.into()],
                    OPEN_THEN_CLOSED,
                    1,
                    MAX_CAPABILITY_SERIES,
                );
                if failed {
                    let reason = event
                        .error_kind
                        .as_deref()
                        .map(label_value)
                        .unwrap_or_else(|| UNKNOWN_LABEL_VALUE.to_string());
                    state.capability_failures.increment(
                        &mut state.overflow,
                        "capability_failures",
                        vec![capability, reason],
                        OPEN_OPEN,
                        1,
                        MAX_CAPABILITY_SERIES,
                    );
                }
            }
            _ => {}
        }
    }

    pub(crate) fn record_approval_resolved(&self, resolution: ApprovalResolution) {
        let mut state = self.state();
        let state = &mut *state;
        state.approvals_resolved.increment(
            &mut state.overflow,
            "approvals_resolved",
            vec![resolution.as_str().into()],
            OUTCOME_ONLY,
            1,
            2,
        );
    }

    /// One provider request finished — `Ok` with its token usage, or `Err`
    /// with a closed-vocabulary error kind.
    pub(crate) fn record_llm_request(
        &self,
        labels: LlmRequestLabels<'_>,
        outcome: Result<LlmTokenUsage, &'static str>,
        duration: Duration,
    ) {
        let mut provider = label_value(labels.provider);
        let mut model = label_value(labels.model);
        let mut state = self.state();
        let state = &mut *state;
        let outcome_label = if outcome.is_ok() { "success" } else { "error" };
        if outcome.is_err() {
            // Only a served response mints a provider/model series; a failure
            // reuses one that already exists or lands on `unknown`.
            let served = state.llm_requests.series.contains_key(&vec![
                provider.clone(),
                model.clone(),
                "success".to_string(),
            ]);
            if !served {
                model = UNKNOWN_LABEL_VALUE.to_string();
                if !labels.provider_configured {
                    provider = UNKNOWN_LABEL_VALUE.to_string();
                }
            }
        }
        state.llm_requests.increment(
            &mut state.overflow,
            "llm_requests",
            vec![provider.clone(), model.clone(), outcome_label.into()],
            OPEN_OPEN_CLOSED,
            1,
            MAX_LLM_SERIES,
        );
        state
            .llm_request_duration
            .observe(vec![outcome_label.into()], duration);
        match outcome {
            Ok(usage) => {
                for (kind, count) in [
                    ("input", usage.input),
                    ("output", usage.output),
                    ("cache_read", usage.cache_read),
                    ("cache_write", usage.cache_write),
                ] {
                    if count > 0 {
                        state.llm_tokens.increment(
                            &mut state.overflow,
                            "llm_tokens",
                            vec![provider.clone(), model.clone(), kind.into()],
                            OPEN_OPEN_CLOSED,
                            count,
                            MAX_LLM_SERIES * 4,
                        );
                    }
                }
            }
            Err(kind) => {
                state.llm_errors.increment(
                    &mut state.overflow,
                    "llm_errors",
                    vec![provider, model, kind.into()],
                    OPEN_OPEN_CLOSED,
                    1,
                    MAX_LLM_SERIES,
                );
            }
        }
    }

    /// Prometheus text exposition format 0.0.4.
    pub fn render_prometheus(&self) -> String {
        let state = self.state();
        let mut out = String::with_capacity(4096);
        render_counter(
            &mut out,
            "ironclaw_turn_runs_total",
            "Turn runs that committed a terminal state, by outcome.",
            &["outcome"],
            &state.runs,
        );
        render_histogram(
            &mut out,
            "ironclaw_turn_run_duration_seconds",
            "Turn run wall-clock duration from its first observed lifecycle transition to its terminal one, gate waits included.",
            &["outcome"],
            &state.run_duration,
        );
        render_counter(
            &mut out,
            "ironclaw_model_calls_total",
            "Model calls made by the agent loop, by outcome.",
            &["outcome"],
            &state.model_calls,
        );
        render_histogram(
            &mut out,
            "ironclaw_model_call_duration_seconds",
            "Agent-loop model call duration.",
            &["outcome"],
            &state.model_call_duration,
        );
        render_counter(
            &mut out,
            "ironclaw_capability_invocations_total",
            "Capability (tool) invocations requested by the agent loop, by capability and outcome.",
            &["capability_id", "outcome"],
            &state.capability_invocations,
        );
        render_counter(
            &mut out,
            "ironclaw_capability_failures_total",
            "Failed capability invocations, by capability and sanitized failure kind.",
            &["capability_id", "reason"],
            &state.capability_failures,
        );
        render_counter(
            &mut out,
            "ironclaw_llm_requests_total",
            "LLM provider requests, by provider, model and outcome.",
            &["provider", "model", "outcome"],
            &state.llm_requests,
        );
        render_counter(
            &mut out,
            "ironclaw_llm_errors_total",
            "Failed LLM provider requests, by provider, model and error kind.",
            &["provider", "model", "kind"],
            &state.llm_errors,
        );
        render_counter(
            &mut out,
            "ironclaw_llm_tokens_total",
            "Tokens reported by LLM providers, by provider, model and token kind.",
            &["provider", "model", "kind"],
            &state.llm_tokens,
        );
        render_histogram(
            &mut out,
            "ironclaw_llm_request_duration_seconds",
            "LLM provider request duration, by outcome.",
            &["outcome"],
            &state.llm_request_duration,
        );
        let _ = writeln!(
            out,
            "# HELP ironclaw_approval_gates_opened_total Turn runs that blocked on an approval gate."
        );
        let _ = writeln!(out, "# TYPE ironclaw_approval_gates_opened_total counter");
        let _ = writeln!(
            out,
            "ironclaw_approval_gates_opened_total {}",
            state.approvals_opened
        );
        render_counter(
            &mut out,
            "ironclaw_approval_gates_resolved_total",
            "User-actionable approval gates that left the pending state, by resolution.",
            &["resolution"],
            &state.approvals_resolved,
        );
        let _ = writeln!(
            out,
            "# HELP ironclaw_metrics_series_overflow_total Samples folded into the __other__ series because a family hit its series ceiling."
        );
        let _ = writeln!(out, "# TYPE ironclaw_metrics_series_overflow_total counter");
        for (family, count) in &state.overflow {
            let _ = writeln!(
                out,
                "ironclaw_metrics_series_overflow_total{{family=\"{family}\"}} {count}"
            );
        }
        out
    }
}

impl OperatorMetricsService for OperatorMetrics {
    fn render_prometheus(&self) -> String {
        OperatorMetrics::render_prometheus(self)
    }
}

/// Counter family keyed by its label values.
#[derive(Debug, Default)]
struct LabeledCounter {
    series: BTreeMap<Vec<String>, u64>,
}

impl LabeledCounter {
    /// `open[i]` marks label `i` as open-vocabulary: past `max_series` only
    /// those labels fold into `__other__`, closed ones are kept, so the
    /// overflow is at most one series per closed-label combination.
    fn increment(
        &mut self,
        overflow: &mut BTreeMap<&'static str, u64>,
        family: &'static str,
        labels: Vec<String>,
        open: &[bool],
        by: u64,
        max_series: usize,
    ) {
        let key = if self.series.contains_key(&labels) || self.series.len() < max_series {
            labels
        } else {
            *overflow.entry(family).or_default() += 1;
            labels
                .into_iter()
                .enumerate()
                .map(|(index, value)| {
                    if open.get(index).copied().unwrap_or(true) {
                        OVERFLOW_LABEL_VALUE.to_string()
                    } else {
                        value
                    }
                })
                .collect()
        };
        let slot = self.series.entry(key).or_default();
        *slot = slot.saturating_add(by);
    }
}

/// Histogram family keyed by its (closed-vocabulary) label values.
#[derive(Debug, Default)]
struct LabeledHistogram {
    series: BTreeMap<Vec<String>, Histogram>,
}

impl LabeledHistogram {
    fn observe(&mut self, labels: Vec<String>, duration: Duration) {
        self.series.entry(labels).or_default().observe(duration);
    }
}

#[derive(Debug, Default)]
struct Histogram {
    /// Non-cumulative per-bucket counts; rendering accumulates them.
    buckets: [u64; DURATION_BUCKETS_SECONDS.len()],
    count: u64,
    sum_seconds: f64,
}

impl Histogram {
    fn observe(&mut self, duration: Duration) {
        let seconds = duration.as_secs_f64();
        if let Some(index) = DURATION_BUCKETS_SECONDS
            .iter()
            .position(|bound| seconds <= *bound)
        {
            self.buckets[index] += 1;
        }
        self.count += 1;
        self.sum_seconds += seconds;
    }
}

fn render_counter(
    out: &mut String,
    name: &str,
    help: &str,
    label_names: &[&str],
    counter: &LabeledCounter,
) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} counter");
    for (values, count) in &counter.series {
        let _ = writeln!(
            out,
            "{name}{} {count}",
            label_set(label_names, values, None)
        );
    }
}

fn render_histogram(
    out: &mut String,
    name: &str,
    help: &str,
    label_names: &[&str],
    histogram: &LabeledHistogram,
) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} histogram");
    for (values, series) in &histogram.series {
        let mut cumulative = 0_u64;
        for (bound, count) in DURATION_BUCKETS_SECONDS.iter().zip(series.buckets.iter()) {
            cumulative += count;
            let _ = writeln!(
                out,
                "{name}_bucket{} {cumulative}",
                label_set(label_names, values, Some(&bound.to_string()))
            );
        }
        let _ = writeln!(
            out,
            "{name}_bucket{} {}",
            label_set(label_names, values, Some("+Inf")),
            series.count
        );
        let labels = label_set(label_names, values, None);
        let _ = writeln!(out, "{name}_sum{labels} {}", series.sum_seconds);
        let _ = writeln!(out, "{name}_count{labels} {}", series.count);
    }
}

fn label_set(names: &[&str], values: &[String], le: Option<&str>) -> String {
    let mut pairs: Vec<String> = names
        .iter()
        .zip(values.iter())
        .map(|(name, value)| format!("{name}=\"{}\"", escape_label_value(value)))
        .collect();
    if let Some(le) = le {
        pairs.push(format!("le=\"{le}\""));
    }
    if pairs.is_empty() {
        String::new()
    } else {
        format!("{{{}}}", pairs.join(","))
    }
}

/// Reduce an identifier to the label alphabet: ASCII alphanumerics and
/// `_ . : / @ -`; anything else becomes `_`, and the value is cut at
/// [`MAX_LABEL_VALUE_CHARS`]. Applied to every producer-supplied value, so a
/// capability id or model name can never smuggle exposition syntax.
fn label_value(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return UNKNOWN_LABEL_VALUE.to_string();
    }
    trimmed
        .chars()
        .take(MAX_LABEL_VALUE_CHARS)
        .map(|character| {
            if character.is_ascii_alphanumeric()
                || matches!(character, '_' | '.' | ':' | '/' | '@' | '-')
            {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/// Exposition-format escaping; a defense in depth behind [`label_value`].
fn escape_label_value(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests;
