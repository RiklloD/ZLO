//! Native macOS notifications for new inbox items.
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

use zlo_client::notification_inbox::{
    ProductNotification, ProductNotificationAction, ProductNotificationKind,
};

#[cfg(target_os = "macos")]
mod macos;

/// Where clicking a posted system notification routes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlertTarget {
    pub computer: String,
    pub thread_id: Option<String>,
    /// Gateway notification id (`None` for local `run:` alerts).
    pub notification_id: Option<String>,
}

#[derive(Default)]
pub struct Alerts {
    /// Computers whose first unread page has already been primed (by computer key).
    primed: HashSet<String>,
    /// Unread notification ids already seen, keyed by `(computer_key, notification_id)`.
    seen: HashSet<(String, String)>,
    /// Notification tag to its routing target, for click handling.
    targets: HashMap<String, AlertTarget>,
}

impl Alerts {
    /// The unread items from `computer` not seen before. The first call for
    /// each computer only primes it (marking existing items seen without
    /// replaying them). Because keys are `(computer, id)`, moving focus
    /// between computers never duplicates an alert already seen on a peer or
    /// while focused.
    pub fn take_new<'a>(
        &mut self,
        computer: &str,
        page: &'a [ProductNotification],
    ) -> Vec<&'a ProductNotification> {
        let fresh: Vec<_> = page
            .iter()
            // Finished and failed runs arrive already resolved; only an
            // answered approval or sign-in is stale by the time we poll.
            .filter(|n| n.read_at.is_none() && (n.resolved_at.is_none() || is_outcome(n.kind)))
            .filter(|n| self.seen.insert((computer.to_string(), n.id.clone())))
            .collect();
        if self.primed.insert(computer.to_string()) {
            return Vec::new();
        }
        fresh
    }

    pub fn post(
        &mut self,
        computer: &str,
        computer_label: Option<&str>,
        item: &ProductNotification,
        thread_title: Option<&str>,
    ) {
        let tag = notification_tag(computer, &item.id);
        tracing::debug!(computer, id = %item.id, ?item.kind, "posting system notification");
        self.targets.insert(
            tag.clone(),
            AlertTarget {
                computer: computer.to_string(),
                thread_id: thread_of(item).map(str::to_string),
                notification_id: Some(item.id.clone()),
            },
        );
        let title = format_title(computer_label, describe(item.kind));
        notify(&tag, &title, thread_title.unwrap_or("Conversation"));
    }

    /// A chat run seen finishing on the live stream.
    pub fn post_run(
        &mut self,
        computer: &str,
        computer_label: Option<&str>,
        thread_id: &str,
        failed: bool,
        thread_title: Option<&str>,
    ) {
        let tag = format!("run:{computer}:{thread_id}");
        tracing::debug!(%tag, failed, "posting system notification");
        self.targets.insert(
            tag.clone(),
            AlertTarget {
                computer: computer.to_string(),
                thread_id: Some(thread_id.to_string()),
                notification_id: None,
            },
        );
        let base = if failed { "Run failed" } else { "Run finished" };
        let title = format_title(computer_label, base);
        notify(&tag, &title, thread_title.unwrap_or("Conversation"));
    }

    pub fn target_for(&self, tag: &str) -> Option<AlertTarget> {
        self.targets.get(tag).cloned()
    }
}

/// Stable tag for a gateway notification, scoped by computer so equal ids on
/// two computers never collide.
pub fn notification_tag(computer: &str, id: &str) -> String {
    format!("{computer}:{id}")
}

