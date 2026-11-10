use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use tokio::sync::RwLock;

// ─── Risk: hardcoded VS Code Copilot identity ───────────────────────────────
//
// The editor identity headers below are extracted from the VS Code Copilot
// Chat extension.  This is the *only* publicly documented way to access the
// Copilot completions API with a personal GitHub token.
//
// **Known risks:**
//   • Using another product's identity may violate GitHub's Terms of
//     Service.  Maintainers should seek explicit guidance from GitHub
//     before shipping this to a wide audience.
//   • The editor version strings (`vscode/1.99.3`, `copilot-chat/0.26.7`)
//     will become stale and could eventually be rejected by the API.
//
// **Mitigation:** If GitHub publishes an official Copilot API client ID or
// an OAuth app registration flow for third-party tools, migrate to it
// immediately.
// ─────────────────────────────────────────────────────────────────────────────
pub(crate) const GITHUB_COPILOT_TOKEN_URL: &str =
    "https://api.github.com/copilot_internal/v2/token";
pub(crate) const GITHUB_COPILOT_USER_AGENT: &str = "GitHubCopilotChat/0.26.7";
pub(crate) const GITHUB_COPILOT_EDITOR_VERSION: &str = "vscode/1.99.3";
pub(crate) const GITHUB_COPILOT_EDITOR_PLUGIN_VERSION: &str = "copilot-chat/0.26.7";
pub(crate) const GITHUB_COPILOT_INTEGRATION_ID: &str = "vscode-chat";

/// Buffer before token expiry to trigger a refresh (5 minutes).
const TOKEN_REFRESH_BUFFER_SECS: u64 = 300;

#[derive(Debug, thiserror::Error)]
pub(crate) enum GithubCopilotAuthError {
    #[error("github copilot token validation failed: {0}")]
    Validation(String),
}

pub(crate) fn default_headers() -> Vec<(String, String)> {
    vec![
        (
            "User-Agent".to_string(),
            GITHUB_COPILOT_USER_AGENT.to_string(),
        ),
        (
            "Editor-Version".to_string(),
            GITHUB_COPILOT_EDITOR_VERSION.to_string(),
        ),
        (
            "Editor-Plugin-Version".to_string(),
            GITHUB_COPILOT_EDITOR_PLUGIN_VERSION.to_string(),
        ),
        (
            "Copilot-Integration-Id".to_string(),
            GITHUB_COPILOT_INTEGRATION_ID.to_string(),
        ),
    ]
}

/// Response from the Copilot token exchange endpoint.
///
/// The `token` field is an HMAC-signed session token (not a JWT) used as
/// `Authorization: Bearer <token>` for requests to `api.githubcopilot.com`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct CopilotTokenResponse {
    /// The Copilot session token (HMAC-signed, not a JWT).
    pub(crate) token: String,
    /// Unix timestamp (seconds) when this token expires.
    pub(crate) expires_at: u64,
}

/// Exchange a GitHub OAuth token for a Copilot API session token.
///
/// Calls `GET https://api.github.com/copilot_internal/v2/token` with the
/// GitHub OAuth token in `Authorization: token <oauth_token>` format.
/// Returns a short-lived session token for `api.githubcopilot.com`.
pub(crate) async fn exchange_copilot_token(
    client: &reqwest::Client,
    oauth_token: &str,
) -> Result<CopilotTokenResponse, GithubCopilotAuthError> {
    let token_trimmed = oauth_token.trim();
    let mut request = client
        .get(GITHUB_COPILOT_TOKEN_URL)
        .header(reqwest::header::ACCEPT, "application/json")
        // GitHub Copilot uses `token` auth scheme, not `Bearer`
        .header(
            reqwest::header::AUTHORIZATION,
            format!("token {token_trimmed}"),
        )
        .timeout(Duration::from_secs(15));

    for (key, value) in default_headers() {
        request = request.header(&key, value);
    }

    let response = request.send().await.map_err(|e| {
        tracing::warn!(
            error = %e,
            is_timeout = e.is_timeout(),
            is_connect = e.is_connect(),
            "Copilot: token exchange HTTP request failed"
        );
        GithubCopilotAuthError::Validation(format_reqwest_error(&e))
    })?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        tracing::warn!(
            status = %status,
            body = %truncate_for_error(&body),
            "Copilot: token exchange endpoint returned error"
        );
        return Err(GithubCopilotAuthError::Validation(format!(
            "Copilot token exchange failed: HTTP {status}: {}",
            truncate_for_error(&body)
        )));
    }

    let token_response = response.json::<CopilotTokenResponse>().await.map_err(|e| {
        tracing::warn!(error = %e, "Copilot: failed to parse token exchange response");
        GithubCopilotAuthError::Validation(e.to_string())
    })?;

    Ok(token_response)
}

