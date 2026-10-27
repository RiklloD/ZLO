//! Window root (t3code's layout): connects to the gateway; the sidebar lists
//! threads, or the settings pages while one is open; the panel shows the
//! open chat, the new-thread screen or a settings page under a breadcrumb.

use std::time::Duration;

use ironclaw_gpui_client::inbox::InboxAction;
use ironclaw_gpui_client::usage::ThreadTelemetry;
use ironclaw_gpui_client::wire::SessionInfo;
use ironclaw_gpui_client::{Backend, Client, ClientError, Connection};

use crate::alerts::Alerts;
use crate::chat::{ChatView, OpenRunLogs, OpenThread, RunEnded, ThreadUpdated};
use crate::design::battery::BatterySaver;
use crate::design::prefs::Prefs;
use crate::design::{self, Palette, Style, kit, motion, panels, wallpaper};
use crate::jobs;
use crate::pages::PageEvent;
use crate::pages::automations::AutomationsView;
use crate::pages::extensions::ExtensionsView;
use crate::pages::files::FilesView;
use crate::pages::notifications::NotificationsView;
use crate::pages::operator::OperatorView;
use crate::pages::profiles::ProfilesView;
use crate::pages::projects::ProjectsView;
use crate::pages::providers::ProvidersView;
use crate::pages::settings::{Section, SettingsView};
use crate::pages::skills::SkillsView;
use crate::pages::usage::{UsageModel, UsageView};
use crate::ui::*;

mod command;
mod draft;
mod inbox;
mod quick;

pub(crate) use inbox::title_of as row_title;

/// The inbox has no live stream, so the badge polls. One tiny request.
const UNREAD_POLL: Duration = Duration::from_secs(15);
/// Retry schedule while the gateway is unreachable (seconds, last repeats).
/// It is a local listener, so a refused connect is free to repeat.
/// Enough to catch every new item between two polls.
const ALERT_PAGE: u32 = 20;
const CONNECT_RETRY: [u64; 5] = [1, 2, 5, 10, 30];
/// The app starts the gateway itself, so for this long after launch a
/// refused connect means "still booting": keep the loading screen and poll
/// fast instead of showing an error.
const STARTUP_GRACE: Duration = Duration::from_secs(25);
const STARTUP_POLL: Duration = Duration::from_millis(150);
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
    /// The new-thread screen.
    Draft,
    Notifications,
    Automations,
    Files,
    Projects,
    Profiles,
    Extensions,
    Skills,
    Settings,
    Appearance,
    Permissions,
    Traces,
    Providers,
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
    providers: Option<Entity<ProvidersView>>,
    operator: Option<Entity<OperatorView>>,
    usage: Option<Entity<UsageView>>,
}

