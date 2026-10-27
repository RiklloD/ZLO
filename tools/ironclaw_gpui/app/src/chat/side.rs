//! The chat's right pane: MonoCode Native's `right_panel.rs` layout (MIT)
//! with its project tools, plus the assistant tabs of MonoCode's Mono
//! overview. One resizable column beside the conversation; tabs are icons in
//! its header and the last one picked comes back on the next thread.
//!
//! Assistant tabs read the gateway (Activity, Outputs, Agents, Routines);
//! Browser shows a web page natively (`native_view`).
//! Coding tabs (Files, Changes, History, Console) read a project folder on
//! this Mac with the system `git`, picked once per thread.

use std::path::{Path, PathBuf};

use super::ChatView;
use crate::assets::ExtraIcon;
use crate::design::prefs::Prefs;
use crate::design::{Palette, Style};
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

const WIDTH_MIN: f32 = 260.;
const WIDTH_MAX: f32 = 900.;
const WIDTH_DEFAULT: f32 = 360.;
/// The expanded pane, as a share of the window (MonoCode's "maximize").
const WIDE_SHARE: f32 = 0.62;

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
}

impl Tab {
    const ALL: [Tab; 9] = [
        Tab::Activity,
        Tab::Outputs,
        Tab::Browser,
        Tab::Agents,
        Tab::Routines,
        Tab::Files,
        Tab::Changes,
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
        }
    }

    fn from_id(id: &str) -> Tab {
        Tab::ALL
            .into_iter()
            .find(|t| t.id() == id)
            .unwrap_or(Tab::Activity)
    }

    /// Tabs that read the project folder.
    fn coding(self) -> bool {
        matches!(
            self,
            Tab::Files | Tab::Changes | Tab::History | Tab::Console
        )
    }
}

/// Per-thread pane state. Open, width and tab live in [`Prefs`].
#[derive(Default)]
pub(super) struct Side {
    /// The project folder, and whether git knows it (its top folder then).
    pub(super) folder: Option<PathBuf>,
    pub(super) repo: Option<PathBuf>,
    /// Live width while the edge is dragged.
    drag: Option<f32>,
    wide: bool,
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
    /// Restore this thread's project folder and load the shown tab.
    pub(super) fn side_init(&mut self, cx: &mut Context<Self>) {
        if let Some(folder) = Prefs::of(cx).project_folders.get(&self.thread_id).cloned() {
            self.side_set_folder(folder, false, cx);
        }
        if side_open(cx) {
            self.side_load(side_tab(cx), cx);
        }
    }

    pub(super) fn toggle_side(&mut self, cx: &mut Context<Self>) {
        let open = !side_open(cx);
        Prefs::update(cx, |p| p.side_open = open);
        if open {
            self.side_load(side_tab(cx), cx);
        }
        cx.notify();
    }

    /// A link clicked in the conversation: a file the agent wrote opens in
    /// Outputs, a web page in the Browser tab. `external` (⌘-click) sends a
    /// web page to the default browser instead.
    pub(super) fn open_link(&mut self, href: &str, external: bool, cx: &mut Context<Self>) {
        match link_target(href) {
            Link::File(path) => {
                self.show_side(Tab::Outputs, cx);
                self.outputs_open(path, cx);
            }
            Link::Web(url) if !external => self.browser_go(url, cx),
            Link::Web(url) | Link::Other(url) => cx.open_url(&url),
        }
    }

    /// Open the pane on `tab`.
    pub(super) fn show_side(&mut self, tab: Tab, cx: &mut Context<Self>) {
        Prefs::update(cx, |p| {
            p.side_open = true;
            p.side_tab = tab.id().into();
        });
        self.side_load(tab, cx);
        cx.notify();
    }

