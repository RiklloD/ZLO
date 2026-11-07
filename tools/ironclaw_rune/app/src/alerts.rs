//! Native macOS notifications for new inbox items (GPUI `alerts.rs`).
//!
//! The gateway inbox is the source: the workspace polls it, and each unread
//! item this process has not seen yet becomes one system notification. Items
//! already in the inbox at startup are only marked seen, so launching the app
//! never replays old alerts. macOS shows them only when the app runs from its
//! `.app` bundle; elsewhere they are skipped with a log line why.
//!
//! Rune has no notification API, so `notify` posts through
//! `UNUserNotificationCenter` itself (`alerts/macos.rs`); a click lands in
//! [`crate::workspace::alert_clicked`] with the notification's tag.

use std::collections::{HashMap, HashSet};

use ironclaw_gpui_client::notification_inbox::{
    ProductNotification, ProductNotificationAction, ProductNotificationKind,
};

#[cfg(target_os = "macos")]
mod macos;

#[derive(Default)]
pub struct Alerts {
    primed: bool,
    seen: HashSet<String>,
    /// Notification id to the thread it opens, for click handling.
    threads: HashMap<String, String>,
}

impl Alerts {
    /// The unread items not seen before. The first call only primes.
    pub fn take_new<'a>(
        &mut self,
        page: &'a [ProductNotification],
    ) -> Vec<&'a ProductNotification> {
        let fresh: Vec<_> = page
            .iter()
            // Finished and failed runs arrive already resolved; only an
            // answered approval or sign-in is stale by the time we poll.
            .filter(|n| n.read_at.is_none() && (n.resolved_at.is_none() || is_outcome(n.kind)))
            .filter(|n| self.seen.insert(n.id.clone()))
            .collect();
        if !self.primed {
            self.primed = true;
            return Vec::new();
        }
        fresh
    }

    pub fn post(&mut self, item: &ProductNotification, thread_title: Option<&str>) {
        tracing::debug!(id = %item.id, ?item.kind, "posting system notification");
        if let Some(thread) = thread_of(item) {
            self.threads.insert(item.id.clone(), thread.to_string());
        }
        notify(
            &item.id,
            describe(item.kind),
            thread_title.unwrap_or("Conversation"),
        );
    }

    /// A chat run seen finishing on the live stream.
    pub fn post_run(&mut self, thread_id: &str, failed: bool, thread_title: Option<&str>) {
        let tag = format!("run:{thread_id}");
        tracing::debug!(%tag, failed, "posting system notification");
        self.threads.insert(tag.clone(), thread_id.to_string());
        notify(
            &tag,
            if failed { "Run failed" } else { "Run finished" },
            thread_title.unwrap_or("Conversation"),
        );
    }

    pub fn thread_for(&self, notification_id: &str) -> Option<String> {
        self.threads.get(notification_id).cloned()
    }
}

/// Ask macOS for permission to notify, and route clicks back to the app.
/// Called once at startup.
pub fn install(handle: rune::AppHandle<crate::state::State>) {
    #[cfg(target_os = "macos")]
    macos::install(handle);
    #[cfg(not(target_os = "macos"))]
    let _ = handle;
}

fn notify(tag: &str, title: &str, body: &str) {
    #[cfg(target_os = "macos")]
    macos::post(tag, title, body);
    #[cfg(not(target_os = "macos"))]
    let _ = (tag, title, body);
}

/// The thread a notification opens (GPUI `pages::notifications::thread_of`,
/// kept here so alerts need nothing from the page).
pub fn thread_of(item: &ProductNotification) -> Option<&str> {
    match &item.action {
        ProductNotificationAction::OpenThread { thread_id } => Some(thread_id),
        ProductNotificationAction::None => item.thread_id.as_deref(),
    }
}

/// The alert title per kind (the label half of GPUI's
/// `pages::notifications::describe`).
pub fn describe(kind: ProductNotificationKind) -> &'static str {
    match kind {
        ProductNotificationKind::ApprovalRequired => "Approval needed",
        ProductNotificationKind::AuthenticationRequired => "Sign-in needed",
        ProductNotificationKind::RunBlocked => "Run blocked",
        ProductNotificationKind::RunFailed => "Run failed",
        ProductNotificationKind::RunCompleted => "Run finished",
        ProductNotificationKind::DeliveryFailed => "Delivery failed",
    }
}

fn is_outcome(kind: ProductNotificationKind) -> bool {
    matches!(
        kind,
        ProductNotificationKind::RunCompleted
            | ProductNotificationKind::RunFailed
            | ProductNotificationKind::DeliveryFailed
    )
}
