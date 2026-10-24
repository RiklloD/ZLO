//! The per-provider cache and backoff in front of the fetchers. Fetching is
//! on demand: nothing runs until someone asks for a report, and each provider
//! is polled at most once per its [`UsageSource::min_poll`]. Between polls a
//! Claude login's cached windows take any newer reading the CLI driver saved
//! after a turn, so limits move live with use (t3code's sparse updates).

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

use crate::antigravity::AntigravityLogin;
use crate::claude::ClaudeAccount;
use crate::codex::CodexLogin;
use crate::http::Fetched;
use crate::muse::MuseReading;
use crate::zai::ZaiKey;

const MIN_POLL: Duration = Duration::from_secs(60);
const RETRY_AFTER_ERROR: Duration = Duration::from_secs(120);
const RETRY_UNCONFIGURED: Duration = Duration::from_secs(300);
const BACKOFF_START: Duration = Duration::from_secs(120);
const BACKOFF_MAX: Duration = Duration::from_secs(1_800);

pub enum UsageSource {
    Claude(ClaudeAccount),
    Codex(CodexLogin),
    Zai(ZaiKey),
    Antigravity(AntigravityLogin),
    Muse(MuseReading),
}

impl UsageSource {
    fn identity(&self) -> (&str, &str) {
        match self {
            Self::Claude(account) => (&account.provider_id, &account.label),
            Self::Codex(_) => ("codex", "Codex"),
            Self::Zai(_) => ("zai", "Z.ai"),
            Self::Antigravity(_) => ("antigravity", "Antigravity"),
            Self::Muse(_) => ("muse_code", "Muse Code"),
        }
    }

    /// The CLI probes start a process, so they run less often than an HTTP
    /// read; Muse is a local file.
    fn min_poll(&self) -> Duration {
        match self {
            Self::Claude(_) | Self::Codex(_) => Duration::from_secs(120),
            Self::Antigravity(_) => Duration::from_secs(180),
            Self::Zai(_) => MIN_POLL,
            Self::Muse(_) => Duration::from_secs(15),
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
    plan: Option<String>,
    detail: Option<String>,
}

pub struct UsageMonitor {
    egress: Arc<dyn NetworkHttpEgress>,
    slots: Vec<Slot>,
}

impl UsageMonitor {
    /// Both Claude logins, Codex, Antigravity and Muse Code under `home`,
    /// plus the Z.ai key from `secrets`.
    pub fn new(
        egress: Arc<dyn NetworkHttpEgress>,
        home: &Path,
        secrets: Arc<dyn OperatorSecretValueStore>,
    ) -> Self {
        let mut sources: Vec<UsageSource> = ClaudeAccount::defaults(home)
            .into_iter()
            .map(UsageSource::Claude)
            .collect();
        sources.extend([
            UsageSource::Codex(CodexLogin::new(home)),
            UsageSource::Zai(ZaiKey::new(secrets)),
            UsageSource::Antigravity(AntigravityLogin::new(home)),
            UsageSource::Muse(MuseReading::new(home)),
        ]);
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
                    plan: None,
                    detail: None,
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
        let now_ms = chrono::Utc::now().timestamp_millis();
        if let (Some(answer), Some(next)) = (&state.answer, state.next_poll)
            && Instant::now() < next
        {
            let mut answer = answer.clone();
            if let UsageSource::Claude(account) = &slot.source
                && let Some((fresh, at)) = account
                    .observed_since(answer.fetched_at.unwrap_or(0), now_ms)
                    .await
            {
                overlay(&mut answer.windows, fresh);
                answer.fetched_at = Some(at);
                state.last_good = Some((answer.windows.clone(), at));
                state.answer = Some(answer.clone());
            }
            return answer;
        }
        let poll = slot.source.min_poll();
        let fetched = match &slot.source {
            UsageSource::Claude(account) => account.fetch(self.egress.as_ref(), now_ms).await,
            UsageSource::Codex(login) => login.fetch().await,
            UsageSource::Zai(key) => key.fetch(self.egress.as_ref(), now_ms).await,
            UsageSource::Antigravity(login) => login.fetch(self.egress.as_ref(), now_ms).await,
            UsageSource::Muse(reading) => reading.fetch(now_ms).await,
        };
        let (status, message, delay) = match fetched {
            Fetched::Windows(reading) => {
                state.last_good = Some((reading.windows, now_ms));
                state.plan = reading.plan;
                state.detail = reading.detail;
                state.backoff = BACKOFF_START;
                (UsageStatus::Ok, None, poll)
            }
            Fetched::Observed { windows, at, note } => {
                state.last_good = Some((windows, at));
                (UsageStatus::Ok, Some(note), poll)
            }
            Fetched::RateLimited { retry_after } => {
                let delay = retry_after.unwrap_or(state.backoff).max(poll);
                state.backoff = (state.backoff * 2).min(BACKOFF_MAX);
                let message = "Rate limited by the provider; showing the last reading.";
                (UsageStatus::RateLimited, Some(message.to_string()), delay)
            }
            Fetched::NotConfigured(message) => {
                state.last_good = None;
                state.plan = None;
                state.detail = None;
                (
                    UsageStatus::NotConfigured,
                    Some(message),
                    RETRY_UNCONFIGURED,
                )
            }
            Fetched::Unauthorized(message) => {
                state.last_good = None;
                state.plan = None;
                state.detail = None;
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
            plan: state.plan.clone(),
            detail: state.detail.clone(),
        };
        state.answer = Some(answer.clone());
        state.next_poll = Some(Instant::now() + delay);
        answer
    }
}

/// Newer windows replace same-id ones; windows the reading does not name
/// keep their last value.
fn overlay(windows: &mut Vec<UsageWindow>, fresh: Vec<UsageWindow>) {
    for window in fresh {
        match windows.iter_mut().find(|w| w.id == window.id) {
            Some(slot) => *slot = window,
            None => windows.push(window),
        }
    }
}

#[async_trait::async_trait]
impl UsageReportSource for UsageMonitor {
    async fn report(&self) -> UsageReport {
        UsageMonitor::report(self).await
    }
}
