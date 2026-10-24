//! Sign in with ChatGPT, plan usage for open-source and local apps.
//!
//! OpenAI's documented route for spending a user's ChatGPT plan from a
//! third-party app (developers.openai.com/siwc/token-sharing-open-source):
//! a browser OAuth flow with PKCE that registers this install as a
//! user-defined agent (`client_id=dynamic_agent_client` on first sign-in,
//! the issued `oaiapp_…` id afterwards), then inference on the public
//! Responses API (`https://api.openai.com/v1/responses`) with the access
//! token as a bearer key.
//!
//! This is the default mode of the `openai_codex` provider. The older route
//! (Codex CLI's client id against `chatgpt.com/backend-api/codex`) stays
//! reachable only by pointing `OPENAI_CODEX_API_URL` at that backend.

use std::time::Duration;

use base64::Engine as _;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::error::LlmError;
use crate::openai_codex_session::{OpenAiCodexSession, OpenAiCodexSessionManager};

/// Responses API base for plan usage; also the OAuth `resource`.
pub(crate) const PLAN_API_BASE_URL: &str = "https://api.openai.com/v1";
/// First-sign-in client id; the callback carries the issued one.
const DYNAMIC_CLIENT_ID: &str = "dynamic_agent_client";
/// App name OpenAI shows in ChatGPT settings → Apps.
const AGENT_NAME: &str = "IronClaw";
const ISSUER: &str = "https://auth.openai.com";
const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// The documented example port; any free port works when it is taken.
const PREFERRED_PORT: u16 = 1455;
const CALLBACK_PATH: &str = "/auth/callback";
/// How long a started sign-in waits for the browser.
const LOGIN_WINDOW: Duration = Duration::from_secs(600);

/// The plan grant kept with a session: which registration issued the tokens.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct PlanGrant {
    /// Issued client id (`oaiapp_…`), bound to the user and workspace.
    pub client_id: String,
    /// Granted scopes; plan usage needs `chatgpt.tokens.use.direct`.
    pub scope: String,
    /// Last ID token, sent back as `id_token_hint` on re-sign-in.
    #[serde(default)]
    pub id_token: Option<String>,
}

impl PlanGrant {
    pub(crate) fn plan_usage_granted(&self) -> bool {
        self.scope.split_whitespace().any(|s| s == PLAN_SCOPE)
    }
}

/// A started browser sign-in. Show `auth_url`, then hand this back to
/// [`OpenAiCodexSessionManager::complete_plan_login`].
pub struct PlanLoginStart {
    pub auth_url: String,
    listener: TcpListener,
    redirect_uri: String,
    client_id: String,
    state: String,
    nonce: String,
    verifier: String,
}

impl std::fmt::Debug for PlanLoginStart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlanLoginStart")
            .field("redirect_uri", &self.redirect_uri)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct PlanTokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    expires_in: u64,
    #[serde(default)]
    scope: String,
    #[serde(default)]
    id_token: Option<String>,
}

fn login_error(reason: impl Into<String>) -> LlmError {
    LlmError::SessionRenewalFailed {
        provider: "openai_codex".to_string(),
        reason: reason.into(),
    }
}

fn random_token() -> String {
    ironclaw_common::pkce::generate_code_verifier()
}

impl OpenAiCodexSessionManager {
    /// Whether this manager talks to the public API with plan tokens.
    pub fn is_plan_mode(&self) -> bool {
        self.config().is_plan_mode()
    }

