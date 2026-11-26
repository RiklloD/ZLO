//! The notification inbox: approvals, failures and finished runs the gateway
//! recorded for this user, plus which channels deliver them outside the app.

use rune::{Cursor, Cx, El, col, el, row};
use rune_kit::Kit;
use std::collections::HashMap;

use rune::Tx;
use zlo_client::notification_inbox::{
    ProductListNotificationsResponse, ProductNotification, ProductNotificationAction,
    ProductNotificationKind, ProductNotificationSeverity,
};
use zlo_client::product_wire::{
    RebornNotificationSetupStatusResponse, RebornOutboundDeliveryTargetId,
    RebornOutboundDeliveryTargetOption,
};
use zlo_client::{Client, ClientError};

use super::common::{
    BASE, Btn, SM, XS, actions, button, card, empty_state, error_line, glyph, header, icon_button,
    label, page_body, para, refresh_button, switch,
};
use crate::design::motion;
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
    /// What the gateway last listed. Never edited ahead of its answer: the
    /// person's archives and reads in flight are [`Self::pending`], which
    /// the view lays over these records.
    pub items: Vec<ProductNotification>,
    pub channels: Vec<Channel>,
    pub loaded: bool,
    pub error: Option<String>,
    /// The person's changes the view already shows (so they ride the
    /// click) while the gateway has not confirmed them, by what they change.
    pending: HashMap<Change, Pending>,
    /// Optimistic peer changes in flight or confirmed awaiting the peer's next
    /// notifications read.
    peer_pending: HashMap<PeerChange, Pending>,
    /// The listing whose answer may land; every newer listing or change
    /// replaces it, so an older answer never overwrites newer state.
    list_gen: jobs::Generation,
}

/// One change the person made that the view shows ahead of the gateway.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum Change {
    Archive(String),
    Read(String),
    ReadAll,
}

/// One optimistic change on a background peer's notifications.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum PeerChange {
    Archive { computer: String, id: String },
    Read { computer: String, id: String },
    ReadAll { computer: String },
}

impl PeerChange {
    fn computer(&self) -> &str {
        match self {
            PeerChange::Archive { computer, .. }
            | PeerChange::Read { computer, .. }
            | PeerChange::ReadAll { computer } => computer,
        }
    }
}

/// A change in flight, or confirmed and waiting for a listing that shows it.
#[derive(Clone, Copy, Debug)]
struct Pending {
    asked: jobs::Generation,
    confirmed: bool,
}

impl NotificationsState {
    /// The records as the person sees them: archived ones hidden.
    fn shown(&self) -> impl Iterator<Item = &ProductNotification> {
        self.items
            .iter()
            .filter(|n| !self.pending.contains_key(&Change::Archive(n.id.clone())))
    }

    /// Whether `n` shows as unread, counting reads in flight.
    fn unread(&self, n: &ProductNotification) -> bool {
        n.read_at.is_none()
            && !self.pending.contains_key(&Change::ReadAll)
            && !self.pending.contains_key(&Change::Read(n.id.clone()))
    }

    fn peer_archived(&self, computer: &str, id: &str) -> bool {
        self.peer_pending.contains_key(&PeerChange::Archive {
            computer: computer.to_string(),
            id: id.to_string(),
        })
    }

    fn peer_unread(&self, computer: &str, item: &ProductNotification) -> bool {
        item.read_at.is_none()
            && !self.peer_pending.contains_key(&PeerChange::ReadAll {
                computer: computer.to_string(),
            })
            && !self.peer_pending.contains_key(&PeerChange::Read {
                computer: computer.to_string(),
                id: item.id.clone(),
            })
    }

    fn peer_has_unread(&self, peer: &crate::remote::peers::Peer) -> bool {
        if peer.conn != crate::remote::peers::PeerConn::Ready {
            return false;
        }
        let key = peer.computer.key();
        if self.peer_pending.contains_key(&PeerChange::ReadAll {
            computer: key.clone(),
        }) {
            return false;
        }
        peer.unread_count > 0
            || peer
                .notifications
                .iter()
                .any(|n| !self.peer_archived(&key, &n.id) && self.peer_unread(&key, n))
    }
}

/// A peer's notifications listing landed: drop any confirmed optimistic peer
/// changes for `computer` now that the server's updated list carries them.
pub(crate) fn peer_list_landed(s: &mut State, computer: &str) {
    s.pages
        .notifications
        .peer_pending
        .retain(|change, p| !(change.computer() == computer && p.confirmed));
}

/// The page became visible: load on first visit, refresh after.
pub fn opened(s: &mut State) {
    refresh(s);
}

