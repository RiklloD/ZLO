//! The remote-access routes of wire contract v1: the public descriptor,
//! redeeming a pairing code or a tailnet identity for a device token, and
//! the operator's pairing codes and device list.
//!
//! The public calls take an origin, not a [`Client`]: there is no token yet.
//! Every origin passes `environments::check_transport` first.

use std::time::Duration;

use ironclaw_product_contracts::environment_access::{
    DEVICES_PATH, DeviceList, ENVIRONMENT_DESCRIPTOR_PATH, EnvironmentDescriptor, PAIR_PATH,
    PAIR_TAILNET_PATH, PAIRING_CODES_PATH, PairClient, PairRequest, PairedDevice,
    PairingCodeIssued, PairingErrorCode, TailnetPairRequest,
};
use reqwest::Method;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::api::{Client, seg};
use crate::environments::check_transport;
use crate::error::ClientError;

/// Discovery probes give each peer this long.
pub const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);
const CALL_TIMEOUT: Duration = Duration::from_secs(15);

fn http() -> Result<reqwest::Client, ClientError> {
    Ok(reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .user_agent(concat!("zlo/", env!("CARGO_PKG_VERSION")))
        .redirect(crate::api::same_origin_redirects())
        .build()?)
}

fn origin(url: &str) -> Result<String, ClientError> {
    check_transport(url).map_err(|e| ClientError::Config(e.to_string()))
}

/// Largest body these calls read. Pairing and descriptor answers are a few
/// hundred bytes; discovery reads them from peers that are not trusted yet.
const MAX_BODY_BYTES: usize = 64 * 1024;