/// Manages a cached Copilot API session token with automatic refresh.
///
/// The GitHub Copilot API requires a two-step authentication:
/// 1. A long-lived GitHub OAuth token (from device login or IDE sign-in)
/// 2. A short-lived Copilot session token (exchanged via `/copilot_internal/v2/token`)
///
/// This manager caches the session token and refreshes it automatically
/// before it expires (with a 5-minute buffer).
pub(crate) struct CopilotTokenManager {
    client: reqwest::Client,
    oauth_token: SecretString,
    cached: RwLock<Option<CachedCopilotToken>>,
}

#[derive(Clone)]
struct CachedCopilotToken {
    token: SecretString,
    expires_at: u64,
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

impl CopilotTokenManager {
    /// Create a new token manager with the given GitHub OAuth token.
    pub(crate) fn new(client: reqwest::Client, oauth_token: String) -> Self {
        Self {
            client,
            oauth_token: SecretString::from(oauth_token),
            cached: RwLock::new(None),
        }
    }

    /// Get a valid Copilot session token, refreshing if needed.
    ///
    /// Returns the cached token if it has more than 5 minutes remaining,
    /// otherwise exchanges the OAuth token for a fresh session token.
    pub(crate) async fn get_token(&self) -> Result<SecretString, GithubCopilotAuthError> {
        // Fast path: check if cached token is still valid under read lock.
        {
            let guard = self.cached.read().await;
            if let Some(ref cached) = *guard {
                let now = unix_now();
                if cached.expires_at > now + TOKEN_REFRESH_BUFFER_SECS {
                    return Ok(cached.token.clone());
                }
                tracing::debug!(
                    expires_at = cached.expires_at,
                    now = now,
                    "Copilot: cached session token expired or expiring soon, refreshing"
                );
            }
        }

        // Slow path: acquire write lock and re-check (another caller may have
        // already refreshed while we waited for the lock).
        let mut guard = self.cached.write().await;
        if let Some(ref cached) = *guard {
            let now = unix_now();
            if cached.expires_at > now + TOKEN_REFRESH_BUFFER_SECS {
                return Ok(cached.token.clone());
            }
        }

        let response =
            exchange_copilot_token(&self.client, self.oauth_token.expose_secret()).await?;
        let token = SecretString::from(response.token);

        let expires_at = response.expires_at;
        *guard = Some(CachedCopilotToken {
            token: token.clone(),
            expires_at,
        });

        tracing::debug!(expires_at = expires_at, "Copilot session token refreshed");

        Ok(token)
    }

    /// Invalidate the cached session token.
    ///
    /// Called when the API returns 401, so the next `get_token()` call
    /// will perform a fresh token exchange instead of reusing the stale token.
    pub(crate) async fn invalidate(&self) {
        let mut guard = self.cached.write().await;
        *guard = None;
        tracing::debug!("Copilot session token invalidated");
    }
}

fn truncate_for_error(body: &str) -> String {
    const LIMIT: usize = 200;
    if body.len() <= LIMIT {
        return body.to_string();
    }
    // Walk back to a UTF-8 char boundary. Equivalent to the (still-unstable)
    // `str::floor_char_boundary` used by the main crate's `crate::util`.
    let mut end = LIMIT;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &body[..end])
}

