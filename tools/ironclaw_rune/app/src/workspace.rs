//! Window root (t3code's layout): connects to the gateway; the sidebar lists
//! threads, or the settings pages while one is open; the panel shows the
//! open chat, the new-thread screen or a settings page under a breadcrumb.
//! (GPUI `workspace.rs`.)
//!
//! GPUI's `Workspace` entity is [`WorkspaceState`] in `State::workspace`. Its
//! timers (unread badge, status bar, inbox follow-up, the Working tick, the
//! snooze wake) are deadlines in [`Timers`], run from [`poll`] with
//! `Tx::wake_after`, instead of spawned loops. Other areas call
//! [`open_thread`], [`show`], [`thread_updated`], [`run_ended`] and
//! [`open_run_logs`] where GPUI emitted events to the workspace.
//!
//! Files: `shell.rs` draws the window, `keys.rs` holds shortcuts and the
//! native menu, `status.rs` the bottom bar, `inbox*` the sidebar list,
//! `command.rs` ⌘K, `context.rs` the context meter, `draft.rs` the
//! new-thread screen, `message_search.rs` past-chat search and `quick.rs`
//! the ⇧⌘Space composer.

use std::time::{Duration, Instant};

use ironclaw_gpui_client::inbox::InboxAction;
use ironclaw_gpui_client::usage::ThreadTelemetry;
use ironclaw_gpui_client::{Backend, Client, ClientError, Connection};
use rune::{Cx, El, KeyEvent, Tx};

use crate::alerts::Alerts;
use crate::jobs;
use crate::state::{Conn, Page, State};

mod command;
mod context;
mod draft;
pub mod fixtures;
mod inbox;
mod keys;
mod message_search;
pub mod quick;
mod shell;
mod status;
pub(crate) mod ui;

// Voice (area F) and SnapShot (area F) call these.
#[allow(unused_imports)]
pub use draft::{
    ComposerTarget, composer_edit, start_in_profile, stage_snapshot, voice_composer,
};
pub use inbox::title_of as row_title;
pub use keys::{menu_items, on_close};

/// The inbox has no live stream, so the badge polls. One tiny request.
const UNREAD_POLL: Duration = Duration::from_secs(15);
/// Enough to catch every new item between two polls.
const ALERT_PAGE: u32 = 20;
/// Retry schedule while the gateway is unreachable (seconds, last repeats).
/// It is a local listener, so a refused connect is free to repeat.
const CONNECT_RETRY: [u64; 5] = [1, 2, 5, 10, 30];
/// The app starts the gateway itself, so for this long after launch a
/// refused connect means "still booting": keep the loading screen and poll
/// fast instead of showing an error.
const STARTUP_GRACE: Duration = Duration::from_secs(25);
const STARTUP_POLL: Duration = Duration::from_millis(150);
/// Status bar refresh while the window is in use. Telemetry also refreshes
/// right after each run; usage windows are cached for a minute upstream.
const STATUS_POLL: Duration = Duration::from_secs(10);
/// How often terminal sessions are rescanned after the launch import
/// (GPUI `pages/settings/session_import.rs::RESCAN_EVERY`).
const SESSION_RESCAN: Duration = Duration::from_secs(15 * 60);

/// Deadlines `poll` runs; `None` is off.
#[derive(Default)]
pub struct Timers {
    pub unread: Option<Instant>,
    pub status: Option<Instant>,
    /// The inbox follow-up while a row works (`inbox::POLL`).
    pub inbox: Option<Instant>,
    /// Repaints the Working timers once a second.
    pub tick: Option<Instant>,
    /// A snooze or usage limit that ends.
    pub wake: Option<Instant>,
    pub session_import: Option<Instant>,
}

