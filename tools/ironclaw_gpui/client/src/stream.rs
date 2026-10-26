//! Live thread events over SSE, with the reconnect behavior `CONTRACT.md`
//! specifies for the browser:
//!
//! - resume with `Last-Event-ID` (keep-alives carry no id, so the last real
//!   frame stays the resume point);
//! - jittered 1/2/4/8/16/30 s backoff, honoring `Retry-After`;
//! - backoff resets after the stream stayed open 15 s, including idle streams;
//! - one stable `connection_id` per subscription with an increasing
//!   `connection_generation`, so a reconnect supersedes its own old slot
//!   instead of consuming another of the 3 per-user stream slots;
//! - an activity watchdog: the server sends `keep_alive` every 15 s, so 45 s
//!   of silence means a dead connection.
//!
//! One desktop-only shortcut: when the connect itself fails (nothing listens,
//! typically a gateway restart) the backoff wait ends early as soon as a
//! plain TCP connect to the gateway succeeds, probed once a second. A refused
//! connect costs the server nothing, and HTTP-level rejections (429/503 with
//! `Retry-After`) still wait the full backoff.
//!
//! Dropping the [`EventStream`] handle stops the task and frees the slot.

use std::time::Duration;

use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::{Method, StatusCode};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::api::{Client, V2, seg};
use crate::wire::{StreamErrorFrame, StreamFrame};

const BACKOFF_STEPS: [u64; 6] = [1, 2, 4, 8, 16, 30];
const STABLE_AFTER: Duration = Duration::from_secs(15);
const ACTIVITY_TIMEOUT: Duration = Duration::from_secs(45);
const CHANNEL_DEPTH: usize = 256;
const LISTENER_PROBE: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamStatus {
    Connecting,
    Live,
    Reconnecting { attempt: u32, delay_secs: u64 },
}

#[derive(Debug, Clone)]
pub enum StreamMessage {
    Status(StreamStatus),
    Frame(Box<StreamFrame>),
    /// A frame the client could not decode. The stream continues.
    Undecodable {
        event: String,
        error: String,
    },
    /// Terminal: the stream stopped and will not reconnect.
    Closed {
        reason: String,
    },
}

