//! Fixture-driven tests through `UsageMonitor::report`; a recording egress
//! stands in for the network, so nothing reaches the real endpoints.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use ironclaw_host_api::ids::SecretHandle;
use ironclaw_network::{
    NetworkHttpEgress, NetworkHttpError, NetworkHttpRequest, NetworkHttpResponse, NetworkUsage,
};
use ironclaw_product_contracts::operator_secrets::OperatorSecretValueStore;
use ironclaw_product_contracts::test_support::fakes::FakeOperatorSecretValueStore;
use ironclaw_product_contracts::usage::UsageStatus;
use secrecy::SecretString;

use crate::{ClaudeAccount, UsageMonitor, UsageSource, ZaiKey};

/// `(url, header names and values)` of one request.
type Call = (String, Vec<(String, String)>);

#[derive(Default)]
struct RecordingEgress {
    replies: Mutex<VecDeque<NetworkHttpResponse>>,
    calls: Mutex<Vec<Call>>,
}

impl RecordingEgress {
    fn reply(&self, status: u16, headers: &[(&str, &str)], body: &str) {
        self.replies.lock().unwrap().push_back(NetworkHttpResponse {
            status,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: body.as_bytes().to_vec(),
            usage: NetworkUsage::default(),
        });
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl NetworkHttpEgress for RecordingEgress {
    async fn execute(
        &self,
        request: NetworkHttpRequest,
    ) -> Result<NetworkHttpResponse, NetworkHttpError> {
        assert_eq!(request.policy.allowed_targets.len(), 1, "one-host policy");
        self.calls
            .lock()
            .unwrap()
            .push((request.url.clone(), request.headers.clone()));
        Ok(self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("test queued a reply"))
    }
}

const CLAUDE_USAGE: &str = r#"{
  "five_hour": {"utilization": 42.0, "resets_at": "2026-10-23T20:00:00+00:00"},
  "seven_day": {"utilization": 130, "resets_at": null},
  "seven_day_opus": null,
  "some_new_field": {"anything": true}
}"#;

fn claude_account(dir: &Path) -> ClaudeAccount {
    ClaudeAccount::new("claude_cli", "Claude CLI", dir.to_path_buf(), None)
}

fn write_credentials(dir: &Path, expires_at: i64) {
    let doc = format!(
        r#"{{"claudeAiOauth":{{"accessToken":"tok-1","refreshToken":"r","expiresAt":{expires_at}}}}}"#
    );
    std::fs::write(dir.join(".credentials.json"), doc).unwrap();
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

#[tokio::test(start_paused = true)]
async fn cli_login_windows_are_cached_and_survive_a_429() {
    let dir = tempfile::tempdir().unwrap();
    write_credentials(dir.path(), i64::MAX);
    let egress = Arc::new(RecordingEgress::default());
    let monitor = UsageMonitor::with_sources(
        egress.clone(),
        vec![UsageSource::Claude(claude_account(dir.path()))],
    );

    egress.reply(200, &[], CLAUDE_USAGE);
    let first = monitor.report().await.providers.remove(0);
    assert_eq!(first.status, UsageStatus::Ok);
    let ids: Vec<_> = first.windows.iter().map(|w| w.id.as_str()).collect();
    assert_eq!(
        ids,
        ["five_hour", "seven_day"],
        "null and unknown keys are dropped"
    );
    assert_eq!(first.windows[0].used_percent, 42.0);
    assert_eq!(first.windows[0].resets_at, Some(1_792_785_600_000));
    assert_eq!(first.windows[1].used_percent, 100.0, "clamped");

    let calls = egress.calls();
    assert_eq!(calls[0].0, "https://api.anthropic.com/api/oauth/usage");
    assert_eq!(header(&calls[0].1, "authorization"), Some("Bearer tok-1"));
    assert_eq!(
        header(&calls[0].1, "anthropic-beta"),
        Some("oauth-2025-04-20")
    );

    // Inside the 120 s floor the cache answers.
    let cached = monitor.report().await.providers.remove(0);
    assert_eq!(cached, first);
    assert_eq!(egress.calls().len(), 1);

    // A 429 keeps the last windows and honours Retry-After.
    tokio::time::advance(Duration::from_secs(121)).await;
    egress.reply(429, &[("Retry-After", "600")], "");
    let limited = monitor.report().await.providers.remove(0);
    assert_eq!(limited.status, UsageStatus::RateLimited);
    assert_eq!(limited.windows, first.windows);
    tokio::time::advance(Duration::from_secs(300)).await;
    monitor.report().await;
    assert_eq!(egress.calls().len(), 2, "still backing off after 5 min");
}

#[tokio::test]
async fn missing_or_expired_login_never_calls_the_provider() {
    let dir = tempfile::tempdir().unwrap();
    let egress = Arc::new(RecordingEgress::default());
    let missing = UsageMonitor::with_sources(
        egress.clone(),
        vec![UsageSource::Claude(claude_account(dir.path()))],
    );
    let report = missing.report().await;
    assert_eq!(report.providers[0].status, UsageStatus::NotConfigured);

    write_credentials(dir.path(), 1_000);
    let expired = UsageMonitor::with_sources(
        egress.clone(),
        vec![UsageSource::Claude(claude_account(dir.path()))],
    );
    let report = expired.report().await;
    assert_eq!(report.providers[0].status, UsageStatus::Unauthorized);
    assert!(egress.calls().is_empty());
}

#[tokio::test]
async fn zai_quota_reads_the_stored_key_and_maps_limits() {
    let secrets = Arc::new(FakeOperatorSecretValueStore::new());
    let egress = Arc::new(RecordingEgress::default());
    let monitor = UsageMonitor::with_sources(
        egress.clone(),
        vec![UsageSource::Zai(ZaiKey::new(secrets.clone()))],
    );
    assert_eq!(
        monitor.report().await.providers[0].status,
        UsageStatus::NotConfigured
    );
    assert!(egress.calls().is_empty());

    secrets
        .put(
            &SecretHandle::new("llm_provider_glm_api_key").unwrap(),
            SecretString::from("zk-1".to_string()),
        )
        .await
        .unwrap();
    let monitor =
        UsageMonitor::with_sources(egress.clone(), vec![UsageSource::Zai(ZaiKey::new(secrets))]);
    let reset = chrono::Utc::now().timestamp_millis() + 3_600_000;
    egress.reply(
        200,
        &[],
        &format!(
            r#"{{"code":200,"success":true,"data":{{"limits":[
              {{"type":"TIME_LIMIT","unit":5,"number":1,"usage":1000,"currentValue":250,"percentage":25,"nextResetTime":{reset}}},
              {{"type":"TOKENS_LIMIT","unit":3,"number":5,"percentage":12,"nextResetTime":{reset}}},
              {{"type":"FUTURE_LIMIT","unit":3,"number":5,"percentage":1}}
            ]}}}}"#
        ),
    );
    let zai = monitor.report().await.providers.remove(0);
    assert_eq!(zai.status, UsageStatus::Ok);
    assert_eq!(
        header(&egress.calls()[0].1, "authorization"),
        Some("Bearer zk-1")
    );
    let windows: Vec<_> = zai
        .windows
        .iter()
        .map(|w| {
            (
                w.id.as_str(),
                w.label.as_str(),
                w.used_percent,
                w.window_minutes,
            )
        })
        .collect();
    assert_eq!(
        windows,
        [
            ("five_hour", "5-hour", 12.0, Some(300)),
            ("mcp", "MCP (monthly)", 25.0, None),
        ]
    );
    assert_eq!(zai.windows[0].resets_at, Some(reset));
}

