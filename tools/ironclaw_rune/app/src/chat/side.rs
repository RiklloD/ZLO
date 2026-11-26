//! The chat's two docks, Zeron's arrangement. The work
//! panel (Changes, Files, Outputs) sits beside the conversation; the right
//! pane (Activity, Browser, Agents, Routines, History, Console and the open
//! side chat) sits at the window's edge. Each is a resizable column whose
//! tabs ride the titlebar above it; the last tab picked in each comes back
//! on the next thread. The session card (`session.rs`) is the third
//! titlebar toggle.
//!
//! Assistant tabs read the gateway (Activity, Outputs, Agents, Routines);
//! Browser shows a web page natively (`native_view`). Coding tabs (Files,
//! Changes, History, Console) read a project folder on this Mac with the
//! system `git`, picked once per thread. With the backend on another
//! computer (`crate::remote`), Files reads that thread's workspace through
//! the gateway and the git and shell tabs say where they run instead.
//!
//! # Entry points
//!
//! For the chat (layout and events):
//! - `side::init(s)` once after `chat::open` built the `ChatState`: restores the thread's folder, worktree, side chat and loads
//!   the shown tabs.
//! - `side::docks(s, cx, k, layer) -> Vec<El>`: work panel then right pane, to
//!   put after the conversation column in the chat's row. Or `side::work_view` / `side::view` one by one.
//! - `side::on_turn_started(s)` before a turn is sent, `side::on_turn_ended(s)`
//!   when a run ends.
//! - `side::open_link(s, href, external, tx)` for links clicked in a reply,
//!   also from an embedded side chat.
//! - `side::adopt_folder(s, path)`: a mention root picked for a thread without
//!   a project folder.
//! - `side::changes_selection(s)` for the composer's `@` selection mention.
//! - `side::new_side_chat_allowed(s)`: false inside an embedded side chat.
//!
//! For the workspace:
//! - `side::titlebar_actions(s, cx, k, layer) -> El`: the thread's end of the
//!   window titlebar.
//! - `side::toggle_right_pane(s)` (⌥⌘B), `side::toggle_work_panel(s)` (⌘J),
//!   `side::new_side_chat(s)` (⌥⌘N). Off the chat page the toggles only flip
//!   the preference. Call `side::dock_slide(s, tx)` first so the docks
//!   slide on `panels::REVEAL`.
//! - `side::dock_widths(s, viewport, chat)` for the wallpaper's conversation area.
//! - `side::expanded(s)`: the dock expanded over the conversation, if any.
//! - `main.rs`: `native_view::poll` in `poll`, `native_view::on_window` on
//!   the `App`; the root view calls `native_view::cover` on every run (see
//!   `native_view.rs` for when the previews are reconciled).

use std::path::{Path, PathBuf};
use std::time::Instant;

use rune::{Cx, El, FileDialogOptions, Tx, el};
use rune_kit::Kit;
use zlo_client::orchestration::WorktreeView;

use crate::jobs::{self, Generation};
use crate::state::{Page, State};

mod activity;
pub(crate) mod agents;
mod bridge;
mod browser;
mod busy;
mod changes;
mod console;
mod details;
mod dock;
mod drill;
mod flash;
pub mod git;
mod history;
mod machines;
mod outputs;
mod preview;
mod project;
mod remote_repo;
mod routines;
mod session;
mod ui;

pub mod fixtures;

// Entry points for areas A and B (see the module docs).
pub use changes::on_cli_report;
pub(crate) use details::{
    ITEMS as DETAILS_ITEMS, left_closes_menu as details_left_closes_menu,
    set_item as set_details_item, visibility as details_visibility,
};
pub use dock::{docks, expanded, titlebar_actions};
pub use session::new_side_chat;

pub(crate) const WIDTH_MIN: f32 = 260.;
pub(crate) const WIDTH_MAX: f32 = 900.;
pub(crate) const WIDTH_DEFAULT: f32 = 380.;
/// Room the conversation and the sidebar keep when the docks are sized.
pub(crate) const KEEP: f32 = 360. + 240.;
/// Native views wait this long after their dock opens, so they don't float
/// over a column that is still sliding in.
const NATIVE_SETTLE: std::time::Duration = std::time::Duration::from_millis(380);

/// The two columns beside the conversation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dock {
    /// Changes, Files, Outputs: what the agent touched.
    Work,
    /// The right pane at the window's edge.
    Side,
}

impl Dock {
    fn ix(self) -> usize {
        match self {
            Dock::Work => 0,
            Dock::Side => 1,
        }
    }

    fn key(self) -> &'static str {
        match self {
            Dock::Work => "work",
            Dock::Side => "side",
        }
    }

    fn tabs(self) -> &'static [Tab] {
        match self {
            Dock::Work => &Tab::WORK,
            Dock::Side => &Tab::SIDE,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Activity,
    Outputs,
    Browser,
    Agents,
    Routines,
    Files,
    Changes,
    History,
    Console,
    /// The computers ZLO works on and the threads running on them.
    Machines,
    /// The side chat open on this thread (only listed while one is).
    SideChat,
    /// A plugin's `panel.tab` slot; which one is in `prefs.side_tab`
    /// (`plugin:{extension}/{slot}`, see [`plugin_tab`]).
    Plugin,
}

