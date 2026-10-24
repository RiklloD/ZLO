//! Coding-plan quota (`GET api.z.ai/api/monitor/usage/quota/limit`), per
//! CodexBar's `zai` plugin: `data.limits[]` with `type`, `unit` + `number`
//! (window length), an integer `percentage`, optional `usage` /
//! `currentValue` / `remaining` counts and `nextResetTime` (unix millis).

use std::sync::Arc;

use ironclaw_host_api::ids::SecretHandle;
use ironclaw_network::NetworkHttpEgress;
use ironclaw_product_contracts::operator_secrets::OperatorSecretValueStore;
use ironclaw_product_contracts::usage::UsageWindow;
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;

use crate::http::{Fetched, classify_status, get};

const HOST: &str = "api.z.ai";
const URL: &str = "https://api.z.ai/api/monitor/usage/quota/limit";
/// LLM provider ids whose stored key is a Z.ai key, in lookup order.
const PROVIDER_IDS: &[&str] = &["zai", "glm"];
/// Clock skew allowed on a 5-hour window's reset (CodexBar drops later ones).
const FIVE_HOUR_RESET_SKEW_MS: i64 = (5 * 3_600 + 60) * 1_000;

/// The Z.ai API key, read from the operator secret store where the LLM
/// settings keep provider keys.
pub struct ZaiKey {
    secrets: Arc<dyn OperatorSecretValueStore>,
}

impl ZaiKey {
    pub fn new(secrets: Arc<dyn OperatorSecretValueStore>) -> Self {
        Self { secrets }
    }

    /// Same handle grammar as `ironclaw_operator::LlmKeyStore`
    /// (`llm_provider_{id}_api_key`).
    async fn read(&self) -> Result<Option<SecretString>, String> {
        for id in PROVIDER_IDS {
            let handle = SecretHandle::new(format!("llm_provider_{id}_api_key"))
                .map_err(|error| format!("invalid secret handle: {error}"))?;
            match self.secrets.read(&handle).await {
                Ok(Some(key)) => return Ok(Some(key)),
                Ok(None) => {}
                Err(error) => {
                    tracing::debug!(%error, "z.ai key read failed");
                    return Err("The stored Z.ai key is unreadable.".to_string());
                }
            }
        }
        Ok(None)
    }

    pub(crate) async fn fetch(&self, egress: &dyn NetworkHttpEgress, now_ms: i64) -> Fetched {
        let key = match self.read().await {
            Ok(Some(key)) => key,
            Ok(None) => return Fetched::NotConfigured("No Z.ai key is stored.".to_string()),
            Err(reason) => return Fetched::Error(reason),
        };
        let headers = vec![
            (
                "Authorization".to_string(),
                format!("Bearer {}", key.expose_secret()),
            ),
            ("Accept".to_string(), "application/json".to_string()),
        ];
        let response = match get(egress, HOST, URL.to_string(), headers).await {
            Ok(response) => response,
            Err(fetched) => return fetched,
        };
        if let Some(fetched) = classify_status(&response) {
            return fetched;
        }
        let body = match serde_json::from_slice::<Value>(&response.body) {
            Ok(body) => body,
            Err(error) => {
                tracing::debug!(%error, "z.ai usage response is not JSON");
                return Fetched::Error("The usage response was unreadable.".to_string());
            }
        };
        match parse_limits(&body, now_ms) {
            Some(windows) if !windows.is_empty() => Fetched::Windows(windows.into()),
            Some(_) => Fetched::Error("The plan reported no quota windows.".to_string()),
            None if matches!(body.get("code").and_then(Value::as_i64), Some(401 | 1001)) => {
                Fetched::Unauthorized("Z.ai rejected the stored key.".to_string())
            }
            None => Fetched::Error("The usage response had an unexpected shape.".to_string()),
        }
    }
}

/// `None` when the envelope has no `data.limits` array.
pub(crate) fn parse_limits(body: &Value, now_ms: i64) -> Option<Vec<UsageWindow>> {
    let limits = body.get("data")?.get("limits")?.as_array()?;
    let mut tokens = Vec::new();
    let mut mcp = None;
    for raw in limits {
        let Some(kind) = raw.get("type").and_then(Value::as_str) else {
            continue;
        };
        let Some(percent) = used_percent(raw) else {
            continue;
        };
        let unit = raw.get("unit").and_then(Value::as_i64);
        let number = raw.get("number").and_then(Value::as_i64).unwrap_or(0);
        let minutes = window_minutes(unit, number);
        let resets_at = raw.get("nextResetTime").and_then(Value::as_i64);
        match kind {
            "TOKENS_LIMIT" | "CREDIT_LIMIT" => {
                // A 5-hour reset further out than 5 h is stale; drop it.
                let resets_at = resets_at
                    .filter(|at| minutes != Some(300) || *at <= now_ms + FIVE_HOUR_RESET_SKEW_MS);
                let (id, label) = token_window_name(minutes);
                tokens.push(UsageWindow {
                    id,
                    label,
                    used_percent: percent,
                    resets_at,
                    window_minutes: minutes,
                });
            }
            "TIME_LIMIT" => {
                // unit 5 + number 1 is the monthly MCP marker, not 1 minute.
                let monthly = unit == Some(5) && number == 1;
                mcp = Some(UsageWindow {
                    id: "mcp".to_string(),
                    label: if monthly { "MCP (monthly)" } else { "MCP" }.to_string(),
                    used_percent: percent,
                    resets_at,
                    window_minutes: if monthly { None } else { minutes },
                });
            }
            _ => {}
        }
    }
    tokens.sort_by_key(|window| window.window_minutes.unwrap_or(u32::MAX));
    tokens.extend(mcp);
    Some(tokens)
}

/// Counts win over the integer `percentage` when both are present.
fn used_percent(raw: &Value) -> Option<f64> {
    let reported = raw.get("percentage").and_then(Value::as_f64)?;
    let count = |key: &str| raw.get(key).and_then(Value::as_f64);
    let percent = match (count("usage"), count("currentValue"), count("remaining")) {
        (Some(limit), current, Some(remaining)) if limit > 0.0 => {
            let used = (limit - remaining).max(current.unwrap_or(limit - remaining));
            used.clamp(0.0, limit) / limit * 100.0
        }
        (Some(limit), Some(current), None) if limit > 0.0 => {
            current.clamp(0.0, limit) / limit * 100.0
        }
        _ => reported,
    };
    Some(percent.clamp(0.0, 100.0))
}

/// unit: 1 day, 3 hour, 5 minute, 6 week.
fn window_minutes(unit: Option<i64>, number: i64) -> Option<u32> {
    let per_unit = match unit? {
        1 => 1_440,
        3 => 60,
        5 => 1,
        6 => 10_080,
        _ => return None,
    };
    u32::try_from(number.checked_mul(per_unit)?)
        .ok()
        .filter(|minutes| *minutes > 0)
}

fn token_window_name(minutes: Option<u32>) -> (String, String) {
    match minutes {
        Some(300) => ("five_hour".into(), "5-hour".into()),
        Some(10_080) => ("weekly".into(), "Weekly".into()),
        Some(minutes) if minutes % 1_440 == 0 => (
            format!("tokens_{minutes}m"),
            format!("{}-day", minutes / 1_440),
        ),
        Some(minutes) if minutes % 60 == 0 => (
            format!("tokens_{minutes}m"),
            format!("{}-hour", minutes / 60),
        ),
        Some(minutes) => (format!("tokens_{minutes}m"), format!("{minutes}-minute")),
        None => ("tokens".into(), "Tokens".into()),
    }
}
