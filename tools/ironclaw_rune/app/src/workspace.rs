//! Window root (t3code's layout): connects to the gateway; the sidebar lists
//! threads, or the settings pages while one is open; the panel shows the
//! open chat, the new-thread screen or a settings page under a breadcrumb.
//!
//! Its state is [`WorkspaceState`] in `State::workspace`. Its
//! timers (unread badge, status bar, inbox follow-up, the Working tick, the
//! snooze wake) are deadlines in [`Timers`], run from [`poll`] with
//! `Tx::wake_after`, instead of spawned loops. The chat and the pages call
//! [`open_thread`], [`show`], [`thread_updated`], [`run_ended`] and
//! [`open_run_logs`] to tell the workspace what happened.
//!
//! Files: `shell.rs` draws the window, `keys.rs` holds shortcuts and the
//! native menu, `status.rs` the bottom bar, `inbox*` the sidebar list,
//! `command.rs` ⌘K, `context.rs` the context meter, `draft.rs` the
//! new-thread screen, `message_search.rs` past-chat search and `quick.rs`
//! the ⇧⌘Space composer.

use std::time::{Duration, Instant};

use rune::{Cx, El, KeyEvent, Tx};
use zlo_client::inbox::InboxAction;
use zlo_client::usage::ThreadTelemetry;

use crate::alerts::Alerts;
use crate::jobs;
use crate::state::{Conn, Page, State};

mod command;
mod context;
mod draft;
pub mod fixtures;
pub(crate) mod inbox;
pub mod intro;
mod keys;
mod message_search;
#[cfg(test)]
mod motion_tests;
pub mod profile_icons;
pub mod quick;
mod shell;
mod sidebar_pane;
mod status;
pub(crate) mod thread_drag;
mod triage;
pub(crate) mod ui;

// Voice and SnapShot call these.
pub use draft::{
    composer_edit, send as send_draft, stage_snapshot, start_in_profile, voice_composer,
};
#[cfg(test)]
pub use inbox::RowId;
pub use inbox::title_of as row_title;
pub use inbox::{ProfileChange, profiles_changed};
pub use keys::{menu_items, on_close, quit};

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
/// How often terminal sessions are rescanned after the launch import.
const SESSION_RESCAN: Duration = Duration::from_secs(15 * 60);
/// The first look for updates after connecting.
const UPDATES_FIRST_POLL: Duration = Duration::from_secs(8);

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
    /// The background read of every computer's update status.
    pub updates: Option<Instant>,
}

pub struct WorkspaceState {
    /// The composers' shared-element flight id generation
    /// (`chat::composer::dock_matched`): bumped when one docked composer
    /// replaces another, so a thread switch never flies the composer onto
    /// itself; the new-thread composer and the dock keep one id and fly.
    pub dock_flight: u64,
    pub connect_attempt: usize,
    /// Bumped per connect: a stale connect answer or retry timer is dropped.
    pub connect_gen: jobs::Generation,
    pub inbox: inbox::Inbox,
    /// Profile folders' logos, per computer (`profile_icons`).
    pub profile_icons: profile_icons::ProfileIcons,
    /// Unread inbox notifications on the focused computer.
    pub focused_unread: usize,
    /// Total unread inbox notifications (focused + ready peers), for the
    /// Notifications badge. The notifications page writes the focused count
    /// through [`set_unread`].
    pub unread: usize,
    pub alerts: Alerts,
    pub titles: crate::titles::Titles,
    /// The right pane's Agents tab: the delegation family's tasks and the
    /// trail of agents opened from it. Kept across thread switches.
    pub agents: crate::chat::side::agents::AgentsNav,
    pub run_clock: crate::run_clock::RunClock,
    pub drafts: crate::drafts::Store,
    /// The new-thread screen, kept while its draft lives.
    pub draft: Option<draft::DraftState>,
    /// Bumped per new draft, so a late attach never lands in a newer one.
    pub draft_opens: jobs::Generation,
    /// The new-thread draft's triage state while it is a triage draft;
    /// kept across a switch to another computer (`triage::carry`).
    pub(crate) triage: Option<triage::TriageDraft>,
    /// An unsent new-thread draft carried across a computer focus switch
    /// ("Runs on" picker or "New threads start on" preference).
    pub(crate) carried_draft: Option<draft::runs_on::CarriedDraft>,
    /// Whether the "New threads start on" select menu on Settings → Computers
    /// is open.
    pub default_computer_menu: bool,
    /// The ⌘K menu while open.
    pub command: Option<command::CommandMenu>,
    pub command_opens: u64,
    /// The thread last started from the new-thread screen and its profile,
    /// so "the current profile" knows it before the inbox lists the thread.
    pub started: Option<(String, String)>,
    /// Message-text search shared by ⌘K and the inbox field.
    pub message_search: message_search::MessageSearch,
    pub quick: quick::Quick,
    /// The open thread's telemetry for the status bar (`None` until known,
    /// or when the gateway lacks the route).
    pub telemetry: Option<ThreadTelemetry>,
    /// The context meter's popover, report and banner state.
    pub context: context::ContextMeter,
    /// The sidebar pane: its width and whether it shows (the frame's
    /// `SplitView` drags and peeks it). Prefs mirror it (`persist_sidebar`).
    pub sidebar: rune_kit::shell::PaneState,
    /// A sidebar thread being dragged onto a chat or the draft.
    pub thread_drag: thread_drag::ThreadDrag,
    /// The window is the key window.
    pub window_active: bool,
    /// A text field held the caret when the window was last in front
    /// (noted by the view, `shell::note_field_focus`), so coming back
    /// leaves focus there instead of moving it to the composer.
    pub field_focused: std::cell::Cell<bool>,
    /// The sidebar was just toggled; the view that commits the slide
    /// queues its follow-ups (`settle_sidebar_slide`).
    pub sidebar_slid: std::cell::Cell<bool>,
    /// The window has drawn once (`shell::view`): later system appearance
    /// flips morph instead of landing.
    pub(crate) painted: std::cell::Cell<bool>,
    /// The thread whose delete is being confirmed.
    pub confirm_delete: Option<inbox::RowId>,
    /// The footer's update dialog is open, for these computers (the ones
    /// with an update waiting when it opened): only they are ever listed or
    /// updated from it.
    pub confirm_update: Option<Vec<String>>,
    /// The update button's "What's changed" card.
    pub update_preview: shell::update_button::UpdatePreview,
    pub timers: Timers,
    /// A scene shows the quick composer's card over the shell.
    pub quick_fixture: bool,
    /// Session import ran its launch pass.
    session_import_launched: bool,
}