pub struct WorkspaceState {
    pub connect_attempt: usize,
    /// Bumped per connect: a stale connect answer or retry timer is dropped.
    pub connect_gen: jobs::Generation,
    pub inbox: inbox::Inbox,
    /// Unread inbox notifications, for the Notifications badge. The
    /// notifications page (area D) writes it too (GPUI `PageEvent::Unread`).
    pub unread: usize,
    pub alerts: Alerts,
    pub titles: crate::titles::Titles,
    pub run_clock: crate::run_clock::RunClock,
    pub drafts: crate::drafts::Store,
    /// The new-thread screen, kept while its draft lives.
    pub draft: Option<draft::DraftState>,
    /// Bumped per new draft, so a late attach never lands in a newer one.
    pub draft_opens: jobs::Generation,
    /// The ⌘K menu while open.
    pub command: Option<command::CommandMenu>,
    pub command_opens: u64,
    /// Message-text search shared by ⌘K and the inbox field.
    pub message_search: message_search::MessageSearch,
    pub quick: quick::Quick,
    /// The open thread's telemetry for the status bar (`None` until known,
    /// or when the gateway lacks the route).
    pub telemetry: Option<ThreadTelemetry>,
    /// The context meter's popover, report and banner state.
    pub context: context::ContextMeter,
    /// Live sidebar width while its edge is dragged; saved on release.
    pub sidebar_drag: Option<f32>,
    /// The window is the key window (GPUI `window.is_window_active()`).
    pub window_active: bool,
    /// The thread whose delete is being confirmed.
    pub confirm_delete: Option<String>,
    pub timers: Timers,
    /// A scene shows the quick composer's card over the shell.
    pub quick_fixture: bool,
    /// Session import ran its launch pass.
    session_import_launched: bool,
}

impl Default for WorkspaceState {
    fn default() -> Self {
        Self {
            connect_attempt: 0,
            connect_gen: Default::default(),
            inbox: inbox::Inbox::new(),
            unread: 0,
            alerts: Alerts::default(),
            titles: Default::default(),
            run_clock: Default::default(),
            drafts: Default::default(),
            draft: None,
            draft_opens: Default::default(),
            command: None,
            command_opens: 0,
            message_search: Default::default(),
            quick: Default::default(),
            telemetry: None,
            context: Default::default(),
            sidebar_drag: None,
            window_active: true,
            confirm_delete: None,
            timers: Timers::default(),
            quick_fixture: false,
            session_import_launched: false,
        }
    }
}

impl WorkspaceState {
    /// A scene's unsent new-thread draft, in memory only.
    pub fn drafts_fixture(&mut self, text: &str, profile: Option<&str>) {
        let mut draft = crate::drafts::Draft::text(text);
        draft.profile = profile.map(str::to_string);
        self.drafts.insert_fixture(crate::drafts::NEW_THREAD, draft);
    }
}

/// Called once from `App::init`, before the first frame.
pub fn start(s: &mut State) {
    crate::drafts::load(s);
    crate::alerts::install(s.rt.handle.clone());
    crate::snapshot::start(s);
    crate::voice::start(s);
    connect(s);
}

/// The open thread's id, if any.
pub fn active_id(s: &State) -> Option<&str> {
    s.chat.as_ref().map(|c| c.thread_id.as_str())
}

/// Whether the bottom status bar shows. Toggled app-wide with ⇧⌘S.
pub fn toggle_status_bar(s: &mut State) {
    s.update_prefs(|p| p.status_bar_hidden = !p.status_bar_hidden);
}

/// Flip between the light and dark theme without opening Settings (⇧⌘Y).
pub fn toggle_scheme(s: &mut State) {
    let dark = crate::design::scheme_is_dark(&s.prefs, s.system_dark);
    s.update_prefs(|p| p.scheme = if dark { "light" } else { "dark" }.into());
}

pub fn toggle_sidebar(s: &mut State) {
    s.update_prefs(|p| p.sidebar_hidden = !p.sidebar_hidden);
}

