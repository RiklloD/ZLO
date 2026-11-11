use super::*;

/// Auth scheme applied to a model-discovery request.
#[derive(Clone)]
pub(crate) enum ModelsAuth {
    /// `Authorization: Bearer <key>` (OpenAI-compatible, NEAR AI).
    Bearer(String),
    /// `x-api-key: <key>` plus an `anthropic-version` header (Anthropic).
    AnthropicKey { api_key: String, version: String },
    /// No auth header (Ollama).
    None,
}

/// Response body shape returned by a model-discovery endpoint.
#[derive(Clone, Copy)]
pub(crate) enum ModelsShape {
    /// OpenAI / Anthropic: `{ "data": [ { "id": ... } ] }`.
    OpenAiData,
    /// Ollama `/api/tags`: `{ "models": [ { "name": ... } ] }`.
    OllamaTags,
}

/// Connection details for a provider model-discovery request.
#[derive(Clone)]
pub(crate) struct ModelsEndpoint {
    /// Provider id, used for error/log context.
    pub(crate) provider_id: String,
    /// Fully-built request URL (base + discovery path).
    pub(crate) url: String,
    /// Auth scheme for the request.
    pub(crate) auth: ModelsAuth,
    /// Response body shape to parse.
    pub(crate) shape: ModelsShape,
    /// Extra headers applied to every request to this provider.
    pub(crate) extra_headers: reqwest::header::HeaderMap,
}

impl ModelsEndpoint {
    /// Issue the model-discovery `GET` and return the model ids.
    ///
    /// Validates the URL against the baseline SSRF guard, applies the
    /// adapter-specific auth scheme, and parses the adapter-specific response
    /// shape. Network, auth, and parse failures map to `LlmError` so the caller
    /// can surface a real message instead of an empty list.
    pub(super) async fn fetch_models(&self) -> Result<Vec<String>, LlmError> {
        let validated = crate::url_check::check_models_url(&self.provider_id, &self.url).await?;

        // `check_models_url` validates only the initial URL. Disable redirect
        // following so a host that passes the guard cannot 3xx-redirect the
        // request to a blocked target (e.g. the cloud-metadata IP) — a 3xx is
        // surfaced as a non-success status below instead of being chased. The
        // shared builder also bypasses the proxy for loopback providers.
        let mut builder =
            crate::config::hardened_client_builder(crate::config::AUXILIARY_REQUEST_TIMEOUT_SECS)
                .redirect(reqwest::redirect::Policy::none());
        // Pin the client to the addresses the guard validated, so the
        // connect-time resolver can't rebind the hostname to a blocked IP after
        // the check passed (DNS TOCTOU). `None` for literal-IP / proxy-resolved
        // hosts, where there is nothing to pin.
        if let Some((host, addrs)) = &validated.pin {
            builder = builder.resolve_to_addrs(host, addrs);
        }
        let client = crate::url_check::build_http_client(&self.provider_id, &self.url, builder)?;

        let mut builder = client.get(&self.url).headers(self.extra_headers.clone());
        builder = match &self.auth {
            ModelsAuth::Bearer(key) => builder.bearer_auth(key),
            ModelsAuth::AnthropicKey { api_key, version } => builder
                .header("x-api-key", api_key)
                .header("anthropic-version", version),
            ModelsAuth::None => builder,
        };

        let response = builder.send().await.map_err(|e| LlmError::RequestFailed {
            provider: self.provider_id.clone(),
            reason: format!("request to {} failed: {e}", self.url),
        })?;

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(LlmError::AuthFailed {
                provider: self.provider_id.clone(),
            });
        }
        if !status.is_success() {
            return Err(LlmError::RequestFailed {
                provider: self.provider_id.clone(),
                reason: format!("{} returned HTTP {}", self.url, status.as_u16()),
            });
        }

        // Bound the body: a model list is a few KB, but an operator-configured
        // (or compromised) endpoint could slow-drip megabytes within the 30s
        // timeout. Reject a declared oversize length up front, then stream with
        // a hard cap so memory stays bounded even when content-length is absent.
        use futures::StreamExt;
        const MAX_MODELS_BODY_BYTES: usize = 4 * 1024 * 1024;
        if let Some(len) = response.content_length()
            && len > MAX_MODELS_BODY_BYTES as u64
        {
            return Err(LlmError::InvalidResponse {
                provider: self.provider_id.clone(),
                reason: format!("models response too large ({len} bytes)"),
            });
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| LlmError::StreamInterrupted {
                provider: self.provider_id.clone(),
                reason: format!("could not read models response: {e}"),
            })?;
            if body.len() + chunk.len() > MAX_MODELS_BODY_BYTES {
                return Err(LlmError::InvalidResponse {
                    provider: self.provider_id.clone(),
                    reason: "models response exceeded 4 MiB cap".to_string(),
                });
            }
            body.extend_from_slice(&chunk);
        }
        parse_models_response(&self.provider_id, self.shape, &body)
    }
}

/// Extract model ids from a model-discovery response body. Empty/whitespace
/// ids are dropped.
///
/// Split out from the HTTP call so the parsing contract is unit-testable
/// without a live endpoint. `ModelsShape` selects the JSON shape:
/// OpenAI/Anthropic `{ "data": [{ "id" }] }` vs Ollama
/// `{ "models": [{ "name" }] }`.
pub(super) fn parse_models_response(
    provider_id: &str,
    shape: ModelsShape,
    body: &[u8],
) -> Result<Vec<String>, LlmError> {
    #[derive(serde::Deserialize)]
    struct OpenAiResponse {
        #[serde(default)]
        data: Vec<OpenAiEntry>,
    }
    #[derive(serde::Deserialize)]
    struct OpenAiEntry {
        id: String,
    }
    #[derive(serde::Deserialize)]
    struct OllamaResponse {
        #[serde(default)]
        models: Vec<OllamaEntry>,
    }
    #[derive(serde::Deserialize)]
    struct OllamaEntry {
        name: String,
    }

    let parse_err = |e: serde_json::Error| LlmError::InvalidResponse {
        provider: provider_id.to_string(),
        reason: format!("could not parse models response: {e}"),
    };

    let ids = match shape {
        ModelsShape::OpenAiData => serde_json::from_slice::<OpenAiResponse>(body)
            .map_err(parse_err)?
            .data
            .into_iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>(),
        ModelsShape::OllamaTags => serde_json::from_slice::<OllamaResponse>(body)
            .map_err(parse_err)?
            .models
            .into_iter()
            .map(|entry| entry.name)
            .collect::<Vec<_>>(),
    };

    Ok(ids.into_iter().filter(|id| !id.trim().is_empty()).collect())
}