impl Tab {
    /// The project tools beside the conversation.
    const WORK: [Tab; 5] = [
        Tab::Changes,
        Tab::Files,
        Tab::Outputs,
        Tab::History,
        Tab::Console,
    ];
    /// The right pane's tools, after the side chat when one is open.
    const SIDE_TOOLS: [Tab; 5] = [
        Tab::Activity,
        Tab::Browser,
        Tab::Agents,
        Tab::Routines,
        Tab::Machines,
    ];
    const SIDE: [Tab; 6] = [
        Tab::SideChat,
        Tab::Activity,
        Tab::Browser,
        Tab::Agents,
        Tab::Routines,
        Tab::Machines,
    ];

    fn id(self) -> &'static str {
        match self {
            Tab::Activity => "activity",
            Tab::Outputs => "outputs",
            Tab::Browser => "browser",
            Tab::Agents => "agents",
            Tab::Routines => "routines",
            Tab::Files => "files",
            Tab::Changes => "changes",
            Tab::History => "history",
            Tab::Console => "console",
            Tab::Machines => "machines",
            Tab::SideChat => "side-chat",
            Tab::Plugin => "plugin",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Tab::Activity => "Activity",
            Tab::Outputs => "Outputs",
            Tab::Browser => "Browser",
            Tab::Agents => "Agents",
            Tab::Routines => "Routines",
            Tab::Files => "Files",
            Tab::Changes => "Changes",
            Tab::History => "History",
            Tab::Console => "Console",
            Tab::Machines => "Machines",
            Tab::SideChat => "Side chat",
            Tab::Plugin => "Plugin",
        }
    }

    fn icon(self) -> &'static rune::Icon {
        use crate::icons;
        match self {
            Tab::Activity => &icons::ACTIVITY,
            Tab::Outputs => &icons::FILE_TEXT,
            Tab::Browser => &icons::GLOBE,
            Tab::Agents => &icons::BOT,
            Tab::Routines => &icons::CALENDAR,
            Tab::Files => &icons::FOLDER_TREE,
            Tab::Changes => &icons::FILE_DIFF,
            Tab::History => &icons::GIT_COMMIT_HORIZONTAL,
            Tab::Console => &icons::SQUARE_TERMINAL,
            Tab::Machines => &icons::MONITOR,
            Tab::SideChat => &icons::MESSAGES_SQUARE,
            Tab::Plugin => &icons::PUZZLE,
        }
    }

    fn dock(self) -> Dock {
        match self {
            Tab::Changes | Tab::Files | Tab::Outputs | Tab::History | Tab::Console => Dock::Work,
            _ => Dock::Side,
        }
    }

    /// The saved tab of `dock`, or its first one.
    fn from_id(id: &str, dock: Dock) -> Tab {
        dock.tabs()
            .iter()
            .copied()
            .find(|t| t.id() == id)
            .unwrap_or(match dock {
                Dock::Work => Tab::Changes,
                Dock::Side => Tab::Activity,
            })
    }

    /// Tabs that run git or a shell on the project folder, which only this
    /// Mac's backend shares: off while the backend is remote.
    fn local_only(self) -> bool {
        matches!(self, Tab::Changes | Tab::History | Tab::Console)
    }

    /// Tabs that read the project folder.
    fn coding(self) -> bool {
        matches!(
            self,
            Tab::Files | Tab::Changes | Tab::History | Tab::Console
        )
    }
}

/// The side chat shown in the right pane: an embedded chat (a
/// [`crate::chat::ChatState`], built by `chat::build_side_chat`) with its
/// own stream, drawn with `chat::embedded_view`. Dropping it closes the
/// stream.
pub struct SideChat {
    pub thread_id: String,
    pub state: Box<crate::chat::ChatState>,
}

impl SideChat {
    /// A side chat for `thread_id`; call [`crate::chat::start_side_chat`]
    /// once it sits in its slot.
    pub(crate) fn new(s: &State, thread_id: String) -> Self {
        let state = Box::new(crate::chat::build_side_chat(s, thread_id.clone()));
        Self { thread_id, state }
    }
}

/// The open chat's side chat when it shows `thread_id` (`chat::chat_for`
/// finds embedded chats through this).
pub fn side_chat_mut<'a>(
    s: &'a mut State,
    thread_id: &str,
) -> Option<&'a mut crate::chat::ChatState> {
    s.chat
        .as_mut()?
        .side
        .chat
        .as_mut()
        .filter(|c| c.thread_id == thread_id)
        .map(|c| &mut *c.state)
}

/// Read-only [`side_chat_mut`].
pub fn side_chat<'a>(s: &'a State, thread_id: &str) -> Option<&'a crate::chat::ChatState> {
    s.chat
        .as_ref()?
        .side
        .chat
        .as_ref()
        .filter(|c| c.thread_id == thread_id)
        .map(|c| &*c.state)
}

/// The thread ids of the open side chats (at most one).
pub fn side_chat_ids(s: &State) -> Vec<String> {
    s.chat
        .as_ref()
        .and_then(|c| c.side.chat.as_ref())
        .map(|c| c.thread_id.clone())
        .into_iter()
        .collect()
}

