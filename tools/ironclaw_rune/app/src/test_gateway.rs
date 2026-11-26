//! Tests: a scripted loopback gateway. Each request takes the next scripted
//! answer (the last one repeats), after that answer's delay, so a test can
//! hold a request in flight, fail one, then recover. It records each
//! request's path and the most that were in flight at once.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use zlo_client::{Backend, Client, Connection, Token};

#[derive(Clone)]
pub struct Answer {
    pub status: u16,
    pub body: String,
    pub delay: Duration,
    /// Close the socket without sending a response (simulates a transport error).
    pub drop_conn: bool,
}

impl Answer {
    pub fn ok(body: impl Into<String>) -> Self {
        Answer {
            status: 200,
            body: body.into(),
            delay: Duration::ZERO,
            drop_conn: false,
        }
    }

    pub fn fail() -> Self {
        Answer {
            status: 503,
            body: r#"{"error":"unavailable"}"#.into(),
            delay: Duration::ZERO,
            drop_conn: false,
        }
    }

    pub fn drop_connection() -> Self {
        Answer {
            status: 0,
            body: String::new(),
            delay: Duration::ZERO,
            drop_conn: true,
        }
    }

    pub fn after(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

#[derive(Default)]
struct Shared {
    script: Mutex<VecDeque<Answer>>,
    /// Each request's path, in order.
    paths: Mutex<Vec<String>>,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    /// The inbox push stream (`/inbox/events`): `None` answers 404 as a
    /// backend without the route does; `Some` holds the open connections.
    /// Never takes a scripted answer.
    inbox_events: Mutex<Option<Vec<std::sync::mpsc::Sender<String>>>>,
    /// Fixed answers by path suffix ([`Gateway::route`]).
    routes: Mutex<Vec<(String, Answer)>>,
}

/// One `/inbox/events` connection: 404 when the route is off, else an open
/// event stream fed by [`Gateway::hint_inbox_changed`] until the client goes.
fn serve_inbox_events(shared: &Shared, mut sock: std::net::TcpStream) {
    let rx = {
        let mut open = shared
            .inbox_events
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let Some(open) = open.as_mut() else {
            let _ =
                sock.write_all(b"HTTP/1.1 404 X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        open.push(tx);
        rx
    };
    if sock
        .write_all(
            b"HTTP/1.1 200 X\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n: connected\n\n",
        )
        .is_err()
    {
        return;
    }
    while let Ok(frame) = rx.recv() {
        if sock.write_all(frame.as_bytes()).is_err() || sock.flush().is_err() {
            return;
        }
    }
}

pub struct Gateway {
    pub base_url: String,
    shared: Arc<Shared>,
}

impl Gateway {
    pub fn start(script: Vec<Answer>) -> Gateway {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let base_url = format!("http://{}", listener.local_addr().expect("address"));
        let shared = Arc::new(Shared {
            script: Mutex::new(script.into()),
            ..Shared::default()
        });
        let serving = shared.clone();
        std::thread::spawn(move || {
            for sock in listener.incoming() {
                let Ok(mut sock) = sock else { return };
                let shared = serving.clone();
                std::thread::spawn(move || {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut chunk) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let head = String::from_utf8_lossy(&buf);
                    let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                    shared
                        .paths
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(path.clone());
                    if path.contains("/inbox/events") {
                        return serve_inbox_events(&shared, sock);
                    }
                    let now = shared.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    shared.max_in_flight.fetch_max(now, Ordering::SeqCst);
                    let routed = shared
                        .routes
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .iter()
                        .find(|(suffix, _)| path.ends_with(suffix.as_str()))
                        .map(|(_, answer)| answer.clone());
                    let answer = if routed.is_some() {
                        routed
                    } else if path.contains("/config-sync/") {
                        // Never a scripted answer: a backend without the
                        // routes, unless a test serves them.
                        Some(Answer {
                            status: 404,
                            ..Answer::ok("")
                        })
                    } else {
                        let mut script =
                            shared.script.lock().unwrap_or_else(PoisonError::into_inner);
                        if script.len() > 1 {
                            script.pop_front()
                        } else {
                            script.front().cloned()
                        }
                    }
                    .unwrap_or_else(Answer::fail);
                    std::thread::sleep(answer.delay);
                    shared.in_flight.fetch_sub(1, Ordering::SeqCst);
                    if answer.drop_conn {
                        return;
                    }
                    let reply = format!(
                        "HTTP/1.1 {} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        answer.status,
                        answer.body.len(),
                        answer.body
                    );
                    let _ = sock.write_all(reply.as_bytes());
                });
            }
        });
        Gateway { base_url, shared }
    }

    /// Serve the inbox push stream from now on (a backend that has it).
    pub fn serve_inbox_events(&self) {
        *self
            .shared
            .inbox_events
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(Vec::new());
    }

    /// Send one `inbox_changed` hint to every open inbox stream.
    pub fn hint_inbox_changed(&self) {
        let frame = "event: inbox_changed\ndata: {\"kind\":\"inbox_changed\"}\n\n".to_string();
        if let Some(open) = self
            .shared
            .inbox_events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_mut()
        {
            open.retain(|tx| tx.send(frame.clone()).is_ok());
        }
    }

    /// How many inbox streams are open now.
    pub fn inbox_streams_open(&self) -> usize {
        self.hint_keepalive();
        self.shared
            .inbox_events
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map_or(0, Vec::len)
    }

    /// Comment frames tell a closed connection apart: the first write to
    /// one still succeeds, a later one fails and its writer thread ends,
    /// which the next send to it then sees.
    fn hint_keepalive(&self) {
        for _ in 0..4 {
            if let Some(open) = self
                .shared
                .inbox_events
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .as_mut()
            {
                open.retain(|tx| tx.send(": ping\n\n".to_string()).is_ok());
            }
            std::thread::sleep(Duration::from_millis(80));
        }
    }

    /// Answer every request whose path ends with `suffix` with `answer`,
    /// from now on and without touching the script.
    pub fn route(&self, suffix: &str, answer: Answer) {
        let mut routes = self
            .shared
            .routes
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        routes.retain(|(s, _)| s != suffix);
        routes.push((suffix.to_string(), answer));
    }

    /// Replace what the next requests get.
    pub fn script(&self, script: Vec<Answer>) {
        *self
            .shared
            .script
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = script.into();
    }

    /// Requests whose path ends with `suffix`.
    pub fn requests_to(&self, suffix: &str) -> usize {
        self.shared
            .paths
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|p| p.ends_with(suffix))
            .count()
    }

    pub fn paths(&self) -> Vec<String> {
        self.shared
            .paths
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub fn max_in_flight(&self) -> usize {
        self.shared.max_in_flight.load(Ordering::SeqCst)
    }

    pub fn backend(&self) -> Backend {
        let conn = Connection {
            base_url: self.base_url.clone(),
            token: Token::new("test-token").expect("token"),
            token_source: "test".into(),
        };
        Backend::new(Client::new(conn).expect("client")).expect("backend")
    }
}
