//! `POST /v1/embeddings` — the OpenAI Embeddings wire contract and the
//! host-supplied embedding port behind it.
//!
//! This crate must not call `ironclaw_llm` (see `AGENTS.md`), so — mirroring
//! the model-catalog port — it declares [`OpenAiCompatEmbeddings`] and host
//! composition injects an implementation over the deployment's configured
//! embedding provider. No configured provider means no port, and the route
//! stays fail-closed at `501`.
//!
//! The route parses, validates and bounds the whole payload **before** the
//! port is called: the model must be the configured one, `input` must be a
//! string or an array of strings (token-id arrays are rejected), the batch and
//! each input are bounded by the smaller of the port's limits and this crate's
//! ceilings, and a `dimensions` request must equal the model's dimension. The
//! port's answer is re-checked (one vector per input, each of the advertised
//! dimension) before anything is serialized back.

use async_trait::async_trait;
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::{OpenAiCompatAuthenticatedCaller, OpenAiCompatErrorKind, OpenAiCompatHttpError};

/// Ingress body ceiling for an embeddings request. Single source of truth for
/// the route descriptor's `body_limit` and the in-handler check.
pub(crate) const MAX_EMBEDDINGS_BODY_BYTES: usize = 2 * 1024 * 1024;
/// Ceiling on inputs per request, whatever the port advertises.
pub(crate) const MAX_EMBEDDINGS_INPUTS: usize = 2048;
/// Ceiling on one input's UTF-8 bytes, whatever the port advertises.
pub(crate) const MAX_EMBEDDINGS_INPUT_BYTES: usize = 32_000;

/// The embedding model a deployment exposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiCompatEmbeddingModel {
    /// Model id clients must send as `model`.
    pub id: String,
    /// Length of every returned vector.
    pub dimension: u32,
    /// Maximum inputs per request the backing provider accepts.
    pub max_inputs: usize,
    /// Maximum UTF-8 bytes per input the backing provider accepts.
    pub max_input_bytes: usize,
}

/// One embedded batch, in input order.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenAiCompatEmbeddingBatch {
    pub embeddings: Vec<Vec<f32>>,
    pub prompt_tokens: u64,
    pub total_tokens: u64,
}

/// Host-supplied embedding service behind `POST /v1/embeddings`.
#[async_trait]
pub trait OpenAiCompatEmbeddings: Send + Sync {
    /// The configured model and its bounds.
    fn model(&self) -> OpenAiCompatEmbeddingModel;

    /// Embed an already-validated batch for the authenticated caller. Errors
    /// are sanitized API errors; implementations never surface provider
    /// bodies or credential material.
    async fn embed(
        &self,
        caller: &OpenAiCompatAuthenticatedCaller,
        inputs: Vec<String>,
    ) -> Result<OpenAiCompatEmbeddingBatch, OpenAiCompatHttpError>;
}

/// `input`: one string or an array of strings. Token-id arrays (`[1, 2]` and
/// `[[1, 2]]`) are part of the OpenAI shape but are not supported here.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum OpenAiEmbeddingsInput {
    Single(String),
    Batch(Vec<String>),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenAiEmbeddingEncodingFormat {
    #[default]
    Float,
    Base64,
}

/// OpenAI `POST /v1/embeddings` request body.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct OpenAiEmbeddingsRequest {
    pub model: String,
    pub input: serde_json::Value,
    #[serde(default)]
    pub encoding_format: Option<OpenAiEmbeddingEncodingFormat>,
    #[serde(default)]
    pub dimensions: Option<u32>,
    /// End-user id hint; accepted and ignored (attribution comes from auth).
    #[serde(default)]
    pub user: Option<String>,
}

/// One vector in the response: a float array, or base64 of little-endian f32.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum OpenAiEmbeddingVector {
    Float(Vec<f32>),
    Base64(String),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OpenAiEmbeddingObject {
    pub object: &'static str,
    pub index: usize,
    pub embedding: OpenAiEmbeddingVector,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct OpenAiEmbeddingsUsage {
    pub prompt_tokens: u64,
    pub total_tokens: u64,
}

/// OpenAI `POST /v1/embeddings` response body.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OpenAiEmbeddingsResponse {
    pub object: &'static str,
    pub data: Vec<OpenAiEmbeddingObject>,
    pub model: String,
    pub usage: OpenAiEmbeddingsUsage,
}

