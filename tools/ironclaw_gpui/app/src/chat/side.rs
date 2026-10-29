//! The chat's two docks, Zeron's arrangement. The work panel (Changes,
//! Files, Outputs) sits beside the conversation; the right pane (Activity,
//! Browser, Agents, Routines, History, Console and the open side chat) sits
//! at the window's edge. Each is a resizable column whose tabs ride the
//! titlebar above it; the last tab picked in each comes back on the next
//! thread. The session card (`session.rs`) is the third titlebar toggle.
//!
//! Assistant tabs read the gateway (Activity, Outputs, Agents, Routines);
//! Browser shows a web page natively (`native_view`).
//! Coding tabs (Files, Changes, History, Console) read a project folder on
//! this Mac with the system `git`, picked once per thread.

use std::path::{Path, PathBuf};

use super::ChatView;
use crate::assets::ExtraIcon;
use crate::design::prefs::Prefs;
use crate::design::{Palette, Style, flip, motion, panels};
use crate::ui::*;

mod activity;
mod agents;
mod browser;
mod changes;
mod console;
pub(super) mod git;
mod history;
mod outputs;
mod preview;
mod project;
mod routines;
mod session;

use session::SideChat;

const WIDTH_MIN: f32 = 260.;
const WIDTH_MAX: f32 = 900.;
const WIDTH_DEFAULT: f32 = 360.;
/// The expanded column, as a share of the window (MonoCode's "maximize").
const WIDE_SHARE: f32 = 0.62;
/// Room the conversation and the sidebar keep when the docks are sized.
const KEEP: f32 = 360. + 240.;

/// The two columns beside the conversation.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Dock {
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
pub(super) enum Tab {
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

    fn icon(self) -> Icon {
        match self {
            Tab::Activity => Icon::new(ExtraIcon::Activity),
            Tab::Outputs => Icon::new(IconName::FolderOpen),
            Tab::Browser => Icon::new(IconName::Globe),
            Tab::Agents => Icon::new(IconName::Bot),
            Tab::Routines => Icon::new(IconName::Calendar),
            Tab::Files => Icon::new(ExtraIcon::FolderGit),
            Tab::Changes => Icon::new(ExtraIcon::FileDiff),
            Tab::History => Icon::new(ExtraIcon::GitCommitHorizontal),
            Tab::Console => Icon::new(IconName::SquareTerminal),
            Tab::SideChat => Icon::new(ExtraIcon::MessagesSquare),
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
        let tabs = dock.tabs();
        tabs.iter()
            .copied()
            .find(|t| t.id() == id)
            .unwrap_or(match dock {
                Dock::Work => Tab::Changes,
                Dock::Side => Tab::Activity,
            })
    }

    /// Tabs that read the project folder.
    fn coding(self) -> bool {
        matches!(
            self,
            Tab::Files | Tab::Changes | Tab::History | Tab::Console
        )
    }
}

/// Per-thread dock state. Open, width and tab live in [`Prefs`].
#[derive(Default)]
pub(super) struct Side {
    /// The project folder, and whether git knows it (its top folder then).
    pub(super) folder: Option<PathBuf>,
    pub(super) repo: Option<PathBuf>,
    /// The dock whose inner edge is dragged, and its live width.
    drag: Option<(Dock, f32)>,
    /// Each dock widened to [`WIDE_SHARE`], by [`Dock::ix`].
    wide: [bool; 2],
    /// The session card is open.
    session_open: bool,
    session_hover: bool,
    /// A side chat is being forked.
    chat_busy: bool,
    /// The side chat shown in the right pane.
    pub(super) chat: Option<SideChat>,
    activity: activity::State,
    outputs: outputs::State,
    browser: browser::State,
    routines: routines::State,
    project: project::State,
    changes: changes::State,
    history: history::State,
    console: console::State,
}

impl ChatView {
    /// Restore this thread's project folder and load the shown tabs.
    pub(super) fn side_init(&mut self, cx: &mut Context<Self>) {
        if let Some(folder) = Prefs::of(cx).project_folders.get(&self.thread_id).cloned() {
            self.side_set_folder(folder, false, cx);
        }
        self.side_chat_restore(cx);
        self.side_load_shown(cx);
    }

