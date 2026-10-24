//! Provider usage-limit descriptor and wire DTOs: the Usage page's view of
//! subscription windows (a CLI login's 5-hour and weekly windows, a coding
//! plan's token quota). Declared here per the transport/product boundary;
//! `ironclaw_usage` fills the report and `ironclaw_assistant::usage` answers
//! the view. Vendor-neutral: providers and windows are opaque ids plus a
//! display label.
//!
//! The provider endpoints behind this are undocumented, so every window field
//! except `id`, `label` and `usedPercent` is optional. Times are unix millis.

use serde::{Deserialize, Serialize};

use crate::descriptors::ProductView;

/// Body-free selector for the usage view.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageReportRequest {}

/// `GET /api/webchat/v2/usage`.
pub const USAGE_REPORT_VIEW: ProductView<UsageReportRequest, UsageReport> =
    ProductView::unpaginated("usage.report");

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageReport {
    pub providers: Vec<ProviderUsage>,
}

/// One provider account: its windows, or why there are none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderUsage {
    /// Stable provider-account id, e.g. a CLI driver id.
    pub provider: String,
    pub label: String,
    pub status: UsageStatus,
    /// Short, user-safe reason when `status` is not `ok`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default)]
    pub windows: Vec<UsageWindow>,
    /// When these windows were fetched from the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<i64>,
    /// Subscription tier as the provider names it (`max`, `pro`, `plus`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// One extra line about the account, e.g. spare reset credits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageStatus {
    Ok,
    /// No credentials on this machine (not logged in, no key stored).
    NotConfigured,
    /// The token was rejected; the owning CLI has to refresh it.
    Unauthorized,
    /// The provider answered 429; the last good windows (if any) are kept.
    RateLimited,
    Error,
}

/// One limit window, e.g. a 5-hour session or a monthly token quota.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageWindow {
    /// Stable key within the provider, e.g. `five_hour`, `seven_day`, `tokens`.
    pub id: String,
    pub label: String,
    /// 0-100.
    pub used_percent: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<u32>,
}

/// The token-history selector: the last `days` days, today included
/// (default 30, at most 90).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageHistoryRequest {
    #[serde(default)]
    pub days: Option<u32>,
}

/// `GET /api/webchat/v2/usage/history`.
pub const USAGE_HISTORY_VIEW: ProductView<UsageHistoryRequest, UsageHistory> =
    ProductView::unpaginated("usage.history");

/// What the coding CLIs on this machine spent, read from their own
/// transcripts (t3code's usage summary): token totals per day, provider and
/// model, priced at list rates. Days are `YYYY-MM-DD` in the host's zone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageHistory {
    pub since_day: String,
    pub until_day: String,
    pub read_at: i64,
    pub scan_ms: u64,
    #[serde(default)]
    pub buckets: Vec<UsageBucket>,
    #[serde(default)]
    pub sources: Vec<UsageHistorySource>,
    pub pricing: UsagePricing,
}

/// Cached and cache-creation input are disjoint from uncached input (the
/// three sum to total input); `reasoning_tokens` is a subset of
/// `output_tokens` and is never added on top.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageTokenTotals {
    pub uncached_input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_creation_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageBucket {
    pub day: String,
    /// A provider-account id, as on the limit cards (`claude_cli`, `codex`).
    pub provider: String,
    pub model: String,
    pub totals: UsageTokenTotals,
    /// List-price estimate, or what the CLI reported when it did.
    pub cost_usd: f64,
    /// What the cached input saved against the uncached rate.
    pub cache_savings_usd: f64,
    pub cost_source: UsageCostSource,
    pub records: u32,
    pub unpriced_records: u32,
    pub sessions: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageCostSource {
    ProviderReported,
    ModelPriced,
    Unpriced,
}

/// One transcript source and how its scan went.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageHistorySource {
    pub provider: String,
    pub label: String,
    pub status: UsageSourceStatus,
    pub scanned_files: u32,
    /// Sessions in the window; buckets' session counts overlap across days.
    pub distinct_sessions: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageSourceStatus {
    #[default]
    Ok,
    /// No transcripts on this machine.
    Missing,
    /// Some files could not be read.
    Partial,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsagePricing {
    pub status: UsagePricingStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<i64>,
    pub known_models: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsagePricingStatus {
    Fresh,
    Cached,
    Unavailable,
}

/// Port the usage views read; implemented by `ironclaw_usage::UsageMonitor`.
/// Infallible by contract: a provider that cannot be read reports its own
/// `status` instead of failing the whole report.
#[async_trait::async_trait]
pub trait UsageReportSource: Send + Sync {
    async fn report(&self) -> UsageReport;

    async fn history(&self, request: UsageHistoryRequest) -> UsageHistory;
}
