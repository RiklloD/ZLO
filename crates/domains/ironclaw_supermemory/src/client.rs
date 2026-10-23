//! Typed Supermemory HTTP client over `ironclaw_network`'s egress.
//!
//! Every call carries the caller's scope, a per-request timeout and a 1 MiB
//! response cap, and goes through the policy egress (public IPs only, the
//! configured API host only). HTTP 429 is retried twice with backoff,
//! honouring `Retry-After` up to 5 s. The API key is read from the secret
//! store and cached for 30 s, so setting or removing it takes effect without a
//! restart; it is only ever placed in the `authorization` header, which the
//! network types zeroize on drop.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use ironclaw_host_api::{
    action::{NetworkMethod, NetworkPolicy, NetworkTargetPattern},
    ids::SecretHandle,
};
use ironclaw_network::{
    NetworkHttpEgress, NetworkHttpRequest, NetworkHttpResponse, PolicyNetworkHttpEgress,
    ReqwestNetworkTransport, network_target_for_url,
};
use ironclaw_product_contracts::operator_secrets::OperatorSecretValueStore;
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;

use crate::binding::SupermemoryCaller;
use crate::config::{SUPERMEMORY_API_KEY_SECRET, SupermemoryConfig};
use crate::error::SupermemoryError;
use crate::wire::{self, WireDocument, WireMemory, WireProfile};

const KEY_CACHE_TTL: Duration = Duration::from_secs(30);
const MAX_RATE_LIMIT_RETRIES: u32 = 2;
const RATE_LIMIT_BASE_DELAY: Duration = Duration::from_millis(500);
const MAX_RETRY_AFTER: Duration = Duration::from_secs(5);
const RESPONSE_BODY_LIMIT: u64 = 1024 * 1024;
/// Routes IronClaw writes into their own source space in the Supermemory app,
/// as `x-sm-source: hermes` does for Hermes.
pub(crate) const SOURCE_TAG: &str = "ironclaw";

struct CachedKey {
    read_at: Instant,
    key: Option<SecretString>,
}

pub struct SupermemoryClient {
    egress: Arc<dyn NetworkHttpEgress>,
    keys: Arc<dyn OperatorSecretValueStore>,
    key_handle: SecretHandle,
    base_url: String,
    policy: NetworkPolicy,
    timeout: Duration,
    retry_base_delay: Duration,
    key_cache: Mutex<Option<CachedKey>>,
}

impl std::fmt::Debug for SupermemoryClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupermemoryClient")
            .field("base_url", &self.base_url)
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

impl SupermemoryClient {
    pub fn new(
        config: &SupermemoryConfig,
        egress: Arc<dyn NetworkHttpEgress>,
        keys: Arc<dyn OperatorSecretValueStore>,
    ) -> Result<Self, SupermemoryError> {
        let target = network_target_for_url(&config.base_url).map_err(|error| {
            SupermemoryError::InvalidConfig {
                reason: format!("base URL: {error}"),
            }
        })?;
        let key_handle = SecretHandle::new(SUPERMEMORY_API_KEY_SECRET).map_err(|error| {
            SupermemoryError::InvalidConfig {
                reason: format!("secret handle: {error}"),
            }
        })?;
        Ok(Self {
            egress,
            keys,
            key_handle,
            base_url: config.base_url.clone(),
            policy: NetworkPolicy {
                allowed_targets: vec![NetworkTargetPattern {
                    scheme: Some(target.scheme),
                    host_pattern: target.host,
                    port: target.port,
                }],
                deny_private_ip_ranges: true,
                max_egress_bytes: None,
            },
            timeout: config.timeout,
            retry_base_delay: RATE_LIMIT_BASE_DELAY,
            key_cache: Mutex::new(None),
        })
    }

    /// The hardened production egress (pinned reqwest transport behind the
    /// network policy).
    pub fn production(
        config: &SupermemoryConfig,
        keys: Arc<dyn OperatorSecretValueStore>,
    ) -> Result<Self, SupermemoryError> {
        let egress = Arc::new(PolicyNetworkHttpEgress::new(ReqwestNetworkTransport::new(
            config.timeout,
        )));
        Self::new(config, egress, keys)
    }

    #[cfg(test)]
    pub(crate) fn without_retry_delay(mut self) -> Self {
        self.retry_base_delay = Duration::ZERO;
        self
    }