#[test]
fn second_login_keychain_item_follows_the_cli_naming() {
    assert_eq!(
        crate::claude::keychain_service_for(Path::new("/Users/lorik/.claude-account2")),
        "Claude Code-credentials-fdc51ded"
    );
}

/// The Usage page lists (and probes) the second Claude login, and scans its
/// history, only for a different signed-in account; probing an unused one
/// created `~/.claude-account2` on a fresh install.
#[test]
fn the_second_login_is_used_only_for_a_different_signed_in_account() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let write = |path: PathBuf, uuid: Option<&str>| {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body = match uuid {
            Some(uuid) => serde_json::json!({"oauthAccount": {"accountUuid": uuid}}),
            None => serde_json::json!({"numStartups": 1}),
        };
        std::fs::write(path, body.to_string()).unwrap();
    };
    let ids = || -> (Vec<String>, Vec<String>) {
        (
            ClaudeAccount::defaults(home)
                .into_iter()
                .map(|a| a.provider_id)
                .collect(),
            crate::history::HistorySource::defaults(home)
                .into_iter()
                .map(|h| h.provider)
                .collect(),
        )
    };
    let one = (
        vec!["claude_cli".to_string()],
        vec!["claude_cli".into(), "codex".into()],
    );
    let second = home.join(".claude-account2").join(".claude.json");
    write(home.join(".claude.json"), Some("acct-a"));
    assert_eq!(ids(), one, "no second login");
    assert!(!home.join(".claude-account2").exists(), "nothing created");
    write(second.clone(), None);
    assert_eq!(ids(), one, "a login file that never signed in");
    write(second.clone(), Some("acct-a"));
    assert_eq!(ids(), one, "the same account twice");
    write(second, Some("acct-b"));
    assert_eq!(
        ids(),
        (
            vec!["claude_cli".to_string(), "claude_cli_2".into()],
            vec!["claude_cli".into(), "claude_cli_2".into(), "codex".into()]
        )
    );
}

