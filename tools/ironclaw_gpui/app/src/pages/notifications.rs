//! The notification inbox: approvals, failures and finished runs the gateway
//! recorded for this user, plus which channels deliver them outside the app.

use std::collections::HashMap;

use ironclaw_gpui_client::Backend;
use ironclaw_gpui_client::notification_inbox::{
    ProductNotification, ProductNotificationAction, ProductNotificationKind,
    ProductNotificationSeverity,
};
use ironclaw_gpui_client::product_wire::{
    RebornNotificationSetupStatusResponse, RebornOutboundDeliveryTargetId,
    RebornOutboundDeliveryTargetOption,
};

use super::{PageEvent, empty_state, error_line, header, page_body, refresh_button};
use crate::jobs;
use crate::ui::*;

const PAGE_LIMIT: u32 = 100;

/// One delivery channel with the user's choice and enrollment state.
struct Channel {
    option: RebornOutboundDeliveryTargetOption,
    selected: bool,
    setup: Option<RebornNotificationSetupStatusResponse>,
}

pub struct NotificationsView {
    backend: Backend,
    items: Vec<ProductNotification>,
    titles: HashMap<String, String>,
    channels: Vec<Channel>,
    loaded: bool,
    error: Option<SharedString>,
}

impl EventEmitter<PageEvent> for NotificationsView {}

impl NotificationsView {
    pub fn new(backend: Backend, titles: HashMap<String, String>, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            backend,
            items: Vec::new(),
            titles,
            channels: Vec::new(),
            loaded: false,
            error: None,
        };
        this.refresh(cx);
        this
    }

    /// Thread titles from the sidebar, so rows can name their conversation.
    pub fn set_titles(&mut self, titles: HashMap<String, String>, cx: &mut Context<Self>) {
        self.titles = titles;
        cx.notify();
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.load_channels(cx);
        jobs::run(
            &self.backend,
            cx,
            |c| async move { c.list_notifications(PAGE_LIMIT).await },
            |this, result, cx| {
                match result {
                    Ok(page) => {
                        this.items = page.notifications;
                        this.error = None;
                        cx.emit(PageEvent::Unread(page.unread_count));
                    }
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                this.loaded = true;
                cx.notify();
            },
        );
    }

    fn load_channels(&mut self, cx: &mut Context<Self>) {
        jobs::run(
            &self.backend,
            cx,
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
            |this, result, cx| {
                match result {
                    Ok(channels) => this.channels = channels,
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    /// Send the full selection with `id` flipped; re-read to show what stuck.
    fn toggle_channel(&mut self, id: RebornOutboundDeliveryTargetId, cx: &mut Context<Self>) {
        let ids: Vec<_> = self
            .channels
            .iter()
            .filter(|ch| (ch.option.target.target_id == id) != ch.selected)
            .map(|ch| ch.option.target.target_id.clone())
            .collect();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.set_notification_channels(ids).await },
            |this, result, cx| match result {
                Ok(_) => this.load_channels(cx),
                Err(e) => {
                    this.error = Some(e.to_string().into());
                    cx.notify();
                }
            },
        );
    }

    fn render_channels(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        v_flex()
            .gap_2()
            .pt_6()
            .child(div().font_semibold().child("Delivery channels"))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .whitespace_normal()
                    .child(
                        "This app shows notifications itself while it runs. \
                         Pick where else IronClaw should send them.",
                    ),
            )
            .children(self.channels.iter().enumerate().map(|(ix, ch)| {
                let target = &ch.option.target;
                let id = target.target_id.clone();
                // Web push enrolls a browser, which only the web UI can do.
                let enrollment = ch.setup.as_ref().filter(|s| s.requires_setup).map(|s| {
                    if s.enabled {
                        "Enrolled".to_string()
                    } else {
                        "No device enrolled yet. Enable it from the web UI in a browser."
                            .to_string()
                    }
                });
                card(cx).child(
                    h_flex()
                        .gap_3()
                        .items_center()
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_medium()
                                        .child(target.display_name.as_str().to_string()),
                                )
                                .when_some(target.description.as_ref(), |d, desc| {
                                    d.child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .whitespace_normal()
                                            .child(desc.as_str().to_string()),
                                    )
                                })
                                .when_some(enrollment, |d, e| {
                                    d.child(
                                        div().text_xs().text_color(theme.muted_foreground).child(e),
                                    )
                                }),
                        )
                        .child(
                            Switch::new(("channel", ix))
                                .checked(ch.selected)
                                .accessibility_label(format!(
                                    "Send notifications to {}",
                                    target.display_name.as_str()
                                ))
                                .on_click(cx.listener(move |this, _: &bool, _, cx| {
                                    this.toggle_channel(id.clone(), cx)
                                })),
                        ),
                )
            }))
            .into_any_element()
    }

    fn open(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(item) = self.items.get(ix) else {
            return;
        };
        let unread_id = item.read_at.is_none().then(|| item.id.clone());
        let thread = thread_of(item).map(str::to_string);
        if let Some(id) = unread_id {
            self.mutate(
                cx,
                move |c| async move { c.mark_notification_read(&id).await },
            );
        }
        if let Some(thread) = thread {
            cx.emit(PageEvent::OpenThread(thread));
        }
    }

    fn mutate<F, Fut, T>(&mut self, cx: &mut Context<Self>, f: F)
    where
        F: FnOnce(ironclaw_gpui_client::Client) -> Fut,
        Fut: std::future::Future<Output = Result<T, ironclaw_gpui_client::ClientError>>
            + Send
            + 'static,
        T: Send + 'static,
    {
        jobs::run(&self.backend, cx, f, |this, result, cx| match result {
            Ok(_) => this.refresh(cx),
            Err(e) => {
                this.error = Some(e.to_string().into());
                cx.notify();
            }
        });
    }

    fn render_row(&self, ix: usize, item: &ProductNotification, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (icon, title) = describe(item.kind);
        let tint = match item.severity {
            ProductNotificationSeverity::Error => theme.danger,
            ProductNotificationSeverity::Warning => theme.warning,
            ProductNotificationSeverity::Success => theme.success,
            ProductNotificationSeverity::Info => theme.muted_foreground,
        };
        let unread = item.read_at.is_none();
        let thread = thread_of(item).map(|id| {
            self.titles
                .get(id)
                .cloned()
                .unwrap_or_else(|| "Conversation".to_string())
        });
        let archive_id = item.id.clone();

        h_flex()
            .id(("notification", ix))
            .role(Role::Button)
            .aria_label(title)
            .group("notification-row")
            .w_full()
            .gap_3()
            .px_3()
            .py_2p5()
            .rounded(theme.radius)
            .items_center()
            .cursor_pointer()
            .hover(|s| s.bg(theme.accent))
            .on_click(cx.listener(move |this, _, _, cx| this.open(ix, cx)))
            .child(div().text_color(tint).child(Icon::new(icon).small()))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_sm()
                            .when(unread, |d| d.font_semibold())
                            .child(title),
                    )
                    .when_some(thread, |this, thread| {
                        this.child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .truncate()
                                .child(thread),
                        )
                    }),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(jobs::ago(item.created_at)),
            )
            .when(unread, |this| {
                this.child(div().size_2().rounded_full().bg(theme.primary))
            })
            .child(
                div()
                    .invisible()
                    .group_hover("notification-row", |s| s.visible())
                    .child(
                        Button::new(("archive-notification", ix))
                            .icon(IconName::Inbox)
                            .ghost()
                            .xsmall()
                            .tooltip("Archive")
                            .accessibility_label("Archive")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                let id = archive_id.clone();
                                this.mutate(cx, move |c| async move {
                                    c.archive_notification(&id).await
                                });
                            })),
                    ),
            )
            .into_any_element()
    }
}

