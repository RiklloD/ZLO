//! Window root: connects to the gateway, lists threads, and switches between
//! the open chat and the full-width pages.

use std::time::Duration;

use ironclaw_gpui_client::inbox::InboxAction;
use ironclaw_gpui_client::usage::ThreadTelemetry;
use ironclaw_gpui_client::wire::SessionInfo;
use ironclaw_gpui_client::{Backend, Client, ClientError, Connection};

use crate::alerts::Alerts;
use crate::chat::{ChatView, OpenThread, RunEnded, ThreadUpdated};
use crate::design::battery::BatterySaver;
use crate::design::prefs::Prefs;
use crate::design::{self, Palette, motion, wallpaper};
use crate::jobs;
use crate::pages::PageEvent;
use crate::pages::automations::AutomationsView;
use crate::pages::extensions::ExtensionsView;
use crate::pages::files::FilesView;
use crate::pages::notifications::NotificationsView;
use crate::pages::operator::OperatorView;
use crate::pages::profiles::ProfilesView;
use crate::pages::projects::ProjectsView;
use crate::pages::settings::SettingsView;
use crate::pages::skills::SkillsView;
use crate::pages::usage::{UsageModel, UsageView};
use crate::ui::*;
use gpui_kit::StyledImage as _;

mod inbox;

pub(crate) use inbox::title_of as row_title;

/// The inbox has no live stream, so the badge polls. One tiny request.
const UNREAD_POLL: Duration = Duration::from_secs(15);
/// Retry schedule while the gateway is unreachable (seconds, last repeats).
/// It is a local listener, so a refused connect is free to repeat.
/// Enough to catch every new item between two polls.
const ALERT_PAGE: u32 = 20;
const CONNECT_RETRY: [u64; 5] = [1, 2, 5, 10, 30];
/// Status bar refresh while the window is in use. Telemetry also refreshes
/// right after each run; usage windows are cached for a minute upstream.
const STATUS_POLL: Duration = Duration::from_secs(10);

/// Whether the bottom status bar shows. Toggled app-wide with Cmd+Shift+S.
pub fn toggle_status_bar(cx: &mut App) {
    Prefs::update(cx, |p| p.status_bar_hidden = !p.status_bar_hidden);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Chat,
    Notifications,
    Automations,
    Files,
    Projects,
    Profiles,
    Extensions,
    Skills,
    Settings,
    Operator,
    Usage,
}

/// Pages are built on first visit and kept, so switching back is instant.
#[derive(Default)]
struct Pages {
    notifications: Option<Entity<NotificationsView>>,
    automations: Option<Entity<AutomationsView>>,
    files: Option<Entity<FilesView>>,
    projects: Option<Entity<ProjectsView>>,
    profiles: Option<Entity<ProfilesView>>,
    extensions: Option<Entity<ExtensionsView>>,
    skills: Option<Entity<SkillsView>>,
    settings: Option<Entity<SettingsView>>,
    operator: Option<Entity<OperatorView>>,
    usage: Option<Entity<UsageView>>,
}

enum Conn {
    Connecting,
    /// `retry_in` is set when the app retries on its own.
    Failed {
        message: SharedString,
        retry_in: Option<u64>,
    },
    Ready,
}

