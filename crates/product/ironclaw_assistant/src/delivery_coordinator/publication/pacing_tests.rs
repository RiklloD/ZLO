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
        session_push_interval: defaults.session_push_interval,
        ..settings()
    }
}

/// Stream `count` growing answers `every` apart and return how many
/// `Progress` reconciles each kind of target heard while they streamed:
/// `(session, vendor)`. The session target has no vendor conversation.
async fn progress_reconciles_while_streaming(
    harness: &Harness,
    count: usize,
    every: Duration,
) -> (usize, usize) {
    let mut text = String::new();
    for index in 0..count {
        text.push_str(&format!("word{index} "));
        harness.text(&text);
        tokio::time::sleep(every).await;
    }
    let progress: Vec<_> = harness
        .sink
        .requests()
        .into_iter()
        .filter(|r| r.point == ReplyReconcilePoint::Progress)
        .collect();
    let session = progress
        .iter()
        .filter(|r| r.target.conversation.is_none())
        .count();
    (session, progress.len() - session)
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
    let (session, _) =
        progress_reconciles_while_streaming(&harness, 16, Duration::from_millis(30)).await;
    // ~480 ms of streaming: 250 ms pacing allows at most 2-3 progress
    // reconciles, display-rate pushes about a dozen.
    assert!(
        session >= 6,
        "the session channel heard only {session} progress revisions in ~480 ms"
    );
}

#[tokio::test]
async fn vendor_channels_keep_the_provider_safe_progress_interval() {
    // Beside a session channel streaming at display rate, a vendor target
    // of the same run stays at the provider-safe interval.
    let harness = harness_with(
        "vendor-rate",
        ReplyTransport::Stream,
        Some("web-app"),
        production_pacing(),
    );
    harness
        .coordinator
        .register_reply_target(harness.registration(ReplyAudience::Private))
        .await
        .unwrap();
    let (session, vendor) =
        progress_reconciles_while_streaming(&harness, 16, Duration::from_millis(30)).await;
    assert!(session >= 6, "the session channel heard {session} updates");
    assert!(
        vendor <= 3,
        "a vendor channel must stay paced at the provider-safe interval, got {vendor} progress reconciles in ~480 ms"
    );
}