#[tokio::test(start_paused = true)]
async fn a_failing_endpoint_falls_back_to_the_clis_own_reading() {
    let dir = tempfile::tempdir().unwrap();
    write_credentials(dir.path(), i64::MAX);
    let observed = dir.path().join("claude-cli.json");
    let future = chrono::Utc::now().timestamp() + 3_600;
    std::fs::write(
        &observed,
        format!(
            r#"{{"observedAt": 1000, "windows": {{
                "five_hour": {{"utilization": 0.42, "resetsAt": {future}}},
                "seven_day": {{"utilization": 0.9, "resetsAt": 1}}}}}}"#
        ),
    )
    .unwrap();
    let egress = Arc::new(RecordingEgress::default());
    let monitor = UsageMonitor::with_sources(
        egress.clone(),
        vec![UsageSource::Claude(
            claude_account(dir.path()).with_observed_file(observed),
        )],
    );
    egress.reply(500, &[], "");
    let usage = monitor.report().await.providers.remove(0);
    assert_eq!(usage.status, UsageStatus::Ok);
    assert!(usage.message.unwrap().contains("CLI's last turn"));
    assert_eq!(usage.fetched_at, Some(1000));
    let five: Vec<_> = usage
        .windows
        .iter()
        .map(|w| (w.id.as_str(), w.used_percent))
        .collect();
    assert_eq!(
        five,
        [("five_hour", 42.0)],
        "a window past its reset is dropped"
    );
}

#[tokio::test(start_paused = true)]
async fn a_turn_reading_newer_than_the_cache_moves_the_bars_live() {
    let dir = tempfile::tempdir().unwrap();
    write_credentials(dir.path(), i64::MAX);
    let observed = dir.path().join("claude-cli.json");
    let egress = Arc::new(RecordingEgress::default());
    let monitor = UsageMonitor::with_sources(
        egress.clone(),
        vec![UsageSource::Claude(
            claude_account(dir.path()).with_observed_file(observed.clone()),
        )],
    );
    egress.reply(200, &[], CLAUDE_USAGE);
    let first = monitor.report().await.providers.remove(0);

    let later = first.fetched_at.unwrap() + 1;
    let future = chrono::Utc::now().timestamp() + 3_600;
    std::fs::write(
        &observed,
        format!(
            r#"{{"observedAt": {later}, "windows": {{
                "five_hour": {{"utilization": 0.5, "resetsAt": {future}}}}}}}"#
        ),
    )
    .unwrap();
    let live = monitor.report().await.providers.remove(0);
    assert_eq!(egress.calls().len(), 1, "still served from the cache");
    assert_eq!(live.fetched_at, Some(later));
    assert_eq!(live.windows[0].used_percent, 50.0, "five_hour overlaid");
    assert_eq!(live.windows[1], first.windows[1], "seven_day kept");
}