pub struct Workspace {
    conn: Conn,
    backend: Option<Backend>,
    session: Option<SessionInfo>,
    inbox: inbox::Inbox,
    active: Option<(String, Entity<ChatView>)>,
    page: Page,
    pages: Pages,
    unread: usize,
    _chat_events: Vec<Subscription>,
    _page_events: Vec<Subscription>,
    _poll: Option<Task<()>>,
    connect_attempt: usize,
    _retry: Option<Task<()>>,
    alerts: Alerts,
    battery: BatterySaver,
    _activation: Subscription,
    /// Status bar data: the shared usage report and the open thread's
    /// telemetry (`None` until known, or when the gateway lacks the route).
    usage: Option<Entity<UsageModel>>,
    telemetry: Option<ThreadTelemetry>,
    _usage_observe: Option<Subscription>,
    _status_poll: Option<Task<()>>,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            conn: Conn::Connecting,
            backend: None,
            session: None,
            inbox: inbox::Inbox::new(window, cx),
            active: None,
            page: Page::Chat,
            pages: Pages::default(),
            unread: 0,
            _chat_events: Vec::new(),
            _page_events: Vec::new(),
            _poll: None,
            connect_attempt: 0,
            _retry: None,
            alerts: Alerts::default(),
            battery: BatterySaver::new(),
            _activation: cx.observe_window_activation(window, |this, window, cx| {
                this.battery.set_active(window, cx);
                this.refresh_status(cx);
                // Back in front: catch up on whatever finished meanwhile.
                if window.is_window_active() {
                    this.refresh_inbox(cx);
                }
            }),
            usage: None,
            telemetry: None,
            _usage_observe: None,
            _status_poll: None,
        };
        this.battery.start(|w: &mut Self| &mut w.battery, cx);
        // Clicking a system notification brings the window up on its thread.
        let weak = cx.entity().downgrade();
        let handle = window.window_handle();
        cx.on_system_notification_response(move |response: SystemNotificationResponse, cx| {
            cx.activate(true);
            let _ = handle.update(cx, |_, window, cx| {
                window.activate_window();
                let _ = weak.update(cx, |this, cx| this.alert_clicked(&response.tag, window, cx));
            });
        });
        this.connect(window, cx);
        this
    }

    fn connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.conn = Conn::Connecting;
        let backend = match Connection::discover()
            .and_then(Client::new)
            .and_then(Backend::new)
        {
            Ok(backend) => backend,
            Err(e) => {
                self.conn = Conn::Failed {
                    message: e.to_string().into(),
                    retry_in: None,
                };
                return;
            }
        };
        self.backend = Some(backend.clone());

        let job = backend.spawn(|c| async move {
            let session = c.session().await?;
            let inbox = c.inbox().await?;
            Ok::<_, ClientError>((session, inbox))
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = job
                .await
                .map_err(|e| ClientError::Config(e.to_string()))
                .and_then(|r| r);
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok((session, inbox)) => {
                        this.connect_attempt = 0;
                        this._retry = None;
                        this.session = Some(session);
                        this.apply_inbox(inbox, cx);
                        this.conn = Conn::Ready;
                        this.start_polling(window, cx);
                        this.start_status(backend.clone(), window, cx);
                        if let Some(first) = this.inbox.first_open() {
                            this.open_thread(first, window, cx);
                        }
                    }
                    Err(e) => this.connect_failed(&e, window, cx),
                }
                cx.notify();
            });
        });
        task.detach();
    }

    /// Show the error, and retry on a schedule when the gateway is simply not
    /// up yet (it starts after the app at login, or is restarting).
    fn connect_failed(&mut self, error: &ClientError, window: &mut Window, cx: &mut Context<Self>) {
        let retry_in = matches!(error, ClientError::Network(_))
            .then(|| CONNECT_RETRY[self.connect_attempt.min(CONNECT_RETRY.len() - 1)]);
        self.conn = Conn::Failed {
            message: friendly(error).into(),
            retry_in,
        };
        let Some(secs) = retry_in else {
            return;
        };
        self.connect_attempt += 1;
        self._retry = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_secs(secs))
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if matches!(this.conn, Conn::Failed { .. }) {
                    this.connect(window, cx);
                    cx.notify();
                }
            });
        }));
    }

    fn open_thread(&mut self, thread_id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.page = Page::Chat;
        self.visit(&thread_id, cx);
        if self.active.as_ref().is_some_and(|(id, _)| *id == thread_id) {
            cx.notify();
            return;
        }
        let (Some(backend), Some(session)) = (self.backend.clone(), self.session.as_ref()) else {
            return;
        };
        let channel = session.session_channel_extension_id.clone();
        let limits = session.attachments.clone();
        let id = thread_id.clone();
        // Replacing the entity drops the old chat, which closes its stream
        // and frees one of the gateway's per-user stream slots.
        let view = cx.new(|cx| ChatView::new(backend, channel, limits, id, window, cx));
        self._chat_events = vec![
            cx.subscribe_in(&view, window, |this, _, _: &ThreadUpdated, window, cx| {
                this.refresh_inbox(cx);
                this.refresh_unread(window, cx);
            }),
            cx.subscribe_in(&view, window, |this, _, event: &RunEnded, window, cx| {
                this.refresh_telemetry(cx);
                // Watched live: the result is seen, so no Done badge.
                if window.is_window_active() && this.page == Page::Chat {
                    this.inbox_mark(event.thread_id.clone(), InboxAction::Visit, None, cx);
                } else {
                    this.refresh_inbox(cx);
                }
                if !window.is_window_active() {
                    let title = crate::titles::get(cx, &event.thread_id);
                    this.alerts
                        .post_run(&event.thread_id, event.failed, title.as_deref(), cx);
                }
            }),
            // `/new`: the gateway created the thread; switch after this event.
            cx.subscribe_in(&view, window, |_, _, event: &OpenThread, window, cx| {
                let id = event.0.clone();
                cx.defer_in(window, move |this, window, cx| {
                    this.refresh_inbox(cx);
                    this.open_thread(id, window, cx);
                });
            }),
        ];
        self.active = Some((thread_id, view));
        self.telemetry = None;
        self.refresh_telemetry(cx);
        cx.notify();
    }

    /// New conversation: the profile picker decides where it lives.
    fn new_thread(&mut self, cx: &mut Context<Self>) {
        self.toggle_picker(cx);
    }

    fn thread_error(&mut self, error: &ClientError, cx: &mut Context<Self>) {
        self.conn = Conn::Failed {
            message: friendly(error).into(),
            retry_in: None,
        };
        cx.notify();
    }

    fn delete_thread(&mut self, thread_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.clone() else {
            return;
        };
        let id = thread_id.clone();
        let job = backend.spawn(move |c| async move { c.delete_thread(&id).await });
        let task = cx.spawn_in(window, async move |this, cx| {
            let ok = matches!(job.await, Ok(Ok(_)));
            let _ = this.update_in(cx, |this, window, cx| {
                // Remove only after the gateway confirmed the delete.
                if !ok {
                    return;
                }
                this.inbox.list.rows.retain(|t| t.thread_id != thread_id);
                if this.active.as_ref().is_some_and(|(id, _)| *id == thread_id) {
                    this.active = None;
                    if let Some(next) = this.inbox.first_open() {
                        this.open_thread(next, window, cx);
                    }
                }
                cx.notify();
            });
        });
        task.detach();
    }

    fn projects_enabled(&self) -> bool {
        self.session
            .as_ref()
            .and_then(|s| s.features.get("reborn_projects"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    fn start_polling(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_unread(window, cx);
        self._poll = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(UNREAD_POLL).await;
                if this
                    .update_in(cx, |this, window, cx| this.refresh_unread(window, cx))
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    /// Badge count plus system notifications for new inbox items.
    fn refresh_unread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.clone() else {
            return;
        };
        jobs::run_in(
            &backend,
            window,
            cx,
            |c| async move { c.list_notifications(ALERT_PAGE).await },
            |this, result, window, cx| {
                // silent-ok: the badge is a hint; the inbox page shows real errors.
                if let Ok(page) = result {
                    let looking_at = (window.is_window_active() && this.page == Page::Chat)
                        .then(|| this.active.as_ref().map(|(id, _)| id.as_str()))
                        .flatten();
                    for item in this.alerts.take_new(&page.notifications) {
                        let thread = crate::pages::notifications::thread_of(item);
                        if thread.is_some() && thread == looking_at {
                            continue;
                        }
                        // Automation runs are not in the thread list; fetch
                        // their title first so the alert can name them.
                        let Some(thread) = thread.map(str::to_string) else {
                            this.alerts.post(item, None, cx);
                            continue;
                        };
                        let Some(backend) = this.backend.clone() else {
                            continue;
                        };
                        let title = crate::titles::resolve(thread, &backend, cx);
                        let item = item.clone();
                        cx.spawn(async move |this, cx| {
                            let title = title.await;
                            let _ = this
                                .update(cx, |this, cx| this.alerts.post(&item, Some(&title), cx));
                        })
                        .detach();
                    }
                    let changed = page.unread_count != this.unread;
                    this.unread = page.unread_count;
                    if changed && let Some(view) = this.pages.notifications.clone() {
                        view.update(cx, |v, cx| v.refresh(cx));
                    }
                    cx.notify();
                }
            },
        );
    }

    fn alert_clicked(&mut self, tag: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(thread) = self.alerts.thread_for(tag) {
            self.open_thread(thread, window, cx);
        }
        // `run:` tags are local; every other tag is an inbox notification id.
        if tag.starts_with("run:") {
            return;
        }
        if let Some(backend) = self.backend.clone() {
            let id = tag.to_string();
            jobs::run_in(
                &backend,
                window,
                cx,
                move |c| async move { c.mark_notification_read(&id).await },
                // silent-ok: read state is cosmetic; the next poll recounts.
                |this, _, window, cx| this.refresh_unread(window, cx),
            );
        }
    }

    fn on_page_event(&mut self, event: &PageEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            PageEvent::OpenThread(id) => self.open_thread(id.clone(), window, cx),
            PageEvent::Unread(n) => {
                self.unread = *n;
                cx.notify();
            }
        }
    }

    /// Show a page, building it on first visit and reloading it after.
    fn show(&mut self, page: Page, window: &mut Window, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.clone() else {
            return;
        };
        macro_rules! open {
            ($slot:ident, $build:expr) => {
                match self.pages.$slot.clone() {
                    Some(view) => view.update(cx, |v, cx| v.refresh(cx)),
                    None => {
                        let view = cx.new($build);
                        self._page_events.push(cx.subscribe_in(
                            &view,
                            window,
                            |this, _, event, window, cx| this.on_page_event(event, window, cx),
                        ));
                        self.pages.$slot = Some(view);
                    }
                }
            };
        }
        match page {
            Page::Chat => {}
            Page::Notifications => {
                open!(notifications, |cx| NotificationsView::new(backend, cx))
            }
            Page::Automations => open!(automations, |cx| AutomationsView::new(backend, cx)),
            Page::Files => open!(files, |cx| FilesView::new(backend, cx)),
            Page::Projects => open!(projects, |cx| ProjectsView::new(backend, window, cx)),
            Page::Profiles => open!(profiles, |cx| ProfilesView::new(backend, window, cx)),
            Page::Extensions => open!(extensions, |cx| ExtensionsView::new(backend, cx)),
            Page::Skills => open!(skills, |cx| SkillsView::new(backend, window, cx)),
            Page::Settings => open!(settings, |cx| SettingsView::new(backend, cx)),
            Page::Operator => open!(operator, |cx| OperatorView::new(backend, cx)),
            Page::Usage => {
                let Some(model) = self.usage.clone() else {
                    return;
                };
                open!(usage, |cx| UsageView::new(model, cx))
            }
        }
        self.page = page;
        cx.notify();
    }

    fn render_nav(&self, cx: &Context<Self>) -> impl IntoElement {
        let mut items = vec![
            (Page::Notifications, IconName::Bell, "Notifications"),
            (Page::Automations, IconName::Calendar, "Automations"),
            (Page::Files, IconName::Folder, "Files"),
        ];
        if self.projects_enabled() {
            items.push((Page::Projects, IconName::LayoutDashboard, "Projects"));
        }
        items.push((Page::Profiles, IconName::User, "Profiles"));
        items.push((Page::Extensions, IconName::Frame, "Extensions"));
        items.push((Page::Skills, IconName::BookOpen, "Skills"));
        items.push((Page::Operator, IconName::SquareTerminal, "Operator"));
        let unread = self.unread;
        v_flex()
            .px_2()
            .pb_2()
            .gap_px()
            .children(
                items
                    .into_iter()
                    .enumerate()
                    .map(|(ix, (page, icon, label))| {
                        sidebar_row(("nav", ix), self.page == page, cx)
                            .aria_label(label)
                            .on_click(
                                cx.listener(move |this, _, window, cx| this.show(page, window, cx)),
                            )
                            .child(Icon::new(icon).small())
                            .child(div().flex_1().child(label))
                            .when(page == Page::Notifications && unread > 0, |d| {
                                d.child(
                                    Tag::primary()
                                        .small()
                                        .rounded_full()
                                        .child(unread.to_string()),
                                )
                            })
                    }),
            )
    }

    fn open_in_browser(&self, cx: &mut App) {
        if let Some(backend) = &self.backend {
            cx.open_url(&backend.client().connection().base_url);
        }
    }

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *Palette::of(cx);
        v_flex()
            .w(px(design::SIDEBAR_WIDTH))
            .h_full()
            .flex_none()
            .text_color(p.text_muted)
            // Titlebar strip: traffic lights sit on the left, macOS drags
            // the window from the empty space.
            .child(
                h_flex()
                    .h(px(design::TITLEBAR_HEIGHT))
                    .flex_none()
                    .justify_end()
                    .items_center()
                    .pr_2()
                    .child(
                        Button::new("new-thread")
                            .icon(IconName::Plus)
                            .ghost()
                            .small()
                            .selected(self.inbox.picker_open)
                            .tooltip("New thread")
                            .accessibility_label("New thread")
                            .on_click(cx.listener(|this, _, _, cx| this.new_thread(cx))),
                    ),
            )
            .child(self.render_nav(cx))
            .child(self.render_inbox(cx))
            .child(
                h_flex()
                    .p_2()
                    .gap_1()
                    .child(
                        Button::new("open-settings")
                            .icon(IconName::Settings)
                            .label("Settings")
                            .ghost()
                            .small()
                            .flex_1()
                            .selected(self.page == Page::Settings)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show(Page::Settings, window, cx)
                            })),
                    )
                    .child(
                        Button::new("open-browser")
                            .icon(IconName::ExternalLink)
                            .ghost()
                            .small()
                            .tooltip("Open in browser")
                            .accessibility_label("Open in browser")
                            .on_click(cx.listener(|this, _, _, cx| this.open_in_browser(cx))),
                    )
                    .child(
                        // gpui-kit's default icon set has no bar chart; the
                        // full catalog would embed 1,830 SVGs for one glyph.
                        Button::new("open-usage")
                            .icon(IconName::ChartPie)
                            .ghost()
                            .small()
                            .selected(self.page == Page::Usage)
                            .tooltip("Usage")
                            .accessibility_label("Usage")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.show(Page::Usage, window, cx)
                            })),
                    ),
            )
    }

    /// Status bar polling: one timer, skipped while the battery saver holds
    /// (background or idle window) or the bar is hidden.
    fn start_status(&mut self, backend: Backend, window: &mut Window, cx: &mut Context<Self>) {
        let model = cx.new(|_| UsageModel::new(backend));
        self._usage_observe = Some(cx.observe(&model, |_, _, cx| cx.notify()));
        self.usage = Some(model);
        self.refresh_status(cx);
        self._status_poll = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(STATUS_POLL).await;
                if this.update(cx, |this, cx| this.refresh_status(cx)).is_err() {
                    break;
                }
            }
        }));
    }

    fn refresh_status(&mut self, cx: &mut Context<Self>) {
        if self.battery.saving() {
            return;
        }
        let usage_visible = self.page == Page::Usage;
        if Prefs::of(cx).status_bar_hidden && !usage_visible {
            return;
        }
        if let Some(model) = self.usage.clone() {
            model.update(cx, |m, cx| m.refresh(false, cx));
        }
        self.refresh_telemetry(cx);
    }

    fn refresh_telemetry(&mut self, cx: &mut Context<Self>) {
        let (Some(backend), Some((thread_id, _))) = (self.backend.clone(), self.active.as_ref())
        else {
            return;
        };
        let id = thread_id.clone();
        let asked = id.clone();
        jobs::run(
            &backend,
            cx,
            move |c| async move { c.thread_telemetry(&id).await },
            move |this, result, cx| {
                // The user switched threads while this was in flight.
                if this.active.as_ref().map(|(id, _)| id.as_str()) != Some(asked.as_str()) {
                    return;
                }
                // silent-ok: the bar shows "—" for anything unknown.
                let telemetry = result.ok().flatten();
                if this.telemetry != telemetry {
                    this.telemetry = telemetry;
                    cx.notify();
                }
            },
        );
    }

    /// Right side of the bar: context, speed, cache, then the busiest
    /// usage window (click opens the Usage page).
    fn render_metrics(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *Palette::of(cx);
        let t = self.telemetry.as_ref();
        let dash = || "—".to_string();
        let context = match t.and_then(|t| t.context_used.zip(t.context_max)) {
            Some((used, max)) => {
                let pct = t
                    .and_then(|t| t.context_pct)
                    .unwrap_or(used as f64 * 100. / max.max(1) as f64);
                format!("{} / {} · {pct:.0}%", tokens(used), tokens(max))
            }
            None => dash(),
        };
        let tps = t
            .and_then(|t| t.tps)
            .map(|v| format!("{v:.0} tok/s"))
            .unwrap_or_else(dash);
        let cache = t
            .and_then(|t| t.cache_hit_rate)
            .map(|r| format!("{:.0}%", r * 100.))
            .unwrap_or_else(dash);
        let peak = self.usage.as_ref().and_then(|m| {
            m.read(cx)
                .peak()
                .map(|(provider, w)| (provider.label.clone(), w.label.clone(), w.used_percent))
        });
        h_flex()
            .gap_3()
            .child(metric("Context", context))
            .child(metric("Speed", tps))
            .child(metric("Cache", cache))
            .when_some(peak, |d, (provider, window, pct)| {
                let color = crate::pages::usage::level_color(pct, cx);
                d.child(
                    h_flex()
                        .id("status-usage")
                        .gap_1()
                        .items_center()
                        .cursor_pointer()
                        .hover(|s| s.text_color(p.text))
                        .on_click(
                            cx.listener(|this, _, window, cx| this.show(Page::Usage, window, cx)),
                        )
                        .child(div().size(px(6.)).rounded_full().bg(color))
                        .child(format!("{provider} {window} {pct:.0}%")),
                )
            })
    }

    /// The bottom strip: connection on the left, metrics on the right.
    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *Palette::of(cx);
        let (dot, label): (_, SharedString) = match &self.conn {
            Conn::Connecting => (p.warning, "Connecting".into()),
            Conn::Failed { .. } => (p.danger, "Disconnected".into()),
            Conn::Ready => {
                let host = self
                    .backend
                    .as_ref()
                    .map(|b| b.client().connection().base_url.clone())
                    .unwrap_or_default();
                let host = host
                    .trim_start_matches("http://")
                    .trim_start_matches("https://")
                    .trim_end_matches('/')
                    .to_string();
                (p.success, format!("Connected · {host}").into())
            }
        };
        h_flex()
            .h(px(design::STATUS_BAR_HEIGHT))
            .flex_none()
            .px_3()
            .gap_2()
            .items_center()
            .text_xs()
            .text_color(p.text_faint)
            .child(div().size(px(6.)).rounded_full().bg(dot))
            .child(label)
            .child(div().flex_1())
            .when(matches!(self.conn, Conn::Ready), |d| {
                d.child(self.render_metrics(cx))
            })
    }
}

