//! Live inbox invalidation stream over SSE (`GET /api/webchat/v2/inbox/events`).
//!
//! Emits payload-light [`InboxChangedEvent`] hints whenever the caller's inbox
//! list or unread notifications may have changed. Follows the same reconnect
//! discipline as [`crate::stream`], except `404 Not Found` (an older backend
//! without the route) reports [`InboxStreamMessage::Unsupported`] and stops
//! without retrying.

use std::time::Duration;

use eventsource_stream::Eventsource;
use futures::StreamExt;
use ironclaw_product_contracts::inbox::{INBOX_CHANGED_EVENT_KIND, InboxChangedEvent};
use reqwest::{Method, StatusCode};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::api::{Client, V2, seg};
use crate::stream::{
    BACKOFF_STEPS, STABLE_AFTER, StreamStatus, jitter, wait_for_listener, with_activity_timeout,
};
use crate::wire::StreamErrorFrame;

/// Server sends a keep-alive comment every ~25 s; 75 s of silence means a dead
/// connection.
const INBOX_ACTIVITY_TIMEOUT: Duration = Duration::from_secs(75);
const CHANNEL_DEPTH: usize = 32;

#[derive(Debug, Clone)]
pub enum InboxStreamMessage {
    Status(StreamStatus),
    Changed(InboxChangedEvent),
    /// The backend answered 404 (older version without `/inbox/events`).
    /// The stream stops cleanly without retrying.
    Unsupported,
    /// Terminal refusal (401/403 or non-retryable stream_error).
    Closed {
        reason: String,
    },
}

/// Owns the background SSE task; dropping it cancels the subscription.
pub struct InboxEventStream {
    task: JoinHandle<()>,
}

impl Drop for InboxEventStream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Client {
    /// Subscribe to caller-scoped inbox invalidation hints. Must be called
    /// inside a tokio runtime (see [`crate::Backend::subscribe_inbox`]).
    pub fn subscribe_inbox(&self) -> (InboxEventStream, mpsc::Receiver<InboxStreamMessage>) {
        let (tx, rx) = mpsc::channel(CHANNEL_DEPTH);
        let task = tokio::spawn(run(self.clone(), tx));
        (InboxEventStream { task }, rx)
    }
}

enum Ended {
    Retry {
        stable: bool,
        retry_after: Option<u64>,
        listener_down: bool,
    },
    Unsupported,
    Stop(Option<String>),
}

async fn run(client: Client, tx: mpsc::Sender<InboxStreamMessage>) {
    let connection_id = format!("desktop-inbox-{}", uuid::Uuid::new_v4().simple());
    let mut generation: u64 = 0;
    let mut attempt: u32 = 0;

    if tx
        .send(InboxStreamMessage::Status(StreamStatus::Connecting))
        .await
        .is_err()
    {
        return;
    }

    loop {
        generation += 1;
        let ended = connect_once(&client, &connection_id, generation, &tx).await;

        let (retry_after, listener_down) = match ended {
            Ended::Unsupported => {
                let _ = tx.send(InboxStreamMessage::Unsupported).await;
                return;
            }
            Ended::Stop(reason) => {
                if let Some(reason) = reason {
                    let _ = tx.send(InboxStreamMessage::Closed { reason }).await;
                }
                return;
            }
            Ended::Retry {
                stable,
                retry_after,
                listener_down,
            } => {
                if stable {
                    attempt = 0;
                }
                (retry_after, listener_down)
            }
        };

        let step = BACKOFF_STEPS[(attempt as usize).min(BACKOFF_STEPS.len() - 1)];
        let delay = retry_after.unwrap_or(step).max(step);
        attempt = attempt.saturating_add(1);
        let status = StreamStatus::Reconnecting {
            attempt,
            delay_secs: delay,
        };
        if tx.send(InboxStreamMessage::Status(status)).await.is_err() {
            return;
        }
        let wait = jitter(Duration::from_secs(delay));
        if listener_down {
            wait_for_listener(&client, wait).await;
        } else {
            tokio::time::sleep(wait).await;
        }
    }
}