/// Per-thread dock state, in `ChatState::side`. Open, width and tab live in
/// `Prefs` (`side_open`, `side_width`, `side_tab`, `work_*`).
#[derive(Default)]
pub struct SideState {
    /// The project folder, and whether git knows it (its top folder then).
    pub folder: Option<PathBuf>,
    /// `folder` came from the thread's profile ([`adopt_profile_folder`]),
    /// so a profile edit moves it; a picked folder or a worktree does not.
    from_profile: bool,
    pub repo: Option<PathBuf>,
    /// The thread's own git worktree, when it has one: the coding tabs then
    /// read that folder, and the title shows its branch.
    pub worktree: Option<WorktreeView>,
    /// Bumped when the folder changes, so git work for the old one is dropped.
    folder_gen: Generation,
    /// The dock whose inner edge is dragged, and its live width.
    drag: Option<(Dock, f32)>,
    /// Each dock expanded over the conversation (`dock::expanded`), by
    /// [`Dock::ix`]. Expanding one clears the other.
    wide: [bool; 2],
    /// When each dock last opened, slid or resized (not by a drag), for
    /// native views to wait out the motion ([`NATIVE_SETTLE`]).
    opened_at: [Option<Instant>; 2],
    /// Each dock's column is still drawn: open, or closed less than
    /// [`dock::close_hold`] ago while its clip slides shut ([`poll`]).
    shown: [bool; 2],
    /// When a closed dock's column may go (app clock, seconds).
    hold: [Option<f64>; 2],
    /// The thread details card is open, and its own state.
    session_open: bool,
    details: details::DetailsState,
    /// A side chat is being forked.
    chat_busy: bool,
    /// The side chat shown in the right pane.
    pub chat: Option<SideChat>,
    activity: activity::ActivityState,
    outputs: outputs::OutputsState,
    browser: browser::BrowserState,
    routines: routines::RoutinesState,
    project: project::ProjectState,
    changes: changes::ChangesState,
    history: history::HistoryState,
    console: console::ConsoleState,
    /// What the backend's git and console routes do for this thread, when
    /// the backend is on another computer.
    remote_repo: remote_repo::RemoteRepo,
    /// The one short confirmation showing in the panes ([`flash`]).
    flash: flash::Flash,
}

/// The open chat's side state.
fn side(s: &State) -> Option<&SideState> {
    s.chat.as_ref().map(|c| &c.side)
}

fn side_mut(s: &mut State) -> Option<&mut SideState> {
    s.chat.as_mut().map(|c| &mut c.side)
}

/// For accessors Rune wants total (`TextInput::on_edit`, `DiffView`): the
/// open chat's side state, or a throwaway one when no chat is open (a
/// handler can only fire while one is, so this branch is never taken in
/// practice; the leak is one empty struct).
fn side_total(s: &mut State) -> &mut SideState {
    match s.chat.as_mut() {
        Some(c) => &mut c.side,
        None => Box::leak(Box::default()),
    }
}

/// The open chat's thread id.
fn thread_id(s: &State) -> Option<String> {
    s.chat.as_ref().map(|c| c.thread_id.clone())
}

/// The side state, if `thread` is still the open chat. Async replies use
/// this so a late answer never lands on another thread.
fn side_of<'a>(s: &'a mut State, thread: &str) -> Option<&'a mut SideState> {
    s.chat
        .as_mut()
        .filter(|c| c.thread_id == thread)
        .map(|c| &mut c.side)
}

pub fn dock_open(s: &State, dock: Dock) -> bool {
    match dock {
        Dock::Work => s.prefs.work_open,
        Dock::Side => s.prefs.side_open,
    }
}

/// The right pane is open on its side chat (voice asks whether the side
/// chat's composer is on screen). Read-only.
pub fn side_chat_shown(s: &State) -> bool {
    dock_open(s, Dock::Side) && dock_tab(s, Dock::Side) == Tab::SideChat
}

/// The tab `dock` shows. Side chat falls back to Activity once closed, a
/// plugin's tab once its slot is gone.
fn dock_tab(s: &State, dock: Dock) -> Tab {
    if dock == Dock::Side && s.prefs.side_tab.starts_with(PLUGIN_TAB) {
        return if plugin_tab(s).is_some() {
            Tab::Plugin
        } else {
            Tab::Activity
        };
    }
    let tab = match dock {
        Dock::Work => Tab::from_id(&s.prefs.work_tab, dock),
        Dock::Side => Tab::from_id(&s.prefs.side_tab, dock),
    };
    match tab {
        Tab::SideChat if side(s).is_none_or(|x| x.chat.is_none()) => Tab::Activity,
        tab => tab,
    }
}

/// `prefs.side_tab`'s prefix for a plugin's tab.
const PLUGIN_TAB: &str = "plugin:";

fn plugin_tab_id(slot: &zlo_client::plugins::Slot) -> String {
    format!("{PLUGIN_TAB}{}/{}", slot.extension_id, slot.slot_id)
}

/// The plugin slot the right pane's saved tab names, while it exists.
pub fn plugin_tab(s: &State) -> Option<&zlo_client::plugins::Slot> {
    let (ext, slot) = s.prefs.side_tab.strip_prefix(PLUGIN_TAB)?.split_once('/')?;
    s.plugins
        .find(ext, slot)
        .filter(|x| x.slot == zlo_client::plugins::SlotKind::PanelTab)
}

