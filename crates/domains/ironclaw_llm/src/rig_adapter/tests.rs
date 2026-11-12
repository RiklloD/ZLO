//! Shared fixtures for the rig bridge unit tests; each themed suite under
//! `tests/` is a child of this module, so `super::*` reaches both the
//! adapter items and these helpers.
// untriaged discards: predate the workspace-wide deny (Cargo.toml [workspace.lints.clippy]).
#![allow(clippy::let_underscore_must_use)]

use super::*;
use rig::completion::CompletionError;
use rig::streaming::{RawStreamingChoice, RawStreamingToolCall, StreamingCompletionResponse};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

const LOOPBACK_CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);

fn with_native_streaming<M: CompletionModel>(mut adapter: RigAdapter<M>) -> RigAdapter<M> {
    adapter.native_streaming = true;
    adapter
}

async fn capture_one_http_request() -> (String, oneshot::Receiver<String>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback capture server");
    let addr = listener.local_addr().expect("capture server address");
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let read = socket.read(&mut buffer).await.expect("read request");
            if read == 0 {
                return;
            }
            request.extend_from_slice(&buffer[..read]);
            let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
                continue;
            };
            let headers =
                std::str::from_utf8(&request[..header_end]).expect("request headers are UTF-8");
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then_some(value.trim())
                })
                .expect("content length header")
                .parse::<usize>()
                .expect("content length is numeric");
            let body_start = header_end + 4;
            if request.len() < body_start + content_length {
                continue;
            }
            tx.send(
                String::from_utf8(request[body_start..body_start + content_length].to_vec())
                    .expect("request body is UTF-8"),
            )
            .expect("test receives captured request");
            socket
                .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
                .await
                .expect("write test response");
            return;
        }
    });
    (format!("http://{addr}/v1"), rx)
}

fn tool_request_with_model_override() -> ToolCompletionRequest {
    ToolCompletionRequest::new(
        vec![ChatMessage::user("use the echo tool")],
        vec![IronToolDefinition {
            name: "echo".to_string(),
            description: "Echo the supplied text".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": { "text": { "type": "string" } },
                "required": ["text"]
            }),
        }],
    )
    .with_model("override-model")
}

fn assert_single_model_override(body: &str) {
    assert_eq!(
        body.match_indices("\"model\":").count(),
        1,
        "the raw JSON body must have exactly one top-level model key: {body}",
    );
    assert!(
        body.contains("\"model\":\"override-model\""),
        "the one serialized model must preserve the request override: {body}",
    );
}

async fn captured_request_body(captured_body: oneshot::Receiver<String>) -> String {
    tokio::time::timeout(LOOPBACK_CAPTURE_TIMEOUT, captured_body)
        .await
        .expect("request capture timed out")
        .expect("captured request body")
}

#[derive(Clone)]
struct FailingCompletionModel;

impl CompletionModel for FailingCompletionModel {
    type Response = serde_json::Value;
    type StreamingResponse = ();
    type Client = ();

    fn make(_client: &Self::Client, _model: impl Into<String>) -> Self {
        Self
    }

    async fn completion(
        &self,
        _request: RigRequest,
    ) -> Result<rig::completion::CompletionResponse<Self::Response>, CompletionError> {
        Err(CompletionError::ProviderError(
                "HTTP 400 Bad Request: The input (314325 tokens) is longer than the model's context length (262144 tokens).".to_string(),
            ))
    }

    async fn stream(
        &self,
        _request: RigRequest,
    ) -> Result<StreamingCompletionResponse<Self::StreamingResponse>, CompletionError> {
        Err(CompletionError::ProviderError(
            "stream unsupported".to_string(),
        ))
    }
}

struct RecordingCompletionStreamSink {
    sender: mpsc::UnboundedSender<String>,
}

#[async_trait]
impl CompletionStreamSink for RecordingCompletionStreamSink {
    async fn text_delta(&self, delta: String) {
        let _ = self.sender.send(delta);
    }
}

mod convert_tests;
mod error_tests;
mod models_endpoint_tests;
mod provider_tests;
mod request_tests;
mod schema_tests;
mod wire_tests;

// The finish-reason conformance suite — provider fixtures, payload
// builders, and the matrix itself — lives in its own file so this one
// stays readable: `src/rig_adapter/tests/finish_reason_tests.rs`.
mod finish_reason_tests;
