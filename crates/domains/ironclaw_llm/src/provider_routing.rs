//! Per-request provider routing.
//!
//! [`ProviderRouter`] wraps the primary (active) provider chain. A request
//! whose model override is a [`ModelRoute`] runs on the provider it names
//! (`"<provider_id>::<model>"`), so one thread can use GLM while another uses
//! the active provider, and may carry a reasoning effort
//! (`"<provider_id>::<model>::<effort>"`, provider and model may be empty:
//! the primary, its default model). Every other request passes through to the
//! primary unchanged.
//!
//! The router does not know how to build a provider: a [`RoutedProviderSource`]
//! (implemented by the operator layer, which owns the provider catalog and the
//! stored keys) builds and caches one client per provider id.

use std::sync::Arc;

use async_trait::async_trait;
use rust_decimal::Decimal;

use crate::error::LlmError;
use crate::provider::{
    CompletionRequest, CompletionResponse, CompletionStreamSink, LlmProvider, ModelFallbackRoute,
    ModelMetadata, ToolCompletionRequest, ToolCompletionResponse,
};

const SEPARATOR: &str = "::";

/// Request metadata key carrying a routed request's reasoning effort.
/// Providers with an effort knob read it (the Responses API ones today).
pub const REASONING_EFFORT_METADATA: &str = "reasoning_effort";

/// A model on a named provider (or the primary), and an optional reasoning
/// effort, carried through the per-request model override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRoute {
    provider_id: Option<String>,
    model: Option<String>,
    effort: Option<String>,
}

fn plain_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

impl ModelRoute {
    /// `None` when either part is empty or the provider id is not a plain
    /// catalog id (ASCII alphanumerics, `-`, `_`, `.`).
    pub fn new(provider_id: impl Into<String>, model: impl Into<String>) -> Option<Self> {
        let provider_id = provider_id.into();
        let model = model.into();
        (plain_id(&provider_id) && !model.trim().is_empty() && !model.contains(SEPARATOR))
            .then_some(Self {
                provider_id: Some(provider_id),
                model: Some(model),
                effort: None,
            })
    }

    /// Parse an encoded route; plain model ids return `None`.
    pub fn parse(value: &str) -> Option<Self> {
        let parts: Vec<&str> = value.trim().split(SEPARATOR).collect();
        match parts.as_slice() {
            [provider, model] => Self::new(*provider, *model),
            [provider, model, effort] => {
                let some = |v: &str| (!v.is_empty()).then(|| v.to_string());
                let ok = (provider.is_empty() || plain_id(provider))
                    && model.trim() == *model
                    && plain_id(effort);
                ok.then(|| Self {
                    provider_id: some(provider),
                    model: some(model),
                    effort: some(effort),
                })
            }
            _ => None,
        }
    }

    pub fn encode(&self) -> String {
        let provider = self.provider_id.as_deref().unwrap_or_default();
        let model = self.model.as_deref().unwrap_or_default();
        match &self.effort {
            Some(effort) => format!("{provider}{SEPARATOR}{model}{SEPARATOR}{effort}"),
            None => format!("{provider}{SEPARATOR}{model}"),
        }
    }

    /// `None`: the primary provider.
    pub fn provider_id(&self) -> Option<&str> {
        self.provider_id.as_deref()
    }

    /// `None`: the provider's default model.
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    pub fn effort(&self) -> Option<&str> {
        self.effort.as_deref()
    }
}

/// Builds the client for a configured provider id. Implementations cache:
/// the router asks on every routed request.
#[async_trait]
pub trait RoutedProviderSource: Send + Sync {
    async fn provider(&self, provider_id: &str) -> Result<Arc<dyn LlmProvider>, LlmError>;
}

/// Sends route-encoded requests to their provider; everything else to the
/// primary chain. Metadata methods describe the primary.
pub struct ProviderRouter {
    primary: Arc<dyn LlmProvider>,
    source: Arc<dyn RoutedProviderSource>,
}

impl ProviderRouter {
    pub fn new(primary: Arc<dyn LlmProvider>, source: Arc<dyn RoutedProviderSource>) -> Self {
        Self { primary, source }
    }

    /// The provider for `requested`, and its route (`None`: a plain request).
    async fn target(
        &self,
        requested: Option<&str>,
    ) -> Result<(Arc<dyn LlmProvider>, Option<ModelRoute>), LlmError> {
        let Some(route) = requested.and_then(ModelRoute::parse) else {
            return Ok((Arc::clone(&self.primary), None));
        };
        let provider = match route.provider_id() {
            Some(id) => self.source.provider(id).await?,
            None => Arc::clone(&self.primary),
        };
        Ok((provider, Some(route)))
    }
}