fn invalid(param: &str) -> OpenAiCompatHttpError {
    OpenAiCompatHttpError::invalid_request(Some(param.to_string()))
}

fn parse_inputs(value: serde_json::Value) -> Result<Vec<String>, OpenAiCompatHttpError> {
    match serde_json::from_value::<OpenAiEmbeddingsInput>(value) {
        Ok(OpenAiEmbeddingsInput::Single(text)) => Ok(vec![text]),
        Ok(OpenAiEmbeddingsInput::Batch(texts)) => Ok(texts),
        // Token arrays, objects, numbers, null: not a supported input shape.
        Err(_) => Err(invalid("input")),
    }
}

/// Parse and bound a request against the configured model. Runs before the
/// port is reached.
pub(crate) fn validate_embeddings_request(
    body: &[u8],
    model: &OpenAiCompatEmbeddingModel,
) -> Result<(Vec<String>, OpenAiEmbeddingEncodingFormat), OpenAiCompatHttpError> {
    if body.len() > MAX_EMBEDDINGS_BODY_BYTES {
        return Err(OpenAiCompatHttpError::from_kind(
            413,
            false,
            OpenAiCompatErrorKind::Validation,
            None,
        ));
    }
    let request: OpenAiEmbeddingsRequest =
        serde_json::from_slice(body).map_err(|_| OpenAiCompatHttpError::invalid_request(None))?;
    if request.model.trim() != model.id {
        // OpenAI answers an unknown model with 404 `model_not_found`.
        return Err(OpenAiCompatHttpError::not_found(Some("model".to_string())));
    }
    if let Some(dimensions) = request.dimensions
        && dimensions != model.dimension
    {
        return Err(invalid("dimensions"));
    }
    let inputs = parse_inputs(request.input)?;
    let max_inputs = model.max_inputs.min(MAX_EMBEDDINGS_INPUTS);
    let max_bytes = model.max_input_bytes.min(MAX_EMBEDDINGS_INPUT_BYTES);
    if inputs.is_empty() || inputs.len() > max_inputs {
        return Err(invalid("input"));
    }
    if inputs
        .iter()
        .any(|input| input.trim().is_empty() || input.len() > max_bytes)
    {
        return Err(invalid("input"));
    }
    Ok((inputs, request.encoding_format.unwrap_or_default()))
}

