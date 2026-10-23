//! CLI-login usage (`GET api.anthropic.com/api/oauth/usage`), per CodexBar's
//! `ClaudeOAuthUsageFetcher`: the OAuth access token from the CLI's config dir,
//! the `oauth-2025-04-20` beta header, `utilization` (0-100) and an RFC 3339
//! `resets_at` per window. Read-only: tokens are never refreshed here.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use ironclaw_network::NetworkHttpEgress;
use ironclaw_product_contracts::usage::UsageWindow;
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::http::{Fetched, classify_status, get};

const HOST: &str = "api.anthropic.com";
const URL: &str = "https://api.anthropic.com/api/oauth/usage";
const BETA: &str = "oauth-2025-04-20";
const USER_AGENT: &str = "claude-cli/2.1.0 (external, cli)";
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
const KEYCHAIN_TIMEOUT: Duration = Duration::from_secs(10);

/// `(response key, window id, label, window minutes)`.
const WINDOWS: &[(&str, &str, &str, u32)] = &[
    ("five_hour", "five_hour", "5-hour", 300),
    ("seven_day", "seven_day", "Weekly", 10_080),
    ("seven_day_opus", "seven_day_opus", "Weekly Opus", 10_080),
    (
        "seven_day_sonnet",
        "seven_day_sonnet",
        "Weekly Sonnet",
        10_080,
    ),
];

/// One CLI login: its config dir and, on macOS, the Keychain item the CLI
/// falls back to when `.credentials.json` is absent.
pub struct ClaudeAccount {
    pub(crate) provider_id: String,
    pub(crate) label: String,
    config_dir: PathBuf,
    keychain_service: Option<String>,
    /// Keychain answer cached in memory so `security` (and any access prompt)
    /// runs once per token lifetime. Outer `None`: not read yet; `Some(None)`:
    /// read and empty or denied, which is not retried until restart.
    keychain_cache: Mutex<Option<Option<OauthToken>>>,
    /// The CLI driver's last limit reading (`~/.ironclaw/usage/<runtime>.json`),
    /// the fallback when the endpoint fails or is switched off.
    observed_file: Option<PathBuf>,
}

#[derive(Clone)]
struct OauthToken {
    access: SecretString,
    expires_at: Option<i64>,
}

impl ClaudeAccount {
    pub fn new(
        provider_id: impl Into<String>,
        label: impl Into<String>,
        config_dir: PathBuf,
        keychain_service: Option<String>,
    ) -> Self {
        Self {
            provider_id: provider_id.into(),
            label: label.into(),
            config_dir,
            keychain_service,
            keychain_cache: Mutex::new(None),
            observed_file: None,
        }
    }

    pub fn with_observed_file(mut self, path: PathBuf) -> Self {
        self.observed_file = Some(path);
        self
    }

    /// CLI 1 (`~/.claude`) and CLI 2 (`~/.claude-account2`). A config dir set
    /// through `CLAUDE_CONFIG_DIR` gets the Keychain item
    /// `Claude Code-credentials-<first 8 hex of sha256(dir)>`.
    pub fn defaults(home: &Path) -> Vec<Self> {
        let keychain = cfg!(target_os = "macos");
        let second = home.join(".claude-account2");
        let observed = home.join(".ironclaw").join("usage");
        let second_service = format!("{KEYCHAIN_SERVICE}-{}", hex8(&second.to_string_lossy()));
        vec![
            Self::new(
                "claude_cli",
                "Claude CLI",
                home.join(".claude"),
                keychain.then(|| KEYCHAIN_SERVICE.to_string()),
            )
            .with_observed_file(observed.join("claude-cli.json")),
            Self::new(
                "claude_cli_2",
                "Claude CLI 2",
                second,
                keychain.then_some(second_service),
            )
            .with_observed_file(observed.join("claude-cli-2.json")),
        ]
    }

