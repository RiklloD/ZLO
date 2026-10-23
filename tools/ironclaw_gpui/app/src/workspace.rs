//! Window root: connects to the gateway, lists threads, and switches between
//! the open chat and the full-width pages.

use std::collections::HashMap;
use std::time::Duration;

use ironclaw_gpui_client::wire::{SessionInfo, ThreadSummary};
use ironclaw_gpui_client::{Backend, Client, ClientError, Connection};

use crate::chat::{ChatView, ThreadUpdated};
use crate::jobs;
use crate::pages::PageEvent;
use crate::pages::automations::AutomationsView;
use crate::pages::extensions::ExtensionsView;
use crate::pages::files::FilesView;
use crate::pages::notifications::NotificationsView;
use crate::pages::projects::ProjectsView;
use crate::pages::settings::SettingsView;
use crate::pages::skills::SkillsView;
use crate::ui::*;

const THREAD_PAGE: u32 = 100;
const SIDEBAR_WIDTH: f32 = 260.;
/// The inbox has no live stream, so the badge polls. One tiny request.
const UNREAD_POLL: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Chat,
    Notifications,
    Automations,
    Files,
    Projects,
    Extensions,
    Skills,
    Settings,
}

/// Pages are built on first visit and kept, so switching back is instant.
#[derive(Default)]
struct Pages {
    notifications: Option<Entity<NotificationsView>>,
    automations: Option<Entity<AutomationsView>>,
    files: Option<Entity<FilesView>>,
    projects: Option<Entity<ProjectsView>>,
    extensions: Option<Entity<ExtensionsView>>,
    skills: Option<Entity<SkillsView>>,
    settings: Option<Entity<SettingsView>>,
}

enum Conn {
    Connecting,
    Failed(SharedString),
    Ready,
}

