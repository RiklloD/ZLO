//! The linking side of peer links: redeem a pairing code from another
//! environment, keep its token in the operator secret store, and call its
//! peer routes.
//!
//! Transport rule (the same one the desktop client follows): a token is only
//! ever sent over https or to loopback — never plain http to a 100.64/10
//! address, which other CGNAT VPNs share with Tailscale — and only to an
//! address whose public descriptor reports the linked environment's id — a LAN address that now answers as
//! another machine never receives it. Each call walks the link's addresses
//! in order and uses the first that answers as that environment.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use ironclaw_host_api::ids::SecretHandle;
use ironclaw_product_contracts::environment_access::{
    ENVIRONMENT_DESCRIPTOR_PATH, ENVIRONMENT_PROTOCOL_VERSION, EnvironmentAccessService,
    EnvironmentDescriptor, EnvironmentId, PAIR_PATH, PEER_LINK_TTL_DAYS, PairClient, PairRequest,
    PairedDevice,
};
use ironclaw_product_contracts::operator_secrets::OperatorSecretValueStore;
use ironclaw_product_contracts::peer::{
    BalancePreference, EnvironmentResources, LinkEnvironmentRequest, PEER_RESOURCES_PATH,
    PeerLinkError, PeerLinkRecords, PeerLinkService, PeerLinkSummary, PeerToolRequest,
    peer_link_token_handle, peer_tool_path,
};
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;

/// How long a verified address stays trusted without a fresh descriptor read.
const VERIFIED_FOR: Duration = Duration::from_secs(120);
const DESCRIPTOR_TIMEOUT: Duration = Duration::from_secs(4);
const CALL_TIMEOUT: Duration = Duration::from_secs(90);

/// [`PeerLinkService`] over HTTP.
pub struct HttpPeerLinkService {
    records: Arc<dyn PeerLinkRecords>,
    tokens: Arc<dyn OperatorSecretValueStore>,
    access: Arc<dyn EnvironmentAccessService>,
    client: reqwest::Client,
    app_version: String,
    /// `(url, environment_id) -> when it last answered as that environment`.
    verified: Mutex<HashMap<(String, String), Instant>>,
}