    /// The usage endpoint first, unless switched off; the CLI's own last
    /// reading when the endpoint gives no windows.
    pub(crate) async fn fetch(&self, egress: &dyn NetworkHttpEgress, now_ms: i64) -> Fetched {
        let endpoint = endpoint_enabled(std::env::var(ENDPOINT_SWITCH).ok().as_deref());
        let primary = if endpoint {
            self.fetch_endpoint(egress, now_ms).await
        } else {
            Fetched::NotConfigured(format!(
                "Run one Claude CLI turn to see limits ({ENDPOINT_SWITCH} is off)."
            ))
        };
        if matches!(primary, Fetched::Windows(_)) {
            return primary;
        }
        match self.observed(now_ms).await {
            Some((windows, at)) => Fetched::Observed {
                windows,
                at,
                note: if endpoint {
                    "The usage endpoint did not answer; showing the CLI's last reading."
                } else {
                    "From the CLI's last turn."
                }
                .to_string(),
            },
            None => primary,
        }
    }

    /// Windows from the driver's file that have not reset yet.
    async fn observed(&self, now_ms: i64) -> Option<(Vec<UsageWindow>, i64)> {
        let bytes = tokio::fs::read(self.observed_file.as_ref()?).await.ok()?; // silent-ok: no file means no CLI turn yet
        let doc: Value = serde_json::from_slice(&bytes).ok()?;
        let at = doc.get("observedAt")?.as_i64()?;
        let windows = parse_observed(doc.get("windows")?, now_ms);
        (!windows.is_empty()).then_some((windows, at))
    }

    async fn fetch_endpoint(&self, egress: &dyn NetworkHttpEgress, now_ms: i64) -> Fetched {
        let token = match self.token(now_ms).await {
            Ok(Some(token)) => token,
            Ok(None) => return Fetched::NotConfigured("Not logged in on this Mac.".to_string()),
            Err(reason) => return Fetched::Error(reason),
        };
        if token.expires_at.is_some_and(|at| at <= now_ms) {
            return Fetched::Unauthorized(
                "The login expired; use the CLI once to refresh it.".to_string(),
            );
        }
        let headers = vec![
            (
                "Authorization".to_string(),
                format!("Bearer {}", token.access.expose_secret()),
            ),
            ("anthropic-beta".to_string(), BETA.to_string()),
            ("Accept".to_string(), "application/json".to_string()),
            ("User-Agent".to_string(), USER_AGENT.to_string()),
        ];
        let response = match get(egress, HOST, URL.to_string(), headers).await {
            Ok(response) => response,
            Err(fetched) => return fetched,
        };
        if let Some(fetched) = classify_status(&response) {
            if matches!(fetched, Fetched::Unauthorized(_)) {
                self.forget_keychain_token();
            }
            return fetched;
        }
        match serde_json::from_slice::<Value>(&response.body) {
            Ok(body) => match parse_windows(&body) {
                windows if windows.is_empty() => {
                    Fetched::Error("The usage response had no windows.".to_string())
                }
                windows => Fetched::Windows(windows),
            },
            Err(error) => {
                tracing::debug!(%error, "usage response is not JSON");
                Fetched::Error("The usage response was unreadable.".to_string())
            }
        }
    }

