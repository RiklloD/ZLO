//! Hermetic loopback HTTP server for native-streaming provider tests.
//!
//! Accepts exactly one request, captures its request line, headers and JSON
//! body, then replies `200 text/event-stream` and writes a scripted sequence
//! of raw SSE frames, each after its own delay, before closing the
//! connection. Delays let tests prove the idle-progress deadline: semantic
//! frames keep a long stream alive past the idle window, keepalive-only
//! frames do not.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// What the server saw.
#[derive(Debug)]
pub(crate) struct CapturedRequest {
    /// `POST /v1/messages HTTP/1.1`
    pub(crate) request_line: String,
    /// Lower-cased header names with their raw values.
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: serde_json::Value,
}

impl CapturedRequest {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// One scripted SSE frame: wait `delay`, then write `frame` verbatim.
pub(crate) struct ScriptedFrame {
    pub(crate) delay: Duration,
    pub(crate) frame: String,
}

impl ScriptedFrame {
    pub(crate) fn now(frame: impl Into<String>) -> Self {
        Self::after(Duration::ZERO, frame)
    }

    pub(crate) fn after(delay: Duration, frame: impl Into<String>) -> Self {
        Self {
            delay,
            frame: frame.into(),
        }
    }

    /// A named SSE event whose data is `data` serialized as one line.
    pub(crate) fn event(event: &str, data: serde_json::Value) -> Self {
        Self::now(format!("event: {event}\ndata: {data}\n\n"))
    }

    /// An unnamed `data:` frame (OpenAI Responses style).
    pub(crate) fn data(data: serde_json::Value) -> Self {
        Self::now(format!("data: {data}\n\n"))
    }

    pub(crate) fn delayed(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

/// Start the server. Returns its `http://127.0.0.1:<port>` base URL and a
/// receiver for the captured request.
pub(crate) async fn serve_sse(
    frames: Vec<ScriptedFrame>,
) -> (String, oneshot::Receiver<CapturedRequest>) {
    serve(200, "text/event-stream", frames).await
}

/// Like [`serve_sse`], but replies with an arbitrary status and content type
/// (one frame is the whole body).
pub(crate) async fn serve(
    status: u16,
    content_type: &'static str,
    frames: Vec<ScriptedFrame>,
) -> (String, oneshot::Receiver<CapturedRequest>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback SSE server");
    let base_url = format!(
        "http://{}",
        listener.local_addr().expect("loopback SSE server address")
    );
    let (tx, rx) = oneshot::channel();
    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut request = Vec::new();
        let mut buffer = [0_u8; 8192];
        let (header_end, content_length) = loop {
            let read = socket.read(&mut buffer).await.expect("read request");
            if read == 0 {
                return;
            }
            request.extend_from_slice(&buffer[..read]);
            let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&request[..header_end]).to_string();
            let content_length = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            break (header_end, content_length);
        };
        let body_start = header_end + 4;
        while request.len() < body_start + content_length {
            let read = socket.read(&mut buffer).await.expect("read request body");
            if read == 0 {
                return;
            }
            request.extend_from_slice(&buffer[..read]);
        }
        let head = String::from_utf8_lossy(&request[..header_end]).to_string();
        let mut lines = head.lines();
        let request_line = lines.next().unwrap_or_default().to_string();
        let headers = lines
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                Some((name.trim().to_ascii_lowercase(), value.trim().to_string()))
            })
            .collect();
        let body = serde_json::from_slice(&request[body_start..body_start + content_length])
            .unwrap_or(serde_json::Value::Null);
        // The receiver may have been dropped by a test that only needs the
        // response; the capture is then simply unused.
        if tx
            .send(CapturedRequest {
                request_line,
                headers,
                body,
            })
            .is_err()
        {
            tracing::debug!("SSE test server capture receiver dropped");
        }

        let reason = if status == 200 { "OK" } else { "Error" };
        let head = format!(
            "HTTP/1.1 {status} {reason}\r\ncontent-type: {content_type}\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n"
        );
        if socket.write_all(head.as_bytes()).await.is_err() {
            return;
        }
        for frame in frames {
            if !frame.delay.is_zero() {
                tokio::time::sleep(frame.delay).await;
            }
            if socket.write_all(frame.frame.as_bytes()).await.is_err() {
                return;
            }
            if socket.flush().await.is_err() {
                return;
            }
        }
        if let Err(error) = socket.shutdown().await {
            tracing::debug!(%error, "SSE test server shutdown failed");
        }
    });
    (base_url, rx)
}

/// Await the captured request with a bounded wait.
pub(crate) async fn captured(rx: oneshot::Receiver<CapturedRequest>) -> CapturedRequest {
    tokio::time::timeout(Duration::from_secs(10), rx)
        .await
        .expect("loopback server must observe a request")
        .expect("captured request")
}

/// A sink recording every text and reasoning delta, in order, plus one
/// ordered log of text deltas and streamed tool calls (`text:<delta>` /
/// `tool:<call id>`) so a test can prove a call was announced mid-stream.
#[derive(Default)]
pub(crate) struct DeltaRecorder {
    pub(crate) text: std::sync::Mutex<Vec<String>>,
    pub(crate) reasoning: std::sync::Mutex<Vec<String>>,
    pub(crate) events: std::sync::Mutex<Vec<String>>,
}

impl DeltaRecorder {
    pub(crate) fn text(&self) -> Vec<String> {
        self.text
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub(crate) fn reasoning(&self) -> Vec<String> {
        self.reasoning
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub(crate) fn events(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn log(&self, entry: String) {
        self.events
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(entry);
    }
}

#[async_trait::async_trait]
impl crate::provider::CompletionStreamSink for DeltaRecorder {
    async fn text_delta(&self, delta: String) {
        self.log(format!("text:{delta}"));
        self.text
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(delta);
    }

    async fn tool_call_ready(&self, call: crate::provider::ToolCall) {
        self.log(format!("tool:{}", call.id));
    }

    async fn reasoning_delta(&self, delta: String) {
        self.reasoning
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(delta);
    }
}
