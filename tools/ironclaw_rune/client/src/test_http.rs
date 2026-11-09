//! A tiny loopback HTTP/1.1 server for the client's tests: one request per
//! connection, every request answered with the same status and body.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// What the mock server saw: method + path, the Authorization header and
/// the body.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    pub line: String,
    pub auth: Option<String>,
    pub body: String,
}

/// A one-request-per-connection HTTP/1.1 server on loopback that answers
/// every request with `status` and `body`.
pub async fn serve(status: u16, body: &'static str) -> (String, Arc<Mutex<Vec<Seen>>>) {
    serve_routes(vec![("", status, body)]).await
}

/// Answer by the first route whose path the request line contains (404
/// when none does).
pub async fn serve_routes(
    routes: Vec<(&'static str, u16, &'static str)>,
) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let log = log.clone();
            let routes = routes.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head, rest) = loop {
                    let n = sock.read(&mut chunk).await.unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break (
                            String::from_utf8_lossy(&buf[..i]).to_string(),
                            buf[i + 4..].to_vec(),
                        );
                    }
                };
                let len = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                let mut body_bytes = rest;
                while body_bytes.len() < len {
                    let n = sock.read(&mut chunk).await.unwrap();
                    body_bytes.extend_from_slice(&chunk[..n]);
                }
                let auth = head.lines().find_map(|l| {
                    l.split_once(':')
                        .filter(|(k, _)| k.eq_ignore_ascii_case("authorization"))
                        .map(|(_, v)| v.trim().to_string())
                });
                log.lock().unwrap().push(Seen {
                    line: head.lines().next().unwrap_or_default().to_string(),
                    auth,
                    body: String::from_utf8_lossy(&body_bytes).to_string(),
                });
                let request_line = head.lines().next().unwrap_or_default().to_string();
                let (status, body) = routes
                    .iter()
                    .find(|(path, _, _)| request_line.contains(path))
                    .map_or((404, "{}"), |(_, status, body)| (*status, *body));
                let reason = if status < 300 { "OK" } else { "ERR" };
                let resp = format!(
                    "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                sock.write_all(resp.as_bytes()).await.unwrap();
            });
        }
    });
    (format!("http://127.0.0.1:{}", addr.port()), seen)
}
