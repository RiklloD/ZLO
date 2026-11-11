//! LLM request metrics at the outermost position of the provider chain.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use ironclaw_llm::{
    CompletionRequest, CompletionResponse, CompletionStreamSink, LlmError, LlmProvider,
    ModelFallbackRoute, ModelMetadata, ToolCompletionRequest, ToolCompletionResponse,
};
use rust_decimal::Decimal;

use super::{LlmTokenUsage, OperatorMetrics};

/// Wraps the composed provider chain and records one sample per completion
/// call: provider and effective model as labels, token usage on success, a
/// closed-vocabulary error kind on failure. Every other method delegates
/// unchanged — a wrapper delegates the complete provider trait
/// (`ironclaw_llm/CONTRACT.md`).
pub struct MetricsLlmProvider {
    inner: Arc<dyn LlmProvider>,
    metrics: Arc<OperatorMetrics>,
}

impl MetricsLlmProvider {
    pub fn new(inner: Arc<dyn LlmProvider>, metrics: Arc<OperatorMetrics>) -> Self {
        Self { inner, metrics }
    }

    fn record<T>(
        &self,
        model: &str,
        started: Instant,
        result: &Result<T, LlmError>,
        usage: impl FnOnce(&T) -> LlmTokenUsage,
    ) {
        let outcome = match result {
            Ok(response) => Ok(usage(response)),
            Err(error) => Err(llm_error_kind(error)),
        };
        self.metrics.record_llm_request(
            &self.inner.provider_id(),
            model,
            outcome,
            started.elapsed(),
        );
    }
}

fn completion_usage(response: &CompletionResponse) -> LlmTokenUsage {
    LlmTokenUsage {
        input: u64::from(response.input_tokens),
        output: u64::from(response.output_tokens),
        cache_read: u64::from(response.cache_read_input_tokens),
        cache_write: u64::from(response.cache_creation_input_tokens),
    }
}

fn tool_completion_usage(response: &ToolCompletionResponse) -> LlmTokenUsage {
    LlmTokenUsage {
        input: u64::from(response.input_tokens),
        output: u64::from(response.output_tokens),
        cache_read: u64::from(response.cache_read_input_tokens),
        cache_write: u64::from(response.cache_creation_input_tokens),
    }
}

/// Stable, payload-free error label. Never the error's `Display`, which can
/// carry provider reasons.
fn llm_error_kind(error: &LlmError) -> &'static str {
    match error {
        LlmError::RequestFailed { .. } => "request_failed",
        LlmError::InvalidRequest { .. } => "invalid_request",
        LlmError::RateLimited { .. } => "rate_limited",
        LlmError::BadGateway { .. } => "bad_gateway",
        LlmError::InvalidResponse { .. } => "invalid_response",
        LlmError::EmptyResponse { .. } => "empty_response",
        LlmError::StreamInterrupted { .. } => "stream_interrupted",
        LlmError::ContextLengthExceeded { .. } => "context_length_exceeded",
        LlmError::ModelNotAvailable { .. } => "model_not_available",
        LlmError::QuotaExceeded { .. } => "quota_exceeded",
        LlmError::AuthFailed { .. } => "auth_failed",
        LlmError::SessionExpired { .. } => "session_expired",
        LlmError::SessionRenewalFailed { .. } => "session_renewal_failed",
        LlmError::Http(_) => "http",
        LlmError::Json(_) => "json",
        LlmError::Io(_) => "io",
    }
}

#[async_trait]
impl LlmProvider for MetricsLlmProvider {
    fn provider_id(&self) -> String {
        self.inner.provider_id()
    }

    fn model_name(&self) -> &str {
        self.inner.model_name()
    }

    fn cost_per_token(&self) -> (Decimal, Decimal) {
        self.inner.cost_per_token()
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let model = self.inner.effective_model_name(request.model.as_deref());
        let started = Instant::now();
        let result = self.inner.complete(request).await;
        self.record(&model, started, &result, completion_usage);
        result
    }

    async fn complete_streaming(
        &self,
        request: CompletionRequest,
        sink: Arc<dyn CompletionStreamSink>,
    ) -> Result<CompletionResponse, LlmError> {
        let model = self.inner.effective_model_name(request.model.as_deref());
        let started = Instant::now();
        let result = self.inner.complete_streaming(request, sink).await;
        self.record(&model, started, &result, completion_usage);
        result
    }

    async fn complete_with_tools(
        &self,
        request: ToolCompletionRequest,
    ) -> Result<ToolCompletionResponse, LlmError> {
        let model = self.inner.effective_model_name(request.model.as_deref());
        let started = Instant::now();
        let result = self.inner.complete_with_tools(request).await;
        self.record(&model, started, &result, tool_completion_usage);
        result
    }

    async fn complete_with_tools_streaming(
        &self,
        request: ToolCompletionRequest,
        sink: Arc<dyn CompletionStreamSink>,
    ) -> Result<ToolCompletionResponse, LlmError> {
        let model = self.inner.effective_model_name(request.model.as_deref());
        let started = Instant::now();
        let result = self
            .inner
            .complete_with_tools_streaming(request, sink)
            .await;
        self.record(&model, started, &result, tool_completion_usage);
        result
    }

    async fn list_models(&self) -> Result<Vec<String>, LlmError> {
        self.inner.list_models().await
    }

    async fn list_model_catalog(
        &self,
    ) -> Result<Vec<ironclaw_llm::models::DiscoveredModel>, LlmError> {
        self.inner.list_model_catalog().await
    }

    async fn model_metadata(&self) -> Result<ModelMetadata, LlmError> {
        self.inner.model_metadata().await
    }

    fn effective_model_name(&self, requested_model: Option<&str>) -> String {
        self.inner.effective_model_name(requested_model)
    }

    fn fallback_route(
        &self,
        fallback_index: u32,
        requested_model: Option<&str>,
    ) -> Result<ModelFallbackRoute, LlmError> {
        self.inner.fallback_route(fallback_index, requested_model)
    }

    fn active_model_name(&self) -> String {
        self.inner.active_model_name()
    }

    fn set_model(&self, model: &str) -> Result<(), LlmError> {
        self.inner.set_model(model)
    }

    fn calculate_cost(&self, input_tokens: u32, output_tokens: u32) -> Decimal {
        self.inner.calculate_cost(input_tokens, output_tokens)
    }

    fn cache_write_multiplier(&self) -> Decimal {
        self.inner.cache_write_multiplier()
    }

    fn cache_read_discount(&self) -> Decimal {
        self.inner.cache_read_discount()
    }
}