/// Format a reqwest error with its full causal chain for debugging.
///
/// `reqwest::Error::to_string()` often just says "error sending request"
/// without the underlying cause (timeout, DNS, TLS, connection refused).
/// This walks the `source()` chain to surface the real problem.
fn format_reqwest_error(e: &reqwest::Error) -> String {
    use std::error::Error;
    let mut msg = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        msg.push_str(&format!(": {cause}"));
        source = cause.source();
    }
    msg
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn token_manager_with_cached_token(
        client: reqwest::Client,
        token: String,
    ) -> CopilotTokenManager {
        CopilotTokenManager {
            client,
            oauth_token: SecretString::from("unused-test-oauth-token".to_string()),
            cached: RwLock::new(Some(CachedCopilotToken {
                token: SecretString::from(token),
                expires_at: u64::MAX,
            })),
        }
    }

    #[test]
    fn default_headers_include_required_identity_headers() {
        let headers = default_headers();
        assert!(headers.iter().any(|(key, value)| {
            key == "Copilot-Integration-Id" && value == GITHUB_COPILOT_INTEGRATION_ID
        }));
        assert!(
            headers
                .iter()
                .any(|(key, value)| key == "Editor-Version"
                    && value == GITHUB_COPILOT_EDITOR_VERSION)
        );
        assert!(
            headers
                .iter()
                .any(|(key, value)| key == "User-Agent" && value == GITHUB_COPILOT_USER_AGENT)
        );
    }

    #[test]
    fn truncate_for_error_preserves_utf8_boundaries() {
        let long = "日本語".repeat(100);
        let truncated = truncate_for_error(&long);
        assert!(truncated.ends_with("..."));
        assert!(truncated.is_char_boundary(truncated.len() - 3));
    }

    #[test]
    fn truncate_for_error_short_strings_unchanged() {
        let short = "hello";
        assert_eq!(truncate_for_error(short), "hello");
    }

    // --- poll_for_access_token response parsing ---

    // --- CopilotTokenManager ---

    fn loopback_failure_client() -> reqwest::Client {
        let proxy = reqwest::Proxy::all("http://127.0.0.1:1").expect("valid loopback proxy");
        reqwest::Client::builder()
            .proxy(proxy)
            .build()
            .expect("build test client")
    }

    #[tokio::test]
    async fn token_manager_caches_token_and_returns_same_value() {
        // Pre-populate the cache with a token that expires far in the future.
        let client = reqwest::Client::new();
        let manager = CopilotTokenManager::new(client, "unused_oauth".to_string());

        let far_future = unix_now() + 3600;
        {
            let mut guard = manager.cached.write().await;
            *guard = Some(CachedCopilotToken {
                token: SecretString::from("cached_session_token".to_string()),
                expires_at: far_future,
            });
        }

        let token = manager.get_token().await.expect("should return cached");
        assert_eq!(token.expose_secret(), "cached_session_token");

        // A second call should return the same cached token.
        let token2 = manager.get_token().await.expect("should return cached");
        assert_eq!(token2.expose_secret(), "cached_session_token");
    }

    #[tokio::test]
    async fn token_manager_invalidation_clears_cache() {
        let client = reqwest::Client::new();
        let manager = CopilotTokenManager::new(client, "unused_oauth".to_string());

        let far_future = unix_now() + 3600;
        {
            let mut guard = manager.cached.write().await;
            *guard = Some(CachedCopilotToken {
                token: SecretString::from("old_token".to_string()),
                expires_at: far_future,
            });
        }

        manager.invalidate().await;

        let guard = manager.cached.read().await;
        assert!(guard.is_none(), "cache should be empty after invalidation");
    }

    #[tokio::test]
    async fn token_manager_expired_token_triggers_refresh_path() {
        let client = loopback_failure_client();
        let manager = CopilotTokenManager::new(client, "unused_oauth".to_string());

        // Set a token that is already expired (expires_at in the past).
        {
            let mut guard = manager.cached.write().await;
            *guard = Some(CachedCopilotToken {
                token: SecretString::from("stale_token".to_string()),
                expires_at: 1, // way in the past
            });
        }

        // get_token will try the slow path (token exchange) which will fail
        // because we have no real server, but this proves the cached stale
        // token is NOT returned.
        let result = manager.get_token().await;
        assert!(
            result.is_err(),
            "expired cached token should trigger exchange, which fails without a server"
        );
    }

    #[tokio::test]
    async fn token_manager_within_buffer_triggers_refresh() {
        let client = loopback_failure_client();
        let manager = CopilotTokenManager::new(client, "unused_oauth".to_string());

        // Set a token that expires within the refresh buffer window.
        let expires_soon = unix_now() + TOKEN_REFRESH_BUFFER_SECS - 10;
        {
            let mut guard = manager.cached.write().await;
            *guard = Some(CachedCopilotToken {
                token: SecretString::from("expiring_soon".to_string()),
                expires_at: expires_soon,
            });
        }

        let result = manager.get_token().await;
        assert!(
            result.is_err(),
            "token within buffer should trigger exchange"
        );
    }

    // --- CopilotTokenResponse parsing ---

    #[test]
    fn parse_copilot_token_response() {
        let json = r#"{"token": "tid=abc;exp=999;sku=123;sig=xyz", "expires_at": 1700000000}"#;
        let resp: CopilotTokenResponse = serde_json::from_str(json).expect("valid JSON");
        assert!(resp.token.starts_with("tid="));
        assert_eq!(resp.expires_at, 1700000000);
    }
}