/// One sidebar row: Zeron's quiet list style, a wash on hover and a
/// stronger one when selected.
fn sidebar_row(
    id: impl Into<ElementId>,
    active: bool,
    cx: &App,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let p = Palette::of(cx);
    h_flex()
        .id(id)
        .role(Role::Button)
        .w_full()
        .h(px(30.))
        .gap_2()
        .px_2()
        .rounded(px(design::CONTROL_RADIUS))
        .text_sm()
        .cursor_pointer()
        .when(active, |d| d.bg(p.active).text_color(p.text))
        .when(!active, |d| d.hover(|s| s.bg(p.hover).text_color(p.text)))
}

fn metric(name: &'static str, value: String) -> impl IntoElement {
    h_flex().gap_1().child(name).child(value)
}

/// 51200 -> "51.2k", 1000000 -> "1M".
fn tokens(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{}M", trim(n as f64 / 1e6)),
        n if n >= 1_000 => format!("{}k", trim(n as f64 / 1e3)),
        n => n.to_string(),
    }
}

fn trim(v: f64) -> String {
    let s = format!("{v:.1}");
    s.strip_suffix(".0").map(str::to_string).unwrap_or(s)
}

/// Turn the two common first-run failures into instructions.
fn friendly(error: &ClientError) -> String {
    match error {
        ClientError::Network(_) => format!(
            "{error}\n\nIronClaw starts its service when the app opens. If this persists, \
             run `ironclaw service install` once, then try again."
        ),
        e if e.is_unauthorized() => format!(
            "{error}\n\nThe WebUI token was rejected. Re-run `ironclaw onboard` to refresh it."
        ),
        e => e.to_string(),
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *Palette::of(cx);
        let backdrop = wallpaper::current(cx);
        let body: AnyElement = match &self.conn {
            Conn::Connecting => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .text_color(p.text_muted)
                .child(Spinner::new())
                .child("Connecting to IronClaw")
                .into_any_element(),
            Conn::Failed { message, retry_in } => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_3()
                .p_8()
                .child(div().font_semibold().child("Can't connect to IronClaw"))
                .child(
                    div()
                        .max_w(px(520.))
                        .text_sm()
                        .text_color(p.text_muted)
                        .whitespace_normal()
                        .child(message.clone()),
                )
                .when_some(*retry_in, |this, secs| {
                    this.child(
                        div()
                            .text_xs()
                            .text_color(p.text_faint)
                            .child(format!("Trying again in {secs} s")),
                    )
                })
                .child(
                    Button::new("retry-connect")
                        .label("Try again")
                        .primary()
                        .on_click(cx.listener(|this, _, window, cx| {
                            // Also brings the gateway back after a Stop from
                            // the Operator page.
                            crate::service::start();
                            this.connect_attempt = 0;
                            this.connect(window, cx)
                        })),
                )
                .into_any_element(),
            Conn::Ready => {
                let page: Option<AnyElement> = match self.page {
                    Page::Chat => None,
                    Page::Notifications => self
                        .pages
                        .notifications
                        .clone()
                        .map(|v| v.into_any_element()),
                    Page::Automations => {
                        self.pages.automations.clone().map(|v| v.into_any_element())
                    }
                    Page::Files => self.pages.files.clone().map(|v| v.into_any_element()),
                    Page::Projects => self.pages.projects.clone().map(|v| v.into_any_element()),
                    Page::Profiles => self.pages.profiles.clone().map(|v| v.into_any_element()),
                    Page::Extensions => self.pages.extensions.clone().map(|v| v.into_any_element()),
                    Page::Skills => self.pages.skills.clone().map(|v| v.into_any_element()),
                    Page::Settings => self.pages.settings.clone().map(|v| v.into_any_element()),
                    Page::Operator => self.pages.operator.clone().map(|v| v.into_any_element()),
                    Page::Usage => self.pages.usage.clone().map(|v| v.into_any_element()),
                };
                // Replays the entrance on every page or thread switch.
                let key: SharedString = match (self.page, &self.active) {
                    (Page::Chat, Some((id, _))) => format!("main-chat-{id}").into(),
                    (page, _) => format!("main-{}", page as u8).into(),
                };
                let main: AnyElement = match (page, &self.active) {
                    (Some(page), _) => page,
                    (None, Some((_, chat))) => chat.clone().into_any_element(),
                    (None, None) => self.render_empty(cx).into_any_element(),
                };
                h_flex()
                    .size_full()
                    .min_h_0()
                    .child(self.render_sidebar(cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .py(px(design::PANEL_INSET))
                            .pr(px(design::PANEL_INSET))
                            .child(
                                div()
                                    .size_full()
                                    .rounded(px(design::PANEL_RADIUS))
                                    .border_1()
                                    .border_color(p.border)
                                    .bg(if backdrop.is_some() {
                                        p.bg.opacity(0.68)
                                    } else {
                                        p.bg
                                    })
                                    .overflow_hidden()
                                    .child(motion::fade_in(
                                        div().size_full().child(main),
                                        ElementId::Name(key),
                                        motion::FADE_QUICK,
                                        0.,
                                    )),
                            ),
                    )
                    .into_any_element()
            }
        };
        let show_status = !Prefs::of(cx).status_bar_hidden;
        v_flex()
            .size_full()
            .relative()
            .bg(p.surface)
            // The wallpaper sits under everything; shell and panel turn to
            // tinted glass over it.
            .when_some(backdrop.clone(), |d, image| {
                d.child(
                    gpui_kit::img(image)
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .object_fit(gpui_kit::ObjectFit::Cover),
                )
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .bg(p.surface.opacity(0.35)),
                )
            })
            .text_color(p.text)
            .font_family(design::FONT_SANS)
            .on_mouse_move(
                cx.listener(|this, _, _, cx| this.battery.input(|w: &mut Self| &mut w.battery, cx)),
            )
            .capture_key_down(
                cx.listener(|this, _, _, cx| this.battery.input(|w: &mut Self| &mut w.battery, cx)),
            )
            .child(div().flex_1().min_h_0().child(body))
            .when(show_status, |d| d.child(self.render_status_bar(cx)))
    }
}

impl Workspace {
    /// No thread open: a soft accent glow at the top of the panel. A static
    /// gradient, so it costs nothing per frame.
    fn render_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *Palette::of(cx);
        div()
            .size_full()
            .relative()
            .child(div().absolute().top_0().left_0().right_0().h(px(320.)).bg(
                gpui_kit::linear_gradient(
                    180.,
                    gpui_kit::linear_color_stop(p.accent_wash, 0.),
                    gpui_kit::linear_color_stop(p.accent_wash.opacity(0.), 1.),
                ),
            ))
            .child(motion::fade_in(
                v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .child(
                        div()
                            .text_xl()
                            .font_semibold()
                            .child("What are we building?"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(p.text_muted)
                            .child("Start a conversation, or pick one from the sidebar."),
                    )
                    .child(
                        Button::new("start-thread")
                            .label("New thread")
                            .primary()
                            .on_click(cx.listener(|this, _, _, cx| this.new_thread(cx))),
                    ),
                "empty-state",
                motion::FADE_IN,
                4.,
            ))
    }
}