pub fn refresh(s: &mut State) {
    load_channels(s, false);
    crate::remote::peers::refresh(s);
    let asked = s.pages.notifications.list_gen.bump();
    jobs::run(
        s,
        "notifications",
        |c| async move { c.list_notifications(PAGE_LIMIT).await },
        move |s, result, _| list_landed(s, asked, result.map_err(|e| e.to_string())),
    );
}

/// A listing answered. Only the newest one asked lands (a change made
/// since, or a later listing, supersedes it); it takes over every change
/// the gateway already confirmed, which it now shows itself.
fn list_landed(
    s: &mut State,
    asked: jobs::Generation,
    result: Result<ProductListNotificationsResponse, String>,
) {
    if s.pages.notifications.list_gen != asked {
        return;
    }
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
            this.pending.retain(|_, p| !p.confirmed);
            this.error = None;
            crate::workspace::set_unread(s, page.unread_count);
        }
        Err(e) => s.pages.notifications.error = Some(e),
    }
    s.pages.notifications.loaded = true;
}

/// Read the channels; `shown` after the person flipped one, so its switch
/// slides to what stuck.
fn load_channels(s: &mut State, shown: bool) {
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
        move |s, result, tx| {
            if shown {
                tx.animated();
            }
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
            Ok(_) => load_channels(s, true),
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
    let this = &s.pages.notifications;
    let Some(item) = this.shown().find(|n| n.id == id) else {
        return;
    };
    let unread_id = this.unread(item).then(|| item.id.clone());
    let thread = thread_of(item).map(str::to_string);
    if let Some(id) = unread_id {
        // Shown read on the click, so its dot fades with it.
        let change = Change::Read(id.clone());
        mutate(s, change, move |c| async move {
            c.mark_notification_read(&id).await
        });
    }
    if let Some(thread) = thread {
        crate::workspace::open_thread(s, thread);
    }
}

/// Archive `id`: its row folds away on the click (the rest glide up) while
/// the gateway is told; a refusal brings it back.
fn archive(s: &mut State, id: String) {
    let change = Change::Archive(id.clone());
    mutate(s, change, move |c| async move {
        c.archive_notification(&id).await
    });
}

fn mark_all_read(s: &mut State) {
    let focused_has_unread = s.workspace.focused_unread > 0
        || s.pages
            .notifications
            .shown()
            .any(|n| s.pages.notifications.unread(n));
    if focused_has_unread {
        mutate(s, Change::ReadAll, |c| async move {
            c.mark_all_notifications_read().await
        });
    }
    let peer_keys: Vec<String> = crate::remote::peers::list(s)
        .into_iter()
        .filter(|p| {
            p.conn == crate::remote::peers::PeerConn::Ready
                && (p.unread_count > 0 || p.notifications.iter().any(|n| n.read_at.is_none()))
        })
        .map(|p| p.computer.key())
        .collect();
    for key in peer_keys {
        mutate_peer(
            s,
            PeerChange::ReadAll {
                computer: key.clone(),
            },
            |c| async move { c.mark_all_notifications_read().await },
        );
    }
}

/// Open a notification belonging to a background peer: focus that computer
/// (and its thread when the notification names one) and mark it read.
fn open_peer(s: &mut State, computer: String, id: String, thread: Option<String>, unread: bool) {
    if let Some(thread) = thread {
        crate::remote::peers::focus_thread(s, &computer, thread);
    } else {
        crate::remote::peers::focus(s, &computer);
    }
    if !unread {
        return;
    }
    if s.onboarding.computer.key() == computer && matches!(s.conn, crate::state::Conn::Ready) {
        jobs::run(
            s,
            "notifications",
            move |c| async move { c.mark_notification_read(&id).await },
            |s, _, _| crate::workspace::refresh_unread(s),
        );
    } else {
        let mark_id = id.clone();
        mutate_peer(s, PeerChange::Read { computer, id }, move |c| async move {
            c.mark_notification_read(&mark_id).await
        });
    }
}

/// Archive a notification belonging to a background peer.
fn archive_peer(s: &mut State, computer: String, id: String) {
    let archive_id = id.clone();
    mutate_peer(
        s,
        PeerChange::Archive { computer, id },
        move |c| async move { c.archive_notification(&archive_id).await },
    );
}

/// Apply an optimistic change to a peer's notifications and reconcile or roll
/// back when the peer gateway replies.
fn mutate_peer<F, Fut, T>(s: &mut State, change: PeerChange, f: F)
where
    F: FnOnce(Client) -> Fut,
    Fut: std::future::Future<Output = Result<T, ClientError>> + Send + 'static,
    T: Send + 'static,
{
    let computer = change.computer().to_string();
    let asked = jobs::Generation::unique();
    s.pages.notifications.peer_pending.insert(
        change.clone(),
        Pending {
            asked,
            confirmed: false,
        },
    );
    crate::remote::peers::run(s, &computer, "notifications", f, move |s, result, tx| {
        peer_change_landed(
            s,
            &change,
            asked,
            result.map(|_| ()).map_err(|e| e.to_string()),
            tx,
        )
    });
}

fn peer_change_landed(
    s: &mut State,
    change: &PeerChange,
    asked: jobs::Generation,
    result: Result<(), String>,
    tx: &mut Tx,
) {
    let this = &mut s.pages.notifications;
    let ours = this
        .peer_pending
        .get(change)
        .is_some_and(|p| p.asked == asked);
    match result {
        Ok(()) => {
            if ours && let Some(p) = this.peer_pending.get_mut(change) {
                p.confirmed = true;
            }
            crate::remote::peers::refresh_one(s, change.computer());
        }
        Err(e) => {
            tx.animated();
            if ours {
                this.peer_pending.remove(change);
            }
            this.error = Some(e);
        }
    }
}

/// Show `change` now and send it. Any listing still on its way was asked
/// before it, so its answer will not land.
fn mutate<F, Fut, T>(s: &mut State, change: Change, f: F)
where
    F: FnOnce(Client) -> Fut,
    Fut: std::future::Future<Output = Result<T, ClientError>> + Send + 'static,
    T: Send + 'static,
{
    let asked = begin(s, change.clone());
    jobs::run(s, "notifications", f, move |s, result, tx| {
        change_landed(
            s,
            &change,
            asked,
            result.map(|_| ()).map_err(|e| e.to_string()),
            tx,
        )
    });
}

/// Lay `change` over the records and supersede listings in flight.
fn begin(s: &mut State, change: Change) -> jobs::Generation {
    let this = &mut s.pages.notifications;
    let asked = jobs::Generation::unique();
    this.pending.insert(
        change,
        Pending {
            asked,
            confirmed: false,
        },
    );
    this.list_gen.bump();
    asked
}

/// The gateway answered `change` (asked as `asked`). Confirmed: it stays
/// shown until a listing asked from now on carries it. Refused: it is taken
/// back with motion (the row returns, the dot comes back) and the error
/// shows. A later change to the same record owns its entry.
fn change_landed(
    s: &mut State,
    change: &Change,
    asked: jobs::Generation,
    result: Result<(), String>,
    tx: &mut Tx,
) {
    let this = &mut s.pages.notifications;
    let ours = this.pending.get(change).is_some_and(|p| p.asked == asked);
    match result {
        Ok(()) => {
            if ours && let Some(p) = this.pending.get_mut(change) {
                p.confirmed = true;
            }
            refresh(s);
        }
        Err(e) => {
            // The person's change undone: as visible as it was made.
            tx.animated();
            if ours {
                this.pending.remove(change);
            }
            this.error = Some(e);
        }
    }
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
    let unread = s.pages.notifications.unread(item);
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
        .hover_motion(motion::HOVER, motion::HOVER)
        .press_scale(motion::PRESS_CARD)
        // Keyed by id: an archived row folds away and the rest glide up.
        .motion(motion::ROW_RESORT)
        .transition(motion::reveal())
        .on_press(move |s: &mut State| open(s, &open_id))
        .child(glyph(icon, 16., tint))
        .child(text_col)
        .child(label(jobs::ago(item.created_at), XS, 400., p.text_muted));
    if unread {
        r = r.child(
            el().key("unread")
                .size(8., 8.)
                .radius(4.)
                .fill(p.accent)
                .transition(motion::glyph_pop()),
        );
    }
    // Archive shows while the pointer is on the row.
    r.child(
        row().key("archive").group_opacity(0., 1.).child(
            icon_button(&icons::INBOX, "Archive")
                .on_press(move |s: &mut State| archive(s, archive_id.clone()))
                .build(k)
                .press_scale(motion::PRESS),
        ),
    )
}

fn render_peer_row(
    s: &State,
    k: &Kit,
    peer_key: &str,
    peer_label: &str,
    item: &ProductNotification,
    thread_title: Option<String>,
) -> El {
    let p = &s.palette;
    let (icon, title) = describe(item.kind);
    let tint = match item.severity {
        ProductNotificationSeverity::Error => p.danger,
        ProductNotificationSeverity::Warning => p.warning,
        ProductNotificationSeverity::Success => p.success,
        ProductNotificationSeverity::Info => p.text_muted,
    };
    let unread = s.pages.notifications.peer_unread(peer_key, item);
    let thread = thread_of(item).map(str::to_string);
    let subtitle = thread
        .as_ref()
        .map(|_| thread_title.unwrap_or_else(|| "Conversation".to_string()));
    let open_computer = peer_key.to_string();
    let open_id = item.id.clone();
    let open_thread = thread.clone();
    let archive_computer = peer_key.to_string();
    let archive_id = item.id.clone();
    let row_key = format!("notification-{peer_key}-{}", item.id);

    let mut text_col = col().grow(1.0).min_w(0.).child(
        row()
            .gap(6.)
            .center_y()
            .child(label(title, SM, if unread { 600. } else { 400. }, p.text))
            .child(label(peer_label.to_string(), XS, 400., p.text_faint)),
    );
    if let Some(sub) = subtitle {
        text_col = text_col.child(label(sub, XS, 400., p.text_muted).truncate());
    }
    let mut r = row()
        .key(row_key)
        .role("button")
        .label(format!("{peer_label}: {title}"))
        .hover_group()
        .w_full()
        .gap(12.)
        .px(12.)
        .py(10.)
        .radius(crate::design::ROW_RADIUS)
        .center_y()
        .cursor(Cursor::Pointer)
        .hover(p.hover)
        .hover_motion(motion::HOVER, motion::HOVER)
        .press_scale(motion::PRESS_CARD)
        .motion(motion::ROW_RESORT)
        .transition(motion::reveal())
        .on_press(move |s: &mut State| {
            open_peer(
                s,
                open_computer.clone(),
                open_id.clone(),
                open_thread.clone(),
                unread,
            )
        })
        .child(glyph(icon, 16., tint))
        .child(text_col)
        .child(label(jobs::ago(item.created_at), XS, 400., p.text_muted));
    if unread {
        r = r.child(
            el().key("unread")
                .size(8., 8.)
                .radius(4.)
                .fill(p.accent)
                .transition(motion::glyph_pop()),
        );
    }
    r.child(
        row().key("archive").group_opacity(0., 1.).child(
            icon_button(&icons::INBOX, "Archive")
                .on_press(move |s: &mut State| {
                    archive_peer(s, archive_computer.clone(), archive_id.clone())
                })
                .build(k)
                .press_scale(motion::PRESS),
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
    let peers = crate::remote::peers::list(s);
    let peer_has_unread = peers.iter().any(|peer| this.peer_has_unread(peer));
    let focused_has_unread = this.shown().any(|n| this.unread(n))
        || (s.workspace.focused_unread > 0 && !this.pending.contains_key(&Change::ReadAll));
    let has_unread = focused_has_unread || peer_has_unread;
    let mut head = Vec::new();
    if has_unread {
        head.push(
            button("Mark all read", Btn::Ghost)
                .icon(&icons::CHECK)
                .on_press(mark_all_read)
                .build(k)
                .key("mark-all-read")
                .press_scale(motion::PRESS),
        );
    }
    head.push(refresh_button(k, refresh));

    let mut rows: Vec<El> = this.shown().map(|item| render_row(s, k, item)).collect();
    for peer in peers
        .into_iter()
        .filter(|peer| peer.conn == crate::remote::peers::PeerConn::Ready)
    {
        let peer_key = peer.computer.key();
        let peer_label = peer.computer.label();
        for item in &peer.notifications {
            if this.peer_archived(&peer_key, &item.id) {
                continue;
            }
            let thread_title = thread_of(item).and_then(|tid| {
                peer.inbox.as_ref().and_then(|inbox| {
                    inbox
                        .rows
                        .iter()
                        .find(|r| r.thread_id == tid)
                        .and_then(|r| r.title.clone())
                })
            });
            rows.push(render_peer_row(
                s,
                k,
                &peer_key,
                &peer_label,
                item,
                thread_title,
            ));
        }
    }
    let mut body = col()
        .w_full()
        .child(header(p, "Notifications", actions(head)));
    if let Some(e) = &this.error {
        body = body.child(error_line(p, e).key("notifications-error"));
    }
    if this.loaded && rows.is_empty() {
        body = body.child(
            empty_state(
                p,
                "Nothing here yet. IronClaw lists approvals it needs, failed runs and finished automations here.",
            )
            .key("notifications-none"),
        );
    }
    body = body.child(col().key("notifications").w_full().gap(2.).children(rows));
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

#[cfg(test)]
#[path = "notifications_tests.rs"]
mod tests;
