//! The notification inbox: approvals, failures and finished runs the gateway
//! recorded for this user, plus which channels deliver them outside the app.
//! Port of GPUI `pages/notifications.rs`.

use zlo_client::notification_inbox::{
    ProductNotification, ProductNotificationAction, ProductNotificationKind,
    ProductNotificationSeverity,
};
use zlo_client::product_wire::{
    RebornNotificationSetupStatusResponse, RebornOutboundDeliveryTargetId,
    RebornOutboundDeliveryTargetOption,
};
use zlo_client::{Client, ClientError};
use rune::{Cursor, Cx, El, col, el, row};
use rune_kit::Kit;

use super::common::{
    BASE, Btn, SM, XS, actions, button, card, empty_state, error_line, glyph, header, icon_button,
    label, page_body, para, refresh_button, switch,
};
use crate::icons;
use crate::jobs;
use crate::state::State;

const PAGE_LIMIT: u32 = 100;

/// One delivery channel with the user's choice and enrollment state.
pub struct Channel {
    pub option: RebornOutboundDeliveryTargetOption,
    pub selected: bool,
    pub setup: Option<RebornNotificationSetupStatusResponse>,
}

#[derive(Default)]
pub struct NotificationsState {
    pub items: Vec<ProductNotification>,
    pub channels: Vec<Channel>,
    pub loaded: bool,
    pub error: Option<String>,
}

/// The page became visible: load on first visit, refresh after.
pub fn opened(s: &mut State) {
    refresh(s);
}

pub fn refresh(s: &mut State) {
    load_channels(s);
    jobs::run(
        s,
        "notifications",
        |c| async move { c.list_notifications(PAGE_LIMIT).await },
        |s, result, _| {
            match result {
                Ok(page) => {
                    let threads: Vec<String> = page
                        .notifications
                        .iter()
                        .filter_map(thread_of)
                        .map(str::to_string)
                        .collect();
                    crate::workspace::titles_ensure(s, threads);
                    let this = &mut s.pages.notifications;
                    this.items = page.notifications;
                    this.error = None;
                    crate::workspace::set_unread(s, page.unread_count);
                }
                Err(e) => s.pages.notifications.error = Some(e.to_string()),
            }
            s.pages.notifications.loaded = true;
        },
    );
}

fn load_channels(s: &mut State) {
    jobs::run(
        s,
        "notifications",
        |c| async move {
            let targets = c.delivery_targets().await?.targets;
            let chosen = c.notification_channels().await?.channels;
            let mut channels = Vec::new();
            for option in targets {
                let id = &option.target.target_id;
                // silent-ok: a channel without a setup route just shows no enrollment line.
                let setup = c
                    .notification_setup(option.target.channel.as_str())
                    .await
                    .ok();
                channels.push(Channel {
                    selected: chosen.iter().any(|ch| ch.target_id == *id),
                    option,
                    setup,
                });
            }
            Ok(channels)
        },
        |s, result, _| {
            let this = &mut s.pages.notifications;
            match result {
                Ok(channels) => this.channels = channels,
                Err(e) => this.error = Some(e.to_string()),
            }
        },
    );
}

/// Send the full selection with `id` flipped; re-read to show what stuck.
fn toggle_channel(s: &mut State, id: RebornOutboundDeliveryTargetId) {
    let ids: Vec<_> = s
        .pages
        .notifications
        .channels
        .iter()
        .filter(|ch| (ch.option.target.target_id == id) != ch.selected)
        .map(|ch| ch.option.target.target_id.clone())
        .collect();
    jobs::run(
        s,
        "notifications",
        move |c| async move { c.set_notification_channels(ids).await },
        |s, result, _| match result {
            Ok(_) => load_channels(s),
            Err(e) => s.pages.notifications.error = Some(e.to_string()),
        },
    );
}