/// Rewrite a routed request for its provider: the plain model, and the
/// effort as metadata.
fn apply(
    route: Option<ModelRoute>,
    model: &mut Option<String>,
    metadata: &mut std::collections::HashMap<String, String>,
) {
    let Some(route) = route else { return };
    *model = route.model;
    if let Some(effort) = route.effort {
        metadata.insert(REASONING_EFFORT_METADATA.to_string(), effort);
    }
}

#[async_trait]
impl LlmProvider for ProviderRouter {
    fn provider_id(&self) -> String {
        self.primary.provider_id()
    }

    fn model_name(&self) -> &str {
        self.primary.model_name()
    }

    fn cost_per_token(&self) -> (Decimal, Decimal) {
        self.primary.cost_per_token()
    }

    async fn complete(
        &self,
        mut request: CompletionRequest,
    ) -> Result<CompletionResponse, LlmError> {
        let (provider, route) = self.target(request.model.as_deref()).await?;
        apply(route, &mut request.model, &mut request.metadata);
        provider.complete(request).await
    }

    async fn complete_streaming(
        &self,
        mut request: CompletionRequest,
        sink: Arc<dyn CompletionStreamSink>,
    ) -> Result<CompletionResponse, LlmError> {
        let (provider, route) = self.target(request.model.as_deref()).await?;
        apply(route, &mut request.model, &mut request.metadata);
        provider.complete_streaming(request, sink).await
    }

    async fn complete_with_tools(
        &self,
        mut request: ToolCompletionRequest,
    ) -> Result<ToolCompletionResponse, LlmError> {
        let (provider, route) = self.target(request.model.as_deref()).await?;
        apply(route, &mut request.model, &mut request.metadata);
        provider.complete_with_tools(request).await
    }

    async fn complete_with_tools_streaming(
        &self,
        mut request: ToolCompletionRequest,
        sink: Arc<dyn CompletionStreamSink>,
    ) -> Result<ToolCompletionResponse, LlmError> {
        let (provider, route) = self.target(request.model.as_deref()).await?;
        apply(route, &mut request.model, &mut request.metadata);
        provider.complete_with_tools_streaming(request, sink).await
    }

    async fn list_models(&self) -> Result<Vec<String>, LlmError> {
        self.primary.list_models().await
    }

    async fn list_model_catalog(&self) -> Result<Vec<crate::models::DiscoveredModel>, LlmError> {
        self.primary.list_model_catalog().await
    }

    async fn model_metadata(&self) -> Result<ModelMetadata, LlmError> {
        self.primary.model_metadata().await
    }

    fn effective_model_name(&self, requested_model: Option<&str>) -> String {
        match requested_model.and_then(ModelRoute::parse) {
            Some(route) => match (route.provider_id, route.model) {
                (Some(_), Some(model)) => model,
                (_, model) => self.primary.effective_model_name(model.as_deref()),
            },
            None => self.primary.effective_model_name(requested_model),
        }
    }

    /// A routed request keeps its encoded model through the gateway so the
    /// completion call can still see which provider and effort it is for.
    /// Routed providers have no ordered fallbacks; the primary's keep the
    /// effort.
    fn fallback_route(
        &self,
        fallback_index: u32,
        requested_model: Option<&str>,
    ) -> Result<ModelFallbackRoute, LlmError> {
        let Some(route) = requested_model.and_then(ModelRoute::parse) else {
            return self.primary.fallback_route(fallback_index, requested_model);
        };
        match route.provider_id() {
            Some(_) if fallback_index == 0 => Ok(ModelFallbackRoute {
                fallback_index,
                model: route.encode(),
            }),
            Some(provider) => Err(LlmError::ModelNotAvailable {
                provider: provider.to_string(),
                model: route.model.unwrap_or_default(),
            }),
            None => {
                let primary = self.primary.fallback_route(fallback_index, route.model())?;
                let model = ModelRoute {
                    model: Some(primary.model),
                    ..route
                }
                .encode();
                Ok(ModelFallbackRoute { model, ..primary })
            }
        }
    }

    fn active_model_name(&self) -> String {
        self.primary.active_model_name()
    }

    fn set_model(&self, model: &str) -> Result<(), LlmError> {
        self.primary.set_model(model)
    }

    fn cache_write_multiplier(&self) -> Decimal {
        self.primary.cache_write_multiplier()
    }

