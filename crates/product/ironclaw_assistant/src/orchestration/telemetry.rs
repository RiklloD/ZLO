//! Per-turn telemetry for IronClaw's own agent loop: a diagnostic-sink
//! decorator that forwards every capture untouched and reports each finished
//! model call's context, speed and cache figures to the orchestrator.

use std::sync::Arc;

use ironclaw_loop_host::{
    HostManagedModelCallDiagnosticCapture, HostManagedModelCallDiagnosticOutcome,
    HostManagedPromptDiagnosticCapture, HostManagedPromptDiagnosticSink,
    HostManagedToolInputDiagnosticCapture, HostManagedToolResultDiagnosticCapture,
    HostManagedToolStartedDiagnosticCapture,
};
use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::model::now_ms;
use ironclaw_orchestrator::runtime::TurnEventSink;
use ironclaw_orchestrator::telemetry::{TurnTelemetry, tokens_per_second};

pub struct OrchestratorTelemetrySink {
    inner: Arc<dyn HostManagedPromptDiagnosticSink>,
    engine: Arc<Engine>,
}

impl OrchestratorTelemetrySink {
    pub fn new(inner: Arc<dyn HostManagedPromptDiagnosticSink>, engine: Arc<Engine>) -> Self {
        Self { inner, engine }
    }

    fn report(&self, capture: &HostManagedModelCallDiagnosticCapture) {
        let HostManagedModelCallDiagnosticCapture::Completed {
            diagnostic,
            duration_ms,
            outcome: HostManagedModelCallDiagnosticOutcome::Succeeded { usage: Some(usage) },
            ..
        } = capture
        else {
            return;
        };
        let (input, read, write) = (
            u64::from(usage.input_tokens),
            u64::from(usage.cache_read_input_tokens),
            u64::from(usage.cache_creation_input_tokens),
        );
        // Providers disagree on whether `input_tokens` already includes the
        // cache buckets (OpenAI-style) or excludes them (Anthropic). A read
        // larger than the input can only be the second shape.
        let fresh = if read > input { input } else { input - read };
        let telemetry = TurnTelemetry {
            model: diagnostic
                .effective_model
                .clone()
                .or_else(|| Some(diagnostic.requested_model.clone())),
            // Whole-call time (includes time to first token): the loop does
            // not see streamed frames.
            tps: tokens_per_second(u64::from(usage.output_tokens), *duration_ms),
            ..TurnTelemetry::from_prompt_tokens(fresh, read, write)
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let engine = Arc::clone(&self.engine);
        let thread_id = diagnostic.context.thread_id.clone();
        handle.spawn(async move {
            engine.turn_telemetry(&thread_id, telemetry, now_ms()).await;
        });
    }
}

impl HostManagedPromptDiagnosticSink for OrchestratorTelemetrySink {
    fn record_prompt(&self, capture: HostManagedPromptDiagnosticCapture) {
        self.inner.record_prompt(capture);
    }

    fn record_model_call(&self, capture: HostManagedModelCallDiagnosticCapture) {
        self.report(&capture);
        self.inner.record_model_call(capture);
    }

    fn record_tool_input(&self, capture: HostManagedToolInputDiagnosticCapture) {
        self.inner.record_tool_input(capture);
    }

    fn record_tool_started(&self, capture: HostManagedToolStartedDiagnosticCapture) {
        self.inner.record_tool_started(capture);
    }

    fn record_tool_result(&self, capture: HostManagedToolResultDiagnosticCapture) {
        self.inner.record_tool_result(capture);
    }
}