impl Default for WorkspaceState {
    fn default() -> Self {
        Self {
            dock_flight: 0,
            connect_attempt: 0,
            connect_gen: Default::default(),
            inbox: inbox::Inbox::new(),
            profile_icons: profile_icons::ProfileIcons::default(),
            focused_unread: 0,
            unread: 0,
            alerts: Alerts::default(),
            titles: Default::default(),
            agents: Default::default(),
            run_clock: Default::default(),
            drafts: Default::default(),
            draft: None,
            draft_opens: Default::default(),
            triage: None,
            carried_draft: None,
            default_computer_menu: false,
            command: None,
            command_opens: 0,
            started: None,
            message_search: Default::default(),
            quick: Default::default(),
            telemetry: None,
            context: Default::default(),
            sidebar: rune_kit::shell::PaneState::new(crate::design::SIDEBAR_WIDTH),
            thread_drag: Default::default(),
            window_active: true,
            field_focused: Default::default(),
            sidebar_slid: Default::default(),
            painted: Default::default(),
            confirm_delete: None,
            confirm_update: None,
            update_preview: Default::default(),
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
    // Headless runs (scenes, scripts, traces) never touch the user's world:
    // no gateway connection (a scripted click on a fixture would otherwise
    // send real requests, e.g. create inbox rows for fixture threads), no
    // hotkeys, voice monitor, notifications or saved drafts. Scenes set the
    // state they render themselves.
    if rune::is_headless() {
        return;
    }
    crate::drafts::load(s);
    crate::alerts::install(s.rt.handle.clone());
    crate::snapshot::start(s);
    crate::voice::start(s);
    // A `gateway.json` computer not in use is moved into the computers
    // list on the side.
    crate::remote::connect::migrate_in_background(s);
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
/// The palette and the wallpaper's repaint are asked in one handler, so the
/// colours glide while the picture repaints for the new side.
pub fn toggle_scheme(s: &mut State, tx: &mut Tx) {
    crate::design::motion::color_morph(tx);
    let dark = crate::design::scheme_is_dark(&s.prefs, s.system_dark);
    s.update_prefs(|p| p.scheme = if dark { "light" } else { "dark" }.into());
    crate::design::look::reload(s);
}

use sidebar_pane::persist_sidebar;
pub use sidebar_pane::{set_sidebar_open, sidebar_pane};

/// Fold the sidebar away or bring it back. One spring owns the motion:
/// the sidebar, the panel, the transcript and the composer all move on it.
///
/// The transcript rewraps at its new width at once. Its rows would hold
/// their old heights for a moment and then ease to the new ones well after
/// the slide; instead the next commit lands them (an instant commit leaves
/// the running slide alone), and the one after has rows pinned to the end
/// ride the viewport's bottom edge as the composer rewraps (`follow_edge`),
/// so nothing moves vertically once the slide is over.
///
/// Those follow-ups wait for the frame that starts the slide
/// ([`settle_sidebar_slide`]). ⌘B reaches here as a menu item, which Rune
/// runs as a queued update; updates merge into the next frame's commit,
/// so follow-ups queued from here could land in the slide's own commit
/// and their `instant()` would cancel it.
pub fn toggle_sidebar(s: &mut State, tx: &mut rune::Tx) {
    tx.motion(crate::design::panels::REVEAL);
    tx.follow_edge();
    s.workspace.sidebar.toggle();
    persist_sidebar(s);
    s.workspace.sidebar_slid.set(true);
}

/// From the view that commits a sidebar slide: queue its follow-ups
/// ([`toggle_sidebar`]), now that the slide itself is committed.
pub(crate) fn settle_sidebar_slide(s: &State) {
    if !s.workspace.sidebar_slid.replace(false) {
        return;
    }
    s.rt.handle.update(|s, tx| {
        tx.instant();
        s.rt.handle.update(|_, tx| {
            tx.motion(crate::design::panels::REVEAL);
            tx.follow_edge();
        });
    });
}

fn connect(s: &mut State) {
    crate::remote::connect(s);
}

/// The computer answered (`remote::connect`): show its threads, reopening
/// `reopen` when it is still there.
pub(crate) fn connected(
    s: &mut State,
    session: zlo_client::wire::SessionInfo,
    list: zlo_client::inbox::InboxListResponse,
    reopen: Option<String>,
    tx: &mut Tx,
) {
    // The launch (or failure) screen giving way to the app is what the
    // person is waiting on: this one reply crossfades (`shell::view`). A
    // route refresh, already on the app, lands as replies do.
    if !matches!(s.conn, Conn::Ready) {
        tx.animated();
    }
    s.workspace.connect_attempt = 0;
    if !crate::remote::backend_is_remote(s) {
        // This Mac's backend answers: its last startup error is history.
        crate::service::clear_startup_failure();
    }
    s.session = Some(session);
    inbox::apply(s, list);
    s.conn = Conn::Ready;
    // First-run setup decides before the launch import.
    crate::onboarding::decide(s);
    start_polling(s);
    status::start(s);
    // Plugins' slots: read once per connection, again on lifecycle changes.
    crate::plugins::refresh_slots(s);
    // An explicit `(computer, thread)` intent (`peers::focus_thread`) takes
    // precedence over the draft default computer and `last_thread`, and opens
    // the thread even when it is absent from the inbox list.
    // Every landing ends it: a stale one (another computer, an older
    // attempt) is dropped here and can never fire later.
    if let Some(pending) = s
        .remote
        .pending_thread
        .take()
        .filter(|p| p.computer == s.onboarding.computer.key() && p.live(s))
    {
        s.workspace.carried_draft = None;
        open_thread(s, pending.thread_id);
        return;
    }
    // An unsent triage draft follows the computer it now talks to, and
    // stays on screen instead of the last thread opened there.
    let triage_draft = triage::connected(s);
    if triage_draft || draft::runs_on::connected(s) {
        return;
    }
    let reopen = reopen.filter(|id| s.workspace.inbox.row(id).is_some());
    match reopen.or_else(|| inbox::first_open(s)) {
        Some(first) => open_thread(s, first),
        // Nothing open: a fresh draft in the default profile.
        None => {
            draft::start_on_default_computer(s, zlo_client::profiles::DEFAULT_PROFILE_ID.into())
        }
    }
}

/// The app is leaving its computer: an unsent triage draft is kept for the
/// next one (`remote::connect::reset_for_switch`, before the draft goes).
pub(crate) fn carry_triage(s: &mut State) {
    triage::carry(s);
}

/// ⌘K "Triage an issue", and its reply landing (tests drive the switch).
#[cfg(test)]
pub(crate) mod triage_test {
    use super::*;

    /// The app connected again (or to a new computer).
    pub(crate) fn connected(s: &mut State) -> bool {
        triage::connected(s)
    }

    pub(crate) fn start(s: &mut State) {
        triage::start(s);
    }

    /// The prepare reply for the open draft, as asked of `computer` (the
    /// one in use when `None`).
    pub(crate) fn land(
        s: &mut State,
        computer: Option<&str>,
        result: Result<zlo_client::triage_calls::PrepareTriageResponse, zlo_client::ClientError>,
    ) {
        let Some(generation) = s.workspace.draft.as_ref().map(|d| d.generation) else {
            return;
        };
        let computer = computer.map_or_else(|| s.onboarding.computer.key(), str::to_string);
        triage::prepared(s, generation, &computer, result);
    }

    pub(crate) fn send_block(s: &State) -> Option<String> {
        triage::send_block(s)
    }

    pub(crate) fn is_ready(s: &State) -> bool {
        matches!(triage::link(s), Some(triage::Link::Ready { .. }))
    }
}

/// Connect again, to the computer setup switched to.
pub fn reconnect(s: &mut State) {
    s.workspace.connect_attempt = 0;
    // A switch needs a launch's grace: this Mac's gateway may be starting.
    s.launched = Instant::now();
    connect(s);
}

/// Setup is over: the launch-time session import it held back runs now
/// (the backend runs it only when due).
pub fn release_session_import(s: &mut State) {
    session_import_tick(s, zlo_client::session_import::ImportTrigger::Launch);
}

/// A coding-agent CLI turned up since launch (an install, the live
/// rescan): the backend imports its terminal sessions when they were never
/// read.
pub fn cli_found(s: &mut State) {
    session_import_tick(s, zlo_client::session_import::ImportTrigger::Launch);
}

/// Read the inbox again (after setup imported threads).
/// A prompt went out in `thread_id`: its sidebar card shows Working now.
pub fn prompt_sent(s: &mut State, thread_id: &str) {
    inbox::note_sent(s, thread_id);
}

/// The send to `thread_id` failed: its card stops showing Working.
pub fn send_failed(s: &mut State, thread_id: &str) {
    inbox::forget_sent(s, thread_id);
}

pub fn refresh_inbox(s: &mut State) {
    inbox::refresh(s);
    crate::remote::peers::refresh(s);
}

/// The connection screen's "Try again": also brings this Mac's gateway
/// back after a Stop from the Operator page, when ZLO runs it
/// (`remote::sync_local_backend`, called by every connect).
pub fn retry_connect(s: &mut State) {
    s.workspace.connect_attempt = 0;
    connect(s);
}

/// Show the problem, and retry on a schedule when the computer is simply
/// not reachable yet (this Mac's gateway starts after the app at login, or
/// is restarting; a remote may be waking up).
pub(crate) fn connect_failed(s: &mut State, problem: crate::remote::connect::Problem) {
    use crate::remote::connect::Problem;
    let generation = s.workspace.connect_gen;
    let (network, message) = match problem {
        Problem::Unreachable(m) => (true, m),
        Problem::Blocked(m) => (false, m),
    };
    let local = !crate::remote::backend_is_remote(s);
    // This app's backend already stopped with its own error: show that now
    // rather than waiting out the grace and then a vaguer symptom.
    let crashed = (network && local && crate::remote::runs_local_backend(s))
        .then(crate::service::startup_failure)
        .flatten();
    if network
        && local
        && crashed.is_none()
        && crate::remote::runs_local_backend(s)
        && s.launched.elapsed() < STARTUP_GRACE
    {
        // Still booting: stay on the loading screen and knock again soon.
        s.conn = Conn::Connecting;
        jobs::after(s, STARTUP_POLL, move |s, _| {
            if s.workspace.connect_gen == generation && matches!(s.conn, Conn::Connecting) {
                connect(s);
            }
        });
        return;
    }
    tracing::warn!(error = %message, attempt = s.workspace.connect_attempt, "gateway connection failed");
    let attempt = s.workspace.connect_attempt;
    let retry_in = network.then(|| CONNECT_RETRY[attempt.min(CONNECT_RETRY.len() - 1)]);
    s.conn = Conn::Failed {
        message: match crashed {
            Some(error) => format!(
                "ZLO's engine on this Mac stopped while starting:\n\n{error}\n\n\
                 ZLO keeps trying. Triage looks into it for you."
            ),
            None if network && local => {
                friendly_unreachable(&message, crate::remote::runs_local_backend(s))
            }
            None => message,
        },
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

/// The failure screen's "Triage": `zlo triage` in Terminal, by the full
/// path of this Mac's backend binary, so it works with the backend down
/// and without `zlo` on the PATH (triage reads no config and starts no
/// runtime). Runs with the app's data folder when the app was given one.
pub(crate) fn triage_in_terminal(s: &mut State) {
    let Some(backend) = crate::service::backend_binary() else {
        note_on_failure(
            s,
            "ZLO could not find its command-line tool. Reinstall ZLO, then try again.",
        );
        return;
    };
    // The data folder the app's backend runs on (also under the legacy
    // `IRONCLAW_REBORN_HOME` or the pre-ZLO folder), so Terminal's triage
    // reads that backend's logs whatever its own environment says.
    let home = zlo_client::reborn_home().ok();
    let command = triage_command(&backend, home.as_deref().map(std::path::Path::as_os_str));
    if s.fixture {
        return;
    }
    let shown = command.clone();
    jobs::blocking(
        s,
        move || crate::terminal::run(&command),
        move |s, result, _| {
            if let Err(e) = result {
                tracing::warn!(error = %e, "Terminal did not open for triage");
                note_on_failure(s, &terminal_failed_note(&shown));
            }
        },
    );
}

/// Terminal would not open: say exactly what to run, rather than "copy the
/// command" with no command on screen.
fn terminal_failed_note(command: &str) -> String {
    format!("Terminal did not open. In a terminal, run:\n{command}")
}

/// The shell line for [`triage_in_terminal`].
fn triage_command(backend: &std::path::Path, zlo_home: Option<&std::ffi::OsStr>) -> String {
    use crate::terminal::shell_quote;
    let run = format!("{} triage", shell_quote(&backend.display().to_string()));
    match zlo_home.filter(|h| !h.is_empty()) {
        Some(home) => format!(
            "ZLO_HOME={} {run}",
            shell_quote(&std::path::Path::new(home).display().to_string())
        ),
        None => run,
    }
}

/// Add `line` under the failure screen's message, once.
fn note_on_failure(s: &mut State, line: &str) {
    if let Conn::Failed { message, .. } = &mut s.conn
        && !message.contains(line)
    {
        message.push_str("\n\n");
        message.push_str(line);
    }
}

/// This Mac's gateway is not answering: say what to do.
fn friendly_unreachable(message: &str, runs_local: bool) -> String {
    if runs_local {
        format!(
            "{message}\n\nZLO starts its engine when the app opens; the first start can take \
             a minute. If this keeps happening, Triage looks into it for you."
        )
    } else {
        format!(
            "{message}\n\n\"Run IronClaw on this Mac\" is off in Settings → Computers, so ZLO does not start it. \
             Start `zlo serve` yourself, turn the switch on, or pick another computer."
        )
    }
}

/// Open a thread in the main column. Other areas call this.
pub fn open_thread(s: &mut State, thread_id: String) {
    // A newer navigation: a thread click still waiting on a switch no
    // longer decides what opens when that switch lands.
    s.remote.pending_thread = None;
    // A docked composer is in the tree (shown, or kept under a settings page).
    let docked = s.chat.is_some() && s.page != Page::Draft;
    s.page = Page::Chat;
    s.workspace.context.close();
    inbox::visit(s, &thread_id);
    if active_id(s) == Some(thread_id.as_str()) {
        crate::chat::focus_composer(s);
        return;
    }
    // One docked composer replaces another: a new flight id, so neither is
    // matched to the other.
    if docked {
        s.workspace.dock_flight += 1;
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
        // The chat scrolls to it once its timeline holds it.
        s.rt.handle
            .update(move |s, tx| crate::chat::reveal_message(s, message_id, tx));
    }
}

/// Re-read the open thread's telemetry now (the status bar polls it; the
/// thread details card's context row asks when it opens).
pub fn refresh_telemetry(s: &mut State) {
    status::refresh_telemetry(s);
}

/// 51200 -> "51.2k", as the status bar shows token counts.
pub fn short_tokens(n: u64) -> String {
    status::tokens(n)
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
        let catalog = s
            .workspace
            .inbox
            .list
            .rows
            .first()
            .map(|r| r.thread_id.clone());
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

/// Esc closes what the user last opened: a model picker, else
/// the settings pages. The command menu handles its own Esc.
fn dismiss(s: &mut State) -> bool {
    if s.workspace.command.is_some() {
        return false;
    }
    // Closing a chat's picker hands the keys back to that chat's composer.
    if crate::chat::model_picker::close_open(s) || inbox::picker::close(s) {
        return true;
    }
    if let Some(d) = s.workspace.draft.as_mut()
        && d.runs_on_open
    {
        d.runs_on_open = false;
        return true;
    }
    if s.workspace.default_computer_menu {
        s.workspace.default_computer_menu = false;
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
                (Page::Computers, &i::MONITOR, "Computers"),
                (Page::Appearance, &i::PALETTE, "Appearance"),
                (Page::Voice, &i::MIC, "Voice"),
                (Page::Notifications, &i::BELL, "Notifications"),
            ],
        ),
        (
            "Agent",
            vec![
                (Page::Providers, &i::BOT, "Providers"),
                (Page::Keys, &i::KEY_ROUND, "Keys"),
                (Page::Profiles, &i::USER, "Profiles"),
                (Page::Memory, &i::FILE_TEXT, "Memory"),
                (Page::Skills, &i::BOOK_OPEN, "Skills"),
                (Page::Extensions, &i::PUZZLE, "Plugins"),
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

/// A settings page's name for the breadcrumb, also when the sidebar hides
/// it (Projects behind its feature flag).
pub fn page_label(page: Page) -> &'static str {
    match page {
        Page::Projects => "Projects",
        _ => "Settings",
    }
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
/// Its worktree goes first when it holds no work (a worktree with changes
/// or commits stays, and the git bar of another thread never sees it).
pub fn delete_thread(s: &mut State, thread_id: String) {
    let id = thread_id.clone();
    jobs::run(
        s,
        "delete thread",
        move |c| async move {
            // Best effort: an older gateway, or a worktree that holds work,
            // never blocks the delete.
            if let Err(e) = c.clean_thread_worktree(&id).await {
                tracing::debug!(error = %e, "worktree not cleaned before delete");
            }
            c.delete_thread(&id).await
        },
        move |s, result, tx| {
            // Remove only after the gateway confirmed the delete.
            if result.is_err() {
                return;
            }
            // The person asked for it: the row fades and the rows under it
            // close the gap on `ROW_RESORT`, and a fallback thread opens with
            // the usual motion (a reply is a queued update, instant otherwise).
            tx.animated();
            s.workspace
                .inbox
                .list
                .rows
                .retain(|t| t.thread_id != thread_id);
            if active_id(s) == Some(thread_id.as_str()) {
                s.chat = None;
                match inbox::first_open(s) {
                    Some(next) => open_thread(s, next),
                    None => draft::open(s),
                }
            }
        },
    );
}

// ── what the chat reports ──────────────────────────────────────────────

/// The open chat changed its thread: the list and
/// the badge catch up.
pub fn thread_updated(s: &mut State) {
    inbox::refresh(s);
    refresh_unread(s);
    crate::remote::peers::refresh(s);
}

/// A run of `thread_id` ended; the chat calls this.
pub fn run_ended(s: &mut State, thread_id: String, failed: bool) {
    status::refresh_telemetry(s);
    crate::run_clock::end(s, &thread_id);
    inbox::forget_sent(s, &thread_id);
    if !failed {
        inbox::poll_for_title(s);
    }
    // Watched live: the result is seen, so no Done badge.
    if s.workspace.window_active && s.page == Page::Chat {
        inbox::mark_quietly(s, thread_id.clone(), InboxAction::Visit);
    } else {
        inbox::refresh(s);
    }
    crate::remote::peers::refresh(s);
    if !s.workspace.window_active {
        let title = crate::titles::get(s, &thread_id);
        let computer = s.onboarding.computer.key();
        let label = multiple_computers_connected(s).then(|| s.onboarding.computer.label());
        s.workspace.alerts.post_run(
            &computer,
            label.as_deref(),
            &thread_id,
            failed,
            title.as_deref(),
        );
    }
}

/// A chat asked for its run's logs: the Operator page.
pub fn open_run_logs(s: &mut State, run: String) {
    show(s, Page::Operator);
    crate::pages::operator::show_run_logs(s, run);
}

// ── polling ─────────────────────────────────────────────────────────────

fn start_polling(s: &mut State) {
    refresh_unread(s);
    let now = Instant::now();
    s.workspace.timers.unread = Some(now + UNREAD_POLL);
    // Soon after connecting, then once a minute: is an update waiting?
    s.workspace.timers.updates = Some(now + UPDATES_FIRST_POLL);
    if !s.workspace.session_import_launched {
        s.workspace.session_import_launched = true;
        // First-run setup holds this back and releases it when it ends.
        session_import_tick(s, zlo_client::session_import::ImportTrigger::Launch);
        s.workspace.timers.session_import = Some(now + SESSION_RESCAN);
    }
}

/// First-launch session import, then the periodic rescan request (the
/// backend skips a pass that is not due).
fn session_import_tick(s: &mut State, trigger: zlo_client::session_import::ImportTrigger) {
    // While first-run setup is open nothing is imported, launch or rescan:
    // its Projects step decides what comes in.
    if crate::onboarding::holds_launch_import(s) {
        return;
    }
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
    // A drag on the sidebar's edge (the frame's grip) lands in the pane.
    persist_sidebar(s);
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
    // The status refresh is skipped while the battery saver holds, so its
    // timer is off meanwhile. When the saver lets go (focus, or input after
    // an idle spell) the bar refreshes at once and the timer runs again.
    if crate::design::look::saving(s) {
        s.workspace.timers.status = None;
    } else if s.workspace.timers.status.is_none() {
        status::start(s);
    } else if due(s.workspace.timers.status) {
        s.workspace.timers.status = Some(now + STATUS_POLL);
        status::refresh_status(s);
    }
    if due(s.workspace.timers.updates) {
        let saving = crate::design::look::saving(s);
        let every = crate::remote::updates::POLL;
        s.workspace.timers.updates = Some(now + if saving { every * 6 } else { every });
        crate::remote::updates::poll(s);
    }
    if due(s.workspace.timers.session_import) {
        s.workspace.timers.session_import = Some(now + SESSION_RESCAN);
        session_import_tick(s, zlo_client::session_import::ImportTrigger::Background);
    }
    if due(s.workspace.timers.inbox) {
        s.workspace.timers.inbox = None;
        inbox::poll_due(s);
    }
    if due(s.workspace.timers.wake) {
        s.workspace.timers.wake = None;
        inbox::refresh(s);
        crate::remote::peers::refresh(s);
    }
    if due(s.workspace.timers.tick) {
        s.workspace.timers.tick = None;
        changed |= inbox::tick_due(s);
    }
    changed |= message_search::poll(s);
    let t = &s.workspace.timers;
    let next = [
        t.unread,
        t.status,
        t.inbox,
        t.wake,
        t.tick,
        t.session_import,
        t.updates,
    ]
    .into_iter()
    .flatten()
    .chain(s.workspace.message_search.due())
    .min();
    if let Some(next) = next {
        tx.wake_after(
            next.saturating_duration_since(now)
                .max(Duration::from_millis(5)),
        );
    }
    changed
}

/// Whether more than one computer is currently connected (the focused computer
/// plus at least one ready peer). Single-computer views omit computer labels.
pub(crate) fn multiple_computers_connected(s: &State) -> bool {
    let focused_ready = matches!(s.conn, Conn::Ready);
    let peer_ready = crate::remote::peers::list(s)
        .into_iter()
        .filter(|p| p.conn == crate::remote::peers::PeerConn::Ready)
        .count();
    (usize::from(focused_ready) + peer_ready) > 1
}

/// Clear focused-computer workspace state that must not carry across a
/// computer switch (called from `remote::connect::reset_for_switch`).
pub(crate) fn reset_for_switch(s: &mut State) {
    s.workspace.message_search.reset_for_switch();
    s.workspace.inbox.sent.clear();
    s.workspace.thread_drag.reset_for_switch();
}

/// Recompute the bell badge count from the focused computer plus every ready
/// peer's server-reported unread total.
pub(crate) fn sync_unread(s: &mut State) {
    let peer_unread: usize = crate::remote::peers::list(s)
        .into_iter()
        .filter(|p| p.conn == crate::remote::peers::PeerConn::Ready)
        .map(|p| p.unread_count)
        .sum();
    s.workspace.unread = s.workspace.focused_unread + peer_unread;
}

/// A ready peer's unread notifications page landed (`remote::peers_io`):
/// prime or post system notifications for new items and update the bell count.
pub(crate) fn peer_notifications_landed(s: &mut State, key: &str) {
    crate::pages::notifications::peer_list_landed(s, key);
    let Some(peer) = crate::remote::peers::get(s, key) else {
        sync_unread(s);
        return;
    };
    let label = peer.computer.label();
    let notifications = peer.notifications.clone();
    let titles: std::collections::HashMap<String, String> = peer
        .inbox
        .as_ref()
        .map(|inbox| {
            inbox
                .rows
                .iter()
                .filter_map(|r| {
                    r.title
                        .as_deref()
                        .filter(|t| !t.trim().is_empty())
                        .map(|t| (r.thread_id.clone(), t.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();
    let multi = multiple_computers_connected(s);
    let fresh: Vec<_> = s
        .workspace
        .alerts
        .take_new(key, &notifications)
        .into_iter()
        .cloned()
        .collect();
    for item in fresh {
        let thread_title = crate::alerts::thread_of(&item)
            .and_then(|tid| titles.get(tid))
            .map(String::as_str);
        s.workspace
            .alerts
            .post(key, multi.then_some(label.as_str()), &item, thread_title);
    }
    sync_unread(s);
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
            let computer = s.onboarding.computer.key();
            let computer_label =
                multiple_computers_connected(s).then(|| s.onboarding.computer.label());
            let fresh: Vec<_> = s
                .workspace
                .alerts
                .take_new(&computer, &page.notifications)
                .into_iter()
                .cloned()
                .collect();
            for item in fresh {
                let thread = crate::alerts::thread_of(&item).map(str::to_string);
                if thread.is_some() && thread == looking_at {
                    continue;
                }
                let computer = computer.clone();
                let computer_label = computer_label.clone();
                // Automation runs are not in the thread list; fetch their
                // title first so the alert can name them.
                let Some(thread) = thread else {
                    s.workspace
                        .alerts
                        .post(&computer, computer_label.as_deref(), &item, None);
                    continue;
                };
                crate::titles::resolve(s, thread, move |s, title, _| {
                    s.workspace.alerts.post(
                        &computer,
                        computer_label.as_deref(),
                        &item,
                        Some(&title),
                    );
                });
            }
            let changed = page.unread_count != s.workspace.focused_unread;
            s.workspace.focused_unread = page.unread_count;
            sync_unread(s);
            if changed {
                crate::pages::notifications::refresh(s);
            }
        },
    );
}

/// A system notification was clicked: bring the window up on its thread and
/// computer.
pub fn alert_clicked(s: &mut State, tag: &str) {
    let Some(target) = s.workspace.alerts.target_for(tag) else {
        return;
    };
    let focused_key = s.onboarding.computer.key();
    if let Some(thread) = target.thread_id {
        crate::remote::peers::focus_thread(s, &target.computer, thread);
    } else if target.computer != focused_key {
        crate::remote::peers::focus(s, &target.computer);
    }
    let Some(id) = target.notification_id else {
        return;
    };
    if s.onboarding.computer.key() == target.computer && matches!(s.conn, Conn::Ready) {
        jobs::run(
            s,
            "notifications",
            move |c| async move { c.mark_notification_read(&id).await },
            // silent-ok: read state is cosmetic; the next poll recounts.
            |s, _, _| refresh_unread(s),
        );
    } else {
        let key = target.computer.clone();
        crate::remote::peers::run(
            s,
            &key,
            "notifications",
            move |c| async move { c.mark_notification_read(&id).await },
            |s, _, _| crate::remote::peers::refresh(s),
        );
    }
}

/// `App::on_focus`: the window became key or lost it.
pub fn window_focus(s: &mut State, focused: bool, tx: &mut Tx) {
    s.workspace.window_active = focused;
    // Timers that rest while the window is in the back (the status bar's,
    // a live chat's seconds) start again from the poll: ask for one now.
    if focused {
        tx.wake_after(Duration::ZERO);
        crate::remote::peers::wake_asleep(s);
        crate::remote::peers::refresh(s);
        crate::remote::config_sync::window_active(s);
    }
    if !matches!(s.conn, Conn::Ready) {
        return;
    }
    crate::onboarding::window_focus(s, focused);
    // Back in front: catch up on whatever finished meanwhile.
    if focused {
        inbox::refresh(s);
        restore_composer(s);
    }
}

/// Keep the composer ready to type: on a chat or
/// the draft, focus it when nothing else holds focus. Another field (the
/// URL bar, a search box, the command menu, a side chat) keeps its focus.
fn restore_composer(s: &mut State) {
    if s.workspace.command.is_some() || s.workspace.field_focused.get() {
        return;
    }
    match s.page {
        Page::Chat => crate::chat::focus_composer(s),
        Page::Draft => {
            if let Some(d) = s.workspace.draft.as_mut() {
                d.input.focused = true;
            }
        }
        _ => {}
    }
}

pub fn on_key(s: &mut State, ev: &KeyEvent, tx: &mut Tx) -> bool {
    keys::on_key(s, ev, tx)
}

pub fn view(s: &State, cx: &mut Cx<State>) -> El {
    shell::view(s, cx)
}

// Thread titles, window focus and the unread badge, as the chat, side pane
// and pages call them.

/// Whether the main window is in front.
pub fn window_active(s: &State) -> bool {
    s.workspace.window_active
}

/// A thread's title if known (the inbox, or fetched by [`titles_ensure`]).
pub fn title_of(s: &State, thread_id: &str) -> Option<String> {
    crate::titles::get(s, thread_id)
}

/// Fetch the titles of these threads if not known yet.
pub fn titles_ensure(s: &mut State, thread_ids: impl IntoIterator<Item = String>) {
    crate::titles::ensure(s, thread_ids);
}

/// The Notifications page counted unread items on the focused computer itself.
pub fn set_unread(s: &mut State, unread: usize) {
    s.workspace.focused_unread = unread;
    sync_unread(s);
}

/// Whether a handler asked for motion (`Tx::animated`). Rune keeps the flag
/// private; its `Debug` shows it. A headless `Runtime` animates every
/// commit, so a test that wants to know whether a queued update (a reply, a
/// menu item) will move in a real window runs the handler on a fresh `Tx`
/// and reads this.
#[cfg(test)]
pub(crate) fn asked_motion(tx: &Tx) -> bool {
    format!("{tx:?}").contains("asked_motion: true")
}

/// Whether a handler set a colour spring for its commit (`Tx::color_motion`).
#[cfg(test)]
pub(crate) fn asked_color_motion(tx: &Tx) -> bool {
    !format!("{tx:?}").contains("color_motion: None")
}

/// Whether a handler or poll requested a timed wake (`Tx::wake_after`).
#[cfg(test)]
pub(crate) fn asked_wake(tx: &Tx) -> bool {
    !format!("{tx:?}").contains("wakes: []")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The seam the motion tests read: a fresh `Tx` asks nothing, and
    /// `animated` is what sets the flag (`motion` alone does not).
    #[test]
    fn triage_runs_the_bundled_cli_by_full_path() {
        let backend =
            std::path::Path::new("/Users/a b/Applications/ZLO.app/Contents/MacOS/zlo-backend");
        assert_eq!(
            super::triage_command(backend, None),
            "'/Users/a b/Applications/ZLO.app/Contents/MacOS/zlo-backend' triage"
        );
        assert_eq!(
            super::triage_command(backend, Some(std::ffi::OsStr::new("/tmp/zh"))),
            "ZLO_HOME=/tmp/zh '/Users/a b/Applications/ZLO.app/Contents/MacOS/zlo-backend' triage"
        );
        assert!(
            !super::triage_command(backend, Some(std::ffi::OsStr::new(""))).contains("ZLO_HOME")
        );
        // No Terminal: the screen shows the command itself.
        let line = super::triage_command(backend, None);
        assert!(super::terminal_failed_note(&line).ends_with(&line));
    }

    #[test]
    fn asked_motion_reads_the_flag() {
        let mut tx = Tx::default();
        assert!(!asked_motion(&tx) && !asked_color_motion(&tx));
        tx.motion(crate::design::motion::DOCK);
        assert!(!asked_motion(&tx), "a spring alone leaves it unprompted");
        tx.animated();
        assert!(asked_motion(&tx));
        crate::design::motion::color_morph(&mut tx);
        assert!(asked_color_motion(&tx));
    }

    #[test]
    fn an_unreachable_mac_says_what_to_do() {
        // ZLO runs this Mac's engine itself: no terminal steps (a
        // `zlo service install` would even be retired by the app on start).
        let own = friendly_unreachable("down", true);
        assert!(own.contains("Triage"), "{own}");
        assert!(!own.contains("zlo "), "{own}");
        assert!(friendly_unreachable("down", false).contains("Run IronClaw on this Mac"));
    }

    /// A thread switch gives the new dock its own flight id, so the
    /// composer never flies onto itself; the new-thread composer and the
    /// dock it hands off to share one, so the send still flies.
    #[test]
    fn only_the_hand_off_flies_the_composer() {
        let mut s = crate::chat::fixtures::test_state();
        s.fixture = true;
        s.conn = Conn::Ready;
        s.page = Page::Draft;
        open_thread(&mut s, "a".into());
        assert_eq!(s.workspace.dock_flight, 0, "new thread → dock flies");
        open_thread(&mut s, "b".into());
        assert_eq!(s.workspace.dock_flight, 1, "dock → dock: a new id");
        s.page = Page::Settings;
        open_thread(&mut s, "a".into());
        assert_eq!(
            s.workspace.dock_flight, 2,
            "the kept dock under settings too"
        );
    }

    /// While the battery saver holds, a status refresh is skipped: the
    /// timer must not keep waking the app only to skip it.
    #[test]
    fn the_status_timer_is_off_while_the_battery_saver_holds() {
        let mut s = crate::chat::fixtures::test_state();
        s.fixture = false;
        s.conn = Conn::Ready;
        status::start(&mut s);
        let mut tx = Tx::default();
        poll(&mut s, &mut tx);
        assert!(s.workspace.timers.status.is_some(), "in use: it polls");

        s.look.battery.set_active(false);
        poll(&mut s, &mut tx);
        assert!(s.workspace.timers.status.is_none(), "saving: no timer");

        // The saver lets go (the window is back in front, or input ended
        // an idle spell): the bar refreshes and the timer runs again.
        s.look.battery.set_active(true);
        let before = Instant::now();
        poll(&mut s, &mut tx);
        let next = s.workspace.timers.status.expect("the timer runs again");
        assert!(next >= before + STATUS_POLL);
    }
}