    fn cache_read_discount(&self) -> Decimal {
        self.primary.cache_read_discount()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::StubLlm;
    use std::sync::Mutex;

    /// Answers "routed" and records the model and effort each request carried.
    #[derive(Default)]
    struct Recording(Mutex<Vec<(Option<String>, Option<String>)>>);

    fn seen(
        model: &Option<String>,
        metadata: &std::collections::HashMap<String, String>,
    ) -> (Option<String>, Option<String>) {
        (
            model.clone(),
            metadata.get(REASONING_EFFORT_METADATA).cloned(),
        )
    }

    #[async_trait]
    impl LlmProvider for Recording {
        fn model_name(&self) -> &str {
            "recording"
        }
        fn cost_per_token(&self) -> (Decimal, Decimal) {
            (Decimal::ZERO, Decimal::ZERO)
        }
        async fn complete(
            &self,
            request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            self.0
                .lock()
                .unwrap()
                .push(seen(&request.model, &request.metadata));
            StubLlm::new("routed").complete(request).await
        }
        async fn complete_with_tools(
            &self,
            request: ToolCompletionRequest,
        ) -> Result<ToolCompletionResponse, LlmError> {
            self.0
                .lock()
                .unwrap()
                .push(seen(&request.model, &request.metadata));
            StubLlm::new("routed").complete_with_tools(request).await
        }
    }

    struct OneProvider(Arc<Recording>, Mutex<Vec<String>>);

    #[async_trait]
    impl RoutedProviderSource for OneProvider {
        async fn provider(&self, provider_id: &str) -> Result<Arc<dyn LlmProvider>, LlmError> {
            self.1.lock().unwrap().push(provider_id.to_string());
            Ok(self.0.clone())
        }
    }

    #[test]
    fn routes_round_trip_and_plain_models_stay_plain() {
        let route = ModelRoute::parse("glm::glm-5.3").unwrap();
        assert_eq!(
            (route.provider_id(), route.model()),
            (Some("glm"), Some("glm-5.3"))
        );
        assert_eq!(route.encode(), "glm::glm-5.3");
        let effort = ModelRoute::parse("::gpt-6.1-sol::high").unwrap();
        assert_eq!(
            (effort.provider_id(), effort.model(), effort.effort()),
            (None, Some("gpt-6.1-sol"), Some("high"))
        );
        assert_eq!(effort.encode(), "::gpt-6.1-sol::high");
        assert_eq!(ModelRoute::parse("::::low").unwrap().model(), None);
        for plain in [
            "glm-5.3",
            "llama3:8b",
            "anthropic/claude",
            "::x",
            "glm::",
            "a b::c",
            "glm::m::",
            "a::b::c::d",
        ] {
            assert!(ModelRoute::parse(plain).is_none(), "{plain}");
        }
    }

    #[tokio::test]
    async fn routed_requests_reach_their_provider_with_the_plain_model() {
        let primary = Arc::new(StubLlm::new("primary"));
        let routed = Arc::new(Recording::default());
        let source = Arc::new(OneProvider(routed.clone(), Mutex::new(Vec::new())));
        let router = ProviderRouter::new(primary.clone(), source.clone());

        let fallback = router.fallback_route(0, Some("glm::glm-5.3")).unwrap();
        assert_eq!(fallback.model, "glm::glm-5.3");
        assert!(router.fallback_route(1, Some("glm::glm-5.3")).is_err());
        assert_eq!(router.effective_model_name(Some("glm::glm-5.3")), "glm-5.3");

        let mut request = CompletionRequest::new(vec![crate::ChatMessage::user("hi")]);
        request.model = Some(fallback.model);
        let response = router.complete(request).await.unwrap();
        assert_eq!(response.content, "routed");
        assert_eq!(source.1.lock().unwrap().as_slice(), ["glm"]);
        assert_eq!(
            routed.0.lock().unwrap().as_slice(),
            [(Some("glm-5.3".to_string()), None)]
        );

        let plain = CompletionRequest::new(vec![crate::ChatMessage::user("hi")]);
        assert_eq!(router.complete(plain).await.unwrap().content, "primary");
        assert_eq!(source.1.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_effort_reaches_the_primary_as_metadata() {
        let primary = Arc::new(Recording::default());
        let source = Arc::new(OneProvider(
            Arc::new(Recording::default()),
            Mutex::new(Vec::new()),
        ));
        let router = ProviderRouter::new(primary.clone(), source.clone());

        let mut request = CompletionRequest::new(vec![crate::ChatMessage::user("hi")]);
        request.model = Some("::gpt-6.1-sol::xhigh".to_string());
        router.complete(request).await.unwrap();
        assert_eq!(
            primary.0.lock().unwrap().as_slice(),
            [(Some("gpt-6.1-sol".to_string()), Some("xhigh".to_string()))]
        );
        assert!(source.1.lock().unwrap().is_empty());
    }
}