fn encode_base64(vector: &[f32]) -> String {
    let bytes: Vec<u8> = vector
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Check the port's answer and build the wire response.
pub(crate) fn embeddings_response(
    model: &OpenAiCompatEmbeddingModel,
    expected_inputs: usize,
    format: OpenAiEmbeddingEncodingFormat,
    batch: OpenAiCompatEmbeddingBatch,
) -> Result<OpenAiEmbeddingsResponse, OpenAiCompatHttpError> {
    let well_formed = batch.embeddings.len() == expected_inputs
        && batch.embeddings.iter().all(|vector| {
            vector.len() == model.dimension as usize && vector.iter().all(|v| v.is_finite())
        });
    if !well_formed {
        tracing::debug!(
            expected_inputs,
            returned = batch.embeddings.len(),
            "embedding port returned a malformed batch"
        );
        return Err(OpenAiCompatHttpError::from_kind(
            503,
            true,
            OpenAiCompatErrorKind::ServiceUnavailable,
            None,
        ));
    }
    let data = batch
        .embeddings
        .into_iter()
        .enumerate()
        .map(|(index, vector)| OpenAiEmbeddingObject {
            object: "embedding",
            index,
            embedding: match format {
                OpenAiEmbeddingEncodingFormat::Float => OpenAiEmbeddingVector::Float(vector),
                OpenAiEmbeddingEncodingFormat::Base64 => {
                    OpenAiEmbeddingVector::Base64(encode_base64(&vector))
                }
            },
        })
        .collect();
    Ok(OpenAiEmbeddingsResponse {
        object: "list",
        data,
        model: model.id.clone(),
        usage: OpenAiEmbeddingsUsage {
            prompt_tokens: batch.prompt_tokens,
            total_tokens: batch.total_tokens,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> OpenAiCompatEmbeddingModel {
        OpenAiCompatEmbeddingModel {
            id: "text-embedding-3-small".to_string(),
            dimension: 2,
            max_inputs: 3,
            max_input_bytes: 10,
        }
    }

    fn body(value: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&value).unwrap()
    }

    #[test]
    fn accepts_string_and_array_inputs_and_defaults_to_float() {
        let (inputs, format) = validate_embeddings_request(
            &body(serde_json::json!({"model": "text-embedding-3-small", "input": "hi"})),
            &model(),
        )
        .unwrap();
        assert_eq!(inputs, vec!["hi"]);
        assert_eq!(format, OpenAiEmbeddingEncodingFormat::Float);
        let (inputs, format) = validate_embeddings_request(
            &body(serde_json::json!({
                "model": "text-embedding-3-small",
                "input": ["a", "b"],
                "encoding_format": "base64",
                "dimensions": 2,
                "user": "u-1"
            })),
            &model(),
        )
        .unwrap();
        assert_eq!(inputs, vec!["a", "b"]);
        assert_eq!(format, OpenAiEmbeddingEncodingFormat::Base64);
    }

    #[test]
    fn rejects_bad_shapes_before_the_port() {
        let cases = [
            (serde_json::json!({"model": "other", "input": "x"}), 404),
            (
                serde_json::json!({"model": "text-embedding-3-small", "input": [1, 2]}),
                400,
            ),
            (
                serde_json::json!({"model": "text-embedding-3-small", "input": [[1]]}),
                400,
            ),
            (
                serde_json::json!({"model": "text-embedding-3-small", "input": []}),
                400,
            ),
            (
                serde_json::json!({"model": "text-embedding-3-small", "input": ["  "]}),
                400,
            ),
            (
                serde_json::json!({"model": "text-embedding-3-small", "input": ["a","b","c","d"]}),
                400,
            ),
            (
                serde_json::json!({"model": "text-embedding-3-small", "input": "01234567890"}),
                400,
            ),
            (
                serde_json::json!({"model": "text-embedding-3-small", "input": "x", "dimensions": 3}),
                400,
            ),
            (
                serde_json::json!({"model": "text-embedding-3-small", "input": "x", "encoding_format": "hex"}),
                400,
            ),
            (serde_json::json!({"input": "x"}), 400),
        ];
        for (request, status) in cases {
            let error = validate_embeddings_request(&body(request.clone()), &model()).unwrap_err();
            assert_eq!(error.status_code(), status, "{request}");
        }
        let oversized = vec![b' '; MAX_EMBEDDINGS_BODY_BYTES + 1];
        assert_eq!(
            validate_embeddings_request(&oversized, &model())
                .unwrap_err()
                .status_code(),
            413
        );
    }

    #[test]
    fn response_encodes_base64_little_endian_and_rejects_malformed_batches() {
        let batch = OpenAiCompatEmbeddingBatch {
            embeddings: vec![vec![1.0, -2.0]],
            prompt_tokens: 1,
            total_tokens: 1,
        };
        let response = embeddings_response(
            &model(),
            1,
            OpenAiEmbeddingEncodingFormat::Base64,
            batch.clone(),
        )
        .unwrap();
        let OpenAiEmbeddingVector::Base64(encoded) = &response.data[0].embedding else {
            panic!("expected base64");
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        let decoded: Vec<f32> = bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|chunk| f32::from_le_bytes(*chunk))
            .collect();
        assert_eq!(decoded, vec![1.0, -2.0]);
        assert!(
            embeddings_response(
                &model(),
                2,
                OpenAiEmbeddingEncodingFormat::Float,
                batch.clone()
            )
            .is_err()
        );
        let wrong_dim = OpenAiCompatEmbeddingBatch {
            embeddings: vec![vec![1.0]],
            ..batch
        };
        assert_eq!(
            embeddings_response(&model(), 1, OpenAiEmbeddingEncodingFormat::Float, wrong_dim)
                .unwrap_err()
                .status_code(),
            503
        );
    }
}
