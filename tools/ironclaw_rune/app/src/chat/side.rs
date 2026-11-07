//! The chat's two docks, Zeron's arrangement (GPUI `chat/side.rs`). The work
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
//! system `git`, picked once per thread.
//!
//! # Entry points for the other areas
//!
//! Area A (chat layout and events):
//! - `side::init(s)` once after `chat::open` built the `ChatState` (GPUI
//!   `side_init`): restores the thread's folder, worktree, side chat and loads
//!   the shown tabs.
//! - `side::docks(s, cx, k, layer) -> Vec<El>`: work panel then right pane, to
//!   put after the conversation column in the chat's row (GPUI
//!   `render_docks`). Or `side::work_view` / `side::view` one by one.
//! - `side::on_turn_started(s)` before a turn is sent, `side::on_turn_ended(s)`
//!   when a run ends (GPUI `side_turn_started` / `side_turn_ended`).
//! - `side::open_link(s, href, external, tx)` for links clicked in a reply,
//!   also from an embedded side chat (GPUI `open_link` / `SideLink`).
//! - `side::adopt_folder(s, path)`: a mention root picked for a thread without
//!   a project folder (GPUI `mention_roots` → `side_set_folder(.., true)`).
//! - `side::changes_selection(s)` for the composer's `@` selection mention.
//! - `side::new_side_chat_allowed(s)`: false inside an embedded side chat.
//!
//! Area B (workspace):
//! - `side::titlebar_actions(s, cx, k, layer) -> El`: the thread's end of the
//!   window titlebar (GPUI `render_titlebar_actions`).
//! - `side::toggle_right_pane(s)` (⌥⌘B), `side::toggle_work_panel(s)` (⌘E),
//!   `side::new_side_chat(s)` (⌥⌘N). Off the chat page the toggles only flip
//!   the preference, as the GPUI workspace did.
//! - `side::dock_widths(s, viewport)` for the wallpaper's conversation area.
//! - `main.rs`: `native_view::poll` in `poll`, `native_view::on_window` on
//!   the `App` (see `native_view.rs`).

use std::path::{Path, PathBuf};
use std::time::Instant;

use ironclaw_gpui_client::orchestration::WorktreeView;
use rune::{Cx, El, FileDialogOptions, Tx};
use rune_kit::Kit;

use crate::jobs::{self, Generation};
use crate::state::{Page, State};

mod activity;
mod agents;
mod bridge;
mod browser;
mod changes;
mod console;
mod dock;
pub mod git;
mod history;
mod outputs;
mod preview;
mod project;
mod routines;
mod session;
mod ui;

pub mod fixtures;

// Entry points for areas A and B (see the module docs); unused until they wire them.
#[allow(unused_imports)]
pub use dock::{docks, dock_widths, titlebar_actions, view, work_view};
#[allow(unused_imports)]
pub use session::{new_side_chat, new_side_chat_allowed};

pub(crate) const WIDTH_MIN: f32 = 260.;
pub(crate) const WIDTH_MAX: f32 = 900.;
pub(crate) const WIDTH_DEFAULT: f32 = 360.;
/// The expanded column, as a share of the window (MonoCode's "maximize").
pub(crate) const WIDE_SHARE: f32 = 0.62;
/// Room the conversation and the sidebar keep when the docks are sized.
pub(crate) const KEEP: f32 = 360. + 240.;
/// Native views wait this long after their dock opens, so they don't float
/// over a column that is still sliding in (GPUI waited for the reveal).
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
    /// The side chat open on this thread (only listed while one is).
    SideChat,
}

impl Tab {
    const WORK: [Tab; 3] = [Tab::Changes, Tab::Files, Tab::Outputs];
    const SIDE: [Tab; 7] = [
        Tab::SideChat,
        Tab::Activity,
        Tab::Browser,
        Tab::Agents,
        Tab::Routines,
        Tab::History,
        Tab::Console,
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
            Tab::SideChat => "side-chat",
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
            Tab::SideChat => "Side chat",
        }
    }

    fn icon(self) -> &'static rune::Icon {
        use crate::icons;
        match self {
            Tab::Activity => &icons::ACTIVITY,
            Tab::Outputs => &icons::FOLDER_OPEN,
            Tab::Browser => &icons::GLOBE,
            Tab::Agents => &icons::BOT,
            Tab::Routines => &icons::CALENDAR,
            Tab::Files => &icons::FOLDER_GIT,
            Tab::Changes => &icons::FILE_DIFF,
            Tab::History => &icons::GIT_COMMIT_HORIZONTAL,
            Tab::Console => &icons::SQUARE_TERMINAL,
            Tab::SideChat => &icons::MESSAGES_SQUARE,
        }
    }

    fn dock(self) -> Dock {
        match self {
            Tab::Changes | Tab::Files | Tab::Outputs => Dock::Work,
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

    /// Tabs that read the project folder.
    fn coding(self) -> bool {
        matches!(self, Tab::Files | Tab::Changes | Tab::History | Tab::Console)
    }
}

/// The side chat shown in the right pane: an embedded chat (area A's
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
pub fn side_chat_mut<'a>(s: &'a mut State, thread_id: &str) -> Option<&'a mut crate::chat::ChatState> {
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
    pub repo: Option<PathBuf>,
    /// The thread's own git worktree, when it has one: the coding tabs then
    /// read that folder, and the title shows its branch.
    pub worktree: Option<WorktreeView>,
    /// Bumped when the folder changes, so git work for the old one is dropped.
    folder_gen: Generation,
    /// The dock whose inner edge is dragged, and its live width.
    drag: Option<(Dock, f32)>,
    /// Each dock widened to [`WIDE_SHARE`], by [`Dock::ix`].
    wide: [bool; 2],
    /// When each dock last opened, for native views to wait out the slide.
    opened_at: [Option<Instant>; 2],
    /// The session card is open.
    session_open: bool,
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