fn render_channels(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let mut d = col()
        .key("channels")
        .w_full()
        .gap(8.)
        .pt(24.)
        .child(label("Delivery channels", BASE, 600., p.text))
        .child(para(
            "This app shows notifications itself while it runs. \
             Pick where else IronClaw should send them.",
            XS,
            p.text_muted,
        ));
    for ch in &s.pages.notifications.channels {
        let target = &ch.option.target;
        let id = target.target_id.clone();
        // Web push enrolls a browser, which only the web UI can do.
        let enrollment = ch.setup.as_ref().filter(|st| st.requires_setup).map(|st| {
            if st.enabled {
                "Enrolled".to_string()
            } else {
                "No device enrolled yet. Enable it from the web UI in a browser.".to_string()
            }
        });
        let mut text_col = col().grow(1.0).min_w(0.).child(label(
            target.display_name.as_str().to_string(),
            SM,
            500.,
            p.text,
        ));
        if let Some(desc) = target.description.as_ref() {
            text_col = text_col.child(para(desc.as_str().to_string(), XS, p.text_muted));
        }
        if let Some(e) = enrollment {
            text_col = text_col.child(label(e, XS, 400., p.text_muted));
        }
        d = d.child(card(p).key(format!("channel-{}", id.as_str())).child(
            row().gap(12.).center_y().child(text_col).child(switch(
                k,
                ch.selected,
                false,
                &format!("Send notifications to {}", target.display_name.as_str()),
                move |s: &mut State| toggle_channel(s, id.clone()),
            )),
        ));
    }
    d
}

fn open(s: &mut State, id: &str) {
    let Some(item) = s.pages.notifications.items.iter().find(|n| n.id == id) else {
        return;
    };
    let unread_id = item.read_at.is_none().then(|| item.id.clone());
    let thread = thread_of(item).map(str::to_string);
    if let Some(id) = unread_id {
        mutate(
            s,
            move |c| async move { c.mark_notification_read(&id).await },
        );
    }
    if let Some(thread) = thread {
        crate::workspace::open_thread(s, thread);
    }
}

fn mutate<F, Fut, T>(s: &mut State, f: F)
where
    F: FnOnce(Client) -> Fut,
    Fut: std::future::Future<Output = Result<T, ClientError>> + Send + 'static,
    T: Send + 'static,
{
    jobs::run(s, "notifications", f, |s, result, _| match result {
        Ok(_) => refresh(s),
        Err(e) => s.pages.notifications.error = Some(e.to_string()),
    });
}

fn render_row(s: &State, k: &Kit, item: &ProductNotification) -> El {
    let p = &s.palette;
    let (icon, title) = describe(item.kind);
    let tint = match item.severity {
        ProductNotificationSeverity::Error => p.danger,
        ProductNotificationSeverity::Warning => p.warning,
        ProductNotificationSeverity::Success => p.success,
        ProductNotificationSeverity::Info => p.text_muted,
    };
    let unread = item.read_at.is_none();
    let thread = thread_of(item)
        .map(|id| crate::workspace::title_of(s, id).unwrap_or_else(|| "Conversation".to_string()));
    let (open_id, archive_id) = (item.id.clone(), item.id.clone());
    let hover_group = format!("notification-{}", item.id);

    let mut text_col =
        col()
            .grow(1.0)
            .min_w(0.)
            .child(label(title, SM, if unread { 600. } else { 400. }, p.text));
    if let Some(thread) = thread {
        text_col = text_col.child(label(thread, XS, 400., p.text_muted).truncate());
    }
    let mut r = row()
        .key(hover_group.clone())
        .role("button")
        .label(title)
        .hover_group()
        .w_full()
        .gap(12.)
        .px(12.)
        .py(10.)
        .radius(crate::design::ROW_RADIUS)
        .center_y()
        .cursor(Cursor::Pointer)
        .hover(p.hover)
        .on_press(move |s: &mut State| open(s, &open_id))
        .child(glyph(icon, 16., tint))
        .child(text_col)
        .child(label(jobs::ago(item.created_at), XS, 400., p.text_muted));
    if unread {
        r = r.child(el().size(8., 8.).radius(4.).fill(p.accent));
    }
    // Archive shows while the pointer is on the row (GPUI `group_hover`).
    r.child(
        row().key("archive").group_opacity(0., 1.).child(
            icon_button(&icons::INBOX, "Archive")
                .on_press(move |s: &mut State| {
                    let id = archive_id.clone();
                    mutate(s, move |c| async move { c.archive_notification(&id).await });
                })
                .build(k),
        ),
    )
}

pub fn thread_of(item: &ProductNotification) -> Option<&str> {
    match &item.action {
        ProductNotificationAction::OpenThread { thread_id } => Some(thread_id),
        ProductNotificationAction::None => item.thread_id.as_deref(),
    }
}