/// The open thread's mount of the shown plugin tab.
fn plugin_mount(s: &State) -> Option<crate::plugins::MountKey> {
    let slot = plugin_tab(s)?;
    Some(crate::plugins::MountKey::new(slot, thread_id(s)))
}

/// Open the right pane on a plugin's tab and render it for this thread.
pub fn show_plugin(s: &mut State, extension_id: &str, slot_id: &str) {
    let Some(slot) = s.plugins.find(extension_id, slot_id).cloned() else {
        return;
    };
    let was_open = dock_open(s, Dock::Side);
    let id = plugin_tab_id(&slot);
    s.update_prefs(|p| {
        p.side_open = true;
        p.side_tab = id.clone();
    });
    if !was_open && let Some(x) = side_mut(s) {
        x.opened_at[Dock::Side.ix()] = Some(Instant::now());
    }
    load(s, Tab::Plugin);
}

/// The slot list changed (connect, a plugin installed or removed): render
/// the plugin tab again if one is shown.
pub fn plugin_slots_changed(s: &mut State) {
    if s.page == Page::Chat && s.chat.is_some() && tab_shown(s, Tab::Plugin) {
        load(s, Tab::Plugin);
    }
}

/// `tab` is on screen (its dock open on it).
fn tab_shown(s: &State, tab: Tab) -> bool {
    dock_open(s, tab.dock()) && dock_tab(s, tab.dock()) == tab
}

/// Restore this thread's project folder and load the shown tabs. Call once
/// after `chat::open`.
pub fn init(s: &mut State) {
    let Some(thread) = thread_id(s) else { return };
    if let Some(folder) = s.prefs.project_folders.get(&thread).cloned() {
        set_folder(s, folder, false);
    }
    adopt_profile_folder(s, &thread);
    worktree_load(s);
    session::restore(s);
    load_shown(s);
}

fn load_shown(s: &mut State) {
    for dock in [Dock::Work, Dock::Side] {
        if dock_open(s, dock) {
            load(s, dock_tab(s, dock));
        }
    }
}

/// ⌥⌘B. Off the chat page it only flips the preference.
pub fn toggle_right_pane(s: &mut State) {
    toggle_dock(s, Dock::Side);
}

/// ⌘J (or ⌘E). Off the chat page it only flips the preference.
pub fn toggle_work_panel(s: &mut State) {
    toggle_dock(s, Dock::Work);
}

fn toggle_dock(s: &mut State, dock: Dock) {
    let open = !dock_open(s, dock);
    s.update_prefs(|p| match dock {
        Dock::Work => p.work_open = open,
        Dock::Side => p.side_open = open,
    });
    if s.page == Page::Chat && s.chat.is_some() {
        // The other dock moves (the work panel rides the right pane's
        // edge) or resizes with this one: its native view waits too.
        mark_opened(s, Dock::Work);
        mark_opened(s, Dock::Side);
        if open {
            load(s, dock_tab(s, dock));
        }
    }
}

/// Ready `tx` for a commit that opens, closes or resizes a dock, as the
/// sidebar's fold does (`workspace::toggle_sidebar`): every column it moves
/// rides [`panels::REVEAL`](crate::design::panels::REVEAL), rows pinned to
/// the transcript's end ride its edge, and the follow-ups land the
/// conversation's rewrapped rows once the slide has started
/// (`workspace::settle_sidebar_slide`). Callers outside the chat's own
/// buttons (⌥⌘B, ⌘J) call it before [`toggle_right_pane`] /
/// [`toggle_work_panel`].
pub fn dock_slide(s: &State, tx: &mut Tx) {
    tx.animated();
    tx.motion(crate::design::panels::REVEAL);
    tx.follow_edge();
    s.workspace.sidebar_slid.set(true);
}

/// Keeps a closed dock's column drawn until its clip has slid shut, so the
/// body and its titlebar tabs leave with the slide instead of fading out
/// ahead of it. Runs from the app's `poll`, whatever closed the dock.
pub fn poll(s: &mut State, clock: f64, tx: &mut Tx) -> bool {
    // The Browser's page changed (its observer woke the app).
    let page = browser::poll(s, tx);
    // The Agents tab keeps itself fresh while shown, however it came up
    // (a tab click, a thread switch, the pane reopened).
    agents::sync_trail(s);
    agents::ensure_poll(s);
    let open = [dock_open(s, Dock::Work), dock_open(s, Dock::Side)];
    let Some(x) = side_mut(s) else { return false };
    let mut changed = page;
    for (ix, open) in open.into_iter().enumerate() {
        if open {
            x.shown[ix] = true;
            x.hold[ix] = None;
        } else if x.shown[ix] {
            match x.hold[ix] {
                None => {
                    let hold = dock::close_hold();
                    x.hold[ix] = Some(clock + hold);
                    tx.wake_after(std::time::Duration::from_secs_f64(hold));
                }
                Some(until) if clock >= until => {
                    x.shown[ix] = false;
                    x.hold[ix] = None;
                    changed = true;
                }
                Some(_) => {}
            }
        }
    }
    changed
}

/// `dock`'s column is drawn: open, or still sliding shut.
fn dock_drawn(s: &State, dock: Dock) -> bool {
    dock_open(s, dock) || side(s).is_some_and(|x| x.shown[dock.ix()])
}