/// A settings sidebar row: page, icon, label.
type NavItem = (Page, IconName, &'static str);

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
    draft: Option<draft::Draft>,
    unread: usize,
    _chat_events: Vec<Subscription>,
    _page_events: Vec<Subscription>,
    _poll: Option<Task<()>>,
    connect_attempt: usize,
    _retry: Option<Task<()>>,
    alerts: Alerts,
    battery: BatterySaver,
    _activation: Subscription,
    /// The window's fallback focus. When the focused element leaves the tree
    /// (the model picker's search box after a pick), focus returns here so
    /// window shortcuts like ⇧⌘M keep reaching the workspace.
    focus: gpui_kit::FocusHandle,
    _focus_lost: Subscription,
    /// Status bar data: the shared usage report and the open thread's
    /// telemetry (`None` until known, or when the gateway lacks the route).
    usage: Option<Entity<UsageModel>>,
    telemetry: Option<ThreadTelemetry>,
    _usage_observe: Option<Subscription>,
    _status_poll: Option<Task<()>>,
    /// The Cmd+K menu while open.
    command: Option<command::CommandMenu>,
    command_opens: u64,
    quick: quick::Quick,
    main_window: gpui_kit::AnyWindowHandle,
    /// Live sidebar width while its edge is dragged; saved on release.
    sidebar_drag: Option<f32>,
    /// App launch, for the startup grace window.
    launched: std::time::Instant,
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
            draft: None,
            unread: 0,
            _chat_events: Vec::new(),
            command: None,
            command_opens: 0,
            quick: quick::Quick::default(),
            main_window: window.window_handle(),
            sidebar_drag: None,
            launched: std::time::Instant::now(),
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
            focus: cx.focus_handle(),
            _focus_lost: cx.on_focus_lost(window, |this, window, cx| {
                window.focus(&this.focus, cx);
            }),
            usage: None,
            telemetry: None,
            _usage_observe: None,
            _status_poll: None,
        };
        this.battery.start(|w: &mut Self| &mut w.battery, cx);
        this.start_quick_composer(cx);
        crate::snapshot::start(cx.entity().downgrade(), window.window_handle(), cx);
        design::apply_window(window, *Style::of(cx));
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
                tracing::warn!(error = %e, "gateway connection settings unavailable");
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
                        match this.inbox.first_open() {
                            Some(first) => this.open_thread(first, window, cx),
                            // Nothing open: a fresh draft in the default profile.
                            None => this.start_in_profile(
                                ironclaw_gpui_client::profiles::DEFAULT_PROFILE_ID.into(),
                                window,
                                cx,
                            ),
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
        let network = matches!(error, ClientError::Network(_));
        if network && self.launched.elapsed() < STARTUP_GRACE {
            // Still booting: stay on the loading screen and knock again soon.
            self.conn = Conn::Connecting;
            self._retry = Some(cx.spawn_in(window, async move |this, cx| {
                cx.background_executor().timer(STARTUP_POLL).await;
                let _ = this.update_in(cx, |this, window, cx| {
                    if matches!(this.conn, Conn::Connecting) {
                        this.connect(window, cx);
                        cx.notify();
                    }
                });
            }));
            return;
        }
        tracing::warn!(error = %error, attempt = self.connect_attempt, "gateway connection failed");
        let retry_in =
            network.then(|| CONNECT_RETRY[self.connect_attempt.min(CONNECT_RETRY.len() - 1)]);
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
            cx.subscribe_in(&view, window, |this, _, event: &OpenRunLogs, window, cx| {
                this.show(Page::Operator, window, cx);
                if let Some(view) = this.pages.operator.clone() {
                    let run = event.0.clone();
                    view.update(cx, |v, cx| v.show_run_logs(run, window, cx));
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
                    match this.inbox.first_open() {
                        Some(next) => this.open_thread(next, window, cx),
                        None => this.open_draft(window, cx),
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
            Page::Chat | Page::Draft => {}
            Page::Notifications => {
                open!(notifications, |cx| NotificationsView::new(backend, cx))
            }
            Page::Automations => open!(automations, |cx| AutomationsView::new(backend, cx)),
            Page::Files => open!(files, |cx| FilesView::new(backend, cx)),
            Page::Projects => open!(projects, |cx| ProjectsView::new(backend, window, cx)),
            Page::Profiles => open!(profiles, |cx| ProfilesView::new(backend, window, cx)),
            Page::Extensions => open!(extensions, |cx| ExtensionsView::new(backend, cx)),
            Page::Skills => open!(skills, |cx| SkillsView::new(backend, window, cx)),
            Page::Settings | Page::Appearance | Page::Permissions | Page::Traces => {
                open!(settings, |cx| SettingsView::new(backend, window, cx));
                let section = match page {
                    Page::Appearance => Section::Appearance,
                    Page::Permissions => Section::Permissions,
                    Page::Traces => Section::Traces,
                    _ => Section::General,
                };
                if let Some(view) = self.pages.settings.clone() {
                    view.update(cx, |v, cx| v.set_section(section, cx));
                }
            }
            Page::Providers => {
                // The orchestrator catalog (CLI agents) is read through any thread.
                let catalog = self.inbox.list.rows.first().map(|r| r.thread_id.clone());
                if let Some(view) = self.pages.providers.clone() {
                    view.update(cx, |v, _| v.set_catalog_thread(catalog.clone()));
                }
                open!(providers, |cx| ProvidersView::new(
                    backend, catalog, window, cx
                ))
            }
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

    /// Settings pages in sidebar order, under named groups (T3's settings
    /// regroup): the app, what the agent can use, what it works on, and
    /// what it reports.
    fn settings_groups(&self) -> Vec<(&'static str, Vec<NavItem>)> {
        let mut work = vec![
            (Page::Automations, IconName::Calendar, "Automations"),
            (Page::Files, IconName::Folder, "Files"),
        ];
        if self.projects_enabled() {
            work.push((Page::Projects, IconName::LayoutDashboard, "Projects"));
        }
        vec![
            (
                "App",
                vec![
                    (Page::Settings, IconName::Settings, "General"),
                    (Page::Appearance, IconName::Palette, "Appearance"),
                    (Page::Notifications, IconName::Bell, "Notifications"),
                ],
            ),
            (
                "Agent",
                vec![
                    (Page::Providers, IconName::Bot, "Providers"),
                    (Page::Profiles, IconName::User, "Profiles"),
                    (Page::Skills, IconName::BookOpen, "Skills"),
                    (Page::Extensions, IconName::Frame, "Extensions"),
                    (Page::Permissions, IconName::CircleCheck, "Permissions"),
                ],
            ),
            ("Work", work),
            (
                "Reports",
                vec![
                    (Page::Usage, IconName::ChartPie, "Usage"),
                    (Page::Traces, IconName::Network, "Trace Commons"),
                    (Page::Operator, IconName::SquareTerminal, "Operator"),
                ],
            ),
        ]
    }

    fn settings_pages(&self) -> Vec<(Page, IconName, &'static str)> {
        self.settings_groups()
            .into_iter()
            .flat_map(|(_, pages)| pages)
            .collect()
    }

    fn in_settings(&self) -> bool {
        !matches!(self.page, Page::Chat | Page::Draft)
    }

    /// Leave settings for the open thread, or the new-thread screen.
    fn back_to_threads(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.active.as_ref().map(|(id, _)| id.clone()) {
            Some(id) => self.open_thread(id, window, cx),
            None => self.open_draft(window, cx),
        }
    }

    fn render_settings_nav(&self, cx: &Context<Self>) -> impl IntoElement {
        const GROUP_GAP: f32 = 10.;
        const HEADING: f32 = 26.;
        let unread = self.unread;
        let s = *Style::of(cx);
        let p = *Palette::of(cx);
        // Rows, headings and gaps have fixed heights, so the selection pill
        // can glide to any row, across groups, instead of jumping.
        let groups = self.settings_groups();
        let mut selected = None;
        let mut slot = 0;
        for (gx, (_, group)) in groups.iter().enumerate() {
            for (page, _, _) in group {
                if *page == self.page {
                    selected = Some(
                        slot as f32 * s.row_height
                            + gx as f32 * GROUP_GAP
                            + (gx + 1) as f32 * HEADING,
                    );
                }
                slot += 1;
            }
        }
        let mut nav = v_flex()
            .relative()
            .child(kit::slide_pill("nav-pill", selected, cx));
        let mut slot = 0;
        for (gx, (heading, group)) in groups.into_iter().enumerate() {
            let mut rows = v_flex().child(
                div()
                    .h(px(HEADING))
                    .flex_none()
                    .flex()
                    .items_end()
                    .pb(px(6.))
                    .px(px(10.))
                    .text_xs()
                    .font_medium()
                    .text_color(p.text_faint)
                    .child(heading),
            );
            for (page, icon, label) in group {
                let on = self.page == page;
                rows = rows.child(
                    kit::pill_row(SharedString::from(format!("nav-{slot}")), on, cx)
                        .role(Role::Button)
                        .aria_label(label)
                        .on_click(
                            cx.listener(move |this, _, window, cx| this.show(page, window, cx)),
                        )
                        .child(Icon::new(icon).small().when(on, |i| i.text_color(p.accent)))
                        .child(div().flex_1().child(label))
                        .when(page == Page::Notifications && unread > 0, |d| {
                            d.child(unread_badge(unread))
                        }),
                );
                slot += 1;
            }
            nav = nav.child(rows.when(gx > 0, |d| d.mt(px(GROUP_GAP))));
        }
        v_flex()
            .id("settings-nav")
            .flex_1()
            .min_h_0()
            .overflow_y_scrollbar()
            .px_2()
            .pb_2()
            .child(nav)
    }

    fn open_in_browser(&self, cx: &mut App) {
        if let Some(backend) = &self.backend {
            cx.open_url(&backend.client().connection().base_url);
        }
    }

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *Palette::of(cx);
        let settings = self.in_settings();
        let unread = self.unread;
        // Titlebar strip: the traffic lights and the window's sidebar button
        // float over its left end, the name follows them; macOS drags the
        // window from the empty space.
        let titlebar = h_flex()
            .h(px(design::TITLEBAR_HEIGHT))
            .flex_none()
            .items_center()
            .pl(px(panels::CLUSTER_END + 10.))
            .pr_2()
            .child(
                div()
                    .text_sm()
                    .font_semibold()
                    .text_color(p.text)
                    .child(if settings { "Settings" } else { "Numoh" }),
            );
        let footer = if settings {
            h_flex().p_2().child(
                kit::button("settings-back")
                    .icon(IconName::ArrowLeft)
                    .label("Back")
                    .ghost()
                    .small()
                    .full()
                    .on_click(cx.listener(|this, _, window, cx| this.back_to_threads(window, cx))),
            )
        } else {
            // Zeron's footer: the account chip on the left ("Local": this
            // Mac's gateway), icon buttons on the right at Hermes's 32 px.
            let foot = |id: &'static str, icon: IconName, label: &'static str| {
                foot_button(id, icon, label, cx).tooltip(move |w, cx| {
                    gpui_kit::component::tooltip::Tooltip::new(label).build(w, cx)
                })
            };
            h_flex()
                .px(px(8.))
                .py(px(8.))
                .gap(px(4.))
                .items_center()
                .child(
                    h_flex()
                        .id("account-local")
                        .gap(px(8.))
                        .px(px(6.))
                        .h(px(design::SIDEBAR_CONTROL))
                        .items_center()
                        .rounded(px(8.))
                        .text_size(px(design::SIDEBAR_TITLE_SIZE))
                        .text_color(p.text.opacity(0.8))
                        .child(
                            div()
                                .size(px(18.))
                                .rounded_full()
                                .bg(p.accent)
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(px(10.))
                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                .text_color(design::on_accent(p))
                                .child("L"),
                        )
                        .child("Local"),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .relative()
                        .child(
                            foot("open-notifications", IconName::Bell, "Notifications").on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.show(Page::Notifications, window, cx)
                                }),
                            ),
                        )
                        .when(unread > 0, |d| {
                            d.child(
                                div()
                                    .absolute()
                                    .top(px(5.))
                                    .right(px(5.))
                                    .size(px(7.))
                                    .rounded_full()
                                    .bg(p.accent),
                            )
                        }),
                )
                .child(foot("open-usage", IconName::ChartPie, "Usage").on_click(
                    cx.listener(|this, _, window, cx| this.show(Page::Usage, window, cx)),
                ))
                .child(
                    foot("open-browser", IconName::ExternalLink, "Open in browser")
                        .on_click(cx.listener(|this, _, _, cx| this.open_in_browser(cx))),
                )
                .child(
                    foot("open-settings", IconName::Settings, "Settings").on_click(
                        cx.listener(|this, _, window, cx| this.show(Page::Settings, window, cx)),
                    ),
                )
        };
        v_flex()
            .w(px(self.sidebar_width(cx)))
            .h_full()
            .flex_none()
            .text_color(p.text_muted)
            .child(titlebar)
            .child(if settings {
                self.render_settings_nav(cx).into_any_element()
            } else {
                self.render_inbox(cx).into_any_element()
            })
            .child(footer)
    }

    /// The panel's top strip: where you are, t3code's "project / thread",
    /// and the open thread's actions at its right end. `sidebar` is the
    /// drawn sidebar width: the crumbs keep clear of the window's titlebar
    /// buttons while the sidebar is folded or sliding.
    fn render_breadcrumb(
        &self,
        sidebar: f32,
        actions: Option<AnyElement>,
        cx: &Context<Self>,
    ) -> gpui_kit::Div {
        let p = Palette::of(cx);
        let (parent, leaf): (String, String) = match self.page {
            Page::Chat => {
                let id = self
                    .active
                    .as_ref()
                    .map(|(id, _)| id.as_str())
                    .unwrap_or("");
                let row = self.inbox.row(id);
                let profile = row
                    .and_then(|r| {
                        self.inbox
                            .list
                            .profiles
                            .iter()
                            .find(|p| p.profile_id == r.profile_id)
                            .map(|p| p.display_name.clone())
                    })
                    .unwrap_or_else(|| "Default".into());
                let title = row
                    .map(|r| row_title(r).to_string())
                    .or_else(|| crate::titles::get(cx, id))
                    .unwrap_or_else(|| "New conversation".into());
                (profile, title)
            }
            Page::Draft => (self.draft_profile_name(), "New thread".into()),
            page => {
                let label = self
                    .settings_pages()
                    .into_iter()
                    .find(|(p, _, _)| *p == page)
                    .map_or("Settings", |(_, _, l)| l);
                ("Settings".into(), label.into())
            }
        };
        // Zeron's titlebar identity: the thread title in the 13 px UI face,
        // its home quieter beside it; no rule, the content fades under it.
        let left = (panels::CLUSTER_END + 10. - sidebar).max(16.);
        let right = if actions.is_some() { 0. } else { 16. };
        h_flex()
            .h(px(design::TITLEBAR_HEIGHT))
            .flex_none()
            .pl(px(left))
            .pr(px(right))
            .items_center()
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(6.))
                    .items_center()
                    .text_size(gpui_kit::rems(13. / 16.))
                    .child(div().flex_none().text_color(p.text_muted).child(parent))
                    .child(div().flex_none().text_color(p.text_faint).child("/"))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                            .text_color(p.text)
                            .child(leaf),
                    ),
            )
            .children(actions)
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
            m.read(cx).peak().map(|(provider, w)| {
                let resets = w
                    .resets_at
                    .and_then(crate::pages::usage::short_countdown)
                    .map(|r| format!(" · {r}"))
                    .unwrap_or_default();
                (
                    provider.label.clone(),
                    format!("{}{resets}", w.label),
                    w.used_percent,
                )
            })
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
                        .child(format!("{provider} {pct:.0}% {window}")),
                )
            })
    }

    /// The bottom strip: connection on the left, metrics on the right.
    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *Palette::of(cx);
        let live = matches!(self.conn, Conn::Connecting);
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
            .child(kit::status_dot("status-dot", dot, live))
            .child(label)
            .child(div().flex_1())
            .when(matches!(self.conn, Conn::Ready), |d| {
                d.child(self.render_metrics(cx))
            })
    }
}

