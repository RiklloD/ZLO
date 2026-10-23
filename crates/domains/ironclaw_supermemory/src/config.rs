//! Deployment settings. The API key is not here: it lives in the secret store
//! under [`SUPERMEMORY_API_KEY_SECRET`] and is read through the client.

use std::time::Duration;

use crate::error::SupermemoryError;

/// Secret-store handle holding the Supermemory API key. No key, no feature.
pub const SUPERMEMORY_API_KEY_SECRET: &str = "supermemory_api_key";

const DEFAULT_BASE_URL: &str = "https://api.supermemory.ai";
/// Hermes `api_timeout` default.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
/// Hermes `max_recall_results` default.
const DEFAULT_MAX_RECALL_RESULTS: usize = 10;
const MAX_RECALL_RESULTS_CEILING: usize = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupermemoryConfig {
    /// API origin without a trailing slash.
    pub base_url: String,
    /// Per-request timeout.
    pub timeout: Duration,
    /// Cap per recall list (profile facts, recent context, memories).
    pub max_recall_results: usize,
    /// Recall before each turn.
    pub auto_recall: bool,
    /// Capture after each completed turn.
    pub auto_capture: bool,
}

impl Default for SupermemoryConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            timeout: DEFAULT_TIMEOUT,
            max_recall_results: DEFAULT_MAX_RECALL_RESULTS,
            auto_recall: true,
            auto_capture: true,
        }
    }
}

impl SupermemoryConfig {
    /// `SUPERMEMORY_BASE_URL`, `SUPERMEMORY_AUTO_RECALL`,
    /// `SUPERMEMORY_AUTO_CAPTURE`, `SUPERMEMORY_MAX_RECALL_RESULTS`; anything
    /// unset keeps the Hermes default.
    pub fn from_env() -> Result<Self, SupermemoryError> {
        let env = |name: &str| std::env::var(name).ok();
        Self::from_values(
            env("SUPERMEMORY_BASE_URL").as_deref(),
            env("SUPERMEMORY_AUTO_RECALL").as_deref(),
            env("SUPERMEMORY_AUTO_CAPTURE").as_deref(),
            env("SUPERMEMORY_MAX_RECALL_RESULTS").as_deref(),
        )
    }

    pub(crate) fn from_values(
        base_url: Option<&str>,
        auto_recall: Option<&str>,
        auto_capture: Option<&str>,
        max_recall_results: Option<&str>,
    ) -> Result<Self, SupermemoryError> {
        let defaults = Self::default();
        let base_url = match base_url.map(str::trim).filter(|value| !value.is_empty()) {
            Some(raw) => {
                let trimmed = raw.trim_end_matches('/');
                ironclaw_network::network_target_for_url(trimmed).map_err(|error| {
                    SupermemoryError::InvalidConfig {
                        reason: format!("SUPERMEMORY_BASE_URL: {error}"),
                    }
                })?;
                trimmed.to_string()
            }
            None => defaults.base_url,
        };
        let max_recall_results = match max_recall_results.map(str::trim) {
            Some(raw) if !raw.is_empty() => raw
                .parse::<usize>()
                .map_err(|error| SupermemoryError::InvalidConfig {
                    reason: format!("SUPERMEMORY_MAX_RECALL_RESULTS: {error}"),
                })?
                .clamp(1, MAX_RECALL_RESULTS_CEILING),
            _ => defaults.max_recall_results,
        };
        Ok(Self {
            base_url,
            timeout: defaults.timeout,
            max_recall_results,
            auto_recall: parse_flag("SUPERMEMORY_AUTO_RECALL", auto_recall, true)?,
            auto_capture: parse_flag("SUPERMEMORY_AUTO_CAPTURE", auto_capture, true)?,
        })
    }
}

fn parse_flag(name: &str, raw: Option<&str>, default: bool) -> Result<bool, SupermemoryError> {
    match raw
        .map(|value| value.trim().to_ascii_lowercase())
        .as_deref()
    {
        None | Some("") => Ok(default),
        Some("1" | "true" | "yes" | "on") => Ok(true),
        Some("0" | "false" | "no" | "off") => Ok(false),
        Some(other) => Err(SupermemoryError::InvalidConfig {
            reason: format!("{name}: expected true/false, got {other:?}"),
        }),
    }
}