    fn side_load_shown(&mut self, cx: &mut Context<Self>) {
        for dock in [Dock::Work, Dock::Side] {
            if dock_open(dock, cx) {
                self.side_load(self.dock_tab(dock, cx), cx);
            }
        }
    }

    pub(crate) fn toggle_side(&mut self, cx: &mut Context<Self>) {
        self.toggle_dock(Dock::Side, cx);
    }

    pub(crate) fn toggle_work(&mut self, cx: &mut Context<Self>) {
        self.toggle_dock(Dock::Work, cx);
    }

    fn toggle_dock(&mut self, dock: Dock, cx: &mut Context<Self>) {
        let open = !dock_open(dock, cx);
        Prefs::update(cx, |p| match dock {
            Dock::Work => p.work_open = open,
            Dock::Side => p.side_open = open,
        });
        if open {
            self.side_load(self.dock_tab(dock, cx), cx);
        }
        cx.notify();
    }

    /// A link clicked in the conversation: a file the agent wrote opens in
    /// Outputs, a web page in the Browser tab. `external` (⌘-click) sends a
    /// web page to the default browser and a file on this Mac to its app.
    pub(super) fn open_link(&mut self, href: &str, external: bool, cx: &mut Context<Self>) {
        if self.embedded {
            cx.emit(super::SideLink {
                href: href.to_string(),
                external,
            });
            return;
        }
        match link_target(href) {
            Link::File(path) => {
                self.show_side(Tab::Outputs, cx);
                self.outputs_open(path, cx);
            }
            Link::Local(path) if !external => {
                self.show_side(Tab::Outputs, cx);
                self.outputs_open_local(path, cx);
            }
            Link::Local(path) => cx.open_url(&format!("file://{}", path.display())),
            Link::Web(url) if !external => self.browser_go(url, cx),
            Link::Web(url) | Link::Other(url) => cx.open_url(&url),
        }
    }

    /// Open `tab`'s dock on it.
    pub(super) fn show_side(&mut self, tab: Tab, cx: &mut Context<Self>) {
        Prefs::update(cx, |p| match tab.dock() {
            Dock::Work => {
                p.work_open = true;
                p.work_tab = tab.id().into();
            }
            Dock::Side => {
                p.side_open = true;
                p.side_tab = tab.id().into();
            }
        });
        self.side_load(tab, cx);
        cx.notify();
    }

    /// The tab `dock` shows. Side chat falls back to Activity once closed.
    pub(super) fn dock_tab(&self, dock: Dock, cx: &App) -> Tab {
        let p = Prefs::of(cx);
        let tab = match dock {
            Dock::Work => Tab::from_id(&p.work_tab, dock),
            Dock::Side => Tab::from_id(&p.side_tab, dock),
        };
        match tab {
            Tab::SideChat if self.side.chat.is_none() => Tab::Activity,
            tab => tab,
        }
    }

    /// `tab` is on screen (its dock open on it).
    pub(super) fn tab_shown(&self, tab: Tab, cx: &App) -> bool {
        dock_open(tab.dock(), cx) && self.dock_tab(tab.dock(), cx) == tab
    }

    /// First visit loads a tab; later visits refresh the ones that go stale.
    fn side_load(&mut self, tab: Tab, cx: &mut Context<Self>) {
        match tab {
            Tab::Activity | Tab::Agents | Tab::Browser | Tab::SideChat => {}
            Tab::Outputs => {
                if self.files.is_none() {
                    self.browse(None, cx);
                }
            }
            Tab::Routines => self.load_routines(cx),
            Tab::Files => self.project_refresh(cx),
            Tab::Changes => self.changes_refresh(cx),
            Tab::History => self.history_refresh(cx),
            Tab::Console => {}
        }
    }