fn connect(s: &mut State) {
    s.conn = Conn::Connecting;
    let generation = s.workspace.connect_gen.bump();
    let backend = match Connection::discover()
        .and_then(Client::new)
        .and_then(Backend::new)
    {
        Ok(backend) => backend,
        Err(e) => {
            tracing::warn!(error = %e, "gateway connection settings unavailable");
            s.conn = Conn::Failed {
                message: friendly(&e),
                retry_in: None,
            };
            return;
        }
    };
    s.rt.backend = Some(backend);
    jobs::run(
        s,
        "workspace",
        |c| async move {
            let session = c.session().await?;
            let inbox = c.inbox().await?;
            Ok::<_, ClientError>((session, inbox))
        },
        move |s, result, _tx| {
            if s.workspace.connect_gen != generation {
                return;
            }
            match result {
                Ok((session, list)) => {
                    s.workspace.connect_attempt = 0;
                    s.session = Some(session);
                    inbox::apply(s, list);
                    s.conn = Conn::Ready;
                    start_polling(s);
                    status::start(s);
                    match s.workspace.inbox.first_open() {
                        Some(first) => open_thread(s, first),
                        // Nothing open: a fresh draft in the default profile.
                        None => start_in_profile(
                            s,
                            ironclaw_gpui_client::profiles::DEFAULT_PROFILE_ID.into(),
                        ),
                    }
                }
                Err(e) => connect_failed(s, &e),
            }
        },
    );
}

/// The connection screen's "Try again": also brings the gateway back after
/// a Stop from the Operator page.
pub fn retry_connect(s: &mut State) {
    crate::service::start();
    s.workspace.connect_attempt = 0;
    connect(s);
}