    /// The file wins (re-read every poll, so CLI refreshes show up); the
    /// Keychain is the fallback.
    async fn token(&self, now_ms: i64) -> Result<Option<OauthToken>, String> {
        let path = self.config_dir.join(".credentials.json");
        match tokio::fs::read(&path).await {
            Ok(bytes) => return Ok(parse_credentials(&bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::debug!(%error, "credentials file unreadable");
                return Err("The CLI credentials file is unreadable.".to_string());
            }
        }
        let Some(service) = &self.keychain_service else {
            return Ok(None);
        };
        match self.cached_keychain_token() {
            Some(None) => return Ok(None),
            // An expired token is re-read: the CLI may have refreshed it.
            Some(Some(cached)) if cached.expires_at.is_none_or(|at| at > now_ms) => {
                return Ok(Some(cached));
            }
            _ => {}
        }
        let token = read_keychain(service).await;
        if let Ok(mut cache) = self.keychain_cache.lock() {
            *cache = Some(token.clone());
        }
        Ok(token)
    }

    fn cached_keychain_token(&self) -> Option<Option<OauthToken>> {
        self.keychain_cache
            .lock()
            .ok()
            .and_then(|cache| cache.clone())
    }

    fn forget_keychain_token(&self) {
        if let Ok(mut cache) = self.keychain_cache.lock() {
            *cache = None;
        }
    }
}

/// `/usr/bin/security find-generic-password -s <service> -w`. A missing item
/// (or any failure) reads as "not logged in"; the secret never reaches a log.
async fn read_keychain(service: &str) -> Option<OauthToken> {
    let output = tokio::time::timeout(
        KEYCHAIN_TIMEOUT,
        tokio::process::Command::new("/usr/bin/security")
            .args(["find-generic-password", "-s", service, "-w"])
            .kill_on_drop(true)
            .output(),
    )
    .await;
    match output {
        Ok(Ok(output)) if output.status.success() => parse_credentials(&output.stdout),
        Ok(Ok(output)) => {
            tracing::debug!(service, code = ?output.status.code(), "keychain item not readable");
            None
        }
        Ok(Err(error)) => {
            tracing::debug!(%error, "keychain read failed");
            None
        }
        Err(_elapsed) => {
            tracing::debug!(service, "keychain read timed out");
            None
        }
    }
}

/// `{"claudeAiOauth": {"accessToken", "expiresAt"}}`, from the file or the
/// Keychain item (same document).
fn parse_credentials(bytes: &[u8]) -> Option<OauthToken> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    let oauth = value.get("claudeAiOauth")?;
    let access = oauth.get("accessToken")?.as_str()?.trim();
    if access.is_empty() {
        return None;
    }
    Some(OauthToken {
        access: SecretString::from(access.to_string()),
        expires_at: oauth.get("expiresAt").and_then(Value::as_i64),
    })
}

pub(crate) fn parse_windows(body: &Value) -> Vec<UsageWindow> {
    WINDOWS
        .iter()
        .filter_map(|(key, id, label, minutes)| {
            let window = body.get(*key)?;
            let used = window.get("utilization")?.as_f64()?;
            let resets_at = window
                .get("resets_at")
                .and_then(Value::as_str)
                .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
                .map(|at| at.timestamp_millis());
            Some(UsageWindow {
                id: (*id).to_string(),
                label: (*label).to_string(),
                used_percent: used.clamp(0.0, 100.0),
                resets_at,
                window_minutes: Some(*minutes),
            })
        })
        .collect()
}

/// `IRONCLAW_USAGE_CLAUDE_ENDPOINT=off` (or `0`, `false`) stops the
/// undocumented endpoint call and with it every Keychain read.
const ENDPOINT_SWITCH: &str = "IRONCLAW_USAGE_CLAUDE_ENDPOINT";

fn endpoint_enabled(raw: Option<&str>) -> bool {
    !matches!(
        raw.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("off" | "0" | "false" | "no")
    )
}

/// The CLI's `unifiedWindows`: utilization is a fraction, `resetsAt` epoch
/// seconds. A window past its reset is dropped (its reading is stale).
pub(crate) fn parse_observed(windows: &Value, now_ms: i64) -> Vec<UsageWindow> {
    const OBSERVED: &[(&str, &str, &str, u32)] = &[
        ("five_hour", "five_hour", "5-hour", 300),
        ("seven_day", "seven_day", "Weekly", 10_080),
    ];
    OBSERVED
        .iter()
        .filter_map(|(key, id, label, minutes)| {
            let window = windows.get(*key)?;
            let used = window.get("utilization")?.as_f64()?;
            let resets_at = window.get("resetsAt")?.as_i64()? * 1000;
            (resets_at > now_ms).then(|| UsageWindow {
                id: (*id).to_string(),
                label: (*label).to_string(),
                used_percent: (used * 100.0).clamp(0.0, 100.0),
                resets_at: Some(resets_at),
                window_minutes: Some(*minutes),
            })
        })
        .collect()
}

fn hex8(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
pub(crate) fn keychain_service_for(dir: &Path) -> String {
    format!("{KEYCHAIN_SERVICE}-{}", hex8(&dir.to_string_lossy()))
}
