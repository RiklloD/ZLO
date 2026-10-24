//! Usage reads for the status bar and the Usage page: subscription windows
//! (`/usage`), the coding CLIs' token history (`/usage/history`) and one
//! thread's live telemetry (`/threads/{id}/telemetry`).

use serde::Deserialize;

use crate::api::{Client, V2, seg};
use crate::error::ClientError;

pub use ironclaw_product_contracts::usage::{
    ProviderUsage, UsageBucket, UsageHistory, UsageHistorySource, UsagePricingStatus, UsageReport,
    UsageSourceStatus, UsageStatus, UsageWindow,
};

/// One thread's latest context use, generation speed and cache hit rate.
/// Every metric is optional: a runtime may not report it, or no turn has
/// finished yet.
// TODO(stage 4): import the owner's DTO once the telemetry route lands; this
// mirrors its fixed wire shape until then.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadTelemetry {
    pub thread_id: String,
    #[serde(default)]
    pub runtime_id: String,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub context_used: Option<u64>,
    #[serde(default)]
    pub context_max: Option<u64>,
    #[serde(default)]
    pub context_pct: Option<f64>,
    #[serde(default)]
    pub tps: Option<f64>,
    #[serde(default)]
    pub cache_hit_rate: Option<f64>,
    #[serde(default)]
    pub updated_at: Option<i64>,
}

impl Client {
    pub async fn usage_report(&self) -> Result<UsageReport, ClientError> {
        self.get(&format!("{V2}/usage")).await
    }

    /// The last `days` days of token use, today included.
    pub async fn usage_history(&self, days: u32) -> Result<UsageHistory, ClientError> {
        self.get(&format!("{V2}/usage/history?days={days}")).await
    }

    /// `None` when the gateway has no telemetry route (older build) or
    /// nothing is known about the thread yet.
    pub async fn thread_telemetry(
        &self,
        thread_id: &str,
    ) -> Result<Option<ThreadTelemetry>, ClientError> {
        match self
            .get(&format!("{V2}/threads/{}/telemetry", seg(thread_id)))
            .await
        {
            Ok(t) => Ok(Some(t)),
            Err(e) if e.is_not_found() => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telemetry_tolerates_nulls_and_missing_fields() {
        let t: ThreadTelemetry = serde_json::from_str(
            r#"{"threadId":"t1","runtimeId":"claude_cli","provider":null,"contextUsed":51200,
                "contextMax":200000,"contextPct":25.6,"tps":null,"newField":1}"#,
        )
        .unwrap();
        assert_eq!(t.context_used, Some(51_200));
        assert_eq!(t.tps, None);
        assert_eq!(t.cache_hit_rate, None);
    }
}
