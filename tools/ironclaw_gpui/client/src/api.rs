//! Typed calls to the WebChat v2 routes (`/api/webchat/v2/*`).
//!
//! Every mutation carries a fresh `client_action_id`, the gateway's
//! idempotency key, exactly like the SPA.

use std::sync::Arc;
use std::time::Duration;

use ironclaw_product_contracts::inbound_requests::{
    ProductCancelRunRequest, ProductCreateThreadRequest, ProductInboundAttachment,
    ProductResolveGateRequest, ProductRetryRunRequest, ProductSubmitTurnRequest,
};
use ironclaw_product_contracts::product_wire::RebornProductCommandListResponse;
use reqwest::{Method, RequestBuilder};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::connection::Connection;
use crate::error::ClientError;
use crate::wire::{
    ATTACHMENTS_ONLY_CONTENT, CommandOutcome, CreatedThread, SessionInfo, SubmitAck, ThreadPage,
    TimelinePage,
};

pub(crate) const V2: &str = "/api/webchat/v2";

/// Cheap to clone; share one per app.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

struct Inner {
    http: reqwest::Client,
    conn: Connection,
}

/// How a person answered a gate (approval or credential prompt).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateDecision {
    Approve { always: bool },
    Decline,
}

pub fn client_action_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Percent-encode one path segment (ids are opaque to the client).
pub(crate) fn seg(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for b in raw.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

impl Client {
    pub fn new(conn: Connection) -> Result<Self, ClientError> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .user_agent(concat!("ironclaw-desktop/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            inner: Arc::new(Inner { http, conn }),
        })
    }

    pub fn connection(&self) -> &Connection {
        &self.inner.conn
    }

    pub(crate) fn request(&self, method: Method, path: &str) -> RequestBuilder {
        let url = format!("{}{}", self.inner.conn.base_url, path);
        self.inner
            .http
            .request(method, url)
            .bearer_auth(self.inner.conn.token.expose())
    }

    pub(crate) async fn send<T: DeserializeOwned>(
        &self,
        req: RequestBuilder,
    ) -> Result<T, ClientError> {
        let resp = req
            .header(reqwest::header::ACCEPT, "application/json")
            .timeout(Duration::from_secs(30))
            .send()
            .await?;
        let status = resp.status();
        let text = resp.text().await?;
        if !status.is_success() {
            return Err(ClientError::from_status(status.as_u16(), &text));
        }
        serde_json::from_str(&text).map_err(|e| ClientError::Decode(e.to_string()))
    }

    /// For routes that answer 204 with no body.
    pub(crate) async fn send_unit(&self, req: RequestBuilder) -> Result<(), ClientError> {
        let resp = req.timeout(Duration::from_secs(30)).send().await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await?;
            return Err(ClientError::from_status(status.as_u16(), &text));
        }
        Ok(())
    }

    pub(crate) async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        self.send(self.request(Method::GET, path)).await
    }

    pub(crate) async fn post<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, ClientError> {
        self.send(self.request(Method::POST, path).json(body)).await
    }

    // --- session + threads ---

    pub async fn session(&self) -> Result<SessionInfo, ClientError> {
        self.get(&format!("{V2}/session")).await
    }

    pub async fn list_threads(
        &self,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<ThreadPage, ClientError> {
        let mut path = format!("{V2}/threads?limit={limit}");
        if let Some(cursor) = cursor {
            path.push_str(&format!("&cursor={}", seg(cursor)));
        }
        self.get(&path).await
    }

    pub async fn create_thread(&self) -> Result<CreatedThread, ClientError> {
        let body = ProductCreateThreadRequest {
            client_action_id: Some(client_action_id()),
            ..Default::default()
        };
        self.post(&format!("{V2}/threads"), &body).await
    }

    pub async fn delete_thread(&self, thread_id: &str) -> Result<serde_json::Value, ClientError> {
        self.send(self.request(Method::DELETE, &format!("{V2}/threads/{}", seg(thread_id))))
            .await
    }

    pub async fn timeline(&self, thread_id: &str, limit: u32) -> Result<TimelinePage, ClientError> {
        self.get(&format!(
            "{V2}/threads/{}/timeline?limit={limit}",
            seg(thread_id)
        ))
        .await
    }

    // --- turns + runs ---

    /// Send a user message through the deployment's session channel
    /// (`session_channel_extension_id` from `/session`).
    pub async fn send_message(
        &self,
        channel: &str,
        thread_id: &str,
        content: &str,
        attachments: Vec<ProductInboundAttachment>,
    ) -> Result<SubmitAck, ClientError> {
        // The gateway wants content; an attachments-only send uses the same
        // placeholder as the web UI, which both clients hide again.
        let content = if content.trim().is_empty() && !attachments.is_empty() {
            ATTACHMENTS_ONLY_CONTENT
        } else {
            content
        };
        let body = ProductSubmitTurnRequest {
            client_action_id: Some(client_action_id()),
            thread_id: Some(thread_id.to_string()),
            content: Some(content.to_string()),
            attachments,
            ..Default::default()
        };
        self.post(&format!("{V2}/channels/{}/messages", seg(channel)), &body)
            .await
    }

    pub async fn cancel_run(
        &self,
        thread_id: &str,
        run_id: &str,
    ) -> Result<serde_json::Value, ClientError> {
        let body = ProductCancelRunRequest {
            client_action_id: Some(client_action_id()),
            ..Default::default()
        };
        self.post(
            &format!(
                "{V2}/threads/{}/runs/{}/cancel",
                seg(thread_id),
                seg(run_id)
            ),
            &body,
        )
        .await
    }

    pub async fn retry_run(
        &self,
        thread_id: &str,
        run_id: &str,
    ) -> Result<serde_json::Value, ClientError> {
        let body = ProductRetryRunRequest {
            client_action_id: Some(client_action_id()),
            ..Default::default()
        };
        self.post(
            &format!("{V2}/threads/{}/runs/{}/retry", seg(thread_id), seg(run_id)),
            &body,
        )
        .await
    }

    pub async fn resolve_gate(
        &self,
        thread_id: &str,
        run_id: &str,
        gate_ref: &str,
        decision: GateDecision,
    ) -> Result<serde_json::Value, ClientError> {
        let (resolution, always) = match decision {
            GateDecision::Approve { always } => ("approved", Some(always)),
            GateDecision::Decline => ("declined", None),
        };
        let body = ProductResolveGateRequest {
            client_action_id: Some(client_action_id()),
            resolution: Some(resolution.to_string()),
            always,
            ..Default::default()
        };
        self.post(
            &format!(
                "{V2}/threads/{}/runs/{}/gates/{}/resolve",
                seg(thread_id),
                seg(run_id),
                seg(gate_ref)
            ),
            &body,
        )
        .await
    }

    // --- slash commands ---

    /// The commands this caller may run, in registry order.
    pub async fn list_commands(&self) -> Result<RebornProductCommandListResponse, ClientError> {
        self.get(&format!("{V2}/commands")).await
    }

    /// Run one raw slash-command line (`/status`, `/model use x`) in a thread.
    pub async fn execute_command(
        &self,
        thread_id: &str,
        text: &str,
    ) -> Result<CommandOutcome, ClientError> {
        self.post(
            &format!("{V2}/threads/{}/commands", seg(thread_id)),
            &serde_json::json!({ "text": text }),
        )
        .await
    }

    /// The bytes of one landed attachment.
    pub async fn attachment_bytes(
        &self,
        thread_id: &str,
        message_id: &str,
        attachment_id: &str,
    ) -> Result<Vec<u8>, ClientError> {
        let path = format!(
            "{V2}/threads/{}/messages/{}/attachments/{}",
            seg(thread_id),
            seg(message_id),
            seg(attachment_id)
        );
        let resp = self
            .request(Method::GET, &path)
            .timeout(Duration::from_secs(60))
            .send()
            .await?;
        let status = resp.status();
        let bytes = resp.bytes().await?;
        if !status.is_success() {
            return Err(ClientError::from_status(
                status.as_u16(),
                &String::from_utf8_lossy(&bytes),
            ));
        }
        Ok(bytes.to_vec())
    }
}