/// One sidebar row: a fading hover wash and a selection tick that grows in.
fn sidebar_row(
    id: impl Into<ElementId>,
    active: bool,
    cx: &App,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let id: ElementId = id.into();
    kit::row(SharedString::from(format!("{id:?}")), active, cx).role(Role::Button)
}

/// A sidebar footer icon button: a round hover wash, 16 px glyph.
fn foot_button(
    id: &'static str,
    icon: IconName,
    label: &'static str,
    cx: &App,
) -> gpui_kit::Stateful<gpui_kit::Div> {
    let p = *Palette::of(cx);
    let key = SharedString::from(format!("foot-{id}"));
    let t = crate::design::motion::hover_t(&key);
    // The composer's round-button wash, so both hovers read the same.
    let wash = if p.light {
        gpui_kit::hsla(0., 0., 0., 0.08)
    } else {
        gpui_kit::hsla(0., 0., 1., 0.10)
    };
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(label)
        .size(px(design::SIDEBAR_CONTROL))
        .flex_none()
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .on_hover(crate::design::motion::hover_listener(
            key,
            Style::of(cx).hover,
        ))
        .bg(wash.opacity(wash.a * t))
        .child(Icon::new(icon).size(px(16.)).text_color(p.text_muted))
}

/// Launch screen while the gateway boots: the claw mark breathing on the
/// shell, its caption fading in late so a fast start shows no text at all.
fn render_loading(cx: &App) -> impl IntoElement {
    use gpui_kit::{Animation, AnimationExt as _};
    let p = *Palette::of(cx);
    let breathe = Animation::new(Duration::from_millis(1400))
        .repeat()
        .with_easing(|t| 0.5 - 0.5 * (t * std::f32::consts::TAU).cos());
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap_4()
        .child(div().child(kit::claw_mark(cx)).with_animation(
            "loading-breathe",
            breathe,
            |el, t| el.opacity(0.55 + 0.45 * t),
        ))
        .child(motion::fade_in_after(
            div()
                .text_sm()
                .text_color(p.text_muted)
                .child("Starting IronClaw"),
            "loading-caption",
            motion::MotionSpec::new(300, motion::EASE_OUT_QUINT),
            4.,
            700,
        ))
}