    /// First visit loads a tab; later visits refresh the ones that go stale.
    fn side_load(&mut self, tab: Tab, cx: &mut Context<Self>) {
        match tab {
            Tab::Activity | Tab::Agents | Tab::Browser => {}
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
                if side_open(cx) {
                    this.side_load(side_tab(cx), cx);
                }
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
        if side_open(cx) && side_tab(cx) == Tab::Files {
            self.project_refresh(cx);
        }
    }

    pub(super) fn side_width(&self, window: &Window, cx: &App) -> f32 {
        if let Some(w) = self.side.drag {
            return w;
        }
        let viewport = f32::from(window.viewport_size().width);
        if self.side.wide {
            return (viewport * WIDE_SHARE).clamp(WIDTH_MIN, WIDTH_MAX.max(viewport * WIDE_SHARE));
        }
        let saved = Prefs::of(cx).side_width;
        let w = if saved > 0. { saved } else { WIDTH_DEFAULT };
        // Leave the conversation at least 360 px.
        w.clamp(
            WIDTH_MIN,
            WIDTH_MAX.min((viewport - 360. - 240.).max(WIDTH_MIN)),
        )
    }

    /// The pane's left edge follows the pointer while dragged.
    pub(super) fn side_drag_move(
        &mut self,
        e: &gpui_kit::MouseMoveEvent,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self.side.drag.is_none() {
            return;
        }
        if e.pressed_button != Some(MouseButton::Left) {
            self.side_drag_end(cx);
            return;
        }
        let inset = Style::of(cx).panel_inset;
        let right = f32::from(window.viewport_size().width) - inset;
        let w = (right - f32::from(e.position.x)).clamp(WIDTH_MIN, WIDTH_MAX);
        self.side.drag = Some(w);
        cx.notify();
    }

    pub(super) fn side_drag_end(&mut self, cx: &mut Context<Self>) {
        if let Some(w) = self.side.drag.take() {
            Prefs::update(cx, |p| p.side_width = w);
        }
    }

    pub(super) fn render_side(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !side_open(cx) {
            return None;
        }
        let p = *Palette::of(cx);
        let tab = side_tab(cx);
        let width = self.side_width(window, cx);
        let body = if tab.coding() && self.side.folder.is_none() {
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
            }
        };
        let dragging = self.side.drag.is_some();
        // The tab row gives way first: in a narrow pane it scrolls sideways
        // and Widen and Hide stay reachable (T3's narrow-window fix).
        let tabs = h_flex().gap_0p5().children(Tab::ALL.into_iter().map(|t| {
            let badge = self.side_badge(t);
            div()
                .relative()
                .flex_none()
                .child(
                    Button::new(SharedString::from(format!("side-tab-{}", t.id())))
                        .icon(t.icon())
                        .ghost()
                        .small()
                        .selected(t == tab)
                        .tooltip(t.label())
                        .accessibility_label(t.label())
                        .on_click(cx.listener(move |this, _, _, cx| this.show_side(t, cx))),
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
        }));
        let header = h_flex()
            .h(px(44.))
            .flex_none()
            .px_2()
            .gap_1()
            .items_center()
            .child(
                div()
                    .id("side-tabs")
                    .flex_1()
                    .min_w_0()
                    .overflow_x_scroll()
                    .child(tabs),
            )
            .child(
                Button::new("side-wide")
                    .icon(if self.side.wide {
                        IconName::Minimize
                    } else {
                        IconName::Maximize
                    })
                    .ghost()
                    .xsmall()
                    .tooltip(if self.side.wide {
                        "Narrow pane"
                    } else {
                        "Widen pane"
                    })
                    .accessibility_label("Widen pane")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.side.wide = !this.side.wide;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("side-close")
                    .icon(IconName::PanelRightClose)
                    .ghost()
                    .xsmall()
                    .tooltip("Hide pane")
                    .accessibility_label("Hide pane")
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_side(cx))),
            );
        let title = h_flex()
            .px_3()
            .pb_1()
            .gap_2()
            .items_center()
            .text_xs()
            .text_color(p.text_faint)
            .child(
                div()
                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                    .text_color(p.text_muted)
                    .child(tab.label()),
            )
            .when(tab.coding(), |d| {
                d.when_some(self.side.folder.as_ref(), |d, folder| {
                    d.child(
                        h_flex()
                            .id("side-folder")
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
            });
        Some(
            v_flex()
                .relative()
                .w(px(width))
                .h_full()
                .flex_none()
                .border_l_1()
                .border_color(p.border)
                .child(header)
                .child(title)
                .child(div().flex_1().min_h_0().flex().flex_col().child(body))
                // The resize strip straddles the left border.
                .child(
                    div()
                        .id("side-edge")
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(px(-3.))
                        .w(px(6.))
                        .cursor_col_resize()
                        .flex()
                        .justify_center()
                        .child(
                            div()
                                .w(px(1.))
                                .h_full()
                                .when(dragging, |d| d.bg(p.accent.opacity(0.6))),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, window, cx| {
                                this.side.wide = false;
                                this.side.drag = Some(this.side_width(window, cx));
                                cx.stop_propagation();
                                cx.notify();
                            }),
                        )
                        .on_click(cx.listener(|this, e: &gpui_kit::ClickEvent, _, cx| {
                            if e.click_count() == 2 {
                                this.side.drag = None;
                                Prefs::update(cx, |p| p.side_width = 0.);
                            }
                        })),
                )
                .into_any_element(),
        )
    }

    /// A dot on a tab that wants attention.
    fn side_badge(&self, tab: Tab) -> bool {
        match tab {
            Tab::Activity => self.activity_waiting() > 0,
            Tab::Agents => self.orch.tasks.iter().any(|t| t.status.is_live()),
            Tab::Changes => self.side.changes.has_turn(),
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
    if href.contains(':') || href.starts_with('#') || href.is_empty() {
        return Link::Other(href.to_string());
    }
    let path = percent_decode(href.split(['#', '?']).next().unwrap_or(href));
    let rel = path.trim_start_matches("./");
    if rel.starts_with("/workspace/") {
        return Link::File(rel.to_string());
    }
    if rel.starts_with('/') {
        return Link::Other(format!("file://{href}"));
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
            link_target("/tmp/a.txt"),
            Link::Other("file:///tmp/a.txt".into())
        );
    }
}

pub(super) fn side_open(cx: &App) -> bool {
    Prefs::of(cx).side_open
}

pub(super) fn side_tab(cx: &App) -> Tab {
    Tab::from_id(&Prefs::of(cx).side_tab)
}

/// `~/code/numoh` for a folder under home, else the last two parts.
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
fn scroll(id: impl Into<ElementId>) -> gpui_kit::Stateful<gpui_kit::Div> {
    v_flex()
        .id(id)
        .flex_1()
        .min_h_0()
        .overflow_y_scroll()
        .pb_3()
}
