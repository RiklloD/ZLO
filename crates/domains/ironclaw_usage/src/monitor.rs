//! The per-provider cache and backoff in front of the fetchers. Fetching is
//! on demand: nothing runs until someone asks for a report, and each provider
//! is polled at most once per [`MIN_POLL`].

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use ironclaw_network::NetworkHttpEgress;
use ironclaw_product_contracts::operator_secrets::OperatorSecretValueStore;
use ironclaw_product_contracts::usage::{
    ProviderUsage, UsageReport, UsageReportSource, UsageStatus, UsageWindow,
};
use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::claude::ClaudeAccount;
use crate::http::Fetched;
use crate::zai::ZaiKey;

const MIN_POLL: Duration = Duration::from_secs(60);
const RETRY_AFTER_ERROR: Duration = Duration::from_secs(120);
const RETRY_UNCONFIGURED: Duration = Duration::from_secs(300);
const BACKOFF_START: Duration = Duration::from_secs(120);
const BACKOFF_MAX: Duration = Duration::from_secs(1_800);

pub enum UsageSource {
    Claude(ClaudeAccount),
    Zai(ZaiKey),
}

impl UsageSource {
    fn identity(&self) -> (&str, &str) {
        match self {
            Self::Claude(account) => (&account.provider_id, &account.label),
            Self::Zai(_) => ("zai", "Z.ai"),
        }
    }
}

struct Slot {
    source: UsageSource,
    state: Mutex<SlotState>,
}

struct SlotState {
    answer: Option<ProviderUsage>,
    next_poll: Option<Instant>,
    backoff: Duration,
    /// Last good windows and their fetch time, kept through 429s and errors.
    last_good: Option<(Vec<UsageWindow>, i64)>,
}

pub struct UsageMonitor {
    egress: Arc<dyn NetworkHttpEgress>,
    slots: Vec<Slot>,
}

impl UsageMonitor {
    /// Both CLI logins under `home` plus the Z.ai key from `secrets`.
    pub fn new(
        egress: Arc<dyn NetworkHttpEgress>,
        home: &Path,
        secrets: Arc<dyn OperatorSecretValueStore>,
    ) -> Self {
        let mut sources: Vec<UsageSource> = ClaudeAccount::defaults(home)
            .into_iter()
            .map(UsageSource::Claude)
            .collect();
        sources.push(UsageSource::Zai(ZaiKey::new(secrets)));
        Self::with_sources(egress, sources)
    }

    /// The production monitor: policy-checked egress over the shared reqwest
    /// transport, the CLI logins under `$HOME`. Without `$HOME` only the
    /// coding-plan key is read.
    pub fn for_host(secrets: Arc<dyn OperatorSecretValueStore>) -> Self {
        let egress = Arc::new(ironclaw_network::PolicyNetworkHttpEgress::new(
            ironclaw_network::ReqwestNetworkTransport::default(),
        ));
        match std::env::var_os("HOME") {
            Some(home) => Self::new(egress, Path::new(&home), secrets),
            None => Self::with_sources(egress, vec![UsageSource::Zai(ZaiKey::new(secrets))]),
        }
    }

    pub fn with_sources(egress: Arc<dyn NetworkHttpEgress>, sources: Vec<UsageSource>) -> Self {
        let slots = sources
            .into_iter()
            .map(|source| Slot {
                source,
                state: Mutex::new(SlotState {
                    answer: None,
                    next_poll: None,
                    backoff: BACKOFF_START,
                    last_good: None,
                }),
            })
            .collect();
        Self { egress, slots }
    }

    /// Every provider, fetched concurrently where its cache has expired.
    pub async fn report(&self) -> UsageReport {
        let providers =
            futures::future::join_all(self.slots.iter().map(|slot| self.poll(slot))).await;
        UsageReport { providers }
    }

    async fn poll(&self, slot: &Slot) -> ProviderUsage {
        // Held across the fetch so concurrent callers share one request.
        let mut state = slot.state.lock().await;
        if let (Some(answer), Some(next)) = (&state.answer, state.next_poll)
            && Instant::now() < next
        {
            return answer.clone();
        }
        let now_ms = chrono::Utc::now().timestamp_millis();
        let fetched = match &slot.source {
            UsageSource::Claude(account) => account.fetch(self.egress.as_ref(), now_ms).await,
            UsageSource::Zai(key) => key.fetch(self.egress.as_ref(), now_ms).await,
        };
        let (status, message, delay) = match fetched {
            Fetched::Windows(windows) => {
                state.last_good = Some((windows, now_ms));
                state.backoff = BACKOFF_START;
                (UsageStatus::Ok, None, MIN_POLL)
            }
            Fetched::RateLimited { retry_after } => {
                let delay = retry_after.unwrap_or(state.backoff).max(MIN_POLL);
                state.backoff = (state.backoff * 2).min(BACKOFF_MAX);
                let message = "Rate limited by the provider; showing the last reading.";
                (UsageStatus::RateLimited, Some(message.to_string()), delay)
            }
            Fetched::NotConfigured(message) => {
                state.last_good = None;
                (
                    UsageStatus::NotConfigured,
                    Some(message),
                    RETRY_UNCONFIGURED,
                )
            }
            Fetched::Unauthorized(message) => {
                state.last_good = None;
                (UsageStatus::Unauthorized, Some(message), RETRY_UNCONFIGURED)
            }
            Fetched::Error(message) => (UsageStatus::Error, Some(message), RETRY_AFTER_ERROR),
        };
        let (provider, label) = slot.source.identity();
        let (windows, fetched_at) = match &state.last_good {
            Some((windows, at)) => (windows.clone(), Some(*at)),
            None => (Vec::new(), None),
        };
        let answer = ProviderUsage {
            provider: provider.to_string(),
            label: label.to_string(),
            status,
            message,
            windows,
            fetched_at,
        };
        state.answer = Some(answer.clone());
        state.next_poll = Some(Instant::now() + delay);
        answer
    }
}

#[async_trait::async_trait]
impl UsageReportSource for UsageMonitor {
    async fn report(&self) -> UsageReport {
        UsageMonitor::report(self).await
    }
}