/// Owns the background task; dropping it cancels the subscription.
pub struct EventStream {
    task: JoinHandle<()>,
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Client {
    /// Subscribe to one thread. Must be called inside the tokio runtime
    /// (see [`crate::Backend::subscribe`]).
    pub fn subscribe(&self, thread_id: &str) -> (EventStream, mpsc::Receiver<StreamMessage>) {
        let (tx, rx) = mpsc::channel(CHANNEL_DEPTH);
        let task = tokio::spawn(run(self.clone(), thread_id.to_string(), tx));
        (EventStream { task }, rx)
    }
}

enum Ended {
    /// Normal end (server lifetime cap, network drop). Reconnect.
    Retry {
        stable: bool,
        retry_after: Option<u64>,
        /// The TCP connect failed: nobody is listening yet.
        listener_down: bool,
    },
    /// The receiver is gone or the gateway refused for good.
    Stop(Option<String>),
}

async fn run(client: Client, thread_id: String, tx: mpsc::Sender<StreamMessage>) {
    let connection_id = format!("desktop-{}", uuid::Uuid::new_v4().simple());
    let mut generation: u64 = 0;
    let mut last_event_id: Option<String> = None;
    let mut attempt: u32 = 0;

    if tx
        .send(StreamMessage::Status(StreamStatus::Connecting))
        .await
        .is_err()
    {
        return;
    }
    loop {
        generation += 1;
        let ended = connect_once(
            &client,
            &thread_id,
            &connection_id,
            generation,
            &mut last_event_id,
            &tx,
        )
        .await;

        let (retry_after, listener_down) = match ended {
            Ended::Stop(reason) => {
                if let Some(reason) = reason {
                    let _ = tx.send(StreamMessage::Closed { reason }).await;
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
        if tx.send(StreamMessage::Status(status)).await.is_err() {
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

/// Sleep up to `max`, returning early once the gateway accepts TCP again.
async fn wait_for_listener(client: &Client, max: Duration) {
    let Some(addr) = listener_addr(&client.connection().base_url) else {
        tokio::time::sleep(max).await;
        return;
    };
    let deadline = Instant::now() + max;
    while Instant::now() < deadline {
        tokio::time::sleep(LISTENER_PROBE.min(deadline - Instant::now())).await;
        let probe = tokio::net::TcpStream::connect(addr.as_str());
        if matches!(tokio::time::timeout(LISTENER_PROBE, probe).await, Ok(Ok(_))) {
            return;
        }
    }
}

fn listener_addr(base_url: &str) -> Option<String> {
    let url = reqwest::Url::parse(base_url).ok()?;
    Some(format!(
        "{}:{}",
        url.host_str()?,
        url.port_or_known_default()?
    ))
}

async fn connect_once(
    client: &Client,
    thread_id: &str,
    connection_id: &str,
    generation: u64,
    last_event_id: &mut Option<String>,
    tx: &mpsc::Sender<StreamMessage>,
) -> Ended {
    let path = format!(
        "{V2}/threads/{}/events?connection_id={}&connection_generation={generation}",
        seg(thread_id),
        seg(connection_id),
    );
    let mut req = client
        .request(Method::GET, &path)
        .header(reqwest::header::ACCEPT, "text/event-stream");
    if let Some(id) = last_event_id.as_deref() {
        req = req.header("Last-Event-ID", id);
    }

    let resp = match req.send().await {
        Ok(resp) => resp,
        Err(e) => {
            tracing::debug!(error = %e, "event stream connect failed");
            return Ended::Retry {
                stable: false,
                retry_after: None,
                listener_down: e.is_connect(),
            };
        }
    };

    match resp.status() {
        StatusCode::OK => {}
        // An older generation of this same connection: nothing to do.
        StatusCode::NO_CONTENT => {
            return Ended::Retry {
                stable: false,
                retry_after: None,
                listener_down: false,
            };
        }
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => {
            return Ended::Stop(Some(format!(
                "the gateway refused the stream ({})",
                resp.status()
            )));
        }
        status => {
            let retry_after = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok());
            tracing::debug!(%status, "event stream rejected, retrying");
            return Ended::Retry {
                stable: false,
                retry_after,
                listener_down: false,
            };
        }
    }

    // HTTP 200 is enough to clear an old reconnect banner. An idle thread
    // need not emit a new application event before it is connected.
    if tx
        .send(StreamMessage::Status(StreamStatus::Live))
        .await
        .is_err()
    {
        return Ended::Stop(None);
    }
    let connected_at = Instant::now();
    let events = with_activity_timeout(resp.bytes_stream(), ACTIVITY_TIMEOUT).eventsource();
    futures::pin_mut!(events);

    loop {
        let next = events.next().await;
        let event = match next {
            Some(Ok(event)) => event,
            Some(Err(e)) => {
                tracing::debug!(error = %e, "event stream read failed");
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
                .unwrap_or_else(|| "stream error".into());
            return Ended::Stop(Some(reason));
        }

        let message = match serde_json::from_str::<StreamFrame>(&event.data) {
            Ok(frame) => {
                if !event.id.is_empty() {
                    *last_event_id = Some(event.id.clone());
                }
                StreamMessage::Frame(Box::new(frame))
            }
            Err(e) => StreamMessage::Undecodable {
                event: event.event.clone(),
                error: e.to_string(),
            },
        };
        if tx.send(message).await.is_err() {
            return Ended::Stop(None);
        }
    }

    Ended::Retry {
        stable: connected_at.elapsed() >= STABLE_AFTER,
        retry_after: None,
        listener_down: false,
    }
}

/// Watch raw transport activity, before the SSE parser discards comment heartbeats.
fn with_activity_timeout<S, T, E>(
    stream: S,
    silence: Duration,
) -> impl futures::Stream<Item = Result<T, std::io::Error>>
where
    S: futures::Stream<Item = Result<T, E>>,
    E: std::error::Error + Send + Sync + 'static,
{
    futures::stream::unfold(Some(Box::pin(stream)), move |stream| async move {
        let mut stream = stream?;
        match tokio::time::timeout(silence, stream.next()).await {
            Ok(Some(item)) => Some((item.map_err(std::io::Error::other), Some(stream))),
            Ok(None) => None,
            Err(_) => Some((
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "event stream went silent",
                )),
                None,
            )),
        }
    })
}

/// +/-20% jitter so many clients don't reconnect in lockstep.
fn jitter(base: Duration) -> Duration {
    let r = (uuid::Uuid::new_v4().as_u128() % 401) as f64 / 1000.0; // 0.0..=0.4
    base.mul_f64(0.8 + r)
}

#[cfg(test)]
mod tests {
    use super::{listener_addr, with_activity_timeout};
    use eventsource_stream::Eventsource;
    use futures::StreamExt;
    use std::time::Duration;

    #[tokio::test]
    async fn comment_heartbeats_keep_an_idle_stream_alive() {
        let comments = futures::stream::unfold((), |()| async {
            tokio::time::sleep(Duration::from_millis(5)).await;
            Some((Ok::<_, std::io::Error>(": heartbeat\n\n"), ()))
        });
        let events = with_activity_timeout(comments, Duration::from_millis(30)).eventsource();
        futures::pin_mut!(events);
        // Comments produce no application event, but they prove transport liveness.
        assert!(
            tokio::time::timeout(Duration::from_millis(100), events.next())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_silent_transport_still_times_out() {
        let bytes = futures::stream::pending::<Result<&str, std::io::Error>>();
        let watched = with_activity_timeout(bytes, Duration::from_millis(5));
        futures::pin_mut!(watched);
        assert_eq!(
            watched
                .next()
                .await
                .expect("watchdog result")
                .expect_err("silent stream")
                .kind(),
            std::io::ErrorKind::TimedOut
        );
        assert!(watched.next().await.is_none());
    }

    #[test]
    fn listener_addr_fills_default_ports() {
        assert_eq!(
            listener_addr("http://127.0.0.1:3000").as_deref(),
            Some("127.0.0.1:3000")
        );
        assert_eq!(
            listener_addr("http://localhost").as_deref(),
            Some("localhost:80")
        );
        assert_eq!(listener_addr("not a url"), None);
    }
}