    /// The stored API key, or `None` when none is set (feature off).
    pub async fn api_key(&self) -> Result<Option<SecretString>, SupermemoryError> {
        {
            let cache = self
                .key_cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if let Some(cached) = cache.as_ref()
                && cached.read_at.elapsed() < KEY_CACHE_TTL
            {
                return Ok(cached.key.clone());
            }
        }
        let key = self
            .keys
            .read(&self.key_handle)
            .await
            .map_err(|error| SupermemoryError::Secret {
                reason: error.to_string(),
            })?
            .filter(|key| !key.expose_secret().trim().is_empty());
        *self
            .key_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(CachedKey {
            read_at: Instant::now(),
            key: key.clone(),
        });
        Ok(key)
    }

    pub(crate) async fn add_document(
        &self,
        caller: &SupermemoryCaller,
        key: &SecretString,
        body: &Value,
    ) -> Result<WireDocument, SupermemoryError> {
        let value = self
            .call(
                caller,
                key,
                NetworkMethod::Post,
                "/v3/documents",
                Some(body),
            )
            .await?;
        wire::document(&value)
    }

    pub(crate) async fn get_document(
        &self,
        caller: &SupermemoryCaller,
        key: &SecretString,
        id: &str,
    ) -> Result<WireDocument, SupermemoryError> {
        if id.is_empty()
            || !id
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
        {
            return Err(SupermemoryError::Decode {
                reason: "document id is not a safe path segment".to_string(),
            });
        }
        let path = format!("/v3/documents/{id}");
        let value = self
            .call(caller, key, NetworkMethod::Get, &path, None)
            .await?;
        wire::document(&value)
    }

    pub(crate) async fn search(
        &self,
        caller: &SupermemoryCaller,
        key: &SecretString,
        body: &Value,
    ) -> Result<Vec<WireMemory>, SupermemoryError> {
        let value = self
            .call(caller, key, NetworkMethod::Post, "/v4/search", Some(body))
            .await?;
        Ok(wire::search_results(&value))
    }

    pub(crate) async fn profile(
        &self,
        caller: &SupermemoryCaller,
        key: &SecretString,
        body: &Value,
    ) -> Result<WireProfile, SupermemoryError> {
        let value = self
            .call(caller, key, NetworkMethod::Post, "/v4/profile", Some(body))
            .await?;
        Ok(wire::profile(&value))
    }

    async fn call(
        &self,
        caller: &SupermemoryCaller,
        key: &SecretString,
        method: NetworkMethod,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, SupermemoryError> {
        let body = match body {
            Some(body) => {
                serde_json::to_vec(body).map_err(|error| SupermemoryError::InvalidInput {
                    reason: format!("request body: {error}"),
                })?
            }
            None => Vec::new(),
        };
        let mut attempt = 0;
        loop {
            let response = self
                .egress
                .execute(self.request(caller, key, method, path, body.clone()))
                .await
                .map_err(|error| SupermemoryError::Transport {
                    reason: error.to_string(),
                })?;
            match response.status {
                200..=299 => return wire::parse_body(&response.body),
                429 if attempt < MAX_RATE_LIMIT_RETRIES => {
                    tokio::time::sleep(self.retry_delay(attempt, &response)).await;
                    attempt += 1;
                }
                429 => return Err(SupermemoryError::RateLimited),
                401 | 403 => {
                    return Err(SupermemoryError::Unauthorized {
                        status: response.status,
                    });
                }
                status => return Err(SupermemoryError::Http { status }),
            }
        }
    }

    fn request(
        &self,
        caller: &SupermemoryCaller,
        key: &SecretString,
        method: NetworkMethod,
        path: &str,
        body: Vec<u8>,
    ) -> NetworkHttpRequest {
        let mut headers = vec![
            (
                "authorization".to_string(),
                format!("Bearer {}", key.expose_secret()),
            ),
            ("accept".to_string(), "application/json".to_string()),
            ("x-sm-source".to_string(), SOURCE_TAG.to_string()),
        ];
        if !body.is_empty() {
            headers.push(("content-type".to_string(), "application/json".to_string()));
        }
        NetworkHttpRequest {
            scope: caller.network_scope(),
            method,
            url: format!("{}{path}", self.base_url),
            headers,
            body,
            policy: self.policy.clone(),
            response_body_limit: Some(RESPONSE_BODY_LIMIT),
            timeout_ms: u32::try_from(self.timeout.as_millis()).ok(),
        }
    }

    fn retry_delay(&self, attempt: u32, response: &NetworkHttpResponse) -> Duration {
        if self.retry_base_delay.is_zero() {
            return Duration::ZERO;
        }
        let retry_after = response
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("retry-after"))
            .and_then(|(_, value)| value.trim().parse::<u64>().ok())
            .map(Duration::from_secs);
        retry_after
            .unwrap_or_else(|| self.retry_base_delay * 2u32.saturating_pow(attempt))
            .min(MAX_RETRY_AFTER)
    }
}