#[test]
fn cli_get_usage_answer_maps_plan_windows_and_model_caps() {
    let answer = serde_json::json!({"subtype": "success", "request_id": "ic-usage", "response": {
        "subscription_type": "max", "rate_limits_available": true, "rate_limits": {
            "five_hour": {"utilization": 11, "resets_at": "2026-10-24T23:50:00.411591+00:00"},
            "seven_day": {"utilization": 28, "resets_at": "2026-10-25T16:00:00+00:00"},
            "seven_day_opus": null,
            "model_scoped": [{"display_name": "Fable", "utilization": 3,
                "resets_at": "2026-10-25T16:00:00+00:00"}],
            "extra_usage": {"is_enabled": true, "used_credits": 4, "monthly_limit": 50}}}});
    let reading = crate::claude::parse_cli_usage(&answer).unwrap();
    assert_eq!(reading.plan.as_deref(), Some("Max"));
    let ids: Vec<_> = reading.windows.iter().map(|w| w.id.as_str()).collect();
    assert_eq!(ids, ["five_hour", "seven_day", "seven_day_fable"]);
    assert_eq!(reading.windows[2].label, "Weekly Fable");
    assert_eq!(
        reading.detail.as_deref(),
        Some("Extra usage 4 of 50 credits this month")
    );
    let failed = serde_json::json!({"subtype": "error", "error": "not logged in"});
    assert!(crate::claude::parse_cli_usage(&failed).is_none());
}

#[test]
fn codex_rate_limits_map_slots_plan_and_reset_credits() {
    let result = serde_json::json!({
        "rateLimits": {"primary": {"usedPercent": 99, "windowDurationMins": 1, "resetsAt": 1}},
        "rateLimitsByLimitId": {"codex": {"planType": "plus", "rateLimitReachedType": null,
            "primary": {"usedPercent": 3, "windowDurationMins": 300, "resetsAt": 1792887193},
            "secondary": {"usedPercent": 4, "windowDurationMins": 10080, "resetsAt": 1793385704}}},
        "rateLimitResetCredits": {"availableCount": 2}});
    let reading = crate::codex::parse(&result, &serde_json::json!({})).unwrap();
    assert_eq!(reading.plan.as_deref(), Some("Plus"));
    let rows: Vec<_> = reading
        .windows
        .iter()
        .map(|w| (w.label.as_str(), w.used_percent, w.resets_at))
        .collect();
    assert_eq!(
        rows,
        [
            ("5-hour", 3.0, Some(1_792_887_193_000)),
            ("Weekly", 4.0, Some(1_793_385_704_000))
        ]
    );
    assert_eq!(
        reading.detail.as_deref(),
        Some("2 free limit resets available")
    );
}

#[test]
fn muse_reading_restarts_windows_past_their_reset() {
    let reading = serde_json::json!({
        "window": {"usedPercent": 80, "windowDurationMins": 300, "resetsAtMs": 1_000},
        "weekly": {"usedPercent": 73, "resetsAtMs": 9_000}});
    let windows = crate::muse::parse(&reading, 5_000);
    assert_eq!(windows[0].used_percent, 0.0, "the 5-hour window restarted");
    assert_eq!(windows[0].resets_at, None);
    assert_eq!(windows[1].used_percent, 73.0);
    assert_eq!(windows[1].label, "Weekly");
}