impl HttpPeerLinkService {
    pub fn new(
        records: Arc<dyn PeerLinkRecords>,
        tokens: Arc<dyn OperatorSecretValueStore>,
        access: Arc<dyn EnvironmentAccessService>,
        app_version: impl Into<String>,
    ) -> Result<Self, PeerLinkError> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| {
                tracing::debug!(%error, "peer client build failed");
                PeerLinkError::Unavailable
            })?;
        Ok(Self {
            records,
            tokens,
            access,
            client,
            app_version: app_version.into(),
            verified: Mutex::new(HashMap::new()),
        })
    }

    fn handle(environment_id: &str) -> Result<SecretHandle, PeerLinkError> {
        SecretHandle::new(peer_link_token_handle(environment_id)).map_err(|error| {
            tracing::debug!(%error, "peer link token handle invalid");
            PeerLinkError::Unavailable
        })
    }

    async fn token(&self, environment_id: &str) -> Result<SecretString, PeerLinkError> {
        self.tokens
            .read(&Self::handle(environment_id)?)
            .await
            .map_err(|_| PeerLinkError::Unavailable)?
            .ok_or_else(|| PeerLinkError::NotLinked(environment_id.to_string()))
    }

    async fn link_record(&self, environment_id: &str) -> Result<PeerLinkSummary, PeerLinkError> {
        self.records
            .get(environment_id)
            .await?
            .ok_or_else(|| PeerLinkError::NotLinked(environment_id.to_string()))
    }

    async fn descriptor(&self, base: &str) -> Result<EnvironmentDescriptor, PeerLinkError> {
        let response = self
            .client
            .get(format!("{base}{ENVIRONMENT_DESCRIPTOR_PATH}"))
            .timeout(DESCRIPTOR_TIMEOUT)
            .send()
            .await
            .map_err(|_| PeerLinkError::Unreachable(base.to_string()))?;
        if !response.status().is_success() {
            return Err(PeerLinkError::Unreachable(base.to_string()));
        }
        response
            .json::<EnvironmentDescriptor>()
            .await
            .map_err(|_| PeerLinkError::Invalid(format!("{base} is not a ZLO backend")))
    }

    /// Whether `base` answers as `environment_id` (cached briefly).
    async fn answers_as(&self, base: &str, environment_id: &str) -> bool {
        let key = (base.to_string(), environment_id.to_string());
        if let Ok(map) = self.verified.lock()
            && map.get(&key).is_some_and(|at| at.elapsed() < VERIFIED_FOR)
        {
            return true;
        }
        let ok = matches!(
            self.descriptor(base).await,
            Ok(d) if d.environment_id == environment_id
                && d.protocol_version == ENVIRONMENT_PROTOCOL_VERSION
        );
        if let Ok(mut map) = self.verified.lock() {
            if ok {
                map.insert(key, Instant::now());
            } else {
                map.remove(&key);
            }
        }
        ok
    }

    async fn mark_revoked(&self, link: &PeerLinkSummary) {
        if link.revoked {
            return;
        }
        let mut link = link.clone();
        link.revoked = true;
        if let Err(error) = self.records.put(&link).await {
            tracing::debug!(%error, "could not record a revoked peer link");
        }
    }

    /// One authenticated request over the first address that answers as
    /// the linked environment.
    async fn send(
        &self,
        environment_id: &str,
        path: &str,
        body: Option<&PeerToolRequest>,
    ) -> Result<Value, PeerLinkError> {
        let link = self.link_record(environment_id).await?;
        if link.expires_at <= Utc::now() {
            self.mark_revoked(&link).await;
            return Err(PeerLinkError::Revoked(link.label.clone()));
        }
        let token = self.token(environment_id).await?;
        let mut last = PeerLinkError::Unreachable(link.label.clone());
        for base in &link.urls {
            if !transport_allowed(base) || !self.answers_as(base, environment_id).await {
                continue;
            }
            let url = format!("{base}{path}");
            let request = match body {
                Some(body) => self.client.post(&url).json(body),
                None => self.client.get(&url),
            };
            let response = match request
                .bearer_auth(token.expose_secret())
                .timeout(CALL_TIMEOUT)
                .send()
                .await
            {
                Ok(response) => response,
                Err(_) => {
                    last = PeerLinkError::Unreachable(link.label.clone());
                    continue;
                }
            };
            let status = response.status();
            if status.is_success() {
                return response.json::<Value>().await.map_err(|_| {
                    PeerLinkError::Refused(format!("{} sent an unreadable answer", link.label))
                });
            }
            let detail = response
                .json::<Value>()
                .await
                .ok()
                .and_then(|v| v["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| format!("{} answered {status}", link.label));
            match status.as_u16() {
                401 => {
                    self.mark_revoked(&link).await;
                    return Err(PeerLinkError::Revoked(link.label.clone()));
                }
                403 => return Err(PeerLinkError::Refused(detail)),
                404 => {
                    return Err(PeerLinkError::Refused(format!(
                        "{} does not support linked environments yet (update it)",
                        link.label
                    )));
                }
                _ => last = PeerLinkError::Unreachable(link.label.clone()),
            }
        }
        Err(last)
    }
}

/// https anywhere; plain http only to loopback.
pub fn transport_allowed(base: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(base) else {
        return false;
    };
    match url.scheme() {
        "https" => true,
        "http" => match url.host() {
            Some(url::Host::Ipv4(ip)) => IpAddr::V4(ip).is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            Some(url::Host::Domain(host)) => host == "localhost",
            None => false,
        },
        _ => false,
    }
}

fn normalize_base(raw: &str) -> String {
    raw.trim().trim_end_matches('/').to_string()
}

#[async_trait]
impl PeerLinkService for HttpPeerLinkService {
    async fn local_environment(&self) -> Result<(EnvironmentId, String), PeerLinkError> {
        let identity = self
            .access
            .environment()
            .await
            .map_err(|_| PeerLinkError::Unavailable)?;
        Ok((identity.environment_id, identity.label))
    }

    async fn links(&self) -> Result<Vec<PeerLinkSummary>, PeerLinkError> {
        self.records.list().await
    }

    async fn link(
        &self,
        request: LinkEnvironmentRequest,
    ) -> Result<PeerLinkSummary, PeerLinkError> {
        let base = normalize_base(&request.url);
        if !transport_allowed(&base) {
            return Err(PeerLinkError::Invalid(
                "Use an https address (for example its Tailscale HTTPS name) or loopback: \
                 a token never travels over plain http."
                    .into(),
            ));
        }
        let descriptor = self.descriptor(&base).await?;
        if descriptor.protocol_version != ENVIRONMENT_PROTOCOL_VERSION {
            return Err(PeerLinkError::Invalid(format!(
                "{} speaks another remote-access protocol; update one of the two machines",
                descriptor.label
            )));
        }
        if !descriptor.peer_links {
            return Err(PeerLinkError::Invalid(format!(
                "{} does not support linked environments yet (update it)",
                descriptor.label
            )));
        }
        let (local_id, local_label) = self.local_environment().await?;
        if descriptor.environment_id == local_id.as_str() {
            return Err(PeerLinkError::Invalid(
                "That address is this machine.".into(),
            ));
        }
        let response = self
            .client
            .post(format!("{base}{PAIR_PATH}"))
            .timeout(Duration::from_secs(15))
            .json(&PairRequest {
                code: request.pairing_code.clone(),
                client: PairClient {
                    label: local_label,
                    os: format!("zlo peer link ({})", std::env::consts::OS),
                    app_version: self.app_version.clone(),
                },
            })
            .send()
            .await
            .map_err(|_| PeerLinkError::Unreachable(descriptor.label.clone()))?;
        if !response.status().is_success() {
            return Err(PeerLinkError::Invalid(
                "That pairing code is not valid. Make a new one on the other machine \
                 (Settings → Computers → Link code, or `zlo pair --link`)."
                    .into(),
            ));
        }
        let paired: PairedDevice = response
            .json()
            .await
            .map_err(|_| PeerLinkError::Invalid("unreadable pairing answer".into()))?;
        if paired.environment_id != descriptor.environment_id {
            return Err(PeerLinkError::Invalid(
                "The machine answered as a different environment than its address reported.".into(),
            ));
        }
        let environment_id = descriptor.environment_id.clone();
        self.tokens
            .put(
                &Self::handle(&environment_id)?,
                SecretString::from(paired.token),
            )
            .await
            .map_err(|_| PeerLinkError::Unavailable)?;
        let mut urls = vec![base.clone()];
        for extra in descriptor
            .tailnet_url
            .iter()
            .chain(descriptor.addresses.iter())
            .map(|u| normalize_base(u))
        {
            if !urls.contains(&extra) && transport_allowed(&extra) {
                urls.push(extra);
            }
        }
        let previous = self.records.get(&environment_id).await?;
        let now = Utc::now();
        let link = PeerLinkSummary {
            environment_id: environment_id.clone(),
            label: descriptor.label.clone(),
            urls,
            access: request.access,
            linked_at: now,
            expires_at: now + chrono::Duration::days(PEER_LINK_TTL_DAYS),
            preference: previous.map(|p| p.preference).unwrap_or_default(),
            revoked: false,
        };
        self.records.put(&link).await?;
        // Prove the grant works as a peer link before reporting success.
        match self.send(&environment_id, PEER_RESOURCES_PATH, None).await {
            Ok(_) => Ok(link),
            Err(PeerLinkError::Refused(message)) => {
                self.forget(&environment_id).await?;
                Err(PeerLinkError::Invalid(format!(
                    "{message}. Make the code with “Link code” (peer link), not a device code."
                )))
            }
            Err(PeerLinkError::Revoked(_)) => {
                self.forget(&environment_id).await?;
                Err(PeerLinkError::Invalid(
                    "The other machine refused the new link. Make a peer-link code there.".into(),
                ))
            }
            Err(other) => Err(other),
        }
    }

    async fn forget(&self, environment_id: &str) -> Result<bool, PeerLinkError> {
        let removed = self.records.remove(environment_id).await?;
        if let Ok(handle) = Self::handle(environment_id) {
            self.tokens
                .delete(&handle)
                .await
                .map_err(|_| PeerLinkError::Unavailable)?;
        }
        Ok(removed)
    }

    async fn set_preference(
        &self,
        environment_id: &str,
        preference: BalancePreference,
    ) -> Result<PeerLinkSummary, PeerLinkError> {
        let mut link = self.link_record(environment_id).await?;
        link.preference = preference;
        self.records.put(&link).await?;
        Ok(link)
    }

    async fn call_tool(
        &self,
        environment_id: &str,
        tool: &str,
        request: PeerToolRequest,
    ) -> Result<Value, PeerLinkError> {
        self.send(environment_id, &peer_tool_path(tool), Some(&request))
            .await
    }

    async fn resources(&self, environment_id: &str) -> Result<EnvironmentResources, PeerLinkError> {
        let value = self.send(environment_id, PEER_RESOURCES_PATH, None).await?;
        serde_json::from_value(value)
            .map_err(|_| PeerLinkError::Refused("unreadable resources answer".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_only_travel_encrypted_or_to_this_machine() {
        assert!(transport_allowed("https://vps.tail9915dd.ts.net"));
        assert!(transport_allowed("http://127.0.0.1:3000"));
        assert!(transport_allowed("http://localhost:3000"));
        assert!(!transport_allowed("http://100.101.102.103:3000"));
        assert!(!transport_allowed("http://192.168.1.20:3000"));
        assert!(!transport_allowed("ftp://vps"));
        assert!(!transport_allowed("not a url"));
    }
}