pub struct Workspace {
    conn: Conn,
    backend: Option<Backend>,
    session: Option<SessionInfo>,
    threads: Vec<ThreadSummary>,
    active: Option<(String, Entity<ChatView>)>,
    page: Page,
    pages: Pages,
    unread: usize,
    _chat_events: Option<Subscription>,
    _page_events: Vec<Subscription>,
    _poll: Option<Task<()>>,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self {
            conn: Conn::Connecting,
            backend: None,
            session: None,
            threads: Vec::new(),
            active: None,
            page: Page::Chat,
            pages: Pages::default(),
            unread: 0,
            _chat_events: None,
            _page_events: Vec::new(),
            _poll: None,
        };
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
                self.conn = Conn::Failed(e.to_string().into());
                return;
            }
        };
        self.backend = Some(backend.clone());

        let job = backend.spawn(|c| async move {
            let session = c.session().await?;
            let threads = c.list_threads(THREAD_PAGE, None).await?;
            Ok::<_, ClientError>((session, threads))
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = job
                .await
                .map_err(|e| ClientError::Config(e.to_string()))
                .and_then(|r| r);
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok((session, page)) => {
                        this.session = Some(session);
                        this.threads = page.threads;
                        this.conn = Conn::Ready;
                        this.start_polling(cx);
                        if let Some(first) = this.threads.first().map(|t| t.thread_id.clone()) {
                            this.open_thread(first, window, cx);
                        }
                    }
                    Err(e) => this.conn = Conn::Failed(friendly(&e).into()),
                }
                cx.notify();
            });
        });
        task.detach();
    }

    fn open_thread(&mut self, thread_id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.page = Page::Chat;
        if self.active.as_ref().is_some_and(|(id, _)| *id == thread_id) {
            cx.notify();
            return;
        }
        let (Some(backend), Some(session)) = (self.backend.clone(), self.session.as_ref()) else {
            return;
        };
        let channel = session.session_channel_extension_id.clone();
        let id = thread_id.clone();
        // Replacing the entity drops the old chat, which closes its stream
        // and frees one of the gateway's per-user stream slots.
        let view = cx.new(|cx| ChatView::new(backend, channel, id, window, cx));
        self._chat_events = Some(cx.subscribe(&view, |this, _, _: &ThreadUpdated, cx| {
            this.refresh_threads(cx);
            this.refresh_unread(cx);
        }));
        self.active = Some((thread_id, view));
        cx.notify();
    }

    /// Titles are computed by the list route only, so re-list after a run.
    fn refresh_threads(&mut self, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.clone() else {
            return;
        };
        let job = backend.spawn(|c| async move { c.list_threads(THREAD_PAGE, None).await });
        let task = cx.spawn(async move |this, cx| {
            if let Ok(Ok(page)) = job.await {
                let _ = this.update(cx, |this, cx| {
                    this.threads = page.threads;
                    if let Some(view) = this.pages.notifications.clone() {
                        let titles = this.thread_titles();
                        view.update(cx, |v, cx| v.set_titles(titles, cx));
                    }
                    cx.notify();
                });
            }
        });
        task.detach();
    }

    fn new_thread(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.clone() else {
            return;
        };
        let job = backend.spawn(|c| async move { c.create_thread().await });
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = job
                .await
                .map_err(|e| ClientError::Config(e.to_string()))
                .and_then(|r| r);
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(created) => {
                    let id = created.thread.thread_id.clone();
                    this.threads.insert(0, created.thread);
                    this.open_thread(id, window, cx);
                }
                Err(e) => {
                    this.conn = Conn::Failed(friendly(&e).into());
                    cx.notify();
                }
            });
        });
        task.detach();
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
                this.threads.retain(|t| t.thread_id != thread_id);
                if this.active.as_ref().is_some_and(|(id, _)| *id == thread_id) {
                    this.active = None;
                    if let Some(next) = this.threads.first().map(|t| t.thread_id.clone()) {
                        this.open_thread(next, window, cx);
                    }
                }
                cx.notify();
            });
        });
        task.detach();
    }

    fn thread_titles(&self) -> HashMap<String, String> {
        self.threads
            .iter()
            .map(|t| (t.thread_id.clone(), t.display_title().to_string()))
            .collect()
    }

    fn projects_enabled(&self) -> bool {
        self.session
            .as_ref()
            .and_then(|s| s.features.get("reborn_projects"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    fn start_polling(&mut self, cx: &mut Context<Self>) {
        self.refresh_unread(cx);
        self._poll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(UNREAD_POLL).await;
                if this.update(cx, |this, cx| this.refresh_unread(cx)).is_err() {
                    break;
                }
            }
        }));
    }

    fn refresh_unread(&mut self, cx: &mut Context<Self>) {
        let Some(backend) = self.backend.clone() else {
            return;
        };
        jobs::run(
            &backend,
            cx,
            |c| async move { c.list_notifications(1).await },
            |this, result, cx| {
                // silent-ok: the badge is a hint; the inbox page shows real errors.
                if let Ok(page) = result {
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
                let titles = self.thread_titles();
                open!(notifications, |cx| NotificationsView::new(
                    backend, titles, cx
                ))
            }
            Page::Automations => open!(automations, |cx| AutomationsView::new(backend, cx)),
            Page::Files => open!(files, |cx| FilesView::new(backend, cx)),
            Page::Projects => open!(projects, |cx| ProjectsView::new(backend, window, cx)),
            Page::Extensions => open!(extensions, |cx| ExtensionsView::new(backend, cx)),
            Page::Skills => open!(skills, |cx| SkillsView::new(backend, window, cx)),
            Page::Settings => open!(settings, |cx| SettingsView::new(backend, cx)),
        }
        self.page = page;
        cx.notify();
    }

    fn render_nav(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let mut items = vec![
            (Page::Notifications, IconName::Bell, "Notifications"),
            (Page::Automations, IconName::Calendar, "Automations"),
            (Page::Files, IconName::Folder, "Files"),
        ];
        if self.projects_enabled() {
            items.push((Page::Projects, IconName::LayoutDashboard, "Projects"));
        }
        items.push((Page::Extensions, IconName::Frame, "Extensions"));
        items.push((Page::Skills, IconName::BookOpen, "Skills"));
        items.push((Page::Settings, IconName::Settings, "Settings"));
        let unread = self.unread;
        v_flex()
            .px_2()
            .pb_2()
            .gap_0p5()
            .children(
                items
                    .into_iter()
                    .enumerate()
                    .map(|(ix, (page, icon, label))| {
                        let active = self.page == page;
                        h_flex()
                            .id(("nav", ix))
                            .role(Role::Button)
                            .aria_label(label)
                            .w_full()
                            .gap_2()
                            .px_2()
                            .py_1p5()
                            .rounded(theme.radius)
                            .text_sm()
                            .cursor_pointer()
                            .when(active, |d| {
                                d.bg(theme.sidebar_accent)
                                    .text_color(theme.sidebar_accent_foreground)
                            })
                            .when(!active, |d| d.hover(|s| s.bg(theme.sidebar_accent)))
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
        let active = self.active.as_ref().map(|(id, _)| id.clone());
        let theme = cx.theme();
        let rows =
            self.threads.iter().enumerate().map(|(ix, thread)| {
                let id = thread.thread_id.clone();
                let delete_id = id.clone();
                let is_active = self.page == Page::Chat && active.as_deref() == Some(id.as_str());
                h_flex()
                    .id(("thread", ix))
                    .role(Role::Button)
                    .aria_label(thread.display_title().to_string())
                    .group("thread-row")
                    .w_full()
                    .px_2()
                    .py_1p5()
                    .gap_1()
                    .rounded(theme.radius)
                    .text_sm()
                    .cursor_pointer()
                    .when(is_active, |this| {
                        this.bg(theme.sidebar_accent)
                            .text_color(theme.sidebar_accent_foreground)
                    })
                    .when(!is_active, |this| {
                        this.hover(|s| s.bg(theme.sidebar_accent))
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_thread(id.clone(), window, cx)
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(thread.display_title().to_string()),
                    )
                    .child(
                        div()
                            .invisible()
                            .group_hover("thread-row", |s| s.visible())
                            .child(
                                Button::new(("delete-thread", ix))
                                    .icon(IconName::Delete)
                                    .ghost()
                                    .xsmall()
                                    .tooltip("Delete conversation")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.delete_thread(delete_id.clone(), window, cx)
                                    })),
                            ),
                    )
            });

        v_flex()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_none()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(
                h_flex()
                    .justify_between()
                    .px_3()
                    .py_3()
                    .child(div().font_semibold().child("IronClaw")),
            )
            .child(self.render_nav(cx))
            .child(
                h_flex()
                    .justify_between()
                    .items_center()
                    .px_3()
                    .pt_2()
                    .pb_1()
                    .child(
                        div()
                            .text_xs()
                            .font_medium()
                            .text_color(cx.theme().muted_foreground)
                            .child("Conversations"),
                    )
                    .child(
                        Button::new("new-thread")
                            .icon(IconName::Plus)
                            .ghost()
                            .xsmall()
                            .tooltip("New conversation")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.new_thread(window, cx)),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .id("thread-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_2()
                    .gap_0p5()
                    .children(rows),
            )
            .child(
                div().p_2().child(
                    Button::new("open-browser")
                        .icon(IconName::ExternalLink)
                        .label("Open in browser")
                        .ghost()
                        .small()
                        .w_full()
                        .on_click(cx.listener(|this, _, _, cx| this.open_in_browser(cx))),
                ),
            )
    }
}

/// Turn the two common first-run failures into instructions.
fn friendly(error: &ClientError) -> String {
    match error {
        ClientError::Network(_) => format!(
            "{error}\n\nStart the gateway with `ironclaw serve` (or `ironclaw service start`)."
        ),
        e if e.is_unauthorized() => format!(
            "{error}\n\nThe WebUI token was rejected. Re-run `ironclaw onboard` to refresh it."
        ),
        e => e.to_string(),
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body: AnyElement = match &self.conn {
            Conn::Connecting => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .child(Spinner::new())
                .child("Connecting to IronClaw")
                .into_any_element(),
            Conn::Failed(message) => v_flex()
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
                        .text_color(cx.theme().muted_foreground)
                        .whitespace_normal()
                        .child(message.clone()),
                )
                .child(
                    Button::new("retry-connect")
                        .label("Try again")
                        .primary()
                        .on_click(cx.listener(|this, _, window, cx| this.connect(window, cx))),
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
                    Page::Extensions => self.pages.extensions.clone().map(|v| v.into_any_element()),
                    Page::Skills => self.pages.skills.clone().map(|v| v.into_any_element()),
                    Page::Settings => self.pages.settings.clone().map(|v| v.into_any_element()),
                };
                let main: AnyElement = match (page, &self.active) {
                    (Some(page), _) => page,
                    (None, Some((_, chat))) => chat.clone().into_any_element(),
                    (None, None) => v_flex()
                        .size_full()
                        .items_center()
                        .justify_center()
                        .gap_3()
                        .text_color(cx.theme().muted_foreground)
                        .child("No conversation open")
                        .child(
                            Button::new("start-thread")
                                .label("Start a conversation")
                                .primary()
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.new_thread(window, cx)),
                                ),
                        )
                        .into_any_element(),
                };
                h_flex()
                    .size_full()
                    .child(self.render_sidebar(cx))
                    .child(div().flex_1().min_w_0().h_full().child(main))
                    .into_any_element()
            }
        };
        div()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(body)
    }
}