fn mark_opened(s: &mut State, dock: Dock) {
    if let Some(x) = side_mut(s) {
        x.opened_at[dock.ix()] = Some(Instant::now());
    }
}

/// Open `tab`'s dock on it.
pub fn show(s: &mut State, tab: Tab) {
    let was_open = dock_open(s, tab.dock());
    s.update_prefs(|p| match tab.dock() {
        Dock::Work => {
            p.work_open = true;
            p.work_tab = tab.id().into();
        }
        Dock::Side => {
            p.side_open = true;
            p.side_tab = tab.id().into();
        }
    });
    if !was_open && let Some(x) = side_mut(s) {
        x.opened_at[tab.dock().ix()] = Some(Instant::now());
    }
    load(s, tab);
}

/// First visit loads a tab; later visits refresh the ones that go stale.
fn load(s: &mut State, tab: Tab) {
    // On another computer these tabs run there, unless its backend said
    // it cannot (an older one, or no host shell for the console).
    if tab.local_only()
        && crate::remote::backend_is_remote(s)
        && !side(s).is_some_and(|x| remote_repo::offered(x, tab))
    {
        return;
    }
    match tab {
        Tab::Activity | Tab::SideChat => {}
        Tab::Agents => agents::load(s),
        Tab::Plugin => {
            if let Some(key) = plugin_mount(s) {
                crate::plugins::render(s, key);
            }
        }
        Tab::Console => console::load(s),
        Tab::Browser => browser::start(s),
        Tab::Outputs => {
            if side(s).is_some_and(|x| x.outputs.files.is_none()) {
                outputs::browse(s, None);
            }
        }
        Tab::Routines => routines::load(s),
        Tab::Machines => machines::load(s),
        Tab::Files => project::refresh(s),
        Tab::Changes => changes::refresh(s),
        Tab::History => history::refresh(s),
    }
}

/// A thread bound to its own git worktree works there: point the coding
/// tabs at it (without remembering it as the thread's project folder).
fn worktree_load(s: &mut State) {
    let Some(thread) = thread_id(s) else { return };
    let asked = thread.clone();
    jobs::run(
        s,
        "side",
        move |c| async move { c.thread_worktree(&asked).await },
        move |s, result, _| match result {
            Ok(Some(view)) if view.bound() => {
                let path = PathBuf::from(&view.path);
                let Some(x) = side_of(s, &thread) else { return };
                x.worktree = Some(view);
                set_folder(s, path, false);
            }
            Ok(_) => {}
            // Older gateways have no worktree route.
            Err(e) => tracing::debug!(error = %e, "worktree status unavailable"),
        },
    );
}

/// Point the coding tabs at `folder`. `remember` saves it for this thread.
pub(crate) fn set_folder(s: &mut State, folder: PathBuf, remember: bool) {
    // A folder on this Mac means nothing to a backend on another computer
    // (Files then reads the backend's workspace instead).
    if crate::remote::backend_is_remote(s) {
        return;
    }
    let Some(thread) = thread_id(s) else { return };
    if remember {
        let saved = folder.clone();
        let key = thread.clone();
        s.update_prefs(|p| {
            p.project_folders.insert(key, saved.clone());
            p.last_project = Some(saved);
        });
    }
    let Some(x) = side_mut(s) else { return };
    changes::folder_changed(x);
    x.folder = Some(folder.clone());
    x.from_profile = false;
    x.repo = None;
    x.project = Default::default();
    x.changes = Default::default();
    x.history = Default::default();
    // The card's pull request and servers belonged to the old folder.
    x.details.forget_folder();
    let generation = x.folder_gen.bump();
    jobs::blocking(
        s,
        move || git::root(&folder).ok(),
        move |s, repo, _| {
            let Some(x) = side_of(s, &thread).filter(|x| x.folder_gen == generation) else {
                return;
            };
            x.repo = repo;
            let card_open = x.session_open;
            load_shown(s);
            if card_open {
                details::opened(s);
            }
        },
    );
}

/// A thread with no project folder of its own works in its profile's folder
/// (the one picked when the profile was made). Called once the profile has
/// loaded, again after each profile save; the folder is not remembered, so
/// a later profile edit moves it.
pub fn adopt_profile_folder(s: &mut State, thread: &str) {
    if thread_id(s).as_deref() != Some(thread) {
        return;
    }
    let Some(folder) = bridge::profile(s).and_then(|(_, f, _)| profile_path(&f)) else {
        return;
    };
    let adopt = side(s).is_some_and(|x| match &x.folder {
        None => true,
        Some(old) => x.from_profile && x.worktree.is_none() && *old != folder,
    });
    if adopt && folder.is_dir() {
        set_folder(s, folder, false);
        if let Some(x) = side_mut(s) {
            x.from_profile = x.folder.is_some();
        }
    }
}

/// A profile's effective folder as a path (`~/` expanded).
fn profile_path(folder: &str) -> Option<PathBuf> {
    let folder = folder.trim();
    if folder.is_empty() {
        return None;
    }
    match folder.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map(|h| PathBuf::from(h).join(rest)),
        None => Some(PathBuf::from(folder)),
    }
}

