//! ChatGPT-plan usage for the Codex CLI, as t3code reads it: `codex
//! app-server` over JSON-RPC, `account/read` for the plan and
//! `account/rateLimits/read` for the `primary` / `secondary` windows
//! (`usedPercent`, `windowDurationMins`, `resetsAt` in unix seconds) and any
//! spare reset credits. The CLI uses its own login; nothing is read here.

use std::path::{Path, PathBuf};

use ironclaw_product_contracts::usage::UsageWindow;
use serde_json::{Value, json};

use crate::http::{Fetched, Reading};
use crate::probe::{self, Step};

pub struct CodexLogin {
    binary: Option<PathBuf>,
}

impl CodexLogin {
    pub fn new(home: &Path) -> Self {
        Self {
            binary: probe::find_binary(home, "codex"),
        }
    }

    pub(crate) async fn fetch(&self) -> Fetched {
        let Some(binary) = &self.binary else {
            return Fetched::NotConfigured("The Codex CLI is not installed.".to_string());
        };
        let mut account: Option<Value> = None;
        let mut limits: Option<Value> = None;
        let answer = probe::run(
            binary,
            &["app-server"],
            &[("OPENAI_API_KEY", None)],
            vec![json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
                "clientInfo": {"name": "ironclaw", "title": "IronClaw", "version": env!("CARGO_PKG_VERSION")}}})],
            |message| {
                match message.get("id").and_then(Value::as_i64) {
                    Some(1) => {
                        return Step::Send(vec![
                            json!({"jsonrpc": "2.0", "method": "initialized"}),
                            json!({"jsonrpc": "2.0", "id": 2, "method": "account/read", "params": {}}),
                            json!({"jsonrpc": "2.0", "id": 3, "method": "account/rateLimits/read"}),
                        ]);
                    }
                    Some(2) => account = Some(message["result"]["account"].clone()),
                    Some(3) => limits = Some(message.clone()),
                    _ => {}
                }
                match (&account, &limits) {
                    (Some(account), Some(limits)) => {
                        Step::Done(json!({"account": account, "limits": limits}))
                    }
                    _ => Step::Send(Vec::new()),
                }
            },
        )
        .await;
        let Some(answer) = answer else {
            return Fetched::Error("Codex did not report usage.".to_string());
        };
        if answer["limits"].get("error").is_some() {
            return Fetched::Unauthorized(
                "Sign in to Codex with ChatGPT to see limits.".to_string(),
            );
        }
        match parse(&answer["limits"]["result"], &answer["account"]) {
            Some(reading) => Fetched::Windows(reading),
            None => Fetched::NotConfigured(
                "This Codex login has no plan limits (API key or free tier).".to_string(),
            ),
        }
    }
}

pub(crate) fn parse(result: &Value, account: &Value) -> Option<Reading> {
    let snapshot = result
        .get("rateLimitsByLimitId")
        .and_then(|m| m.get("codex"))
        .or_else(|| result.get("rateLimits"))?;
    let plan = snapshot
        .get("planType")
        .or_else(|| account.get("planType"))
        .and_then(Value::as_str);
    let monthly_plan = matches!(plan, Some("free" | "go"));
    let windows: Vec<UsageWindow> = [
        ("primary", if monthly_plan { 43_200 } else { 300 }),
        ("secondary", 10_080),
    ]
    .into_iter()
    .filter_map(|(slot, fallback)| {
        let w = snapshot.get(slot)?;
        let used = w.get("usedPercent")?.as_f64()?;
        let minutes = w
            .get("windowDurationMins")
            .and_then(Value::as_u64)
            .and_then(|m| u32::try_from(m).ok())
            .unwrap_or(fallback);
        let label = match minutes {
            m if m >= 43_200 => "Monthly",
            m if m >= 10_080 => "Weekly",
            300 => "5-hour",
            m => return Some(window(slot, &format!("{}-hour", m / 60), used, w, m)),
        };
        Some(window(slot, label, used, w, minutes))
    })
    .collect();
    if windows.is_empty() {
        return None;
    }
    let credits = result
        .get("rateLimitResetCredits")
        .and_then(|c| c.get("availableCount"))
        .and_then(Value::as_u64)
        .filter(|n| *n > 0);
    let reached = snapshot
        .get("rateLimitReachedType")
        .and_then(Value::as_str)
        .map(|_| "Limit reached".to_string());
    let credits = credits.map(|n| match n {
        1 => "1 free limit reset available".to_string(),
        n => format!("{n} free limit resets available"),
    });
    Some(Reading {
        windows,
        plan: plan.and_then(probe::plan_name),
        detail: match (reached, credits) {
            (Some(r), Some(c)) => Some(format!("{r} · {c}")),
            (r, c) => r.or(c),
        },
    })
}

fn window(slot: &str, label: &str, used: f64, raw: &Value, minutes: u32) -> UsageWindow {
    UsageWindow {
        id: slot.to_string(),
        label: label.to_string(),
        used_percent: used.clamp(0.0, 100.0),
        resets_at: raw.get("resetsAt").and_then(probe::epoch_ms),
        window_minutes: Some(minutes),
    }
}