fn unread_badge(n: usize) -> impl IntoElement {
    Tag::primary().small().rounded_full().child(n.to_string())
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Keep frames coming only while a hover fade or glide is mid-flight.
        motion::pump(window);
        let p = *Palette::of(cx);
        let s = *Style::of(cx);
        let body: AnyElement = match &self.conn {
            Conn::Connecting => render_loading(cx).into_any_element(),
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
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new("retry-connect")
                                .label("Try again")
                                .primary()
                                .on_click(cx.listener(|this, _, window, cx| {
                                    // Also brings the gateway back after a Stop
                                    // from the Operator page.
                                    crate::service::start();
                                    this.connect_attempt = 0;
                                    this.connect(window, cx)
                                })),
                        )
                        .child(
                            // The gateway is down, so its in-app log ring is
                            // out of reach: open the files instead.
                            Button::new("connect-logs")
                                .label("Logs")
                                .icon(IconName::File)
                                .tooltip("Open the app and gateway logs in Console")
                                .on_click(|_, _, _| crate::logs::open_files()),
                        ),
                )
                .into_any_element(),
            Conn::Ready => {
                let page: Option<AnyElement> = match self.page {
                    Page::Chat => None,
                    Page::Draft => Some(self.render_draft(window, cx)),
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
                    Page::Settings | Page::Appearance | Page::Permissions | Page::Traces => {
                        self.pages.settings.clone().map(|v| v.into_any_element())
                    }
                    Page::Providers => self.pages.providers.clone().map(|v| v.into_any_element()),
                    Page::Operator => self.pages.operator.clone().map(|v| v.into_any_element()),
                    Page::Usage => self.pages.usage.clone().map(|v| v.into_any_element()),
                };
                // Replays the entrance on every page or thread switch.
                let key: SharedString = match (self.page, &self.active) {
                    (Page::Chat, Some((id, _))) => format!("main-chat-{id}").into(),
                    (page, _) => format!("main-{}", page as u8).into(),
                };
                let sidebar_full = self.sidebar_width(cx);
                let sidebar_t = panels::reveal(
                    "sidebar-reveal",
                    !Prefs::of(cx).sidebar_hidden || self.sidebar_drag.is_some(),
                    cx,
                );
                let sidebar_drawn = sidebar_full * sidebar_t;
                let actions = match (self.page, &self.active) {
                    (Page::Chat, Some((_, chat))) => {
                        Some(chat.update(cx, |chat, cx| chat.render_titlebar_actions(window, cx)))
                    }
                    _ => None,
                };
                let breadcrumb = self
                    .render_breadcrumb(sidebar_drawn, actions, cx)
                    .into_any_element();
                let main: AnyElement = match (page, &self.active) {
                    // Appearance floats over the chat at 85 % so a pick
                    // previews live on the real thread or new-chat screen.
                    (Some(page), active) if self.page == Page::Appearance => {
                        let under = match active {
                            Some((_, chat)) => chat.clone().into_any_element(),
                            None => self.render_empty(cx).into_any_element(),
                        };
                        div()
                            .size_full()
                            .relative()
                            .child(div().absolute().inset_0().child(under))
                            .child(
                                div()
                                    .absolute()
                                    .inset_0()
                                    .occlude()
                                    .bg(p.bg.opacity(0.85))
                                    .child(page),
                            )
                            .into_any_element()
                    }
                    (Some(page), _) => page,
                    (None, Some((_, chat))) => chat.clone().into_any_element(),
                    (None, None) => self.render_empty(cx).into_any_element(),
                };
                let over_main = [
                    self.render_wallpaper("conversation", window, cx),
                    self.render_wallpaper("pane", window, cx),
                ];
                let over_sidebar = self.render_wallpaper("sidebar", window, cx);
                // Zeron's shell: full-height columns to the window's edges.
                // The sidebar keeps its full width inside a clip that grows
                // or shrinks around it, so its rows never rewrap mid-slide.
                let sidebar = (sidebar_t > 0.).then(|| {
                    div()
                        .relative()
                        .flex_none()
                        .h_full()
                        .w(px(sidebar_drawn))
                        .overflow_hidden()
                        .child(
                            div()
                                .absolute()
                                .top_0()
                                .right_0()
                                .h_full()
                                .opacity(motion::lerp(0.35, 1., sidebar_t))
                                .child(self.render_sidebar(cx)),
                        )
                        .children(over_sidebar)
                });
                let shell = h_flex()
                    .size_full()
                    .min_h_0()
                    .children(sidebar)
                    .when(sidebar_t >= 1., |d| d.child(self.render_sidebar_edge(cx)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .relative()
                            .bg(p.bg.opacity(s.panel_alpha))
                            // The sidebar/panel hairline; accent while resizing.
                            .when(sidebar_t > 0., |d| {
                                d.border_l(px(1.5))
                                    .border_color(if self.sidebar_drag.is_some() {
                                        p.accent.opacity(0.6)
                                    } else {
                                        p.border_strong
                                    })
                            })
                            .overflow_hidden()
                            .flex()
                            .flex_col()
                            .child(breadcrumb)
                            .child(motion::fade_in(
                                div().flex_1().min_h_0().w_full().child(main),
                                ElementId::Name(key),
                                s.enter,
                                s.enter_rise,
                            ))
                            .children(over_main.into_iter().flatten()),
                    );
                // The loading screen hands over with a quick fade; the
                // window's sidebar button sits above both columns, so it
                // never moves when the sidebar folds.
                div()
                    .size_full()
                    .relative()
                    .child(motion::fade_in(
                        shell,
                        "workspace-ready",
                        motion::MotionSpec::new(160, motion::EASE_OUT_QUINT),
                        0.,
                    ))
                    .child(
                        h_flex()
                            .absolute()
                            .top_0()
                            .left(px(design::TRAFFIC_LIGHTS_WIDTH))
                            .h(px(design::TITLEBAR_HEIGHT))
                            .items_center()
                            .child(self.sidebar_toggle(cx)),
                    )
                    .into_any_element()
            }
        };
        let show_status = !Prefs::of(cx).status_bar_hidden;
        let command = self.render_command_menu(cx);
        let over_window = self.render_wallpaper("window", window, cx);
        // A native preview not painted this frame hides after it; the
        // command menu would draw under it, so it hides for that too.
        crate::native_view::frame_begin(window, command.is_some());
        // Mouse-wheel notches glide anywhere in the window (Zeron's wheel).
        design::smooth_scroll::smooth_scroll(
            v_flex()
                .size_full()
                .relative()
                .track_focus(&self.focus)
                .capture_any_mouse_down(|_, window, _| crate::native_view::reclaim_focus(window))
                .bg(p.surface.opacity(s.shell_alpha))
                .text_color(p.text)
                .font_family(design::FONT_SANS)
                .on_mouse_move(cx.listener(|this, e: &gpui_kit::MouseMoveEvent, _, cx| {
                    this.battery.input(|w: &mut Self| &mut w.battery, cx);
                    this.drag_sidebar(e, cx);
                }))
                .on_mouse_up(
                    gpui_kit::MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.end_sidebar_drag(cx)),
                )
                .capture_key_down(cx.listener(|this, _, _, cx| {
                    this.battery.input(|w: &mut Self| &mut w.battery, cx)
                }))
                // ⌘1..⌘9 open the Nth sidebar thread; an open model picker
                // takes the key first in its capture phase.
                .on_key_down(cx.listener(|this, e: &gpui_kit::KeyDownEvent, window, cx| {
                    let Some(ix) = kit::cmd_digit(&e.keystroke) else {
                        return;
                    };
                    if this.in_settings() {
                        return;
                    }
                    if let Some(id) = this.inbox.jump.get(ix).cloned() {
                        cx.stop_propagation();
                        this.open_thread(id, window, cx);
                    }
                }))
                .on_modifiers_changed(cx.listener(
                    |this, e: &gpui_kit::ModifiersChangedEvent, _, cx| {
                        let m = &e.modifiers;
                        let held = m.platform && !(m.control || m.alt || m.shift);
                        if this.inbox.cmd_held != held {
                            this.inbox.cmd_held = held;
                            cx.notify();
                        }
                    },
                ))
                .on_action(cx.listener(|this, _: &crate::OpenCommandMenu, window, cx| {
                    if matches!(this.conn, Conn::Ready) {
                        this.toggle_command_menu(window, cx);
                    }
                }))
                // ⇧⌘M opens the composer's model picker, open thread or draft.
                .on_action(cx.listener(|this, _: &crate::OpenModelPicker, window, cx| {
                    let picker = match this.page {
                        Page::Chat => this.active.as_ref().map(|(_, c)| c.read(cx).picker()),
                        Page::Draft => this.draft.as_ref().map(|d| d.picker()),
                        _ => None,
                    };
                    if let Some(picker) = picker {
                        picker.update(cx, |p, cx| p.toggle(window, cx));
                    }
                }))
                // ⇧⌘O: fresh draft in the Default profile; ⌘B / ⌥⌘B fold
                // the left sidebar / right pane.
                .on_action(cx.listener(|this, _: &crate::NewThread, window, cx| {
                    if matches!(this.conn, Conn::Ready) {
                        this.start_in_profile(
                            ironclaw_gpui_client::profiles::DEFAULT_PROFILE_ID.into(),
                            window,
                            cx,
                        );
                    }
                }))
                .on_action(cx.listener(|_, _: &crate::ToggleSidebar, window, cx| {
                    Prefs::update(cx, |p| p.sidebar_hidden = !p.sidebar_hidden);
                    window.refresh();
                }))
                .on_action(cx.listener(|this, _: &crate::ToggleRightPane, window, cx| {
                    match (&this.page, &this.active) {
                        (Page::Chat, Some((_, chat))) => {
                            chat.update(cx, |c, cx| c.toggle_side(cx));
                        }
                        _ => Prefs::update(cx, |p| p.side_open = !p.side_open),
                    }
                    window.refresh();
                }))
                .child(div().flex_1().min_h_0().child(body))
                .when(show_status, |d| d.child(self.render_status_bar(cx)))
                .children(over_window)
                .children(command)
                .child(motion::frame_tail()),
        )
    }
}

