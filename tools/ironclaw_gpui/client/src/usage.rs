//! Usage reads for the status bar and the Usage page: subscription windows
//! (`/usage`), the coding CLIs' token history (`/usage/history`) and one
//! thread's live telemetry (`/threads/{id}/telemetry`).

use serde::Deserialize;

use crate::api::{Client, V2, seg};
use crate::error::ClientError;

pub use ironclaw_product_contracts::usage::{
    ProviderUsage, UsageBucket, UsageCompaction, UsageHistory, UsageHistorySource,
    UsagePricingStatus, UsageReport, UsageSourceStatus, UsageStatus, UsageWindow,
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

    /// Whether idle Claude CLI threads compact themselves (engine setting,
    /// read from `GET /providers`).
    pub async fn idle_compact(&self) -> Result<bool, ClientError> {
        let view: serde_json::Value = self.get(&format!("{V2}/providers")).await?;
        Ok(view["idleCompact"].as_bool().unwrap_or(false))
    }

    /// Switch idle compaction; answers the stored value. The engine setting
    /// is written through the route of one of the user's threads, like the
    /// model-picker preferences, so it needs a thread the orchestrator knows.
    pub async fn set_idle_compact(&self, enabled: bool) -> Result<bool, ClientError> {
        let page = self.list_threads(20, None).await?;
        let body = serde_json::json!({ "idle_compact": enabled });
        for thread in &page.threads {
            let path = format!("{V2}/threads/{}/route", seg(&thread.thread_id));
            // Only a thread the orchestrator does not know (404) moves on to
            // the next one; auth and server errors reach the switch.
            let answer = match self.post::<_, serde_json::Value>(&path, &body).await {
                Ok(answer) => answer,
                Err(e) if e.is_not_found() => continue,
                Err(e) => return Err(e),
            };
            if let Some(stored) = answer["idleCompact"].as_bool() {
                return Ok(stored);
            }
        }
        Err(ClientError::Config(
            "Start a chat first: the setting is saved through one of your threads.".into(),
        ))
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