/// Title prefixed with the computer's label when more than one computer is
/// connected (`Some(label)`), or unchanged with a single computer (`None`).
pub fn format_title(computer_label: Option<&str>, base: &str) -> String {
    match computer_label {
        Some(label) if !label.is_empty() => format!("{label}: {base}"),
        _ => base.to_string(),
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
    post_with_notify_send(tag, title, body);
}

/// Linux desktops show notifications through libnotify's `notify-send`
/// (Rune's notifications are macOS and Windows only). Posting only: a
/// click raises nothing here. No `notify-send` on the login PATH: skipped.
#[cfg(not(target_os = "macos"))]
fn post_with_notify_send(tag: &str, title: &str, body: &str) {
    let Some(program) = crate::system::find_on_path("notify-send", &crate::system::search_path())
    else {
        tracing::debug!(tag, "no notify-send; notification skipped");
        return;
    };
    let spawned = std::process::Command::new(program)
        .args(["--app-name=ZLO", "--"])
        .arg(title)
        .arg(body)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match spawned {
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(e) => tracing::debug!(error = %e, tag, "notification not posted"),
    }
}

/// The thread a notification opens.
pub fn thread_of(item: &ProductNotification) -> Option<&str> {
    match &item.action {
        ProductNotificationAction::OpenThread { thread_id } => Some(thread_id),
        ProductNotificationAction::None => item.thread_id.as_deref(),
    }
}

/// The alert title per kind.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn notif(id: &str, thread: &str) -> ProductNotification {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "kind": "approval_required",
            "severity": "warning",
            "action": { "kind": "open_thread", "thread_id": thread },
            "thread_id": thread,
            "turn_run_id": null,
            "created_at": "2026-11-26T00:00:00Z",
            "updated_at": "2026-11-26T00:00:00Z",
            "read_at": null,
            "resolved_at": null
        }))
        .expect("notification")
    }

    #[test]
    fn priming_is_per_computer_and_replays_nothing_on_first_read() {
        let mut alerts = Alerts::default();
        let a1 = notif("n1", "t1");
        let b1 = notif("n1", "t1");
        let b2 = notif("n2", "t2");
        assert!(
            alerts
                .take_new("this-mac", std::slice::from_ref(&a1))
                .is_empty()
        );
        assert!(
            alerts
                .take_new("studio", &[b1.clone(), b2.clone()])
                .is_empty()
        );

        let a3 = notif("n3", "t3");
        let a_page = [a1, a3];
        let fresh_a: Vec<_> = alerts
            .take_new("this-mac", &a_page)
            .into_iter()
            .map(|n| n.id.as_str())
            .collect();
        assert_eq!(fresh_a, vec!["n3"]);

        let b4 = notif("n4", "t4");
        let b_page = [b1, b2, b4];
        let fresh_b: Vec<_> = alerts
            .take_new("studio", &b_page)
            .into_iter()
            .map(|n| n.id.as_str())
            .collect();
        assert_eq!(fresh_b, vec!["n4"]);
    }

    #[test]
    fn focus_move_never_duplicates_alerts() {
        let mut alerts = Alerts::default();
        let n1 = notif("n1", "t1");
        let n2 = notif("n2", "t2");
        let n3 = notif("n3", "t3");

        // Studio as a peer primes with n1, then sees n2 as new.
        assert!(
            alerts
                .take_new("studio", std::slice::from_ref(&n1))
                .is_empty()
        );
        assert_eq!(
            alerts.take_new("studio", &[n1.clone(), n2.clone()]).len(),
            1
        );

        // Focus moves to Studio: neither n1 nor n2 replays, and n3 still fires.
        assert!(
            alerts
                .take_new("studio", &[n1.clone(), n2.clone()])
                .is_empty()
        );
        let page = [n1.clone(), n2.clone(), n3.clone()];
        let fresh: Vec<_> = alerts
            .take_new("studio", &page)
            .into_iter()
            .map(|n| n.id.as_str())
            .collect();
        assert_eq!(fresh, vec!["n3"]);

        // Focus moves away from Studio (it becomes a peer again): nothing replays.
        assert!(alerts.take_new("studio", &[n1, n2, n3]).is_empty());
    }

    #[test]
    fn title_includes_computer_label_only_when_multiple_computers_connected() {
        assert_eq!(format_title(None, "Approval needed"), "Approval needed");
        assert_eq!(
            format_title(Some("Studio"), "Approval needed"),
            "Studio: Approval needed"
        );
    }
}