pub fn thread_of(item: &ProductNotification) -> Option<&str> {
    match &item.action {
        ProductNotificationAction::OpenThread { thread_id } => Some(thread_id),
        ProductNotificationAction::None => item.thread_id.as_deref(),
    }
}

pub fn describe(kind: ProductNotificationKind) -> (IconName, &'static str) {
    match kind {
        ProductNotificationKind::ApprovalRequired => (IconName::TriangleAlert, "Approval needed"),
        ProductNotificationKind::AuthenticationRequired => (IconName::CircleUser, "Sign-in needed"),
        ProductNotificationKind::RunBlocked => (IconName::Ban, "Run blocked"),
        ProductNotificationKind::RunFailed => (IconName::CircleX, "Run failed"),
        ProductNotificationKind::RunCompleted => (IconName::CircleCheck, "Run finished"),
        ProductNotificationKind::DeliveryFailed => (IconName::CircleAlert, "Delivery failed"),
    }
}

impl Render for NotificationsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_unread = self.items.iter().any(|n| n.read_at.is_none());
        let actions = h_flex()
            .gap_1()
            .when(has_unread, |this| {
                this.child(
                    Button::new("mark-all-read")
                        .icon(IconName::Check)
                        .label("Mark all read")
                        .ghost()
                        .small()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.mutate(
                                cx,
                                |c| async move { c.mark_all_notifications_read().await },
                            )
                        })),
                )
            })
            .child(
                refresh_button("refresh-notifications")
                    .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
            );

        let rows: Vec<AnyElement> = self
            .items
            .iter()
            .enumerate()
            .map(|(ix, item)| self.render_row(ix, item, cx))
            .collect();

        let body = v_flex()
            .child(header("Notifications", actions))
            .when_some(self.error.as_ref(), |this, e| this.child(error_line(e, cx)))
            .when(self.loaded && rows.is_empty(), |this| {
                this.child(empty_state(
                    "Nothing here yet. IronClaw lists approvals it needs, failed runs and finished automations here.",
                    cx,
                ))
            })
            .child(v_flex().gap_0p5().children(rows))
            .when(!self.channels.is_empty(), |d| d.child(self.render_channels(cx)));
        page_body("notifications-page", body)
    }
}
