//! Progress pacing per channel: the session channel's in-process sink is
//! reconciled at display rate, vendor channels at the provider-safe interval.
//! Driven through the production publication lane like `tests.rs`, whose
//! harness this reuses.

use std::time::Duration;

use ironclaw_extension_contracts::channel::ReplyTransport;
use ironclaw_extension_contracts::reply::{ReplyAudience, ReplyReconcilePoint};

use super::ReplyPublicationSettings;
use super::tests::{Harness, harness_with, settings};

/// Production pacing: vendor channels at the provider-safe 250 ms, the
/// session channel at display rate.
fn production_pacing() -> ReplyPublicationSettings {
    let defaults = ReplyPublicationSettings::default();
    ReplyPublicationSettings {
        min_progress_interval: defaults.min_progress_interval,
        session_progress_interval: defaults.session_progress_interval,
        ..settings()
    }
}

/// Stream `count` growing answers `every` apart and return how many
/// `Progress` reconciles the sink heard while they streamed.
async fn progress_reconciles_while_streaming(
    harness: &Harness,
    count: usize,
    every: Duration,
) -> usize {
    let mut text = String::new();
    for index in 0..count {
        text.push_str(&format!("word{index} "));
        harness.text(&text);
        tokio::time::sleep(every).await;
    }
    harness
        .sink
        .requests()
        .iter()
        .filter(|r| r.point == ReplyReconcilePoint::Progress)
        .count()
}

#[tokio::test]
async fn the_session_channel_streams_progress_at_display_rate() {
    // The session sink feeds the in-process live projection the desktop and
    // WebUI streams tail; pacing it like a rate-limited vendor API turned a
    // smooth token stream into quarter-second blocks.
    let harness = harness_with(
        "session-rate",
        ReplyTransport::Stream,
        Some("web-app"),
        production_pacing(),
    );
    let progress =
        progress_reconciles_while_streaming(&harness, 16, Duration::from_millis(30)).await;
    // ~480 ms of streaming: 250 ms pacing allows at most 2-3 progress
    // reconciles, display-rate pacing about a dozen.
    assert!(
        progress >= 6,
        "the session channel reconciled only {progress} progress revisions in ~480 ms"
    );
}

#[tokio::test]
async fn vendor_channels_keep_the_provider_safe_progress_interval() {
    let harness = harness_with(
        "vendor-rate",
        ReplyTransport::Stream,
        None,
        production_pacing(),
    );
    harness
        .coordinator
        .register_reply_target(harness.registration(ReplyAudience::Private))
        .await
        .unwrap();
    let progress =
        progress_reconciles_while_streaming(&harness, 16, Duration::from_millis(30)).await;
    assert!(
        progress <= 3,
        "a vendor channel must stay paced at the provider-safe interval, got {progress} progress reconciles in ~480 ms"
    );
}
