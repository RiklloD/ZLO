//! One-host GET through `ironclaw_network`, and the status classification all
//! providers share.

use std::time::Duration;

use ironclaw_host_api::action::{
    NetworkMethod, NetworkPolicy, NetworkScheme, NetworkTargetPattern,
};
use ironclaw_host_api::resource::ResourceScope;
use ironclaw_network::{NetworkHttpEgress, NetworkHttpRequest, NetworkHttpResponse};
use ironclaw_product_contracts::usage::UsageWindow;

const RESPONSE_BODY_LIMIT: u64 = 256 * 1024;
const TIMEOUT_MS: u32 = 15_000;

/// A successful reading: the windows plus what the provider says about the
/// account.
#[derive(Debug, Default)]
pub(crate) struct Reading {
    pub windows: Vec<UsageWindow>,
    pub plan: Option<String>,
    pub detail: Option<String>,
}

impl From<Vec<UsageWindow>> for Reading {
    fn from(windows: Vec<UsageWindow>) -> Self {
        Self {
            windows,
            ..Self::default()
        }
    }
}

/// What one provider poll produced.
#[derive(Debug)]
pub(crate) enum Fetched {
    Windows(Reading),
    /// Read from the CLI's own last turn instead of the provider endpoint.
    Observed {
        windows: Vec<UsageWindow>,
        at: i64,
        note: String,
    },
    NotConfigured(String),
    Unauthorized(String),
    RateLimited {
        retry_after: Option<Duration>,
    },
    Error(String),
}

/// GET `url`, allowing only `host` over HTTPS. Transport failures become
/// [`Fetched::Error`]; the caller sees every HTTP status.
pub(crate) async fn get(
    egress: &dyn NetworkHttpEgress,
    host: &str,
    url: String,
    headers: Vec<(String, String)>,
) -> Result<NetworkHttpResponse, Fetched> {
    send(egress, NetworkMethod::Get, host, url, headers, Vec::new()).await
}

/// POST `body` to `url`, same policy as [`get`].
pub(crate) async fn post(
    egress: &dyn NetworkHttpEgress,
    host: &str,
    url: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
) -> Result<NetworkHttpResponse, Fetched> {
    send(egress, NetworkMethod::Post, host, url, headers, body).await
}

async fn send(
    egress: &dyn NetworkHttpEgress,
    method: NetworkMethod,
    host: &str,
    url: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
) -> Result<NetworkHttpResponse, Fetched> {
    let request = NetworkHttpRequest {
        scope: ResourceScope::system(),
        method,
        url,
        headers,
        body,
        policy: NetworkPolicy {
            allowed_targets: vec![NetworkTargetPattern {
                scheme: Some(NetworkScheme::Https),
                host_pattern: host.to_string(),
                port: None,
            }],
            deny_private_ip_ranges: true,
            max_egress_bytes: None,
        },
        response_body_limit: Some(RESPONSE_BODY_LIMIT),
        timeout_ms: Some(TIMEOUT_MS),
    };
    egress.execute(request).await.map_err(|error| {
        tracing::debug!(host, %error, "usage request failed");
        Fetched::Error("Could not reach the provider.".to_string())
    })
}

/// The non-200 outcomes every provider maps the same way.
pub(crate) fn classify_status(response: &NetworkHttpResponse) -> Option<Fetched> {
    match response.status {
        200 => None,
        401 | 403 => Some(Fetched::Unauthorized(
            "The provider rejected the login; use the CLI once to refresh it.".to_string(),
        )),
        429 => Some(Fetched::RateLimited {
            retry_after: retry_after(response),
        }),
        status => Some(Fetched::Error(format!(
            "The provider answered HTTP {status}."
        ))),
    }
}

/// `Retry-After` in seconds; HTTP dates are ignored (the backoff covers them).
fn retry_after(response: &NetworkHttpResponse) -> Option<Duration> {
    response
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
        .and_then(|(_, value)| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}
