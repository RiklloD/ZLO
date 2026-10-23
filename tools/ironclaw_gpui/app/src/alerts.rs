//! Native macOS notifications for new inbox items.
//!
//! The gateway inbox is the source: the workspace polls it, and each unread
//! item this process has not seen yet becomes one system notification. Items
//! already in the inbox at startup are only marked seen, so launching the app
//! never replays old alerts. macOS shows them only when the app runs from its
//! `.app` bundle; elsewhere GPUI skips them and logs why.

use std::collections::{HashMap, HashSet};

use ironclaw_gpui_client::notification_inbox::{ProductNotification, ProductNotificationKind};

use crate::pages::notifications::{describe, thread_of};
use crate::ui::*;

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

    pub fn post(&mut self, item: &ProductNotification, thread_title: Option<&str>, cx: &App) {
        let (_, label) = describe(item.kind);
        tracing::debug!(id = %item.id, ?item.kind, "posting system notification");
        if let Some(thread) = thread_of(item) {
            self.threads.insert(item.id.clone(), thread.to_string());
        }
        cx.show_system_notification(SystemNotification {
            tag: item.id.clone().into(),
            title: label.into(),
            body: thread_title.unwrap_or("Conversation").to_string().into(),
            actions: Vec::new(),
        });
    }

    /// A chat run seen finishing on the live stream.
    pub fn post_run(
        &mut self,
        thread_id: &str,
        failed: bool,
        thread_title: Option<&str>,
        cx: &App,
    ) {
        let tag = format!("run:{thread_id}");
        tracing::debug!(%tag, failed, "posting system notification");
        self.threads.insert(tag.clone(), thread_id.to_string());
        cx.show_system_notification(SystemNotification {
            tag: tag.into(),
            title: if failed { "Run failed" } else { "Run finished" }.into(),
            body: thread_title.unwrap_or("Conversation").to_string().into(),
            actions: Vec::new(),
        });
    }

    pub fn thread_for(&self, notification_id: &str) -> Option<String> {
        self.threads.get(notification_id).cloned()
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