pub fn describe(kind: ProductNotificationKind) -> (&'static rune::Icon, &'static str) {
    match kind {
        ProductNotificationKind::ApprovalRequired => (&icons::TRIANGLE_ALERT, "Approval needed"),
        ProductNotificationKind::AuthenticationRequired => (&icons::CIRCLE_USER, "Sign-in needed"),
        ProductNotificationKind::RunBlocked => (&icons::BAN, "Run blocked"),
        ProductNotificationKind::RunFailed => (&icons::CIRCLE_X, "Run failed"),
        ProductNotificationKind::RunCompleted => (&icons::CIRCLE_CHECK, "Run finished"),
        ProductNotificationKind::DeliveryFailed => (&icons::CIRCLE_ALERT, "Delivery failed"),
    }
}

pub fn view(s: &State, _cx: &mut Cx<State>, k: &Kit, _layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let this = &s.pages.notifications;
    let has_unread = this.items.iter().any(|n| n.read_at.is_none());
    let mut head = Vec::new();
    if has_unread {
        head.push(
            button("Mark all read", Btn::Ghost)
                .icon(&icons::CHECK)
                .on_press(|s: &mut State| {
                    mutate(s, |c| async move { c.mark_all_notifications_read().await })
                })
                .build(k),
        );
    }
    head.push(refresh_button(k, refresh));

    let rows: Vec<El> = this
        .items
        .iter()
        .map(|item| render_row(s, k, item))
        .collect();
    let mut body = col()
        .w_full()
        .child(header(p, "Notifications", actions(head)));
    if let Some(e) = &this.error {
        body = body.child(error_line(p, e));
    }
    if this.loaded && rows.is_empty() {
        body = body.child(empty_state(
            p,
            "Nothing here yet. IronClaw lists approvals it needs, failed runs and finished automations here.",
        ));
    }
    body = body.child(col().w_full().gap(2.).children(rows));
    if !this.channels.is_empty() {
        body = body.child(render_channels(s, k));
    }
    page_body("notifications-page", body)
}

pub fn fixture(s: &mut State) {
    let now = chrono::Utc::now();
    let n = |id: &str, kind: &str, severity: &str, thread: Option<&str>, mins: i64, read: bool| {
        serde_json::json!({
            "id": id, "kind": kind, "severity": severity,
            "action": match thread {
                Some(t) => serde_json::json!({"kind": "open_thread", "thread_id": t}),
                None => serde_json::json!({"kind": "none"}),
            },
            "thread_id": thread, "turn_run_id": null,
            "created_at": now - chrono::Duration::minutes(mins),
            "updated_at": now - chrono::Duration::minutes(mins),
            "read_at": if read { Some(now) } else { None },
            "resolved_at": null,
        })
    };
    let items = serde_json::from_value::<Vec<ProductNotification>>(serde_json::json!([
        n("n1", "approval_required", "warning", Some("t1"), 3, false),
        n("n2", "run_failed", "error", Some("t2"), 45, false),
        n("n3", "run_completed", "success", Some("t1"), 180, true),
        n("n4", "delivery_failed", "error", None, 60 * 26, true),
    ]));
    let channels = serde_json::from_value::<Vec<RebornOutboundDeliveryTargetOption>>(
        serde_json::json!([
            {"target": {"target_id": "slack:dm", "channel": "slack", "display_name": "Slack DM",
                        "description": "A direct message from the IronClaw app."},
             "capabilities": {"final_replies": true, "gate_prompts": true, "auth_prompts": false, "notifications": true}},
            {"target": {"target_id": "web_push", "channel": "web_push", "display_name": "Browser push"},
             "capabilities": {"final_replies": false, "gate_prompts": false, "auth_prompts": false, "notifications": true}}
        ]),
    );
    let this = &mut s.pages.notifications;
    match (items, channels) {
        (Ok(items), Ok(options)) => {
            this.items = items;
            this.channels = options
                .into_iter()
                .enumerate()
                .map(|(ix, option)| Channel {
                    selected: ix == 0,
                    setup: (ix == 1).then(|| RebornNotificationSetupStatusResponse {
                        extension_id: "web_push".into(),
                        requires_setup: true,
                        enabled: false,
                        detail: serde_json::Value::Null,
                    }),
                    option,
                })
                .collect();
        }
        (Err(e), _) | (_, Err(e)) => tracing::warn!(error = %e, "notifications fixture"),
    }
    this.loaded = true;
}