    /// Point the coding tabs at `folder`. `remember` saves it for this thread.
    pub(super) fn side_set_folder(
        &mut self,
        folder: PathBuf,
        remember: bool,
        cx: &mut Context<Self>,
    ) {
        if remember {
            let thread = self.thread_id.clone();
            let saved = folder.clone();
            Prefs::update(cx, |p| {
                p.project_folders.insert(thread, saved.clone());
                p.last_project = Some(saved);
            });
        }
        self.side.folder = Some(folder.clone());
        self.side.repo = None;
        self.side.project = Default::default();
        self.side.changes = Default::default();
        self.side.history = Default::default();
        let task = cx.background_spawn(async move { git::root(&folder).ok() });
        cx.spawn(async move |this, cx| {
            let repo = task.await;
            let _ = this.update(cx, |this, cx| {
                this.side.repo = repo;
                this.side_load_shown(cx);
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn pick_project_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Use folder".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = picked.await
                && let Some(folder) = paths.pop()
            {
                let _ = this.update(cx, |this, cx| this.side_set_folder(folder, true, cx));
            }
        })
        .detach();
    }

    /// A turn is about to start: remember the working tree for its card.
    pub(super) fn side_turn_started(&mut self, cx: &mut Context<Self>) {
        self.changes_checkpoint(cx);
    }

    /// A turn ended: files, changes and outputs may have moved.
    pub(super) fn side_turn_ended(&mut self, cx: &mut Context<Self>) {
        if self.side.repo.is_some() {
            self.changes_refresh(cx);
        }
        if self.files.is_some() {
            let path = self.files.as_ref().and_then(|f| f.path.clone());
            self.browse(path, cx);
        }
        if self.tab_shown(Tab::Files, cx) {
            self.project_refresh(cx);
        }
    }

    /// `dock`'s width at rest: saved or default, or widened, leaving the
    /// conversation its room (beside the right pane, for the work panel).
    fn dock_width(&self, dock: Dock, window: &Window, cx: &App) -> f32 {
        let viewport = f32::from(window.viewport_size().width);
        let room = match dock {
            Dock::Work => {
                let (full, t) = self.dock_geometry(Dock::Side, window, cx);
                viewport - KEEP - full * t
            }
            // An open work panel keeps at least its minimum.
            Dock::Side => {
                let work = panels::reveal("work-reveal", dock_open(Dock::Work, cx), cx);
                viewport - KEEP - WIDTH_MIN * work
            }
        };
        let saved = match dock {
            Dock::Work => Prefs::of(cx).work_width,
            Dock::Side => Prefs::of(cx).side_width,
        };
        let normal = if saved > 0. { saved } else { WIDTH_DEFAULT }
            .clamp(WIDTH_MIN, WIDTH_MAX.min(room.max(WIDTH_MIN)));
        let wide = (viewport * WIDE_SHARE).clamp(WIDTH_MIN, room.max(WIDTH_MIN));
        let k = motion::state_t(
            &format!("{}-wide", dock.key()),
            self.side.wide[dock.ix()],
            panels::REVEAL,
            cx.reduce_motion(),
        );
        motion::lerp(normal, wide, k)
    }

    /// A dock's inner edge follows the pointer while dragged.
    pub(super) fn side_drag_move(
        &mut self,
        e: &gpui_kit::MouseMoveEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some((dock, _)) = self.side.drag else {
            return;
        };
        if e.pressed_button != Some(MouseButton::Left) {
            self.side_drag_end(cx);
            return;
        }
        let viewport = f32::from(window.viewport_size().width);
        let mut right = viewport;
        if dock == Dock::Work {
            let (full, t) = self.dock_geometry(Dock::Side, window, cx);
            right -= full * t;
        }
        // The same room `dock_width` leaves, so release does not jump.
        let max = (right - KEEP).clamp(WIDTH_MIN, WIDTH_MAX);
        let w = (right - f32::from(e.position.x)).clamp(WIDTH_MIN, max);
        self.side.drag = Some((dock, w));
        cx.notify();
    }

    pub(super) fn side_drag_end(&mut self, cx: &mut Context<Self>) {
        if let Some((dock, w)) = self.side.drag.take() {
            Prefs::update(cx, |p| match dock {
                Dock::Work => p.work_width = w,
                Dock::Side => p.side_width = w,
            });
        }
    }

    /// A dock's laid-out width and its 0..1 reveal. Docks slide open and
    /// shut and ease between their normal and widened sizes; a drag or a
    /// window resize applies at once.
    fn dock_geometry(&self, dock: Dock, window: &Window, cx: &App) -> (f32, f32) {
        let t = panels::reveal(&format!("{}-reveal", dock.key()), dock_open(dock, cx), cx);
        if let Some((d, w)) = self.side.drag
            && d == dock
        {
            return (w, t);
        }
        (self.dock_width(dock, window, cx), t)
    }

    /// The right pane's and the work panel's (width, reveal).
    pub(crate) fn dock_sizes(&self, window: &Window, cx: &App) -> ((f32, f32), (f32, f32)) {
        (
            self.dock_geometry(Dock::Side, window, cx),
            self.dock_geometry(Dock::Work, window, cx),
        )
    }

    /// The thread's end of the window titlebar, Zeron's three toggles: the
    /// session card, the work panel, the right pane. Each dock's toggle
    /// sits at its column's top right, and when it opens its tabs slide in
    /// to the toggle's left on the column's tint, so the column reads as
    /// one full-height sheet. Closed, the toggles sit side by side.
    pub(crate) fn render_titlebar_actions(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = *Palette::of(cx);
        let slot = panels::TOGGLE_SIZE + 16.;
        let (wf, wt) = self.dock_geometry(Dock::Work, window, cx);
        let (sf, st) = self.dock_geometry(Dock::Side, window, cx);
        let (ww, sw) = (wf * wt, sf * st);
        // The block spans both columns, and always room for both toggles.
        let block = (ww + sw).max(sw.max(slot) + slot);
        // The work toggle sits at the work column's right, never on the
        // right pane's toggle.
        let work_right = sw.max(slot);
        let thread = h_flex()
            .gap_1()
            .pr_2()
            .items_center()
            .when(self.computer.read(cx).has_session(), |row| {
                let shown = !self.computer.read(cx).hidden();
                row.child(
                    Button::new("computer-toggle")
                        .icon(ExtraIcon::Monitor)
                        .ghost()
                        .small()
                        .when(shown, |b| b.selected(true))
                        .tooltip("Computer use screen share")
                        .accessibility_label("Computer use screen share")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.computer
                                .update(cx, |share, cx| share.toggle_hidden(cx))
                        })),
                )
            })
            .child(self.render_session_toggle(cx));
        let tint = |right: f32, width: f32| {
            div()
                .absolute()
                .top_0()
                .h_full()
                .right(px(right))
                .w(px(width))
                .bg(p.surface.opacity(0.45))
                .border_l_1()
                .border_color(p.border)
        };
        let work_toggle = panels::toggle_with(
            "work-toggle",
            dock_open(Dock::Work, cx),
            if dock_open(Dock::Work, cx) {
                "Hide changes and files"
            } else {
                "Changes, files and outputs"
            },
            cx,
            |_, ink| {
                Icon::new(ExtraIcon::FolderTree)
                    .size(px(panels::GLYPH_PX))
                    .text_color(ink)
                    .into_any_element()
            },
        )
        .on_click(cx.listener(|this, _, window, cx| {
            this.toggle_work(cx);
            window.refresh();
        }));
        let side_toggle = panels::toggle(
            "side-toggle",
            dock_open(Dock::Side, cx),
            true,
            if dock_open(Dock::Side, cx) {
                "Hide pane"
            } else {
                "Activity, browser and side chat"
            },
            cx,
        )
        .on_click(cx.listener(|this, _, window, cx| {
            this.toggle_side(cx);
            window.refresh();
        }));
        let toggle_at = |right: f32, toggle: gpui_kit::Stateful<gpui_kit::Div>| {
            h_flex()
                .absolute()
                .top_0()
                .h_full()
                .right(px(right + 8.))
                .items_center()
                .child(toggle)
        };
        let work_tabs = (wt > 0.).then(|| {
            let width = (sw + ww - work_right - slot).max(0.);
            self.render_dock_header(Dock::Work, sw + ww - width, width, wt, cx)
        });
        let side_tabs = (st > 0.)
            .then(|| self.render_dock_header(Dock::Side, slot, (sw - slot).max(0.), st, cx));
        h_flex()
            .h_full()
            .items_center()
            .child(thread)
            .child(
                div()
                    .relative()
                    .h_full()
                    .flex_none()
                    .w(px(block))
                    .overflow_hidden()
                    .when(ww > slot, |d| d.child(tint(sw, ww)))
                    .when(sw > slot, |d| d.child(tint(0., sw)))
                    .children(work_tabs)
                    .children(side_tabs)
                    .child(toggle_at(work_right, work_toggle))
                    .child(toggle_at(0., side_toggle)),
            )
            .into_any_element()
    }