#[test]
fn antigravity_summary_names_shared_and_gemini_buckets() {
    let summary = serde_json::json!({"groups": [{"displayName": "Plan", "buckets": [
        {"bucketId": "gemini-pro-5h", "window": "5h", "remainingFraction": 0.75,
         "resetTime": "2026-10-24T20:00:00Z"},
        {"bucketId": "3p-weekly", "window": "weekly", "remainingFraction": 1.0},
        {"bucketId": "3p-5h", "window": "5h", "remainingFraction": 0.5},
        {"bucketId": "off", "disabled": true, "remainingFraction": 0.0}]}]});
    let windows = crate::antigravity::summary_windows(&summary);
    let rows: Vec<_> = windows
        .iter()
        .map(|w| (w.label.as_str(), w.used_percent, w.window_minutes))
        .collect();
    assert_eq!(
        rows,
        [
            ("Gemini 5-hour", 25.0, Some(300)),
            ("Claude & GPT 5-hour", 50.0, Some(300)),
            ("Claude & GPT Weekly", 0.0, Some(10_080))
        ]
    );
}

/// t3code #38957: agy 1.5 writes no `acp_token.json`; its file sign-in
/// (`antigravity-oauth-token`) holds an access token and its expiry, used
/// as is until it expires (it carries no OAuth client to refresh it).
#[tokio::test]
async fn antigravity_reads_the_cli_sign_in_and_its_expiry() {
    let home = tempfile::tempdir().unwrap();
    let acp = home
        .path()
        .join(".ironclaw/antigravity/profile/antigravity-acp");
    std::fs::create_dir_all(&acp).unwrap();
    std::fs::write(
        acp.join("antigravity-oauth-token"),
        r#"{"token":{"access_token":"ya29.x","refresh_token":"r","expiry":"2026-11-24T12:00:00Z"}}"#,
    )
    .unwrap();
    let expiry = chrono::DateTime::parse_from_rfc3339("2026-11-24T12:00:00Z")
        .unwrap()
        .timestamp_millis();
    match crate::antigravity::cli_access(&acp.join("antigravity-oauth-token")).await {
        Some(crate::antigravity::Credential::Access { expires_ms, .. }) => {
            assert_eq!(expires_ms, expiry)
        }
        _ => panic!("the CLI sign-in was not read"),
    }
}

/// Review P3: a sign-in agy keeps only in the OS keyring reads as signed in
/// with its quota unavailable, never as "sign in"; the keyring entry is
/// looked up by its attributes alone, and no provider is called.
#[tokio::test]
async fn antigravity_keyring_sign_in_is_not_reported_signed_out() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let args = dir.path().join("args");
    let tool = |name: &str, code: i32| {
        let path = dir.path().join(name);
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit {code}\n",
                args.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    };
    let report = |keyring_tool: Option<PathBuf>| {
        let egress = Arc::new(RecordingEgress::default());
        let login = crate::antigravity::AntigravityLogin {
            sign_in_dirs: vec![dir.path().join("profile/antigravity-acp")],
            access: Mutex::new(None),
            keyring_tool,
        };
        async move {
            let monitor =
                UsageMonitor::with_sources(egress.clone(), vec![UsageSource::Antigravity(login)]);
            let answer = monitor.report().await.providers.remove(0);
            assert!(egress.calls().is_empty(), "no provider call");
            answer
        }
    };
    let signed_in = report(Some(tool("present", 0))).await;
    assert_eq!(signed_in.status, UsageStatus::NotConfigured);
    let message = signed_in.message.unwrap();
    assert_eq!(message, crate::antigravity::KEYRING_SIGN_IN);
    assert!(!message.contains("Sign in to"), "{message}");
    let signed_out = report(Some(tool("absent", 44))).await;
    assert!(
        signed_out
            .message
            .unwrap()
            .contains("Sign in to Antigravity"),
        "no keyring entry: still asked to sign in"
    );
    // The same lookup, word for word, as `ironclaw_cli_providers`'
    // `signin::keyring` (its test `production_lookup_matches_the_usage_monitor`
    // pins this literal too): both crates must ask about one entry.
    let lookups = std::fs::read_to_string(&args).unwrap();
    assert!(
        !lookups.is_empty()
            && lookups
                .lines()
                .all(|l| l == "find-generic-password -s gemini -a antigravity"),
        "{lookups}"
    );
}