/// Show the error, and retry on a schedule when the gateway is simply not
/// up yet (it starts after the app at login, or is restarting).
fn connect_failed(s: &mut State, error: &ClientError) {
    let network = matches!(error, ClientError::Network(_));
    let generation = s.workspace.connect_gen;
    if network && s.launched.elapsed() < STARTUP_GRACE {
        // Still booting: stay on the loading screen and knock again soon.
        s.conn = Conn::Connecting;
        jobs::after(s, STARTUP_POLL, move |s, _| {
            if s.workspace.connect_gen == generation && matches!(s.conn, Conn::Connecting) {
                connect(s);
            }
        });
        return;
    }
    tracing::warn!(error = %error, attempt = s.workspace.connect_attempt, "gateway connection failed");
    let attempt = s.workspace.connect_attempt;
    let retry_in = network.then(|| CONNECT_RETRY[attempt.min(CONNECT_RETRY.len() - 1)]);
    s.conn = Conn::Failed {
        message: friendly(error),
        retry_in,
    };
    let Some(secs) = retry_in else { return };
    s.workspace.connect_attempt += 1;
    jobs::after(s, Duration::from_secs(secs), move |s, _| {
        if s.workspace.connect_gen == generation && matches!(s.conn, Conn::Failed { .. }) {
            connect(s);
        }
    });
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

/// Open a thread in the main column. Other areas call this.
pub fn open_thread(s: &mut State, thread_id: String) {
    s.page = Page::Chat;
    s.workspace.context.close();
    inbox::visit(s, &thread_id);
    if active_id(s) == Some(thread_id.as_str()) {
        crate::chat::focus_composer(s);
        return;
    }
    // Replacing the chat drops the old one, which closes its stream and
    // frees one of the gateway's per-user stream slots.
    crate::chat::open(s, thread_id);
    s.workspace.telemetry = None;
    status::refresh_telemetry(s);
}

/// Open `thread_id` scrolled to `message_id` (a message-search hit).
pub fn open_thread_at(s: &mut State, thread_id: String, message_id: String) {
    open_thread(s, thread_id.clone());
    if active_id(s) == Some(thread_id.as_str()) {
        crate::chat::reveal_message(s, message_id);
    }
}

/// Show a page in the main column, loading it on first visit and refreshing
/// it after. Other areas call this.
pub fn show(s: &mut State, page: Page) {
    if s.rt.backend.is_none() && !s.fixture {
        return;
    }
    if page == Page::Draft {
        draft::open(s);
        return;
    }
    if page == Page::Chat {
        back_to_threads(s);
        return;
    }
    if page == Page::Providers {
        // The orchestrator catalog (CLI agents) is read through any thread.
        let catalog = s.workspace.inbox.list.rows.first().map(|r| r.thread_id.clone());
        crate::pages::providers::set_catalog_thread(s, catalog);
    }
    s.page = page;
    crate::pages::opened(s, page);
}

/// Leave settings for the open thread, or the new-thread screen.
pub fn back_to_threads(s: &mut State) {
    match active_id(s).map(str::to_string) {
        Some(id) => open_thread(s, id),
        None => draft::open(s),
    }
}

pub fn in_settings(s: &State) -> bool {
    !matches!(s.page, Page::Chat | Page::Draft)
}

/// Esc closes what the user last opened: the model or thinking picker, else
/// the settings pages. The command menu handles its own Esc.
fn dismiss(s: &mut State) -> bool {
    if s.workspace.command.is_some() {
        return false;
    }
    if crate::chat::model_picker::close_open(s) {
        crate::chat::focus_composer(s);
        return true;
    }
    if in_settings(s) {
        back_to_threads(s);
        return true;
    }
    false
}

fn projects_enabled(s: &State) -> bool {
    s.session
        .as_ref()
        .and_then(|s| s.features.get("reborn_projects"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// A settings sidebar row: page, icon, label.
pub type NavItem = (Page, &'static rune::Icon, &'static str);

/// Settings pages in sidebar order, under named groups (T3's settings
/// regroup): the app, what the agent can use, what it works on, and what it
/// reports.
pub fn settings_groups(s: &State) -> Vec<(&'static str, Vec<NavItem>)> {
    use crate::icons as i;
    let mut work: Vec<NavItem> = vec![
        (Page::Automations, &i::CALENDAR, "Automations"),
        (Page::Files, &i::FOLDER, "Files"),
    ];
    if projects_enabled(s) {
        work.push((Page::Projects, &i::LAYOUT_DASHBOARD, "Projects"));
    }
    vec![
        (
            "App",
            vec![
                (Page::Settings, &i::SETTINGS, "General"),
                (Page::Appearance, &i::PALETTE, "Appearance"),
                (Page::Voice, &i::MIC, "Voice"),
                (Page::Notifications, &i::BELL, "Notifications"),
            ],
        ),
        (
            "Agent",
            vec![
                (Page::Providers, &i::BOT, "Providers"),
                (Page::Profiles, &i::USER, "Profiles"),
                (Page::Memory, &i::FILE_TEXT, "Memory"),
                (Page::Skills, &i::BOOK_OPEN, "Skills"),
                (Page::Extensions, &i::FRAME, "Extensions"),
                (Page::Permissions, &i::CIRCLE_CHECK, "Permissions"),
                (Page::Hooks, &i::REPLACE, "Hooks"),
            ],
        ),
        ("Work", work),
        (
            "Reports",
            vec![
                (Page::Usage, &i::CHART_PIE, "Usage"),
                (Page::Traces, &i::NETWORK, "Trace Commons"),
                (Page::Operator, &i::SQUARE_TERMINAL, "Operator"),
            ],
        ),
    ]
}

pub fn settings_pages(s: &State) -> Vec<NavItem> {
    settings_groups(s)
        .into_iter()
        .flat_map(|(_, pages)| pages)
        .collect()
}

/// The gateway's web UI in the browser.
pub fn open_in_browser(s: &State, tx: &mut Tx) {
    if let Some(backend) = &s.rt.backend {
        let url = backend.client().connection().base_url.clone();
        if let Err(e) = tx.open_url(&url) {
            tracing::warn!(error = %e, "could not open the web UI");
        }
    }
}

/// Delete a thread; it leaves the list only after the gateway confirmed.
pub fn delete_thread(s: &mut State, thread_id: String) {
    let id = thread_id.clone();
    jobs::run(
        s,
        "delete thread",
        move |c| async move { c.delete_thread(&id).await },
        move |s, result, _| {
            // Remove only after the gateway confirmed the delete.
            if result.is_err() {
                return;
            }
            s.workspace.inbox.list.rows.retain(|t| t.thread_id != thread_id);
            if active_id(s) == Some(thread_id.as_str()) {
                s.chat = None;
                match s.workspace.inbox.first_open() {
                    Some(next) => open_thread(s, next),
                    None => draft::open(s),
                }
            }
        },
    );
}

// ── events from other areas (GPUI `ChatView` events) ───────────────────

/// The open chat changed its thread (GPUI `ThreadUpdated`): the list and
/// the badge catch up.
pub fn thread_updated(s: &mut State) {
    inbox::refresh(s);
    refresh_unread(s);
}

/// A run of `thread_id` ended (GPUI `RunEnded`). Area A calls this.
pub fn run_ended(s: &mut State, thread_id: String, failed: bool) {
    status::refresh_telemetry(s);
    crate::run_clock::end(s, &thread_id);
    if !failed {
        inbox::poll_for_title(s);
    }
    // Watched live: the result is seen, so no Done badge.
    if s.workspace.window_active && s.page == Page::Chat {
        inbox::mark(s, thread_id.clone(), InboxAction::Visit, None);
    } else {
        inbox::refresh(s);
    }
    if !s.workspace.window_active {
        let title = crate::titles::get(s, &thread_id);
        s.workspace
            .alerts
            .post_run(&thread_id, failed, title.as_deref());
    }
}

/// A chat asked for its run's logs (GPUI `OpenRunLogs`): the Operator page.
pub fn open_run_logs(s: &mut State, run: String) {
    show(s, Page::Operator);
    crate::pages::operator::show_run_logs(s, run);
}

// ── polling ─────────────────────────────────────────────────────────────

fn start_polling(s: &mut State) {
    refresh_unread(s);
    let now = Instant::now();
    s.workspace.timers.unread = Some(now + UNREAD_POLL);
    if !s.workspace.session_import_launched {
        s.workspace.session_import_launched = true;
        session_import_tick(s, ironclaw_gpui_client::session_import::ImportTrigger::Launch);
        s.workspace.timers.session_import = Some(now + SESSION_RESCAN);
    }
}

/// First-launch session import, then the periodic rescan request (the
/// backend skips a pass that is not due).
fn session_import_tick(s: &mut State, trigger: ironclaw_gpui_client::session_import::ImportTrigger) {
    jobs::run(
        s,
        "session import",
        move |c| async move { c.session_import_tick(trigger).await },
        |_, result, _| {
            if let Err(e) = result {
                tracing::debug!(error = %e, "session import tick failed");
            }
        },
    );
}

/// Runs before every frame: due timers. True when the view should re-run.
pub fn poll(s: &mut State, tx: &mut Tx) -> bool {
    if s.fixture || !matches!(s.conn, Conn::Ready) {
        return false;
    }
    let now = Instant::now();
    let due = |at: Option<Instant>| at.is_some_and(|at| at <= now);
    let mut changed = false;
    if due(s.workspace.timers.unread) {
        s.workspace.timers.unread = Some(now + UNREAD_POLL);
        refresh_unread(s);
    }
    if due(s.workspace.timers.status) {
        s.workspace.timers.status = Some(now + STATUS_POLL);
        status::refresh_status(s);
    }
    if due(s.workspace.timers.session_import) {
        s.workspace.timers.session_import = Some(now + SESSION_RESCAN);
        session_import_tick(s, ironclaw_gpui_client::session_import::ImportTrigger::Background);
    }
    if due(s.workspace.timers.inbox) {
        s.workspace.timers.inbox = None;
        inbox::poll_due(s);
    }
    if due(s.workspace.timers.wake) {
        s.workspace.timers.wake = None;
        inbox::refresh(s);
    }
    if due(s.workspace.timers.tick) {
        s.workspace.timers.tick = None;
        changed |= inbox::tick_due(s);
    }
    changed |= message_search::poll(s);
    let t = &s.workspace.timers;
    let next = [t.unread, t.status, t.inbox, t.wake, t.tick, t.session_import]
        .into_iter()
        .flatten()
        .chain(s.workspace.message_search.due())
        .min();
    if let Some(next) = next {
        tx.wake_after(next.saturating_duration_since(now).max(Duration::from_millis(5)));
    }
    changed
}

/// Badge count plus system notifications for new inbox items.
pub fn refresh_unread(s: &mut State) {
    jobs::run(
        s,
        "notifications",
        |c| async move { c.list_notifications(ALERT_PAGE).await },
        |s, result, _| {
            // silent-ok: the badge is a hint; the inbox page shows real errors.
            let Ok(page) = result else { return };
            let looking_at = (s.workspace.window_active && s.page == Page::Chat)
                .then(|| active_id(s).map(str::to_string))
                .flatten();
            let fresh: Vec<_> = s
                .workspace
                .alerts
                .take_new(&page.notifications)
                .into_iter()
                .cloned()
                .collect();
            for item in fresh {
                let thread = crate::alerts::thread_of(&item).map(str::to_string);
                if thread.is_some() && thread == looking_at {
                    continue;
                }
                // Automation runs are not in the thread list; fetch their
                // title first so the alert can name them.
                let Some(thread) = thread else {
                    s.workspace.alerts.post(&item, None);
                    continue;
                };
                crate::titles::resolve(s, thread, move |s, title, _| {
                    s.workspace.alerts.post(&item, Some(&title));
                });
            }
            let changed = page.unread_count != s.workspace.unread;
            s.workspace.unread = page.unread_count;
            if changed {
                crate::pages::notifications::refresh(s);
            }
        },
    );
}

/// A system notification was clicked: bring the window up on its thread.
pub fn alert_clicked(s: &mut State, tag: &str) {
    if let Some(thread) = s.workspace.alerts.thread_for(tag) {
        open_thread(s, thread);
    }
    // `run:` tags are local; every other tag is an inbox notification id.
    if tag.starts_with("run:") {
        return;
    }
    let id = tag.to_string();
    jobs::run(
        s,
        "notifications",
        move |c| async move { c.mark_notification_read(&id).await },
        // silent-ok: read state is cosmetic; the next poll recounts.
        |s, _, _| refresh_unread(s),
    );
}

/// `App::on_focus`: the window became key or lost it.
pub fn window_focus(s: &mut State, focused: bool, _tx: &mut Tx) {
    s.workspace.window_active = focused;
    if !matches!(s.conn, Conn::Ready) {
        return;
    }
    status::refresh_status(s);
    // Back in front: catch up on whatever finished meanwhile.
    if focused {
        inbox::refresh(s);
        crate::chat::focus_composer(s);
        if s.page == Page::Draft
            && let Some(d) = s.workspace.draft.as_mut()
        {
            d.input.focused = true;
        }
    }
}

pub fn on_key(s: &mut State, ev: &KeyEvent, tx: &mut Tx) -> bool {
    keys::on_key(s, ev, tx)
}

pub fn view(s: &State, cx: &mut Cx<State>) -> El {
    shell::view(s, cx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn friendly_errors_say_what_to_do() {
        let e = ClientError::Config("no token".into());
        assert_eq!(friendly(&e), e.to_string());
    }
}

// Cross-area API: thread titles, window focus and the unread badge, as the
// chat, side pane and pages call them.

/// Whether the main window is in front (GPUI `window.is_window_active()`).
pub fn window_active(s: &State) -> bool {
    s.workspace.window_active
}

/// A thread's title if known (the inbox, or fetched by [`ensure_titles`]).
pub fn thread_title(s: &State, thread_id: &str) -> Option<String> {
    crate::titles::get(s, thread_id)
}

/// Same as [`thread_title`] (the pages' name for it).
pub fn title_of(s: &State, thread_id: &str) -> Option<String> {
    crate::titles::get(s, thread_id)
}

/// Fetch the titles of these threads if not known yet.
pub fn ensure_titles(s: &mut State, thread_ids: impl IntoIterator<Item = String>) {
    crate::titles::ensure(s, thread_ids);
}

/// Same as [`ensure_titles`] (the pages' name for it).
pub fn titles_ensure(s: &mut State, thread_ids: impl IntoIterator<Item = String>) {
    crate::titles::ensure(s, thread_ids);
}

/// The Notifications page counted unread items itself.
pub fn set_unread(s: &mut State, unread: usize) {
    s.workspace.unread = unread;
}