/// A mention root picked for a thread with no project folder of its own
/// adopts that folder.
pub fn adopt_folder(s: &mut State, root: PathBuf) {
    if side(s).is_some_and(|x| x.folder.is_none()) {
        set_folder(s, root, true);
    }
}

/// The project folder of the open chat, for other areas (mentions).
pub fn folder(s: &State) -> Option<&Path> {
    side(s).and_then(|x| x.folder.as_deref())
}

fn pick_project_folder(tx: &mut Tx) {
    let options = FileDialogOptions {
        title: "Use folder".into(),
        ..Default::default()
    };
    tx.open_folder(options, |s: &mut State, result, _| match result {
        Ok(Some(folder)) => {
            // Picked in the open panel, so macOS lets ZLO have just this
            // folder: keep it, so a project in Documents never needs the
            // whole-Documents switch (`folder_access.rs`). A running backend
            // gets it at its next start; it is never restarted from here.
            crate::folder_access::adopt_pick(&folder);
            set_folder(s, folder, true);
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(error = %e, "folder picker failed"),
    });
}

/// A turn is about to start: remember the working tree for its card.
pub fn on_turn_started(s: &mut State) {
    changes::checkpoint(s);
}

/// A turn ended: files, changes and outputs may have moved.
pub fn on_turn_ended(s: &mut State) {
    let Some(x) = side(s) else { return };
    let has_repo = x.repo.is_some();
    let browsed = x.outputs.files.as_ref().map(|f| f.path.clone());
    if has_repo {
        changes::refresh(s);
    }
    if let Some(path) = browsed {
        outputs::browse(s, path);
    }
    if tab_shown(s, Tab::Files) {
        project::refresh(s);
    }
}

/// The lines picked for a comment in Changes, for the composer's `@`
/// selection mention.
pub fn changes_selection(s: &State) -> Option<(PathBuf, zlo_client::mentions::LineRange)> {
    side(s).and_then(changes::selection)
}

/// A link clicked in the conversation: a file the agent wrote opens in
/// Outputs, a web page in the Browser tab. `external` (⌘-click) sends a web
/// page to the default browser and a file on this Mac to its app.
pub fn open_link(s: &mut State, href: &str, external: bool, tx: &mut Tx) {
    match link_target(href) {
        Link::File(path) => {
            show(s, Tab::Outputs);
            outputs::open(s, path);
        }
        // A path from a remote backend is on that computer, not this Mac:
        // fetch it through the gateway like the agent's own files.
        Link::Local(path) if crate::remote::backend_is_remote(s) => {
            show(s, Tab::Outputs);
            outputs::open(s, path.to_string_lossy().into_owned());
        }
        Link::Local(path) if !external => {
            show(s, Tab::Outputs);
            outputs::open_local(s, path);
        }
        Link::Local(path) => ui::open_path(&path, false),
        Link::Web(url) if !external => browser::go(s, url),
        Link::Web(url) | Link::Other(url) => {
            if let Err(e) = tx.open_url(&url) {
                tracing::warn!(error = %e, "link not opened");
            }
        }
    }
}

/// When a dock that opened a moment ago has finished sliding, the next
/// time [`native_wanted`] can change with no state change behind it.
/// `None` once every dock has settled.
pub fn native_settles_at(s: &State) -> Option<Instant> {
    let now = Instant::now();
    side(s)?
        .opened_at
        .iter()
        .flatten()
        .map(|opened| *opened + NATIVE_SETTLE)
        .filter(|at| *at > now)
        .min()
}

/// The native views the side pane wants on screen now, by
/// `native_view::Slot` (browser, preview). `native_view` reconciles the
/// screen with it.
pub fn native_wanted(s: &State) -> [Option<crate::native_view::Source>; 2] {
    use crate::native_view::Source;
    let mut out = [None, None];
    let Some(x) = side(s) else { return out };
    if s.page != Page::Chat || x.session_open {
        return out;
    }
    let settled = |dock: Dock| {
        x.opened_at[dock.ix()].is_none_or(|t| t.elapsed() >= NATIVE_SETTLE)
            && x.drag.is_none_or(|(d, _)| d != dock)
    };
    if tab_shown(s, Tab::Browser)
        && settled(Dock::Side)
        && let Some(url) = browser::page()
    {
        out[0] = Some(Source::Url(url));
    }
    if settled(Dock::Work) {
        let opened = if tab_shown(s, Tab::Outputs) {
            x.outputs.opened.as_ref()
        } else if tab_shown(s, Tab::Files) {
            x.project.opened.as_ref()
        } else {
            None
        };
        if let Some(o) = opened.filter(|o| matches!(o.preview, preview::Preview::Native)) {
            out[1] = Some(Source::File(o.path.clone()));
        }
    }
    out
}

/// A dot on a tab that has live activity or wants attention.
fn badge(s: &State, tab: Tab) -> bool {
    match tab {
        Tab::Activity => activity::waiting(s) > 0 || bridge::running(s),
        Tab::Agents => agents::summary(s).0 > 0,
        Tab::Changes => side(s).is_some_and(|x| x.changes.has_turn()),
        Tab::Browser => browser::page().is_some(),
        Tab::Console => console::is_running(s),
        Tab::SideChat => bridge::side_chat_running(s),
        _ => false,
    }
}

/// Whether `dock`'s titlebar toggle should show an attention dot.
fn dock_badge(s: &State, dock: Dock) -> bool {
    match dock {
        Dock::Work => badge(s, Tab::Changes) || badge(s, Tab::Console),
        Dock::Side => activity::waiting(s) > 0 || badge(s, Tab::Agents),
    }
}

/// Whether `tab` shows its own body, not the remote or no-folder notice
/// [`body`] puts in its place.
fn tab_ready(s: &State, tab: Tab) -> bool {
    if crate::remote::backend_is_remote(s) {
        !tab.local_only() || side(s).is_some_and(|x| remote_repo::offered(x, tab))
    } else {
        !(tab.coding() && side(s).is_some_and(|x| x.folder.is_none()))
    }
}

/// The tab's own buttons, drawn at the right of the dock's titlebar header
/// before the widen button, so every tab keeps its actions in one place.
/// Tabs whose controls belong to their content (Changes' git bar, Console's
/// prompt, Browser's address bar) keep them there.
fn tab_actions(s: &State, k: &Kit, tab: Tab) -> Vec<El> {
    if !tab_ready(s, tab) {
        return Vec::new();
    }
    match tab {
        Tab::Files => project::actions(s, k),
        Tab::Outputs => outputs::actions(s, k),
        Tab::History => history::actions(s, k),
        Tab::Routines => routines::actions(s, k),
        Tab::Agents => agents::actions(k),
        Tab::Plugin => vec![
            ui::ibtn(&crate::icons::REFRESH_CW, "Refresh")
                .on_press(|s: &mut State| load(s, Tab::Plugin))
                .build(k)
                .key("side-plugin-refresh"),
        ],
        Tab::Machines => machines::actions(k),
        _ => Vec::new(),
    }
}

/// How a dock header's status line reads: its dot's colour, and whether the
/// dot breathes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Tone {
    /// Something runs now.
    Live,
    /// Waits on the person.
    Warn,
    Bad,
    Good,
    /// Nothing to report.
    Quiet,
}