    /// This install's stable `ext_agent_host_id`, created on first use and
    /// kept across sign-outs.
    async fn host_id(&self) -> Result<String, LlmError> {
        let path = self.config().session_path.with_extension("host_id");
        if let Ok(existing) = tokio::fs::read_to_string(&path).await {
            let existing = existing.trim();
            if existing.starts_with("urn:uuid:") {
                return Ok(existing.to_string());
            }
        }
        let id = format!("urn:uuid:{}", uuid::Uuid::new_v4());
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(LlmError::Io)?;
        }
        tokio::fs::write(&path, &id).await.map_err(LlmError::Io)?;
        Ok(id)
    }

    /// Step 1: bind the loopback callback and build the authorize URL.
    pub async fn begin_plan_login(&self) -> Result<PlanLoginStart, LlmError> {
        let host_id = self.host_id().await?;
        let listener = match TcpListener::bind(("127.0.0.1", PREFERRED_PORT)).await {
            Ok(listener) => listener,
            Err(_) => TcpListener::bind(("127.0.0.1", 0))
                .await
                .map_err(LlmError::Io)?,
        };
        let port = listener.local_addr().map_err(LlmError::Io)?.port();
        let redirect_uri = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");

        let prior = self.plan_grant().await;
        let client_id = prior
            .as_ref()
            .map(|g| g.client_id.clone())
            .unwrap_or_else(|| DYNAMIC_CLIENT_ID.to_string());
        let state = random_token();
        let nonce = random_token();
        let verifier = ironclaw_common::pkce::generate_code_verifier();
        let challenge = ironclaw_common::pkce::s256_challenge(verifier.as_bytes());

        let mut params: Vec<(&str, &str)> = vec![
            ("client_id", &client_id),
            ("ext_agent_host_id", &host_id),
            ("response_type", "code"),
            ("redirect_uri", &redirect_uri),
            ("scope", SCOPES),
            ("resource", PLAN_API_BASE_URL),
            ("state", &state),
            ("nonce", &nonce),
            ("code_challenge_method", "S256"),
            ("code_challenge", &challenge),
        ];
        if prior.is_none() {
            params.push(("agent_name_hint", AGENT_NAME));
        }
        let hint = prior.and_then(|g| g.id_token);
        if let Some(hint) = hint.as_deref() {
            params.push(("id_token_hint", hint));
        }
        let auth_url = url::Url::parse_with_params(
            &format!("{}/api/accounts/authorize", self.config().auth_endpoint),
            &params,
        )
        .map_err(|e| login_error(format!("authorize URL: {e}")))?;

        Ok(PlanLoginStart {
            auth_url: auth_url.to_string(),
            listener,
            redirect_uri,
            client_id,
            state,
            nonce,
            verifier,
        })
    }

    /// Step 2: wait for the browser callback, exchange the code, check the
    /// ID token and the plan scope, and save the session.
    pub async fn complete_plan_login(&self, start: PlanLoginStart) -> Result<(), LlmError> {
        let callback = tokio::time::timeout(LOGIN_WINDOW, wait_for_callback(&start.listener))
            .await
            .map_err(|_| login_error("ChatGPT sign-in timed out"))??;
        if callback.state.as_deref() != Some(start.state.as_str()) {
            return Err(login_error("sign-in state did not match"));
        }
        if let Some(error) = callback.error {
            return Err(login_error(format!("ChatGPT sign-in declined: {error}")));
        }
        let code = callback
            .code
            .ok_or_else(|| login_error("callback carried no code"))?;
        let client_id = match (start.client_id.as_str(), callback.client_id) {
            (DYNAMIC_CLIENT_ID, Some(issued)) => issued,
            (DYNAMIC_CLIENT_ID, None) => {
                return Err(login_error("registration returned no client id"));
            }
            (saved, Some(other)) if other != saved => {
                return Err(login_error("callback named a different client id"));
            }
            (saved, _) => saved.to_string(),
        };

        let resp = self
            .http()
            .post(format!(
                "{}/api/accounts/oauth/token",
                self.config().auth_endpoint
            ))
            .form(&[
                ("grant_type", "authorization_code"),
                ("client_id", client_id.as_str()),
                ("code", code.as_str()),
                ("code_verifier", start.verifier.as_str()),
                ("redirect_uri", start.redirect_uri.as_str()),
                ("resource", PLAN_API_BASE_URL),
            ])
            .send()
            .await
            .map_err(|e| login_error(format!("token exchange failed: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            tracing::debug!(%status, "ChatGPT plan token exchange rejected");
            return Err(login_error(format!("token exchange failed: HTTP {status}")));
        }
        let tokens: PlanTokenResponse = resp
            .json()
            .await
            .map_err(|e| login_error(format!("token response: {e}")))?;
        if let Some(id_token) = tokens.id_token.as_deref() {
            check_id_token(id_token, &client_id, &start.nonce)?;
        }
        let grant = PlanGrant {
            client_id,
            scope: tokens.scope.clone(),
            id_token: tokens.id_token.clone(),
        };
        let usage = grant.plan_usage_granted();
        self.store_plan_tokens(tokens, grant).await?;
        if !usage {
            return Err(login_error(
                "Signed in, but ChatGPT plan usage was not allowed. Sign in again and allow it.",
            ));
        }
        Ok(())
    }

    /// Refresh a plan session with its issued client id.
    pub(crate) async fn refresh_plan_tokens(
        &self,
        refresh_token: &str,
        grant: PlanGrant,
    ) -> Result<(), LlmError> {
        let resp = self
            .http()
            .post(format!(
                "{}/api/accounts/oauth/token",
                self.config().auth_endpoint
            ))
            .form(&[
                ("grant_type", "refresh_token"),
                ("client_id", grant.client_id.as_str()),
                ("refresh_token", refresh_token),
                ("resource", PLAN_API_BASE_URL),
            ])
            .send()
            .await
            .map_err(|e| login_error(format!("token refresh request failed: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            return Err(login_error(format!("token refresh failed: HTTP {status}")));
        }
        let tokens: PlanTokenResponse = resp
            .json()
            .await
            .map_err(|e| login_error(format!("refresh response: {e}")))?;
        let grant = PlanGrant {
            scope: if tokens.scope.is_empty() {
                grant.scope
            } else {
                tokens.scope.clone()
            },
            id_token: tokens.id_token.clone().or(grant.id_token),
            client_id: grant.client_id,
        };
        self.store_plan_tokens(tokens, grant).await
    }

    async fn store_plan_tokens(
        &self,
        tokens: PlanTokenResponse,
        grant: PlanGrant,
    ) -> Result<(), LlmError> {
        let expires_in = if tokens.expires_in > 0 {
            tokens.expires_in
        } else {
            3600
        };
        let session = OpenAiCodexSession {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            expires_at: Utc::now() + chrono::Duration::seconds(expires_in as i64),
            created_at: Utc::now(),
            plan: Some(grant),
        };
        self.save_session(&session).await?;
        self.set_session(session).await;
        Ok(())
    }
}

struct Callback {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    client_id: Option<String>,
}

/// Serve the loopback until the callback path arrives; other requests
/// (favicon, probes) get a 404 and the wait goes on.
async fn wait_for_callback(listener: &TcpListener) -> Result<Callback, LlmError> {
    loop {
        let (mut stream, _) = listener.accept().await.map_err(LlmError::Io)?;
        let mut buf = vec![0u8; 8192];
        let n = stream.read(&mut buf).await.unwrap_or(0); // silent-ok: a dropped probe connection is skipped below
        let head = String::from_utf8_lossy(&buf[..n]);
        let target = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("");
        let Ok(url) = url::Url::parse(&format!("http://127.0.0.1{target}")) else {
            let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
            continue;
        };
        if url.path() != CALLBACK_PATH {
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\n\r\n").await;
            continue;
        }
        let get = |key: &str| {
            url.query_pairs()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.into_owned())
        };
        let callback = Callback {
            code: get("code"),
            state: get("state"),
            error: get("error"),
            client_id: get("client_id"),
        };
        let page = if callback.code.is_some() {
            "<h2>IronClaw is signed in to ChatGPT.</h2><p>You can close this tab.</p>"
        } else {
            "<h2>ChatGPT sign-in did not finish.</h2><p>Return to IronClaw and try again.</p>"
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
            page.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        return Ok(callback);
    }
}

/// Claims check on the ID token. It came straight from the token endpoint
/// over TLS, so per OIDC Core 3.1.3.7 the TLS server check stands in for
/// the signature; issuer, audience, nonce and expiry are checked here.
fn check_id_token(id_token: &str, client_id: &str, nonce: &str) -> Result<(), LlmError> {
    let payload = id_token
        .split('.')
        .nth(1)
        .ok_or_else(|| login_error("malformed ID token"))?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|e| login_error(format!("ID token payload: {e}")))?;
    let claims: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| login_error(format!("ID token claims: {e}")))?;
    if claims["iss"].as_str() != Some(ISSUER) {
        return Err(login_error("ID token issuer mismatch"));
    }
    let aud_ok = match &claims["aud"] {
        serde_json::Value::String(aud) => aud == client_id,
        serde_json::Value::Array(auds) => auds.iter().any(|a| a.as_str() == Some(client_id)),
        _ => false,
    };
    if !aud_ok {
        return Err(login_error("ID token audience mismatch"));
    }
    if claims["nonce"].as_str() != Some(nonce) {
        return Err(login_error("ID token nonce mismatch"));
    }
    if claims["exp"]
        .as_i64()
        .is_some_and(|exp| exp < Utc::now().timestamp())
    {
        return Err(login_error("ID token expired"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(claims: serde_json::Value) -> String {
        let e = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!("{}.{}.sig", e.encode(b"{}"), e.encode(claims.to_string()))
    }

    #[test]
    fn id_token_claims_are_checked() {
        let good = serde_json::json!({
            "iss": ISSUER, "aud": "oaiapp_1", "nonce": "n",
            "exp": Utc::now().timestamp() + 60,
        });
        assert!(check_id_token(&token(good.clone()), "oaiapp_1", "n").is_ok());
        assert!(check_id_token(&token(good.clone()), "oaiapp_2", "n").is_err());
        assert!(check_id_token(&token(good), "oaiapp_1", "other").is_err());
        let foreign = serde_json::json!({"iss": "https://evil", "aud": "oaiapp_1", "nonce": "n"});
        assert!(check_id_token(&token(foreign), "oaiapp_1", "n").is_err());
    }

    #[test]
    fn plan_usage_needs_the_direct_scope() {
        let grant = |scope: &str| PlanGrant {
            client_id: "oaiapp_1".into(),
            scope: scope.into(),
            id_token: None,
        };
        assert!(grant("openid chatgpt.tokens.use.direct").plan_usage_granted());
        assert!(!grant("openid profile email").plan_usage_granted());
    }

    #[tokio::test]
    async fn callback_skips_other_paths_and_reads_the_query() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = tokio::spawn(async move {
            for path in [
                "/favicon.ico",
                "/auth/callback?code=c1&state=s1&client_id=oaiapp_9",
            ] {
                let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
                    .await
                    .unwrap();
                s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
                    .await
                    .unwrap();
                let mut out = Vec::new();
                let _ = s.read_to_end(&mut out).await;
            }
        });
        let cb = wait_for_callback(&listener).await.unwrap();
        client.await.unwrap();
        assert_eq!(cb.code.as_deref(), Some("c1"));
        assert_eq!(cb.state.as_deref(), Some("s1"));
        assert_eq!(cb.client_id.as_deref(), Some("oaiapp_9"));
    }
}
