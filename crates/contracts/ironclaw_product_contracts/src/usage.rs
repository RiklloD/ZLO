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

/// Port the usage view reads; implemented by `ironclaw_usage::UsageMonitor`.
/// Infallible by contract: a provider that cannot be read reports its own
/// `status` instead of failing the whole report.
#[async_trait::async_trait]
pub trait UsageReportSource: Send + Sync {
    async fn report(&self) -> UsageReport;
}