async fn connect_once(
    client: &Client,
    connection_id: &str,
    generation: u64,
    tx: &mpsc::Sender<InboxStreamMessage>,
) -> Ended {
    let path = format!(
        "{V2}/inbox/events?connection_id={}&connection_generation={generation}",
        seg(connection_id),
    );
    let req = client
        .request(Method::GET, &path)
        .header(reqwest::header::ACCEPT, "text/event-stream");

    let resp = match req.send().await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::debug!(error = %e, "inbox event stream connect failed");
            return Ended::Retry {
                stable: false,
                retry_after: None,
                listener_down: e.is_connect(),
            };
        }
    };

    match resp.status() {
        StatusCode::OK => {}
        StatusCode::NOT_FOUND => {
            tracing::debug!("inbox event stream returned 404; falling back to polling");
            return Ended::Unsupported;
        }
        StatusCode::NO_CONTENT => {
            return Ended::Retry {
                stable: false,
                retry_after: None,
                listener_down: false,
            };
        }
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            return Ended::Stop(Some(format!(
                "the gateway refused the inbox stream ({})",
                resp.status()
            )));
        }
        status => {
            let retry_after = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok());
            tracing::debug!(%status, "inbox event stream rejected, retrying");
            return Ended::Retry {
                stable: false,
                retry_after,
                listener_down: false,
            };
        }
    }

    if tx
        .send(InboxStreamMessage::Status(StreamStatus::Live))
        .await
        .is_err()
    {
        return Ended::Stop(None);
    }
    let connected_at = Instant::now();
    let events = with_activity_timeout(resp.bytes_stream(), INBOX_ACTIVITY_TIMEOUT).eventsource();
    futures::pin_mut!(events);

    loop {
        let next = events.next().await;
        let event = match next {
            Some(Ok(event)) => event,
            Some(Err(e)) => {
                tracing::debug!(error = %e, "inbox event stream read failed");
                break;
            }
            None => break,
        };

        if event.event == "stream_error" {
            let frame: StreamErrorFrame =
                serde_json::from_str(&event.data).unwrap_or(StreamErrorFrame {
                    error: None,
                    kind: None,
                    retryable: true,
                });
            if frame.retryable {
                break;
            }
            let reason = frame
                .kind
                .or(frame.error)
                .unwrap_or_else(|| "inbox stream error".into());
            return Ended::Stop(Some(reason));
        }

        let parsed = match serde_json::from_str::<InboxChangedEvent>(&event.data) {
            Ok(ev) if ev.kind == INBOX_CHANGED_EVENT_KIND => Some(ev),
            Ok(_) => None,
            Err(e) => {
                tracing::debug!(event = %event.event, error = %e, "ignoring undecodable inbox SSE frame");
                None
            }
        };
        if let Some(ev) = parsed
            && tx.send(InboxStreamMessage::Changed(ev)).await.is_err()
        {
            return Ended::Stop(None);
        }
    }

    Ended::Retry {
        stable: connected_at.elapsed() >= STABLE_AFTER,
        retry_after: None,
        listener_down: false,
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    use super::*;
    use crate::{Connection, Token};

    fn start_raw(response: String) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        std::thread::spawn(move || {
            if let Ok((mut sock, _)) = listener.accept() {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf);
                let _ = sock.write_all(response.as_bytes());
            }
        });
        format!("http://{addr}")
    }

    fn client(base_url: String) -> Client {
        Client::new(Connection {
            base_url,
            token: Token::new("test-token").expect("token"),
            token_source: "test".into(),
        })
        .expect("client")
    }

    #[tokio::test]
    async fn parses_inbox_changed_hint_from_sse_stream() {
        let body = ": connected\n\nevent: inbox_changed\ndata: {\"kind\":\"inbox_changed\",\"thread_ids\":[\"t-1\"]}\n\n";
        let resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let c = client(start_raw(resp));
        let (_stream, mut rx) = c.subscribe_inbox();

        let mut saw_live = false;
        let mut saw_hint = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
                Ok(Some(InboxStreamMessage::Status(StreamStatus::Live))) => saw_live = true,
                Ok(Some(InboxStreamMessage::Changed(ev))) => {
                    assert_eq!(ev.kind, INBOX_CHANGED_EVENT_KIND);
                    assert_eq!(ev.thread_ids, vec!["t-1".to_string()]);
                    saw_hint = true;
                    break;
                }
                Ok(Some(_)) => {}
                _ => break,
            }
        }
        assert!(saw_live, "expected Live status");
        assert!(saw_hint, "expected Changed event");
    }

    #[tokio::test]
    async fn not_found_reports_unsupported_and_stops_without_retrying() {
        let resp =
            "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_string();
        let c = client(start_raw(resp));
        let (_stream, mut rx) = c.subscribe_inbox();

        let mut saw_unsupported = false;
        while let Ok(Some(msg)) = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await {
            if matches!(msg, InboxStreamMessage::Unsupported) {
                saw_unsupported = true;
                break;
            }
        }
        assert!(saw_unsupported, "expected Unsupported on 404");
        // Stream task must terminate rather than retry.
        assert!(
            tokio::time::timeout(Duration::from_millis(300), rx.recv())
                .await
                .expect("stream terminates")
                .is_none()
        );
    }
}