/// The dock header's one-line status for `tab`: what the tab holds right
/// now, instead of repeating the name its pill already shows. `None` for
/// tabs whose header holds something else (folder chip, browser bar, side
/// chat controls).
fn tab_status(s: &State, tab: Tab) -> Option<(String, Tone)> {
    let plural = |n: usize, one: &str, many: &str| {
        if n == 1 {
            format!("1 {one}")
        } else {
            format!("{n} {many}")
        }
    };
    Some(match tab {
        Tab::Activity => activity::status(s),
        Tab::Agents => match agents::summary(s) {
            (_, 0) => ("No subagents".into(), Tone::Quiet),
            (0, n) => (
                format!("{} · all done", plural(n, "subagent", "subagents")),
                Tone::Good,
            ),
            (l, n) => (format!("{l} running · {n} total"), Tone::Live),
        },
        Tab::Routines => match routines::count(s) {
            None => ("Loading…".into(), Tone::Quiet),
            Some(0) => ("No routines".into(), Tone::Quiet),
            Some(n) => (plural(n, "routine", "routines"), Tone::Good),
        },
        Tab::Machines => {
            let n = machines::running_count(s).unwrap_or(0);
            let label = crate::remote::computer_label(s);
            match n {
                0 => (format!("{label} · idle"), Tone::Good),
                n => (format!("{label} · {n} running"), Tone::Live),
            }
        }
        // A plugin's tab: its own title, since every plugin pill is told
        // apart by it.
        Tab::Plugin => (
            plugin_tab(s).map_or_else(|| tab.label().into(), |x| x.title.clone()),
            Tone::Quiet,
        ),
        Tab::Outputs => match side(s).and_then(|x| x.outputs.count()) {
            None => ("Workspace files".into(), Tone::Quiet),
            Some(0) => ("No files yet".into(), Tone::Quiet),
            Some(n) => (plural(n, "file", "files"), Tone::Good),
        },
        _ => return None,
    })
}

/// The live count shown on a tab pill (only when non-zero).
fn tab_count(s: &State, tab: Tab) -> Option<usize> {
    match tab {
        Tab::Changes => side(s).map(|x| x.changes.count()).filter(|n| *n > 0),
        Tab::Outputs => side(s).and_then(|x| x.outputs.count()).filter(|n| *n > 0),
        Tab::Activity => {
            let n = activity::waiting(s);
            (n > 0).then_some(n)
        }
        Tab::Agents => {
            let n = agents::summary(s).1;
            (n > 0).then_some(n)
        }
        Tab::Routines => routines::count(s).filter(|n| *n > 0),
        Tab::Machines => machines::running_count(s).filter(|n| *n > 0),
        _ => None,
    }
}

