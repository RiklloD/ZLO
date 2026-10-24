//! Muse Code's 5-hour and weekly windows. Muse reports them only during a
//! turn (`usage/changed`; `usage/read` is empty before one) and keeps no
//! copy, so this reads the last reading a runtime saved: IronClaw's driver
//! (`~/.ironclaw/usage/muse-code.json`, `{"observedAt", "usage": {...}}`) or
//! the Hermes runtime (`muse-usage.json`, the bare reading with
//! `observedAtMs`), whichever is newer. As in t3code, a window past its reset
//! has restarted and reads 0 %.

use std::path::{Path, PathBuf};

use ironclaw_product_contracts::usage::UsageWindow;
use serde_json::Value;

use crate::http::Fetched;

pub struct MuseReading {
    files: Vec<PathBuf>,
}

impl MuseReading {
    pub fn new(home: &Path) -> Self {
        Self {
            files: vec![
                home.join(".ironclaw/usage/muse-code.json"),
                home.join(".hermes/plugin-data/t3code/muse-usage.json"),
            ],
        }
    }

    pub(crate) async fn fetch(&self, now_ms: i64) -> Fetched {
        match self.latest(now_ms).await {
            Some((windows, at)) => Fetched::Observed {
                windows,
                at,
                note: "Muse Code reports limits during a turn; this is its last reading."
                    .to_string(),
            },
            None => Fetched::NotConfigured(
                "Muse Code reports its limits during a turn. Run one Muse Code chat to see them."
                    .to_string(),
            ),
        }
    }

    /// The newest saved reading, for the live overlay too.
    pub(crate) async fn latest(&self, now_ms: i64) -> Option<(Vec<UsageWindow>, i64)> {
        let mut best: Option<(Vec<UsageWindow>, i64)> = None;
        for path in &self.files {
            let Ok(bytes) = tokio::fs::read(path).await else {
                continue; // silent-ok: a missing file means that runtime never ran a turn
            };
            let Ok(doc) = serde_json::from_slice::<Value>(&bytes) else {
                tracing::debug!(path = %path.display(), "muse usage file is not JSON");
                continue;
            };
            let (reading, at) = match doc.get("usage") {
                Some(usage) => (usage, doc.get("observedAt").and_then(Value::as_i64)),
                None => (&doc, doc.get("observedAtMs").and_then(Value::as_i64)),
            };
            let Some(at) = at else { continue };
            let windows = parse(reading, now_ms);
            if !windows.is_empty() && best.as_ref().is_none_or(|(_, b)| at > *b) {
                best = Some((windows, at));
            }
        }
        best
    }
}

pub(crate) fn parse(reading: &Value, now_ms: i64) -> Vec<UsageWindow> {
    [
        ("window", "five_hour", 300),
        ("weekly", "seven_day", 10_080),
    ]
    .into_iter()
    .filter_map(|(key, id, fallback)| {
        let raw = reading.get(key)?;
        let used = raw.get("usedPercent")?.as_f64()?;
        let minutes = raw
            .get("windowDurationMins")
            .and_then(Value::as_u64)
            .and_then(|m| u32::try_from(m).ok())
            .unwrap_or(fallback);
        let resets_at = raw.get("resetsAtMs").and_then(Value::as_i64);
        let restarted = resets_at.is_some_and(|at| at <= now_ms);
        Some(UsageWindow {
            id: id.to_string(),
            label: if minutes >= 10_080 {
                "Weekly"
            } else {
                "5-hour"
            }
            .to_string(),
            used_percent: if restarted {
                0.0
            } else {
                used.clamp(0.0, 100.0)
            },
            resets_at: resets_at.filter(|_| !restarted),
            window_minutes: Some(minutes),
        })
    })
    .collect()
}