impl Workspace {
    /// Zeron's sidebar button: folds the left sidebar away or brings it
    /// back, its glyph morphing with the slide; the choice survives a
    /// relaunch.
    fn sidebar_toggle(&self, cx: &Context<Self>) -> gpui_kit::Stateful<gpui_kit::Div> {
        let open = !Prefs::of(cx).sidebar_hidden;
        let label = if open { "Hide sidebar" } else { "Show sidebar" };
        panels::toggle("sidebar-toggle", open, false, label, cx).on_click(cx.listener(
            |_, _, window, cx| {
                Prefs::update(cx, |p| p.sidebar_hidden = !p.sidebar_hidden);
                window.refresh();
            },
        ))
    }

    fn sidebar_width(&self, cx: &App) -> f32 {
        let saved = Prefs::of(cx).sidebar_width;
        let width = self.sidebar_drag.unwrap_or(if saved > 0. {
            saved
        } else {
            design::SIDEBAR_WIDTH
        });
        width.clamp(design::SIDEBAR_MIN_WIDTH, design::SIDEBAR_MAX_WIDTH)
    }

    /// A thin strip on the sidebar's right edge: drag it to resize the
    /// sidebar, double-click it to restore the default width.
    fn render_sidebar_edge(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("sidebar-edge")
            .flex_none()
            .w(px(6.))
            .ml(px(-3.))
            .mr(px(-3.))
            .h_full()
            .cursor_col_resize()
            .on_mouse_down(
                gpui_kit::MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.sidebar_drag = Some(this.sidebar_width(cx));
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .on_click(cx.listener(|this, e: &gpui_kit::ClickEvent, _, cx| {
                if e.click_count() == 2 {
                    this.sidebar_drag = None;
                    Prefs::update(cx, |p| p.sidebar_width = 0.);
                }
            }))
    }

    fn drag_sidebar(&mut self, e: &gpui_kit::MouseMoveEvent, cx: &mut Context<Self>) {
        if self.sidebar_drag.is_none() {
            return;
        }
        if e.pressed_button != Some(gpui_kit::MouseButton::Left) {
            // The release happened outside the window.
            self.end_sidebar_drag(cx);
            return;
        }
        let width =
            f32::from(e.position.x).clamp(design::SIDEBAR_MIN_WIDTH, design::SIDEBAR_MAX_WIDTH);
        self.sidebar_drag = Some(width);
        cx.notify();
    }

    fn end_sidebar_drag(&mut self, cx: &mut Context<Self>) {
        if let Some(width) = self.sidebar_drag.take() {
            Prefs::update(cx, |p| p.sidebar_width = width);
        }
    }

    /// The wallpaper over one area of the window, when the person picked
    /// that area (`wallpaper_area`): the whole window, the conversation,
    /// the sidebar or the right pane. Each layer is painted at its area's
    /// settled size, so a sidebar or pane slide costs one repaint after it
    /// ends, and sits above the area's content, as in VS Code.
    fn render_wallpaper(
        &self,
        area: &str,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let prefs = Prefs::of(cx);
        if prefs.wallpaper_area != area {
            return None;
        }
        let viewport = window.viewport_size();
        let (vw, vh) = (f32::from(viewport.width), f32::from(viewport.height));
        let status = if prefs.status_bar_hidden {
            0.
        } else {
            design::STATUS_BAR_HEIGHT
        };
        let sidebar = if prefs.sidebar_hidden {
            0.
        } else {
            self.sidebar_width(cx)
        };
        // The right pane belongs to an open thread (Appearance floats over it).
        let (pane, pane_t) = match (self.page, &self.active) {
            (Page::Chat | Page::Appearance, Some((_, chat))) => {
                chat.read(cx).side_geometry(window, cx)
            }
            _ => (0., 0.),
        };
        let pane_settled = if prefs.side_open { pane } else { 0. };
        let layer = match area {
            "window" => wallpaper::overlay(vw, vh, cx)?.top_0().left_0(),
            "conversation" => wallpaper::overlay(vw - sidebar - pane_settled, vh - status, cx)?
                .top_0()
                .left_0(),
            "sidebar" => wallpaper::overlay(sidebar, vh - status, cx)?
                .top_0()
                .right_0(),
            "pane" if pane > 0. => wallpaper::overlay(pane, vh - status, cx)?
                .top_0()
                .right(px(pane * pane_t - pane)),
            _ => return None,
        };
        Some(layer.into_any_element())
    }

    /// No thread open: the mark, a line and one button.
    /// Static, so it costs nothing per frame once settled.
    fn render_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *Palette::of(cx);
        let s = *Style::of(cx);
        div().size_full().relative().child(motion::fade_in(
            v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_4()
                .child(crate::design::kit::claw_mark(cx))
                .child(
                    v_flex()
                        .items_center()
                        .gap_1p5()
                        .child(
                            div()
                                .text_size(px(26.))
                                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                .child("What are we building?"),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(p.text_muted)
                                .child("Start a conversation, or pick one from the sidebar."),
                        ),
                )
                .child(
                    kit::button("start-thread")
                        .label("New thread")
                        .icon(IconName::Plus)
                        .primary()
                        .on_click(cx.listener(|this, _, window, cx| this.open_draft(window, cx))),
                ),
            "empty-state",
            s.enter,
            s.enter_rise * 2.,
        ))
    }
}