    /// A dock's titlebar strip: its tabs and widen button, `width` wide
    /// with its left edge `right + width` from the block's right.
    fn render_dock_header(
        &self,
        dock: Dock,
        right: f32,
        width: f32,
        t: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let wide = self.side.wide[dock.ix()];
        h_flex()
            .absolute()
            .top_0()
            .right(px(right))
            .h_full()
            .w(px(width))
            .pl_2()
            .gap_1()
            .items_center()
            .opacity(t)
            .child(
                div()
                    .id(SharedString::from(format!("{}-tabs", dock.key())))
                    .flex_1()
                    .min_w_0()
                    .overflow_x_scroll()
                    .child(self.render_dock_tabs(dock, cx)),
            )
            .child(
                Button::new(SharedString::from(format!("{}-wide", dock.key())))
                    .icon(if wide {
                        IconName::Minimize
                    } else {
                        IconName::Maximize
                    })
                    .ghost()
                    .xsmall()
                    .tooltip(if wide { "Narrow" } else { "Widen" })
                    .accessibility_label("Widen")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let w = &mut this.side.wide[dock.ix()];
                        *w = !*w;
                        window.refresh();
                        cx.notify();
                    })),
            )
            .into_any_element()
    }

    /// A dock's tab icons, each with its attention dot.
    fn render_dock_tabs(&self, dock: Dock, cx: &mut Context<Self>) -> impl IntoElement {
        let p = *Palette::of(cx);
        let radius = Style::of(cx).control_radius;
        let tab = self.dock_tab(dock, cx);
        let group: &'static str = match dock {
            Dock::Work => "work-tabs",
            Dock::Side => "side-tabs",
        };
        // One selection pill glides between the tab icons (Zeron's tab
        // slide); the buttons carry only the selected ink.
        let pill = move || {
            flip::flip(
                group,
                "pill",
                flip::PILL_SECS,
                div().absolute().inset_0().rounded(px(radius)).bg(p.active),
            )
        };
        let has_chat = self.side.chat.is_some();
        h_flex()
            .relative()
            .gap_0p5()
            .child(flip::anchor(group))
            .children(
                dock.tabs()
                    .iter()
                    .copied()
                    .filter(|t| *t != Tab::SideChat || has_chat)
                    .map(|t| {
                        let badge = self.side_badge(t, cx);
                        div()
                            .relative()
                            .flex_none()
                            .when(t == tab, |d| d.child(pill()))
                            .child(
                                Button::new(SharedString::from(format!("side-tab-{}", t.id())))
                                    .icon(t.icon())
                                    .ghost()
                                    .small()
                                    .pill_selected(t == tab)
                                    .tooltip(t.label())
                                    .accessibility_label(t.label())
                                    .on_click(
                                        cx.listener(move |this, _, _, cx| this.show_side(t, cx)),
                                    ),
                            )
                            .when(badge, |d| {
                                d.child(
                                    div()
                                        .absolute()
                                        .top(px(3.))
                                        .right(px(3.))
                                        .size(px(6.))
                                        .rounded_full()
                                        .bg(p.accent),
                                )
                            })
                    }),
            )
    }

    /// Both docks, work panel first, for the chat's row.
    pub(super) fn render_docks(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        self.side_chat_release(cx);
        [Dock::Work, Dock::Side]
            .into_iter()
            .filter_map(|dock| self.render_dock(dock, window, cx))
            .collect()
    }

    fn render_dock(
        &mut self,
        dock: Dock,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (width, t) = self.dock_geometry(dock, window, cx);
        if t <= 0. {
            return None;
        }
        let p = *Palette::of(cx);
        let tab = self.dock_tab(dock, cx);
        // Native previews float above GPUI and would not clip with the
        // sliding column, so they wait until it settles.
        let native = matches!(tab, Tab::Browser | Tab::Outputs);
        let body = if t < 1. && native {
            div().into_any_element()
        } else if tab.coding() && self.side.folder.is_none() {
            self.render_no_folder(tab, cx)
        } else {
            match tab {
                Tab::Activity => self.render_activity(cx),
                Tab::Outputs => self.render_outputs(cx),
                Tab::Browser => self.render_browser(window, cx),
                Tab::Agents => self.render_agents(cx),
                Tab::Routines => self.render_routines(cx),
                Tab::Files => self.render_project(cx),
                Tab::Changes => self.render_changes(window, cx),
                Tab::History => self.render_history(window, cx),
                Tab::Console => self.render_console(window, cx),
                Tab::SideChat => self.render_side_chat(window, cx),
            }
        };
        let dragging = self.side.drag.is_some_and(|(d, _)| d == dock);
        let title = h_flex()
            .px_3()
            .pt_2()
            .pb_1()
            .gap_2()
            .items_center()
            .text_xs()
            .text_color(p.text_faint)
            .child(
                div()
                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                    .text_color(p.text)
                    .child(tab.label()),
            )
            .when(tab.coding(), |d| {
                d.when_some(self.side.folder.as_ref(), |d, folder| {
                    d.child(
                        h_flex()
                            .id(SharedString::from(format!("{}-folder", dock.key())))
                            .min_w_0()
                            .gap_1()
                            .items_center()
                            .cursor_pointer()
                            .hover(|s| s.text_color(p.text))
                            .child(div().min_w_0().truncate().child(short_path(folder)))
                            .child(Icon::new(IconName::ChevronDown).xsmall())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.pick_project_folder(window, cx)
                            })),
                    )
                })
            })
            .when(tab == Tab::SideChat, |d| {
                d.child(self.render_side_chat_actions(cx))
            });
        let pane = v_flex()
            .absolute()
            .top_0()
            .left_0()
            .w(px(width))
            .h_full()
            // A tinted sheet, a step darker than the transcript so the
            // column reads as its own layer.
            .bg(p.surface.opacity(0.45))
            .border_l_1()
            .border_color(p.border)
            .child(title)
            .child(div().flex_1().min_h_0().flex().flex_col().child(body))
            // The resize strip lies along the left border.
            .child(
                div()
                    .id(SharedString::from(format!("{}-edge", dock.key())))
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left_0()
                    .w(px(6.))
                    .cursor_col_resize()
                    .flex()
                    .child(
                        div()
                            .w(px(1.))
                            .h_full()
                            .when(dragging, |d| d.bg(p.accent.opacity(0.6))),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.side.wide[dock.ix()] = false;
                            let w = this.dock_width(dock, window, cx);
                            this.side.drag = Some((dock, w));
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    )
                    .on_click(cx.listener(move |this, e: &gpui_kit::ClickEvent, _, cx| {
                        if e.click_count() == 2 {
                            this.side.drag = None;
                            Prefs::update(cx, |p| match dock {
                                Dock::Work => p.work_width = 0.,
                                Dock::Side => p.side_width = 0.,
                            });
                        }
                    })),
            );
        // The column keeps its full width inside a clip that opens and
        // shuts around it, so it slides in without rewrapping.
        Some(
            div()
                .relative()
                .flex_none()
                .h_full()
                .w(px(width * t))
                .overflow_hidden()
                .child(pane)
                .into_any_element(),
        )
    }

    /// A dot on a tab that wants attention.
    fn side_badge(&self, tab: Tab, cx: &App) -> bool {
        match tab {
            Tab::Activity => self.activity_waiting() > 0,
            Tab::Agents => self.orch.tasks.iter().any(|t| t.status.is_live()),
            Tab::Changes => self.side.changes.has_turn(),
            Tab::SideChat => self.side_chat_running(cx),
            _ => false,
        }
    }

    fn render_no_folder(&self, tab: Tab, cx: &mut Context<Self>) -> AnyElement {
        let p = *Palette::of(cx);
        let last = Prefs::of(cx).last_project.clone();
        v_flex()
            .flex_1()
            .items_center()
            .justify_center()
            .gap_3()
            .p_6()
            .child(Icon::new(ExtraIcon::FolderGit).size(px(28.)).text_color(p.text_faint))
            .child(
                div()
                    .text_sm()
                    .text_center()
                    .text_color(p.text_muted)
                    .whitespace_normal()
                    .child(format!(
                        "{} works on a folder on this Mac. Pick the project this conversation is about.",
                        tab.label()
                    )),
            )
            .child(
                Button::new("side-pick-folder")
                    .label("Choose folder…")
                    .icon(ExtraIcon::FolderPlus)
                    .primary()
                    .small()
                    .on_click(cx.listener(|this, _, window, cx| this.pick_project_folder(window, cx))),
            )
            .when_some(last, |d, last| {
                let label = format!("Use {}", short_path(&last));
                d.child(
                    Button::new("side-last-folder")
                        .label(label)
                        .ghost()
                        .small()
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.side_set_folder(last.clone(), true, cx)
                        })),
                )
            })
            .into_any_element()
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
        true => &href[7..],
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