/// The dock's body for its tab.
fn body(s: &State, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>, tab: Tab) -> El {
    let _ = cx;
    if crate::remote::backend_is_remote(s) {
        if tab.local_only() && !side(s).is_some_and(|x| remote_repo::offered(x, tab)) {
            return dock::remote_unavailable(s, tab);
        }
    } else if tab.coding() && side(s).is_some_and(|x| x.folder.is_none()) {
        return dock::no_folder(s, k, tab);
    }
    match tab {
        Tab::Activity => activity::view(s, k),
        Tab::Outputs => outputs::view(s, k),
        Tab::Browser => browser::view(s, k),
        Tab::Agents => agents::view(s, k),
        Tab::Routines => routines::view(s, k),
        Tab::Files => project::view(s, k),
        Tab::Changes => changes::view(s, k),
        Tab::History => history::view(s, k),
        Tab::Console => console::view(s, k),
        Tab::Machines => machines::view(s, k),
        Tab::SideChat => session::chat_view(s, cx, k, layer),
        Tab::Plugin => match plugin_mount(s) {
            Some(key) => ui::scroll(
                format!("side-{}", key.el_key()),
                el().w_full()
                    .px(12.)
                    .pt(4.)
                    .child(crate::plugins::mount_view(
                        s,
                        k,
                        &key,
                        crate::plugins::Density::Panel,
                    )),
            ),
            None => ui::empty(s, "This plugin's tab is gone."),
        },
    }
}

#[derive(Debug, PartialEq)]
enum Link {
    /// A path in the conversation's workspace, as `/workspace/...`.
    File(String),
    /// A file on this Mac (CLI providers write real paths).
    Local(PathBuf),
    Web(String),
    Other(String),
}

/// Agents link their files as `/workspace/report.pdf` or `report.pdf`.
fn link_target(href: &str) -> Link {
    let href = href.trim();
    let lower = href.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Link::Web(href.to_string());
    }
    let href = match lower.starts_with("file://") {
        true => href.get(7..).unwrap_or_default(),
        false => href,
    };
    if href.contains(':') || href.starts_with('#') || href.is_empty() {
        return Link::Other(href.to_string());
    }
    let path = percent_decode(href.split(['#', '?']).next().unwrap_or(href));
    let rel = path.trim_start_matches("./");
    if rel.starts_with("/workspace/") {
        return Link::File(rel.to_string());
    }
    if rel.starts_with('/') {
        return Link::Local(PathBuf::from(rel));
    }
    if let Some(rest) = rel.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return Link::Local(home.join(rest));
    }
    Link::File(format!("/workspace/{rel}"))
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|h| std::str::from_utf8(h).ok())
            .and_then(|h| u8::from_str_radix(h, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(b)) => {
                out.push(b);
                i += 3;
            }
            (b, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `~/code/zlo` for a folder under home, else the full path.
pub(crate) fn short_path(path: &Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

#[cfg(test)]
#[cfg(target_os = "macos")] // the native views exist on macOS only
mod native_tests;

#[cfg(test)]
mod tests {
    use super::{Link, link_target};

    /// A thread with no folder of its own works in its profile's folder
    /// (the details card then names it, not "No project"); a folder the
    /// thread already has wins, and another thread's profile is ignored.
    #[test]
    fn a_thread_without_a_folder_works_in_its_profile_folder() {
        let dir = std::env::temp_dir();
        let profile = zlo_client::profiles::ProfileInfo {
            profile_id: "techclub".into(),
            settings: Default::default(),
            is_default: false,
            effective_folder: dir.display().to_string(),
            git_branch: None,
            thread_count: 1,
            last_used_at: None,
            created_at: 0,
            updated_at: 0,
        };
        let mut s = crate::chat::fixtures::test_state();
        s.chat = Some(crate::chat::ChatState::new("t1".into()));
        let c = s.chat.as_mut().expect("chat");
        c.approval.profile = Some(profile);

        super::adopt_profile_folder(&mut s, "t2");
        assert!(super::folder(&s).is_none(), "another thread's profile");

        super::adopt_profile_folder(&mut s, "t1");
        assert_eq!(super::folder(&s), Some(dir.as_path()));

        // The profile's folder moved (a save on the Profiles page reloads it).
        let moved = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let c = s.chat.as_mut().expect("chat");
        if let Some(p) = c.approval.profile.as_mut() {
            p.effective_folder = moved.display().to_string();
        }
        super::adopt_profile_folder(&mut s, "t1");
        assert_eq!(
            super::folder(&s),
            Some(moved.as_path()),
            "a profile edit follows"
        );

        let own = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let c = s.chat.as_mut().expect("chat");
        c.side.folder = Some(own.to_path_buf());
        c.side.from_profile = false;
        if let Some(p) = c.approval.profile.as_mut() {
            p.effective_folder = dir.display().to_string();
        }
        super::adopt_profile_folder(&mut s, "t1");
        assert_eq!(super::folder(&s), Some(own), "the thread's own folder wins");
    }

    #[test]
    fn chat_links_find_their_tab() {
        let file = |p: &str| Link::File(p.into());
        assert_eq!(
            link_target("/workspace/ten_words.pdf"),
            file("/workspace/ten_words.pdf")
        );
        assert_eq!(
            link_target("./out/My%20deck.pptx"),
            file("/workspace/out/My deck.pptx")
        );
        assert_eq!(
            link_target("https://docs.rs"),
            Link::Web("https://docs.rs".into())
        );
        assert_eq!(
            link_target("mailto:a@b.c"),
            Link::Other("mailto:a@b.c".into())
        );
        assert_eq!(
            link_target("/tmp/a%20b.txt"),
            Link::Local("/tmp/a b.txt".into())
        );
        assert_eq!(
            link_target("file:///tmp/a.txt"),
            Link::Local("/tmp/a.txt".into())
        );
    }
}