/// The body, read in chunks and refused past [`MAX_BODY_BYTES`].
async fn read_limited(resp: reqwest::Response) -> Result<String, ClientError> {
    use futures::StreamExt;
    if resp
        .content_length()
        .is_some_and(|n| n > MAX_BODY_BYTES as u64)
    {
        return Err(ClientError::Decode("the answer is too large".into()));
    }
    let mut body = Vec::new();
    let mut chunks = resp.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk?;
        if body.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(ClientError::Decode("the answer is too large".into()));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

async fn decode<T: DeserializeOwned>(resp: reqwest::Response) -> Result<T, ClientError> {
    let status = resp.status();
    let text = read_limited(resp).await?;
    if !status.is_success() {
        return Err(ClientError::from_status(status.as_u16(), &text));
    }
    serde_json::from_str(&text).map_err(|e| ClientError::Decode(e.to_string()))
}

/// `GET /.well-known/ironclaw/environment`, within `timeout`.
pub async fn fetch_descriptor(
    url: &str,
    timeout: Duration,
) -> Result<EnvironmentDescriptor, ClientError> {
    let origin = origin(url)?;
    let resp = http()?
        .get(format!("{origin}{ENVIRONMENT_DESCRIPTOR_PATH}"))
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(timeout)
        .send()
        .await?;
    decode(resp).await
}

/// The descriptor at an old saved address, which may be plain http over
/// the tailnet. Only for the one-time `gateway.json` move: the request is
/// public and carries no secret, and nothing is saved at a route that
/// fails the transport rule.
pub async fn fetch_public_descriptor(
    url: &str,
    timeout: Duration,
) -> Result<EnvironmentDescriptor, ClientError> {
    let url = url.trim().trim_end_matches('/');
    let scheme_ok = url
        .split_once("://")
        .is_some_and(|(s, _)| s.eq_ignore_ascii_case("http") || s.eq_ignore_ascii_case("https"));
    if !scheme_ok {
        return Err(ClientError::Config(format!("not a web address: {url}")));
    }
    let resp = http()?
        .get(format!("{url}{ENVIRONMENT_DESCRIPTOR_PATH}"))
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(timeout)
        .send()
        .await?;
    decode(resp).await
}

async fn post_public<B: Serialize>(
    url: &str,
    path: &str,
    body: &B,
) -> Result<PairedDevice, ClientError> {
    let origin = origin(url)?;
    let resp = http()?
        .post(format!("{origin}{path}"))
        .header(reqwest::header::ACCEPT, "application/json")
        .json(body)
        .timeout(CALL_TIMEOUT)
        .send()
        .await?;
    decode(resp).await
}

/// `POST /api/auth/pair`: a one-time code for a device token.
pub async fn redeem_code(
    url: &str,
    code: &str,
    client: PairClient,
) -> Result<PairedDevice, ClientError> {
    let body = PairRequest {
        code: code.to_string(),
        client,
    };
    post_public(url, PAIR_PATH, &body).await
}

/// `POST /api/auth/pair/tailnet`: one click for the owner's tailnet login.
pub async fn pair_tailnet(url: &str, client: PairClient) -> Result<PairedDevice, ClientError> {
    post_public(url, PAIR_TAILNET_PATH, &TailnetPairRequest { client }).await
}

/// The contract's pairing error code of a failed call, if it carries one.
pub fn pairing_error(error: &ClientError) -> Option<PairingErrorCode> {
    let ClientError::Api { body, .. } = error else {
        return None;
    };
    let code = body.error.as_deref()?;
    serde_json::from_value(serde_json::Value::String(code.to_string())).ok()
}

impl Client {
    /// `POST /api/auth/pairing-codes` (operator).
    pub async fn issue_pairing_code(&self) -> Result<PairingCodeIssued, ClientError> {
        self.send(
            self.request(Method::POST, PAIRING_CODES_PATH)
                .json(&serde_json::json!({})),
        )
        .await
    }

    /// `GET /api/auth/devices` (operator).
    pub async fn devices(&self) -> Result<DeviceList, ClientError> {
        self.get(DEVICES_PATH).await
    }

    /// `DELETE /api/auth/devices/{device_id}` (operator, idempotent).
    pub async fn revoke_device(&self, device_id: &str) -> Result<(), ClientError> {
        self.send_unit(self.request(
            Method::DELETE,
            &format!("{DEVICES_PATH}/{}", seg(device_id)),
        ))
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::{Connection, Token};
    use crate::test_http::serve;

    fn me() -> PairClient {
        PairClient {
            label: "Lorik's MacBook Air".into(),
            os: "macOS".into(),
            app_version: "0.1.0".into(),
        }
    }

    const PAIRED: &str = r#"{"environment_id":"0f6c2d0e-3b0a-4a51-9b7e-6b1c8f1f2a10","device_id":"0123456789abcdef0123456789abcdef","token":"icd_0123456789abcdef0123456789abcdef_s3cr3t"}"#;

    #[tokio::test]
    async fn the_descriptor_is_read_without_a_token() {
        let (url, seen) = serve(
            200,
            r#"{"environment_id":"e","label":"Studio","server_version":"0.41.0","protocol_version":1,"pairing":["one_time_code"],"tailnet_url":null}"#,
        )
        .await;
        let d = fetch_descriptor(&url, PROBE_TIMEOUT).await.unwrap();
        assert_eq!(d.label, "Studio");
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen[0].line,
            "GET /.well-known/ironclaw/environment HTTP/1.1"
        );
        assert!(seen[0].auth.is_none());
    }

    #[tokio::test]
    async fn a_code_is_redeemed_with_the_client_description() {
        let (url, seen) = serve(200, PAIRED).await;
        let paired = redeem_code(&url, "7KQ2-M9XD-4TRA", me()).await.unwrap();
        assert!(paired.token.starts_with("icd_"));
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].line, "POST /api/auth/pair HTTP/1.1");
        assert!(seen[0].auth.is_none(), "no token is sent to pair");
        let body: serde_json::Value = serde_json::from_str(&seen[0].body).unwrap();
        assert_eq!(body["code"], "7KQ2-M9XD-4TRA");
        assert_eq!(body["client"]["os"], "macOS");
        assert_eq!(body["client"]["label"], "Lorik's MacBook Air");
    }

    #[tokio::test]
    async fn tailnet_pairing_refusals_carry_their_code() {
        let (url, seen) = serve(403, r#"{"error":"not_owner","message":"ask the owner"}"#).await;
        let err = pair_tailnet(&url, me()).await.unwrap_err();
        assert_eq!(pairing_error(&err), Some(PairingErrorCode::NotOwner));
        assert_eq!(
            seen.lock().unwrap()[0].line,
            "POST /api/auth/pair/tailnet HTTP/1.1"
        );
        let body: serde_json::Value = serde_json::from_str(&seen.lock().unwrap()[0].body).unwrap();
        assert!(body.get("code").is_none());

        let (url, _) = serve(
            403,
            r#"{"error":"tailnet_pairing_disabled","message":"off"}"#,
        )
        .await;
        let err = pair_tailnet(&url, me()).await.unwrap_err();
        assert_eq!(
            pairing_error(&err),
            Some(PairingErrorCode::TailnetPairingDisabled)
        );
        let (url, _) = serve(400, r#"{"error":"invalid_code","message":"no"}"#).await;
        let err = redeem_code(&url, "7KQ2-M9XD-4TRA", me()).await.unwrap_err();
        assert_eq!(pairing_error(&err), Some(PairingErrorCode::InvalidCode));
    }

    #[tokio::test]
    async fn operator_calls_send_the_device_token() {
        let (url, seen) = serve(
            200,
            r#"{"devices":[{"device_id":"0123456789abcdef0123456789abcdef","label":"Air","os":"macOS","app_version":"0.1.0","paired_by":"one_time_code","created_at":"2026-11-08T20:00:00Z","last_seen_at":null,"revoked_at":null,"current":true}]}"#,
        )
        .await;
        let client = Client::new(Connection {
            base_url: url,
            token: Token::new("icd_dev_tok").unwrap(),
            token_source: "test".into(),
        })
        .unwrap();
        let list = client.devices().await.unwrap();
        assert!(list.devices[0].current);
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].line, "GET /api/auth/devices HTTP/1.1");
        assert_eq!(seen[0].auth.as_deref(), Some("Bearer icd_dev_tok"));
    }

    #[tokio::test]
    async fn revoke_and_issue_hit_their_routes() {
        let (url, seen) = serve(
            200,
            r#"{"code":"7KQ2-M9XD-4TRA","expires_at":"2026-11-08T21:05:00Z"}"#,
        )
        .await;
        let client = Client::new(Connection {
            base_url: url,
            token: Token::new("t").unwrap(),
            token_source: "test".into(),
        })
        .unwrap();
        let issued = client.issue_pairing_code().await.unwrap();
        assert_eq!(issued.code, "7KQ2-M9XD-4TRA");
        client
            .revoke_device("0123456789abcdef0123456789abcdef")
            .await
            .unwrap();
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].line, "POST /api/auth/pairing-codes HTTP/1.1");
        assert_eq!(
            seen[1].line,
            "DELETE /api/auth/devices/0123456789abcdef0123456789abcdef HTTP/1.1"
        );
    }

    #[tokio::test]
    async fn plain_http_to_another_machine_never_leaves_this_mac() {
        let err = redeem_code("http://100.101.2.3:3000", "7KQ2-M9XD-4TRA", me())
            .await
            .unwrap_err();
        assert!(matches!(err, ClientError::Config(_)), "{err:?}");
    }

    /// A loopback server that answers every request with `response` and
    /// counts the requests it got.
    async fn raw_server(response: String) -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 8192];
                let _ = sock.read(&mut buf).await;
                let _ = sock.write_all(response.as_bytes()).await;
            }
        });
        (port, hits)
    }

    #[tokio::test]
    async fn a_redirect_to_another_origin_never_carries_the_pairing_code() {
        let (elsewhere, hits) = raw_server(
            "HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}".into(),
        )
        .await;
        // Same loopback address, other port: another origin.
        let (port, _) = raw_server(format!(
            "HTTP/1.1 308 Permanent Redirect\r\nlocation: http://127.0.0.1:{elsewhere}/api/auth/pair\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
        ))
        .await;
        let err = redeem_code(&format!("http://127.0.0.1:{port}"), "7KQ2-M9XD-4TRA", me())
            .await
            .unwrap_err();
        assert!(
            matches!(err, ClientError::Api { status: 308, .. }),
            "{err:?}"
        );
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn an_oversized_descriptor_is_refused() {
        let body: &'static str = Box::leak("x".repeat(MAX_BODY_BYTES + 1).into_boxed_str());
        let (url, _) = serve(200, body).await;
        let err = fetch_descriptor(&url, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(matches!(err, ClientError::Decode(_)), "{err:?}");
    }
}
