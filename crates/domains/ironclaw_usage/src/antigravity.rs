//! Antigravity plan quota, as t3code reads it: the ACP sign-in's refresh
//! token (`antigravity-acp/acp_token.json` in the profile IronClaw's driver
//! uses) buys a short-lived access token at the file's own `token_uri`, then
//! `v1internal:retrieveUserQuotaSummary` lists the plan's buckets
//! (`remainingFraction`, `resetTime`, a `5h` / `weekly` window), with
//! `fetchAvailableModels` as the fallback for older accounts. The refreshed
//! access token stays in memory; the file is never written.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ironclaw_network::NetworkHttpEgress;
use ironclaw_product_contracts::usage::UsageWindow;
use secrecy::{ExposeSecret, SecretString};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::http::{Fetched, Reading, classify_status, post};

const QUOTA_HOSTS: &[&str] = &[
    "daily-cloudcode-pa.googleapis.com",
    "cloudcode-pa.googleapis.com",
];
const USER_AGENT: &str = "antigravity/hub/1.0.0 (aidev_client; os_type=darwin; cl=963137146)";

pub struct AntigravityLogin {
    token_files: Vec<PathBuf>,
    /// `(fingerprint of the refresh token, access token, expiry millis)`.
    access: Mutex<Option<(String, SecretString, i64)>>,
}

struct SignIn {
    client_id: String,
    client_secret: SecretString,
    refresh_token: SecretString,
    token_uri: String,
    project_id: Option<String>,
}

impl AntigravityLogin {
    /// Hermes's profile first (IronClaw's driver signs in there when it
    /// exists), then IronClaw's own.
    pub fn new(home: &Path) -> Self {
        let file = |dir: PathBuf| dir.join("antigravity-acp").join("acp_token.json");
        Self {
            token_files: vec![
                file(home.join(".hermes/plugin-data/t3code/antigravity/profile")),
                file(home.join(".ironclaw/antigravity/profile")),
            ],
            access: Mutex::new(None),
        }
    }

    pub(crate) async fn fetch(&self, egress: &dyn NetworkHttpEgress, now_ms: i64) -> Fetched {
        let Some(sign_in) = self.sign_in().await else {
            return Fetched::NotConfigured(
                "Sign in to Antigravity once to see its limits.".to_string(),
            );
        };
        let access = match self.access_token(egress, &sign_in, now_ms).await {
            Ok(access) => access,
            Err(fetched) => return fetched,
        };
        let body = match &sign_in.project_id {
            Some(project) => json!({"project": project}),
            None => json!({}),
        }
        .to_string()
        .into_bytes();
        let headers = vec![
            (
                "Authorization".to_string(),
                format!("Bearer {}", access.expose_secret()),
            ),
            ("Content-Type".to_string(), "application/json".to_string()),
            ("User-Agent".to_string(), USER_AGENT.to_string()),
        ];
        let mut last = Fetched::Error("Antigravity returned no quota.".to_string());
        for (operation, parse) in [
            (
                "retrieveUserQuotaSummary",
                summary_windows as fn(&Value) -> Vec<UsageWindow>,
            ),
            ("fetchAvailableModels", model_windows),
        ] {
            for host in QUOTA_HOSTS {
                let url = format!("https://{host}/v1internal:{operation}");
                let response = match post(egress, host, url, headers.clone(), body.clone()).await {
                    Ok(response) => response,
                    Err(fetched) => {
                        last = fetched;
                        continue;
                    }
                };
                if let Some(fetched) = classify_status(&response) {
                    if matches!(fetched, Fetched::Unauthorized(_)) {
                        self.forget_access();
                    }
                    last = fetched;
                    continue;
                }
                let windows = serde_json::from_slice::<Value>(&response.body)
                    .map(|v| parse(&v))
                    .unwrap_or_default(); // silent-ok: an unreadable body tries the next host
                if !windows.is_empty() {
                    return Fetched::Windows(Reading::from(windows));
                }
            }
        }
        last
    }

    async fn sign_in(&self) -> Option<SignIn> {
        for path in &self.token_files {
            let Ok(bytes) = tokio::fs::read(path).await else {
                continue; // silent-ok: that profile never signed in
            };
            let Ok(doc) = serde_json::from_slice::<Value>(&bytes) else {
                tracing::debug!(path = %path.display(), "antigravity token file is not JSON");
                continue;
            };
            let field = |key: &str| {
                doc.get(key)
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty())
                    .map(str::to_string)
            };
            if let (Some(client_id), Some(secret), Some(refresh), Some(token_uri)) = (
                field("client_id"),
                field("client_secret"),
                field("refresh_token"),
                field("token_uri"),
            ) {
                return Some(SignIn {
                    client_id,
                    client_secret: SecretString::from(secret),
                    refresh_token: SecretString::from(refresh),
                    token_uri,
                    project_id: field("project_id"),
                });
            }
        }
        None
    }

    async fn access_token(
        &self,
        egress: &dyn NetworkHttpEgress,
        sign_in: &SignIn,
        now_ms: i64,
    ) -> Result<SecretString, Fetched> {
        let fingerprint: String = Sha256::digest(sign_in.refresh_token.expose_secret().as_bytes())
            .iter()
            .take(8)
            .map(|b| format!("{b:02x}"))
            .collect();
        if let Ok(cache) = self.access.lock()
            && let Some((fp, token, expires)) = cache.as_ref()
            && *fp == fingerprint
            && *expires > now_ms + 60_000
        {
            return Ok(token.clone());
        }
        let Some(host) = url_host(&sign_in.token_uri) else {
            return Err(Fetched::Error(
                "The Antigravity sign-in is unreadable.".to_string(),
            ));
        };
        let form = format!(
            "client_id={}&client_secret={}&refresh_token={}&grant_type=refresh_token",
            form_encode(&sign_in.client_id),
            form_encode(sign_in.client_secret.expose_secret()),
            form_encode(sign_in.refresh_token.expose_secret()),
        );
        let headers = vec![(
            "Content-Type".to_string(),
            "application/x-www-form-urlencoded".to_string(),
        )];
        let response = post(
            egress,
            &host,
            sign_in.token_uri.clone(),
            headers,
            form.into_bytes(),
        )
        .await?;
        if response.status == 400 || response.status == 401 {
            return Err(Fetched::Unauthorized(
                "The Antigravity sign-in expired; sign in again.".to_string(),
            ));
        }
        if let Some(fetched) = classify_status(&response) {
            return Err(fetched);
        }
        let body: Value = serde_json::from_slice(&response.body).map_err(|error| {
            tracing::debug!(%error, "antigravity token response is not JSON");
            Fetched::Error("The Antigravity sign-in could not be refreshed.".to_string())
        })?;
        let Some(access) = body.get("access_token").and_then(Value::as_str) else {
            return Err(Fetched::Error(
                "The Antigravity sign-in could not be refreshed.".to_string(),
            ));
        };
        let access = SecretString::from(access.to_string());
        let expires = now_ms + body.get("expires_in").and_then(Value::as_i64).unwrap_or(0) * 1000;
        if let Ok(mut cache) = self.access.lock() {
            *cache = Some((fingerprint, access.clone(), expires));
        }
        Ok(access)
    }

    fn forget_access(&self) {
        if let Ok(mut cache) = self.access.lock() {
            *cache = None;
        }
    }
}

