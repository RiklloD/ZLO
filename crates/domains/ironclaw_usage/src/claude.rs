//! CLI-login usage. First the CLI itself, as t3code asks it: `claude -p
//! --input-format stream-json` with a `get_usage` control request, which
//! answers with the plan and every window and keeps the CLI's own token
//! refresh. Then `GET api.anthropic.com/api/oauth/usage` per CodexBar's
//! `ClaudeOAuthUsageFetcher` (the freshest of the credentials file and the
//! Keychain item, the `oauth-2025-04-20` beta header). Both answer
//! `utilization` (0-100) and an RFC 3339 `resets_at` per window. Read-only:
//! tokens are never refreshed here.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use ironclaw_network::NetworkHttpEgress;
use ironclaw_product_contracts::usage::UsageWindow;
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::http::{Fetched, Reading, classify_status, get};
use crate::probe::{self, Step};

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
    /// The `claude` binary for the `get_usage` probe; `None` skips it.
    binary: Option<PathBuf>,
    /// Whether this login lives in a non-default config dir
    /// (`CLAUDE_CONFIG_DIR` is set for the probe only then).
    custom_dir: bool,
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
            binary: None,
            custom_dir: false,
        }
    }

    pub fn with_observed_file(mut self, path: PathBuf) -> Self {
        self.observed_file = Some(path);
        self
    }

    /// Ask `binary` for limits before trying the endpoint; `custom_dir`
    /// passes this login's config dir as `CLAUDE_CONFIG_DIR`.
    pub fn with_cli(mut self, binary: PathBuf, custom_dir: bool) -> Self {
        self.binary = Some(binary);
        self.custom_dir = custom_dir;
        self
    }

    /// CLI 1 (`~/.claude`) and CLI 2 (`~/.claude-account2`). A config dir set
    /// through `CLAUDE_CONFIG_DIR` gets the Keychain item
    /// `Claude Code-credentials-<first 8 hex of sha256(dir)>`.
    pub fn defaults(home: &Path) -> Vec<Self> {
        let keychain = cfg!(target_os = "macos");
        let binary = probe::find_binary(home, "claude");
        let second = home.join(".claude-account2");
        let observed = home.join(".ironclaw").join("usage");
        let second_service = format!("{KEYCHAIN_SERVICE}-{}", hex8(&second.to_string_lossy()));
        let accounts = vec![
            (
                Self::new(
                    "claude_cli",
                    "Claude CLI",
                    home.join(".claude"),
                    keychain.then(|| KEYCHAIN_SERVICE.to_string()),
                )
                .with_observed_file(observed.join("claude-cli.json")),
                false,
            ),
            (
                Self::new(
                    "claude_cli_2",
                    "Claude CLI 2",
                    second,
                    keychain.then_some(second_service),
                )
                .with_observed_file(observed.join("claude-cli-2.json")),
                true,
            ),
        ];
        accounts
            .into_iter()
            .map(|(account, custom)| match &binary {
                Some(binary) => account.with_cli(binary.clone(), custom),
                None => account,
            })
            .collect()
    }

    /// The CLI's `get_usage` answer, then the usage endpoint unless switched
    /// off, then the CLI's last turn reading.
    pub(crate) async fn fetch(&self, egress: &dyn NetworkHttpEgress, now_ms: i64) -> Fetched {
        if let Some(reading) = self.probe_cli().await {
            return Fetched::Windows(reading);
        }
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
                    "Live limits are unavailable; showing the CLI's last turn."
                } else {
                    "From the CLI's last turn."
                }
                .to_string(),
            },
            None => primary,
        }
    }

    /// `get_usage` over the CLI's control protocol. `None` when the CLI is
    /// missing, not logged in, or gave no windows.
    async fn probe_cli(&self) -> Option<Reading> {
        let binary = self.binary.as_ref()?;
        let config_dir = self.custom_dir.then_some(self.config_dir.as_path());
        let answer = probe::run(
            binary,
            &[
                "-p",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--verbose",
            ],
            &[
                ("CLAUDE_CONFIG_DIR", config_dir),
                ("ANTHROPIC_API_KEY", None),
                ("DISABLE_AUTOUPDATER", Some(Path::new("1"))),
            ],
            vec![
                serde_json::json!({"type": "control_request", "request_id": "ic-init",
                    "request": {"subtype": "initialize"}}),
                serde_json::json!({"type": "control_request", "request_id": "ic-usage",
                    "request": {"subtype": "get_usage"}}),
            ],
            |message| {
                let response = &message["response"];
                if message["type"] == "control_response" && response["request_id"] == "ic-usage" {
                    Step::Done(response.clone())
                } else {
                    Step::Send(Vec::new())
                }
            },
        )
        .await?;
        let reading = parse_cli_usage(&answer);
        if reading.is_none() {
            tracing::debug!(provider = %self.provider_id, "claude get_usage gave no windows");
        }
        reading
    }

    /// The driver's file as a live overlay: windows newer than `since`.
    pub(crate) async fn observed_since(
        &self,
        since: i64,
        now_ms: i64,
    ) -> Option<(Vec<UsageWindow>, i64)> {
        self.observed(now_ms).await.filter(|(_, at)| *at > since)
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
                windows => Fetched::Windows(windows.into()),
            },
            Err(error) => {
                tracing::debug!(%error, "usage response is not JSON");
                Fetched::Error("The usage response was unreadable.".to_string())
            }
        }
    }

    /// A live token from the credentials file (re-read every poll, so CLI
    /// refreshes show up), else the Keychain item, else whichever expires
    /// last. The CLI on macOS refreshes only the Keychain copy, so a stale
    /// file left by an older login must not shadow it.
    async fn token(&self, now_ms: i64) -> Result<Option<OauthToken>, String> {
        let path = self.config_dir.join(".credentials.json");
        let file = match tokio::fs::read(&path).await {
            Ok(bytes) => parse_credentials(&bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                tracing::debug!(%error, "credentials file unreadable");
                return Err("The CLI credentials file is unreadable.".to_string());
            }
        };
        if file.as_ref().is_some_and(|t| live(t, now_ms)) {
            return Ok(file);
        }
        let keychain = self.keychain_token(now_ms).await;
        Ok(pick_token(file, keychain, now_ms))
    }

    async fn keychain_token(&self, now_ms: i64) -> Option<OauthToken> {
        let service = self.keychain_service.as_ref()?;
        match self.cached_keychain_token() {
            Some(None) => return None,
            // An expired token is re-read: the CLI may have refreshed it.
            Some(Some(cached)) if cached.expires_at.is_none_or(|at| at > now_ms) => {
                return Some(cached);
            }
            _ => {}
        }
        let token = read_keychain(service).await;
        if let Ok(mut cache) = self.keychain_cache.lock() {
            *cache = Some(token.clone());
        }
        token
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

fn live(token: &OauthToken, now_ms: i64) -> bool {
    token.expires_at.is_none_or(|at| at > now_ms)
}

/// A live file token, else a live Keychain token, else the later expiry.
fn pick_token(
    file: Option<OauthToken>,
    keychain: Option<OauthToken>,
    now_ms: i64,
) -> Option<OauthToken> {
    match (file, keychain) {
        (Some(f), _) if live(&f, now_ms) => Some(f),
        (Some(f), Some(k)) if f.expires_at > k.expires_at => Some(f),
        (f, k) => k.or(f),
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
    let mut windows: Vec<UsageWindow> = WINDOWS
        .iter()
        .filter_map(|(key, id, label, minutes)| window(body.get(*key)?, id, label, *minutes))
        .collect();
    // Per-model weekly caps the CLI names itself (t3code's `model_scoped`).
    for entry in body
        .get("model_scoped")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(name) = entry.get("display_name").and_then(Value::as_str) else {
            continue;
        };
        let id = format!("seven_day_{}", name.to_ascii_lowercase());
        if windows.iter().all(|w| w.id != id)
            && let Some(w) = window(entry, &id, &format!("Weekly {name}"), 10_080)
        {
            windows.push(w);
        }
    }
    windows
}

fn window(raw: &Value, id: &str, label: &str, minutes: u32) -> Option<UsageWindow> {
    let used = raw.get("utilization")?.as_f64()?;
    let resets_at = raw
        .get("resets_at")
        .and_then(Value::as_str)
        .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
        .map(|at| at.timestamp_millis());
    Some(UsageWindow {
        id: id.to_string(),
        label: label.to_string(),
        used_percent: used.clamp(0.0, 100.0),
        resets_at,
        window_minutes: Some(minutes),
    })
}

/// The `get_usage` control response: `subscription_type`, `rate_limits`
/// (same window shape as the endpoint) and an optional `extra_usage`.
pub(crate) fn parse_cli_usage(answer: &Value) -> Option<Reading> {
    if answer.get("subtype").and_then(Value::as_str) != Some("success") {
        return None;
    }
    let body = answer.get("response")?;
    let limits = body.get("rate_limits")?;
    let windows = parse_windows(limits);
    if windows.is_empty() {
        return None;
    }
    let extra = limits
        .get("extra_usage")
        .filter(|e| e.get("is_enabled").and_then(Value::as_bool) == Some(true));
    let detail = extra.and_then(|e| {
        let used = e.get("used_credits")?.as_f64()?;
        let limit = e.get("monthly_limit")?.as_f64()?;
        Some(format!(
            "Extra usage {used:.0} of {limit:.0} credits this month"
        ))
    });
    Some(Reading {
        windows,
        plan: body
            .get("subscription_type")
            .and_then(Value::as_str)
            .and_then(probe::plan_name),
        detail,
    })
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

#[cfg(test)]
mod token_tests {
    use super::*;

    fn token(name: &str, expires_at: i64) -> OauthToken {
        OauthToken {
            access: SecretString::from(name.to_string()),
            expires_at: Some(expires_at),
        }
    }

    /// The bug behind "the usage endpoint did not answer": a credentials file
    /// left by an old login expired a day ago while the CLI kept refreshing
    /// its Keychain copy, and the file won.
    #[test]
    fn an_expired_file_does_not_shadow_a_live_keychain_token() {
        let now = 1_000_000;
        let picked = pick_token(
            Some(token("file", now - 1)),
            Some(token("kc", now + 5)),
            now,
        );
        assert_eq!(picked.unwrap().access.expose_secret(), "kc");
        let picked = pick_token(
            Some(token("file", now + 9)),
            Some(token("kc", now + 5)),
            now,
        );
        assert_eq!(picked.unwrap().access.expose_secret(), "file");
        let picked = pick_token(Some(token("file", now - 1)), None, now);
        assert_eq!(
            picked.unwrap().access.expose_secret(),
            "file",
            "expired still reports"
        );
    }
}