#[cfg(test)]
mod tests {
    use super::{Link, link_target};

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

pub(super) fn dock_open(dock: Dock, cx: &App) -> bool {
    match dock {
        Dock::Work => Prefs::of(cx).work_open,
        Dock::Side => Prefs::of(cx).side_open,
    }
}

/// `~/code/zlo` for a folder under home, else the last two parts.
pub(super) fn short_path(path: &Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

/// A muted line for empty lists.
fn empty(text: impl Into<SharedString>, cx: &App) -> AnyElement {
    let p = Palette::of(cx);
    div()
        .p_4()
        .text_sm()
        .text_color(p.text_muted)
        .whitespace_normal()
        .child(text.into())
        .into_any_element()
}

fn error_line(text: impl Into<SharedString>, cx: &App) -> AnyElement {
    let p = Palette::of(cx);
    div()
        .px_3()
        .py_2()
        .text_xs()
        .text_color(p.danger)
        .whitespace_normal()
        .child(text.into())
        .into_any_element()
}

fn loading() -> AnyElement {
    h_flex()
        .justify_center()
        .py_6()
        .child(Spinner::new())
        .into_any_element()
}

/// A small uppercase group heading inside a tab.
fn heading(text: impl Into<SharedString>, cx: &App) -> gpui_kit::Div {
    let p = Palette::of(cx);
    h_flex()
        .px_3()
        .pt_3()
        .pb_1()
        .gap_2()
        .items_center()
        .text_xs()
        .font_weight(gpui_kit::FontWeight::MEDIUM)
        .text_color(p.text_faint)
        .child(text.into())
}

/// A scrolling column for a tab's content.
fn scroll(
    id: impl Into<ElementId>,
) -> gpui_kit::component::scroll::Scrollable<gpui_kit::Stateful<gpui_kit::Div>> {
    // Each tab keeps its own scroll position: the scrollable's id
    // defaults to this call site, which every tab shares.
    let id: ElementId = id.into();
    v_flex()
        .id(id.clone())
        .flex_1()
        .min_h_0()
        .overflow_y_scrollbar()
        .id((id, "scroll"))
        .pb_3()
}