fn url_host(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?;
    let host = rest.split(['/', ':', '?']).next()?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// `application/x-www-form-urlencoded` value encoding.
fn form_encode(raw: &str) -> String {
    raw.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            b => format!("%{b:02X}"),
        })
        .collect()
}

fn quota_window(
    id: &str,
    label: String,
    quota: &Value,
    minutes: Option<u32>,
) -> Option<UsageWindow> {
    let remaining = quota.get("remainingFraction")?.as_f64()?;
    Some(UsageWindow {
        id: id.to_string(),
        label,
        used_percent: ((1.0 - remaining.clamp(0.0, 1.0)) * 100.0).clamp(0.0, 100.0),
        resets_at: quota
            .get("resetTime")
            .and_then(Value::as_str)
            .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
            .map(|at| at.timestamp_millis()),
        window_minutes: minutes,
    })
}

/// The summary's buckets; Claude and GPT share one `3p-` allowance.
pub(crate) fn summary_windows(summary: &Value) -> Vec<UsageWindow> {
    let groups = summary
        .get("groups")
        .and_then(Value::as_array)
        .filter(|g| g.iter().any(|g| g.get("buckets").is_some()))
        .cloned()
        .unwrap_or_else(|| {
            vec![json!({"buckets": summary.get("buckets").cloned().unwrap_or_default()})]
        });
    let mut windows: Vec<UsageWindow> = Vec::new();
    for group in &groups {
        for bucket in group
            .get("buckets")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(id) = bucket.get("bucketId").and_then(Value::as_str) else {
                continue;
            };
            if bucket.get("disabled").and_then(Value::as_bool) == Some(true)
                || windows.iter().any(|w| w.id == id)
            {
                continue;
            }
            let family = if id.starts_with("gemini-") {
                "Gemini".to_string()
            } else if id.starts_with("3p-") {
                "Claude & GPT".to_string()
            } else {
                group
                    .get("displayName")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|n| !n.is_empty())
                    .unwrap_or(id)
                    .to_string()
            };
            let (span, minutes) = match bucket.get("window").and_then(Value::as_str) {
                Some("weekly") => ("Weekly".to_string(), Some(10_080)),
                Some("5h") => ("5-hour".to_string(), Some(300)),
                _ => (
                    bucket
                        .get("displayName")
                        .and_then(Value::as_str)
                        .unwrap_or("Quota")
                        .trim()
                        .to_string(),
                    None,
                ),
            };
            if let Some(w) = quota_window(id, format!("{family} {span}"), bucket, minutes) {
                windows.push(w);
            }
        }
    }
    windows
}

/// Older accounts: per-model quotas with no window length; the busiest
/// model per family stands for it.
pub(crate) fn model_windows(models: &Value) -> Vec<UsageWindow> {
    let mut windows: Vec<UsageWindow> = Vec::new();
    for (model_id, model) in models
        .get("models")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
    {
        let provider = model
            .get("modelProvider")
            .and_then(Value::as_str)
            .unwrap_or("");
        let family = match () {
            _ if provider == "MODEL_PROVIDER_GOOGLE" || model_id.starts_with("gemini-") => "Gemini",
            _ if provider == "MODEL_PROVIDER_ANTHROPIC" || model_id.starts_with("claude-") => {
                "Claude"
            }
            _ if provider == "MODEL_PROVIDER_OPENAI" || model_id.starts_with("gpt-") => "GPT",
            _ => continue,
        };
        for (suffix, key, minutes) in [
            ("quota", "quotaInfo", None),
            ("weekly", "weeklyQuotaInfo", Some(10_080)),
        ] {
            let Some(quota) = model.get(key) else {
                continue;
            };
            let id = format!("{}-{suffix}", family.to_ascii_lowercase());
            let span = if minutes.is_some() { "Weekly" } else { "Quota" };
            let Some(w) = quota_window(&id, format!("{family} {span}"), quota, minutes) else {
                continue;
            };
            match windows.iter_mut().find(|x| x.id == id) {
                Some(existing) if existing.used_percent < w.used_percent => *existing = w,
                Some(_) => {}
                None => windows.push(w),
            }
        }
    }
    windows
}