/// The tab `dock` shows. Side chat falls back to Activity once closed.
fn dock_tab(s: &State, dock: Dock) -> Tab {
    let tab = match dock {
        Dock::Work => Tab::from_id(&s.prefs.work_tab, dock),
        Dock::Side => Tab::from_id(&s.prefs.side_tab, dock),
    };
    match tab {
        Tab::SideChat if side(s).is_none_or(|x| x.chat.is_none()) => Tab::Activity,
        tab => tab,
    }
}

/// `tab` is on screen (its dock open on it).
fn tab_shown(s: &State, tab: Tab) -> bool {
    dock_open(s, tab.dock()) && dock_tab(s, tab.dock()) == tab
}

/// Restore this thread's project folder and load the shown tabs. Call once
/// after `chat::open` (GPUI `side_init`).
pub fn init(s: &mut State) {
    let Some(thread) = thread_id(s) else { return };
    if let Some(folder) = s.prefs.project_folders.get(&thread).cloned() {
        set_folder(s, folder, false);
    }
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

/// ⌘E. Off the chat page it only flips the preference.
pub fn toggle_work_panel(s: &mut State) {
    toggle_dock(s, Dock::Work);
}

fn toggle_dock(s: &mut State, dock: Dock) {
    let open = !dock_open(s, dock);
    s.update_prefs(|p| match dock {
        Dock::Work => p.work_open = open,
        Dock::Side => p.side_open = open,
    });
    if open && s.page == Page::Chat && s.chat.is_some() {
        mark_opened(s, dock);
        load(s, dock_tab(s, dock));
    }
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
    match tab {
        Tab::Activity | Tab::Agents | Tab::SideChat | Tab::Console => {}
        Tab::Browser => browser::start(s),
        Tab::Outputs => {
            if side(s).is_some_and(|x| x.outputs.files.is_none()) {
                outputs::browse(s, None);
            }
        }
        Tab::Routines => routines::load(s),
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
    x.repo = None;
    x.project = Default::default();
    x.changes = Default::default();
    x.history = Default::default();
    let generation = x.folder_gen.bump();
    jobs::blocking(
        s,
        move || git::root(&folder).ok(),
        move |s, repo, _| {
            let Some(x) = side_of(s, &thread).filter(|x| x.folder_gen == generation) else {
                return;
            };
            x.repo = repo;
            load_shown(s);
        },
    );
}

/// A mention root picked for a thread with no project folder of its own
/// adopts that folder (GPUI `mention_roots`).
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
        Ok(Some(folder)) => set_folder(s, folder, true),
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
pub fn changes_selection(
    s: &State,
) -> Option<(PathBuf, ironclaw_gpui_client::mentions::LineRange)> {
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

/// The native views the side pane wants on screen now, by
/// `native_view::Slot` (browser, preview). `native_view::poll` reads it.
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

/// A dot on a tab that wants attention.
fn badge(s: &State, tab: Tab) -> bool {
    match tab {
        Tab::Activity => activity::waiting(s) > 0,
        Tab::Agents => bridge::tasks(s).iter().any(|t| t.status.is_live()),
        Tab::Changes => side(s).is_some_and(|x| x.changes.has_turn()),
        Tab::SideChat => bridge::side_chat_running(s),
        _ => false,
    }
}

/// The dock's body for its tab.
fn body(s: &State, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>, tab: Tab) -> El {
    let _ = cx;
    if tab.coding() && side(s).is_some_and(|x| x.folder.is_none()) {
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
        Tab::SideChat => session::chat_view(s, cx, k, layer),
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
mod tests {
    use super::{Link, link_target};

    #[test]
    fn chat_links_find_their_tab() {
        let file = |p: &str| Link::File(p.into());
        assert_eq!(link_target("/workspace/ten_words.pdf"), file("/workspace/ten_words.pdf"));
        assert_eq!(link_target("./out/My%20deck.pptx"), file("/workspace/out/My deck.pptx"));
        assert_eq!(link_target("https://docs.rs"), Link::Web("https://docs.rs".into()));
        assert_eq!(link_target("mailto:a@b.c"), Link::Other("mailto:a@b.c".into()));
        assert_eq!(link_target("/tmp/a%20b.txt"), Link::Local("/tmp/a b.txt".into()));
        assert_eq!(link_target("file:///tmp/a.txt"), Link::Local("/tmp/a.txt".into()));
    }
}
