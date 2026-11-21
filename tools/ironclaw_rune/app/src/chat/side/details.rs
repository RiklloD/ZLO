//! The thread details card (Zeron's first titlebar toggle), t3code's
//! `ThreadDetailsPanel`: section for section, in its order and with its
//! labels. Each item can be hidden from the card's right-click menu or
//! Settings → General (`items.rs`), and the card drops rows by its height
//! like t3code's full / compact / essential densities.
//!
//! - Workspace: where the thread runs (computer and folder) and "Open in".
//! - Version control: branch and pull request, the git quick action, the
//!   working tree's changes, or "Initialize Git" for a plain folder.
//! - Servers: local servers running in the thread's folder (`servers.rs`).
//! - Automations: the schedules bound to this thread.
//! - Lineage: the parent, forks and subagents; finished ones fold away
//!   under "Previous agents".
//! - Side chats, with ZLO's own "New side chat" (⌥⌘N) and "Fork chat".
//! - Usage limits and Context window, off by default.
//!
//! The data comes from the sibling tabs (Changes, Routines, Agents, the
//! session's side chats) and the status bar's usage and telemetry; the card
//! only asks them to refresh when it opens ([`opened`]).

mod items;
mod open_in;
mod servers;

pub(crate) use items::{ITEMS, resolve as visibility, set as set_item};

use rune::tok::TextStyle;
use rune::{Align, Color, Cursor, El, PointerPress, ScrollSpec, Side, col, el, icon, row};
use rune_kit::{Kit, Menu, MenuItem, Placement};
use zlo_client::inbox::InboxStatus;
use zlo_client::orchestration::{TaskStatus, TaskView};

use self::items::{Density, Item, Visibility};
use super::ui::{self, sm_med, xs, xs_med};
use super::{
    SideState, Tab, agents, bridge, changes, git, pick_project_folder, remote_repo, routines,
    session, short_path, side, side_mut, side_of, thread_id,
};
use crate::design::kit::{self, Turn};
use crate::design::motion;
use crate::icons;
use crate::jobs;
use crate::state::{Page, State};

/// t3code's `--thread-details-panel-width`.
const WIDTH: f32 = 280.;
/// t3code's `THREAD_DETAILS_CARD_GAP`: room kept below the card.
const GAP: f32 = 12.;
const ROW_H: f32 = 32.;
/// Usage and context rows are shorter (t3code's `h-6`).
const METER_H: f32 = 24.;
/// Lists taller than this scroll inside the card (t3code's 13.5rem).
const LIST_MAX: f32 = 216.;
const LINEAGE_FIRST: usize = 6;
const LINEAGE_PAGE: usize = 12;
const SERVER_PAGE: usize = 4;

/// The card's per-thread state, in `SideState::details`.
#[derive(Default)]
pub(super) struct DetailsState {
    /// The right-click "Show in thread details" menu, where it opened.
    menu_at: Option<PointerPress>,
    /// The row menu that is open.
    dropdown: Option<Dropdown>,
    /// "Previous agents" is unfolded, and how many of them show.
    previous_open: bool,
    previous_shown: usize,
    /// Servers beyond the first page are shown.
    servers_more: bool,
    servers: Vec<servers::Server>,
    stopping: Option<u32>,
    pr: Option<git::PullRequest>,
    initializing: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Dropdown {
    Environment,
    OpenIn,
    Git,
    /// The "Choose what to show" row's menu.
    Choose,
}

impl DetailsState {
    /// What the card read from the old project folder.
    pub(super) fn forget_folder(&mut self) {
        self.pr = None;
        self.servers.clear();
        self.servers_more = false;
        self.stopping = None;
    }
}

fn visible(s: &State) -> Visibility {
    items::resolve(&s.prefs.thread_details_items)
}

/// The card opened (or an item was shown): refresh what its sections read.
pub(super) fn opened(s: &mut State) {
    let Some(thread) = thread_id(s) else { return };
    let v = visible(s);
    let mut ids = session::chats(s);
    ids.extend(related_threads(s));
    bridge::ensure_titles(s, ids);
    let remote = crate::remote::backend_is_remote(s);
    let Some(x) = side(s) else { return };
    let repo = x.repo.clone();
    let folder = x.folder.clone();
    let generation = x.folder_gen;
    if repo.is_some() || remote_repo::remote(s) {
        changes::refresh(s);
    }
    if let Some(repo) = repo.filter(|_| !remote && v.get(Item::Branch)) {
        let asked = thread.clone();
        jobs::blocking(
            s,
            move || git::pull_request(&repo),
            move |s, pr, tx| {
                if let Some(x) = side_of(s, &asked).filter(|x| x.folder_gen == generation) {
                    // A section arriving under the open card fades in and
                    // eases the rows below aside instead of snapping.
                    if x.session_open && x.details.pr.is_none() && pr.is_some() {
                        tx.animated();
                    }
                    x.details.pr = pr;
                }
            },
        );
    }
    if v.get(Item::Automations) {
        routines::load(s);
    }
    if let Some(folder) = folder.filter(|_| !remote && v.get(Item::Servers)) {
        refresh_servers(s, thread, folder, generation);
    }
    if v.get(Item::Usage) {
        crate::pages::usage::refresh(s, false);
    }
    if v.get(Item::Context) {
        crate::workspace::refresh_telemetry(s);
    }
}

fn refresh_servers(
    s: &mut State,
    thread: String,
    folder: std::path::PathBuf,
    generation: jobs::Generation,
) {
    jobs::blocking(
        s,
        move || servers::discover(&folder),
        move |s, found, tx| {
            let Some(x) = side_of(s, &thread).filter(|x| x.folder_gen == generation) else {
                return;
            };
            match found {
                Ok(list) => {
                    // As the PR row: the section arrives with motion.
                    if x.session_open && x.details.servers.is_empty() && !list.is_empty() {
                        tx.animated();
                    }
                    x.details.servers = list;
                }
                Err(e) => tracing::debug!(error = %e, "server discovery failed"),
            }
        },
    );
}

/// Close the card and any menu of it.
pub(super) fn close(s: &mut State) {
    if let Some(x) = side_mut(s) {
        x.session_open = false;
        x.details.menu_at = None;
        x.details.dropdown = None;
    }
}

fn close_menus(s: &mut State) {
    if let Some(x) = side_mut(s) {
        x.details.menu_at = None;
        x.details.dropdown = None;
    }
}

fn toggle_dropdown(s: &mut State, which: Dropdown) {
    if let Some(x) = side_mut(s) {
        x.details.menu_at = None;
        x.details.dropdown = (x.details.dropdown != Some(which)).then_some(which);
    }
}

/// The threads the lineage rows name, for the title cache.
fn related_threads(s: &State) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(lineage) = bridge::lineage(s) {
        if let Some(task) = &lineage.delegated_from {
            ids.push(task.parent_thread_id.clone());
        }
        if let Some(from) = &lineage.forked_from {
            ids.push(from.source_thread.clone());
        }
        ids.extend(lineage.forks.iter().filter_map(|f| f.target_thread.clone()));
    }
    ids
}

// ---------------------------------------------------------------- rows

fn base(key: impl Into<String>) -> El {
    row()
        .key(key)
        .w_full()
        .h(ROW_H)
        .px(10.)
        .gap(10.)
        .items_center()
        .radius(8.)
}

fn glyph(s: &State, g: &'static rune::Icon) -> El {
    icon(g, 16.).color(s.palette.text_muted)
}

fn label(s: &State, content: impl Into<String>, live: bool) -> El {
    let p = &s.palette;
    ui::line(content, sm_med(), if live { p.text } else { p.text_faint })
        .grow(1.)
        .min_w(0.)
}

fn meta(s: &State, content: impl Into<String>) -> El {
    ui::line(content, xs(), s.palette.text_faint)
}

fn pressable(s: &State, e: El) -> El {
    e.role("button")
        .cursor(Cursor::Pointer)
        .hover(s.palette.hover)
        .hover_motion(motion::HOVER, motion::HOVER)
        .press_scale(motion::PRESS_CARD)
}

/// A row split into its action and a chevron that opens its menu.
fn split(key: &str, primary: El, chevron: El) -> El {
    row()
        .key(key.to_string())
        .w_full()
        .h(ROW_H)
        .items_center()
        .child(primary)
        .child(chevron)
}

fn chevron(s: &State, key: &str, which: Dropdown) -> El {
    let open = side(s).is_some_and(|x| x.details.dropdown == Some(which));
    pressable(
        s,
        el().key(format!("{key}-chevron"))
            .w(32.)
            .h(ROW_H)
            .center()
            .radius(8.)
            .child(kit::chevron(
                "glyph",
                open,
                Turn::Flip,
                16.,
                s.palette.text_muted,
            )),
    )
    .label("More options")
    .on_press(move |s: &mut State| toggle_dropdown(s, which))
}

fn dropdown(key: &str, trigger: El, open: bool) -> Menu {
    Menu::new(key.to_string())
        .trigger(trigger)
        .open(open)
        .placement(Placement::BottomStart)
        .min_width(WIDTH - 16.)
        .on_close(close_menus)
}

// ------------------------------------------------------------ sections

/// One card section (t3code's `ThreadDetailsSection`).
struct Section {
    key: &'static str,
    title: String,
    heading: bool,
    separated: bool,
    actions: Vec<El>,
    rows: Vec<El>,
    /// The height its rows ask for to fit; lists that scroll count nothing,
    /// as in t3code, so asking to see more never hides the controls.
    fit: f32,
}

impl Section {
    fn new(key: &'static str, title: impl Into<String>) -> Self {
        Self {
            key,
            title: title.into(),
            heading: true,
            separated: true,
            actions: Vec::new(),
            rows: Vec::new(),
            fit: 0.,
        }
    }

    /// A row that counts toward the card's height.
    fn row(&mut self, e: El) {
        self.rows.push(e);
        self.fit += ROW_H;
    }

    fn height(&self) -> f32 {
        18. + if self.heading { 36. } else { 0. } + self.fit
    }

    fn render(self, s: &State) -> El {
        let p = &s.palette;
        let mut out = col().key(self.key).w_full().px(8.).pt(8.).pb(10.);
        if self.heading {
            out = out.child(
                row()
                    .key("heading")
                    .w_full()
                    .h(ROW_H)
                    .px(10.)
                    .mb(4.)
                    .gap(4.)
                    .items_center()
                    .child(
                        ui::line(self.title, TextStyle::new(rem(11.), 500.), p.text_muted)
                            .grow(1.)
                            .min_w(0.),
                    )
                    .children(self.actions),
            );
        }
        out.children(self.rows)
    }
}

fn rem(px: f32) -> f32 {
    crate::design::styles::rem(px)
}

/// A list that scrolls once it is taller than [`LIST_MAX`].
fn list(key: &str, rows: Vec<El>) -> El {
    col()
        .key(key.to_string())
        .w_full()
        .max_h(LIST_MAX)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .child(col().key("rows").w_full().children(rows))
}

struct Ctx<'a> {
    s: &'a State,
    k: &'a Kit,
    x: &'a SideState,
    v: Visibility,
    density: Density,
    thread: String,
}

fn sections(c: &Ctx, menus: &mut Vec<El>) -> Vec<Section> {
    let full = c.density == Density::Full;
    [
        workspace(c, menus),
        version_control(c, menus),
        if full && c.v.get(Item::Servers) {
            servers_section(c)
        } else {
            None
        },
        if full && c.v.get(Item::Automations) {
            automations(c)
        } else {
            None
        },
        if full && c.v.get(Item::Lineage) {
            lineage(c)
        } else {
            None
        },
        if full && c.v.get(Item::SideChats) {
            Some(side_chats(c))
        } else {
            None
        },
        if c.v.get(Item::Usage) { usage(c) } else { None },
        if c.v.get(Item::Context) {
            context(c)
        } else {
            None
        },
    ]
    .into_iter()
    .flatten()
    .filter(|sec| !sec.rows.is_empty())
    .collect()
}

fn workspace(c: &Ctx, menus: &mut Vec<El>) -> Option<Section> {
    let (s, x) = (c.s, c.x);
    let mut sec = Section::new("thread-details-workspace", "Workspace");
    sec.heading = false;
    sec.separated = false;
    let remote = crate::remote::backend_is_remote(s);
    if c.density == Density::Full && c.v.get(Item::Environment) {
        sec.row(environment_row(c, remote, menus));
    }
    // The folder the coding tabs read (the worktree, when it has one).
    let folder = x.folder.clone();
    if c.density != Density::Essential
        && c.v.get(Item::OpenIn)
        && !remote
        && let (Some(folder), Some(editor)) = (folder, open_in::preferred(&s.prefs.open_in_editor))
    {
        let primary = pressable(
            s,
            base("thread-details-open-in-primary")
                .grow(1.)
                .min_w(0.)
                .child(glyph(s, &icons::CODE))
                .child(label(s, format!("Open in {}", editor.label), true)),
        )
        .on_press(move |_: &mut State| open_in::open(editor, &folder));
        let open = x.details.dropdown == Some(Dropdown::OpenIn);
        let mut menu = dropdown(
            "thread-details-open-in",
            chevron(s, "thread-details-open-in", Dropdown::OpenIn),
            open,
        );
        for e in open_in::editors() {
            let app = e.app;
            menu = menu.item(MenuItem::new(e.label).checked(app == editor.app).on_select(
                move |s: &mut State| {
                    s.update_prefs(|p| p.open_in_editor = app.into());
                    close_menus(s);
                },
            ));
        }
        let chevron = menu.build(c.k, menus);
        sec.row(split("thread-details-open-in-row", primary, chevron));
    }
    Some(sec)
}

fn environment_row(c: &Ctx, remote: bool, menus: &mut Vec<El>) -> El {
    let (s, x) = (c.s, c.x);
    let computer = crate::remote::computer_label(s);
    let (lead, text, path): (&'static rune::Icon, String, Option<String>) = if remote {
        (&icons::MONITOR, computer.clone(), None)
    } else if let Some(wt) = &x.worktree {
        (
            &icons::FOLDER_GIT_2,
            "Worktree".into(),
            Some(wt.path.clone()),
        )
    } else if let Some(folder) = &x.folder {
        (
            &icons::FOLDER,
            short_path(folder),
            Some(folder.display().to_string()),
        )
    } else {
        (&icons::FOLDER, "No project".into(), None)
    };
    let mut trigger = base("thread-details-environment-row")
        .child(glyph(s, lead))
        .child(label(s, text.clone(), true));
    if remote {
        // The workspace icon trails, so every row's label starts on one edge.
        trigger = trigger.child(icon(&icons::FOLDER, 14.).color(s.palette.text_faint));
    }
    let trigger = pressable(s, trigger)
        .label(format!("Run context: {computer}, {text}"))
        .help(path.unwrap_or_else(|| text.clone()))
        .on_press(|s: &mut State| toggle_dropdown(s, Dropdown::Environment));
    let open = x.details.dropdown == Some(Dropdown::Environment);
    let mut menu = dropdown("thread-details-environment", trigger, open)
        .group_label("Run on")
        .item(
            MenuItem::new(computer)
                .icon(&icons::MONITOR)
                .checked(true)
                .on_select(close_menus),
        )
        .item(
            MenuItem::new("Manage computers…").on_select(|s: &mut State| {
                close(s);
                super::show(s, Tab::Machines);
            }),
        )
        .separator()
        .group_label("Workspace");
    if let Some(folder) = x.folder.as_deref().filter(|_| !remote) {
        menu = menu.item(
            MenuItem::new(short_path(folder))
                .icon(if x.worktree.is_some() {
                    &icons::FOLDER_GIT_2
                } else {
                    &icons::FOLDER
                })
                .checked(true)
                .on_select(close_menus),
        );
    }
    menu.item(
        MenuItem::new("Choose folder…")
            .icon(&icons::FOLDER_OPEN)
            .disabled(remote)
            .on_select_tx(|s: &mut State, tx| {
                close(s);
                pick_project_folder(tx);
            }),
    )
    .build(c.k, menus)
}

/// The git quick action's kind (t3code's `resolveQuickAction`, without
/// pull request creation: ZLO opens pull requests from the agent, not here).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QuickKind {
    /// Opens the Changes tab's commit box.
    Commit,
    CommitPush,
    Push,
    Pull,
    /// Nothing to do; the hint says why.
    Hint(&'static str),
}

fn quick_action(branch: &git::Branch, dirty: bool) -> (&'static str, QuickKind) {
    if branch.name.is_empty() || branch.name == "HEAD" {
        return (
            "Commit",
            QuickKind::Hint("Create and check out a branch before pushing."),
        );
    }
    if dirty {
        return if branch.behind > 0 {
            ("Commit", QuickKind::Commit)
        } else {
            ("Commit & push", QuickKind::CommitPush)
        };
    }
    if branch.upstream.is_none() {
        return ("Push", QuickKind::Push);
    }
    match (branch.ahead, branch.behind) {
        (a, b) if a > 0 && b > 0 => (
            "Sync branch",
            QuickKind::Hint("Branch has diverged from upstream. Rebase/merge first."),
        ),
        (_, b) if b > 0 => ("Pull", QuickKind::Pull),
        (a, _) if a > 0 => ("Push", QuickKind::Push),
        _ => (
            "Commit",
            QuickKind::Hint("Branch is up to date. No action needed."),
        ),
    }
}

fn open_changes(s: &mut State) {
    close(s);
    super::show(s, Tab::Changes);
}

fn version_control(c: &Ctx, menus: &mut Vec<El>) -> Option<Section> {
    let (s, x) = (c.s, c.x);
    let p = &s.palette;
    let mut sec = Section::new("thread-details-version-control", "Version Control");
    sec.heading = false;
    sec.separated = c.density == Density::Full;
    let remote = crate::remote::backend_is_remote(s);
    let summary = changes::summary(x).filter(|_| x.repo.is_some());
    if c.v.get(Item::Branch) {
        if let Some(sum) = &summary {
            let b = sum.branch;
            let mut r = base("thread-details-branch")
                .child(glyph(s, &icons::GIT_BRANCH))
                .child(label(s, b.name.clone(), true));
            let sync = match (b.ahead, b.behind) {
                (0, 0) => None,
                (a, 0) => Some(format!("↑{a}")),
                (0, d) => Some(format!("↓{d}")),
                (a, d) => Some(format!("↑{a} ↓{d}")),
            };
            if let Some(sync) = sync {
                r = r.child(meta(s, sync));
            }
            sec.row(
                r.help(
                    b.upstream
                        .clone()
                        .unwrap_or_else(|| "No upstream yet".into()),
                ),
            );
        }
        if let Some(pr) = x.details.pr.as_ref().filter(|_| x.repo.is_some()) {
            let url = pr.url.clone();
            sec.row(
                pressable(
                    s,
                    base("thread-details-pr")
                        .child(glyph(s, &icons::GIT_PULL_REQUEST))
                        .child(label(s, format!("#{} {}", pr.number, pr.title), true))
                        .child(meta(s, pr.state_label())),
                )
                .label(format!("Open pull request #{}", pr.number))
                .on_press_tx(move |_: &mut State, tx| {
                    if let Err(e) = tx.open_url(&url) {
                        tracing::warn!(error = ?e, "could not open the pull request");
                    }
                }),
            );
        }
    }
    if x.repo.is_none()
        && !remote
        && c.v.get(Item::GitActions)
        && let Some(folder) = x.folder.clone()
    {
        let busy = x.details.initializing;
        let mut r = base("thread-details-git-init")
            .child(glyph(s, &icons::GIT_BRANCH_PLUS))
            .child(label(
                s,
                if busy {
                    "Initializing..."
                } else {
                    "Initialize Git"
                },
                !busy,
            ));
        if !busy {
            let thread = c.thread.clone();
            r = pressable(s, r)
                .on_press(move |s: &mut State| init_git(s, thread.clone(), folder.clone()));
        }
        sec.row(r);
    }
    if let Some(sum) = summary.as_ref().filter(|_| c.v.get(Item::GitActions)) {
        let running = sum.busy.map(|b| match b {
            "pull" => "Pulling…",
            "push" => "Pushing…",
            "commit" => "Committing…",
            _ => "Working…",
        });
        // Compact cards keep the quick action only while it works.
        if running.is_some() || c.density == Density::Full {
            let (text, kind) = quick_action(sum.branch, sum.dirty);
            let needs_mac = matches!(kind, QuickKind::Push | QuickKind::Pull) && remote;
            let hint = match kind {
                QuickKind::Hint(h) => Some(h.to_string()),
                _ if needs_mac => Some(format!(
                    "Push and pull aren't available on {} yet.",
                    crate::remote::computer_label(s)
                )),
                _ => None,
            };
            let live = running.is_none() && hint.is_none();
            let lead = match kind {
                QuickKind::Pull => &icons::ARROW_LEFT,
                QuickKind::Push | QuickKind::CommitPush => &icons::UPLOAD,
                _ => &icons::GIT_COMMIT_HORIZONTAL,
            };
            let mut primary = base("thread-details-git-primary")
                .grow(1.)
                .min_w(0.)
                .child(glyph(s, lead))
                .child(label(s, running.unwrap_or(text), live));
            if running.is_some() {
                primary = primary.child(crate::design::kit::Spinner::new().size(12.).build(c.k));
            }
            if let Some(hint) = hint {
                primary = primary.help(hint);
            } else if live {
                primary =
                    pressable(s, primary)
                        .label(text)
                        .on_press(move |s: &mut State| match kind {
                            QuickKind::Commit | QuickKind::CommitPush => open_changes(s),
                            QuickKind::Push => changes::push(s),
                            QuickKind::Pull => changes::pull(s),
                            QuickKind::Hint(_) => {}
                        });
            }
            let open = x.details.dropdown == Some(Dropdown::Git);
            let can_sync = !remote && running.is_none();
            let chevron = dropdown(
                "thread-details-git",
                chevron(s, "thread-details-git", Dropdown::Git).label("Git action options"),
                open,
            )
            .item(
                MenuItem::new("Commit…")
                    .icon(&icons::GIT_COMMIT_HORIZONTAL)
                    .on_select(open_changes),
            )
            .item(
                MenuItem::new("Push")
                    .icon(&icons::UPLOAD)
                    .disabled(!can_sync)
                    .on_select(|s: &mut State| {
                        close_menus(s);
                        changes::push(s);
                    }),
            )
            .item(
                MenuItem::new("Pull")
                    .icon(&icons::ARROW_LEFT)
                    .disabled(!can_sync || sum.branch.upstream.is_none())
                    .on_select(|s: &mut State| {
                        close_menus(s);
                        changes::pull(s);
                    }),
            )
            .build(c.k, menus);
            sec.row(split("thread-details-git-row", primary, chevron));
        }
    }
    if let Some(sum) = summary.as_ref().filter(|_| c.v.get(Item::Changes)) {
        let count = |n: usize, sign: &str, color: Color| {
            ui::line(
                format!("{sign}{n}"),
                ui::MONO_XS,
                if n > 0 { color } else { p.text_faint },
            )
        };
        sec.row(
            pressable(
                s,
                base("thread-details-changes")
                    .child(glyph(s, &icons::FILE_DIFF))
                    .child(label(s, "Changes", true))
                    .child(count(sum.additions, "+", p.success))
                    .child(count(sum.deletions, "-", p.danger)),
            )
            .label("Open changes")
            .on_press(open_changes),
        );
    }
    Some(sec)
}

fn init_git(s: &mut State, thread: String, folder: std::path::PathBuf) {
    let Some(x) = side_of(s, &thread) else { return };
    x.details.initializing = true;
    let dir = folder.clone();
    jobs::blocking(
        s,
        move || git::init(&dir),
        move |s, result, tx| {
            // The user moved to another thread: its folder and banner are not ours.
            let Some(x) = side_of(s, &thread) else { return };
            // The answer to the person's "Initialize": the banner leaves
            // with motion.
            tx.animated();
            x.details.initializing = false;
            match result {
                // Re-reading the folder finds the new repository.
                Ok(()) => super::set_folder(s, folder.clone(), false),
                Err(e) => bridge::set_error(s, e),
            }
        },
    );
}

fn servers_section(c: &Ctx) -> Option<Section> {
    let (s, x) = (c.s, c.x);
    let all = &x.details.servers;
    if all.is_empty() {
        return None;
    }
    let mut sec = Section::new("thread-details-servers", "Servers");
    let shown = if x.details.servers_more {
        all.len()
    } else {
        all.len().min(SERVER_PAGE)
    };
    for (ix, server) in all[..shown].iter().enumerate() {
        let url = format!("http://localhost:{}", server.port);
        let pid = server.pid;
        let target = server.clone();
        let stopping = x.details.stopping == Some(pid);
        let thread = c.thread.clone();
        let stop = ui::ibtn(&icons::SQUARE, "Stop server")
            .label(format!("Stop localhost:{}", server.port))
            .disabled(x.details.stopping.is_some())
            .on_press(move |s: &mut State| stop_server(s, thread.clone(), target.clone()))
            .build(c.k)
            .key(format!("thread-details-server-stop-{}", server.port));
        let mut r = base(format!("thread-details-server-{}", server.port))
            .child(glyph(s, &icons::SERVER))
            .child(label(s, format!("localhost:{}", server.port), true))
            .child(meta(s, server.process.clone()));
        r = if stopping {
            r.child(crate::design::kit::Spinner::new().size(12.).build(c.k))
        } else {
            r.child(stop)
        };
        let e =
            pressable(s, r)
                .help("Open in the browser panel.")
                .on_press(move |s: &mut State| {
                    close(s);
                    super::browser::go(s, url.clone());
                    super::show(s, Tab::Browser);
                });
        // As t3code: the overflow past the first page never counts toward
        // the card's fit, so "Show more" can't drop the card's density.
        if ix < SERVER_PAGE {
            sec.row(e);
        } else {
            sec.rows.push(e);
        }
    }
    if all.len() > SERVER_PAGE {
        let more = x.details.servers_more;
        let text = if more {
            "Show less".to_string()
        } else {
            format!("Show {} more", all.len() - SERVER_PAGE)
        };
        sec.row(
            pressable(
                s,
                base("thread-details-servers-more")
                    .child(
                        icon(if more { &icons::MINUS } else { &icons::PLUS }, 16.)
                            .color(s.palette.text_faint),
                    )
                    .child(label(s, text, false)),
            )
            .on_press(|s: &mut State| {
                if let Some(x) = side_mut(s) {
                    x.details.servers_more = !x.details.servers_more;
                }
            }),
        );
    }
    Some(sec)
}

fn stop_server(s: &mut State, thread: String, server: servers::Server) {
    let Some(x) = side_of(s, &thread) else { return };
    let Some(dir) = x.folder.clone() else { return };
    x.details.stopping = Some(server.pid);
    let folder = Some(dir.clone());
    jobs::blocking(
        s,
        move || servers::stop(&server, &dir),
        move |s, result, tx| {
            let Some(x) = side_of(s, &thread) else { return };
            x.details.stopping = None;
            // The person's Stop: its spinner settles back with motion.
            tx.animated();
            let generation = x.folder_gen;
            if let Err(e) = result {
                bridge::set_error(s, e);
            }
            if let Some(folder) = folder {
                refresh_servers(s, thread.clone(), folder, generation);
            }
        },
    );
}

fn automations(c: &Ctx) -> Option<Section> {
    let (s, x) = (c.s, c.x);
    let p = &s.palette;
    let bound = routines::bound(x, &c.thread);
    if bound.is_empty() {
        return None;
    }
    let mut sec = Section::new("thread-details-automations", "Automations");
    sec.actions.push(
        ui::ibtn(&icons::SETTINGS_2, "Manage scheduled tasks")
            .on_press(manage_automations)
            .build(c.k)
            .key("thread-details-automations-manage"),
    );
    let rows = bound
        .into_iter()
        .map(|(sch, asked)| {
            let busy = asked.is_some();
            let id = sch.id.clone();
            let next = sch
                .next_run_at
                .filter(|_| sch.enabled)
                .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
                .map(routines::until);
            let line = match (sch.enabled, next) {
                (false, _) => format!("{} · paused", sch.describe),
                (true, Some(n)) => format!("{} · next {n}", sch.describe),
                (true, None) => sch.describe.clone(),
            };
            let dot = match sch.last_outcome() {
                zlo_client::schedules::LastOutcome::Never => p.text_faint,
                zlo_client::schedules::LastOutcome::Queued(_) => p.success,
                zlo_client::schedules::LastOutcome::Failed(_) => p.danger,
            };
            let mark = el()
                .size(16., 16.)
                .child(glyph(s, &icons::CALENDAR_CLOCK))
                .child(ui::dot(dot, 6.).absolute(12., -2.));
            let mut r = row()
                .key(format!("thread-details-automation-{id}"))
                .w_full()
                .py(6.)
                .px(10.)
                .gap(10.)
                .items_center()
                .radius(8.)
                .child(mark)
                .child(
                    col()
                        .grow(1.)
                        .min_w(0.)
                        .child(ui::line(sch.title.clone(), sm_med(), p.text))
                        .child(ui::line(line, xs(), p.text_faint)),
                )
                .child(
                    ui::ibtn(&icons::PENCIL, "Edit automation")
                        .label(format!("Edit {}", sch.title))
                        .on_press(manage_automations)
                        .build(c.k)
                        .key(format!("thread-details-automation-edit-{id}")),
                );
            // Webhook tasks need a request from their URL.
            if !sch.is_webhook() {
                let run = id.clone();
                r = r.child(
                    super::busy::icon_button(
                        c.k,
                        format!("thread-details-automation-run-{id}"),
                        &icons::PLAY,
                        "Run now",
                        routines::busy_of(asked.as_ref(), "run"),
                        move |s: &mut State| routines::schedule_action(s, run.clone(), None),
                    )
                    .label(format!("Run {} now", sch.title)),
                );
            }
            let enabled = sch.enabled;
            let toggle = id.clone();
            r.child(
                crate::pages::common::switch(
                    c.k,
                    enabled,
                    busy,
                    &format!("{} {}", if enabled { "Pause" } else { "Resume" }, sch.title),
                    move |s: &mut State| {
                        routines::schedule_action(s, toggle.clone(), Some(!enabled))
                    },
                )
                .key(format!("thread-details-automation-switch-{id}")),
            )
        })
        .collect();
    sec.rows.push(list("thread-details-automations-list", rows));
    Some(sec)
}

fn manage_automations(s: &mut State) {
    close(s);
    crate::workspace::show(s, Page::Automations);
}

/// One lineage or side chat row.
struct Rel {
    key: String,
    glyph: &'static rune::Icon,
    title: String,
    /// The trailing status or elapsed time.
    status: Option<String>,
    failed: bool,
    /// The thread a press opens.
    thread: Option<String>,
    /// A live subagent's task, which a stop button cancels.
    stop: Option<String>,
}

fn rel_row(c: &Ctx, r: Rel) -> El {
    let s = c.s;
    let p = &s.palette;
    let mut e = base(r.key.clone())
        .child(glyph(s, r.glyph))
        .child(label(s, r.title.clone(), true));
    if let Some(status) = r.status {
        e = e.child(ui::line(
            status,
            xs(),
            if r.failed { p.danger } else { p.text_faint },
        ));
    }
    if let Some(task) = r.stop {
        e = e.child(
            ui::ibtn(&icons::SQUARE, "Stop subagent")
                .label(format!("Stop subagent {}", r.title))
                .on_press(move |s: &mut State| bridge::cancel_task(s, task.clone()))
                .build(c.k)
                .key(format!("{}-stop", r.key)),
        );
    }
    match r.thread {
        Some(thread) => pressable(s, e).on_press(move |s: &mut State| {
            close(s);
            crate::workspace::open_thread(s, thread.clone());
        }),
        None => e,
    }
}

fn task_title(task: &TaskView) -> String {
    if task.title.is_empty() {
        "Subagent".into()
    } else {
        task.title.clone()
    }
}

fn task_rel(task: &TaskView, live: bool) -> Rel {
    let failed = matches!(task.status, TaskStatus::Failed | TaskStatus::Interrupted);
    Rel {
        key: format!("thread-details-agent-{}", task.task_id),
        glyph: &icons::BOT,
        title: task_title(task),
        status: if live && !failed {
            agents::elapsed(task)
        } else {
            Some(task.status.label().to_string())
        },
        failed,
        thread: task.child_thread_id.clone(),
        stop: (live && !task.cancel_requested).then(|| task.task_id.clone()),
    }
}

fn lineage(c: &Ctx) -> Option<Section> {
    let s = c.s;
    let p = &s.palette;
    let side_chats = session::chats(s);
    let mut related = Vec::new();
    let mut delegated = false;
    if let Some(lineage) = bridge::lineage(s) {
        if let Some(task) = &lineage.delegated_from {
            delegated = true;
            let parent = task.parent_thread_id.clone();
            related.push(Rel {
                key: "thread-details-parent-agent".into(),
                glyph: &icons::CORNER_LEFT_UP,
                title: bridge::title(s, &parent).unwrap_or_else(|| "Parent agent".into()),
                status: None,
                failed: false,
                thread: Some(parent),
                stop: None,
            });
        }
        if let Some(from) = &lineage.forked_from {
            let source = from.source_thread.clone();
            related.push(Rel {
                key: "thread-details-forked-from".into(),
                glyph: &icons::CORNER_LEFT_UP,
                title: bridge::title(s, &source).unwrap_or_else(|| "Parent thread".into()),
                status: None,
                failed: false,
                thread: Some(source),
                stop: None,
            });
        }
        for (ix, fork) in lineage.forks.iter().enumerate() {
            let Some(target) = fork.target_thread.clone() else {
                continue;
            };
            // Side chats are forks too; they list under Side chats.
            if side_chats.contains(&target) {
                continue;
            }
            related.push(Rel {
                key: format!("thread-details-fork-{ix}"),
                glyph: &icons::GIT_FORK,
                title: bridge::title(s, &target).unwrap_or_else(|| "Fork".into()),
                status: None,
                failed: false,
                thread: Some(target),
                stop: None,
            });
        }
    }
    let mut tasks: Vec<&TaskView> = bridge::tasks(s).iter().collect();
    tasks.sort_by_key(|t| -t.created_at);
    let (active, previous): (Vec<&TaskView>, Vec<&TaskView>) =
        tasks.into_iter().partition(|t| t.status.is_live());
    let running = active
        .iter()
        .filter(|t| t.status == TaskStatus::Running)
        .count();
    if related.is_empty() && active.is_empty() && previous.is_empty() {
        return None;
    }
    let title = if running > 0 {
        format!("Lineage · {running} running")
    } else {
        "Lineage".into()
    };
    let mut sec = Section::new("thread-details-lineage", title);
    // A lone "Previous agents" row names itself.
    sec.heading = running > 0 || delegated || !related.is_empty() || !active.is_empty();
    let mut rows: Vec<El> = related.into_iter().map(|r| rel_row(c, r)).collect();
    rows.extend(active.iter().map(|t| rel_row(c, task_rel(t, true))));
    if !previous.is_empty() {
        let open = c.x.details.previous_open;
        let failed = previous
            .iter()
            .filter(|t| t.status == TaskStatus::Failed)
            .count();
        let mut head = base("thread-details-previous-agents")
            .child(glyph(s, &icons::BOT))
            .child(label(s, "Previous agents", true));
        if failed > 0 {
            head = head.child(ui::line(format!("{failed} failed"), xs(), p.danger));
        }
        if !open {
            head = head.child(meta(s, previous.len().to_string()));
        }
        head = head.child(kit::chevron(
            "previous-agents-chevron",
            open,
            Turn::Disclose,
            16.,
            p.text_muted,
        ));
        rows.push(pressable(s, head).expanded(open).on_press(|s: &mut State| {
            if let Some(x) = side_mut(s) {
                x.details.previous_open = !x.details.previous_open;
                x.details.previous_shown = 0;
            }
        }));
        if open {
            let shown = (LINEAGE_FIRST + c.x.details.previous_shown).min(previous.len());
            rows.extend(
                previous[..shown]
                    .iter()
                    .map(|t| rel_row(c, task_rel(t, false))),
            );
            let hidden = previous.len() - shown;
            if hidden > 0 {
                rows.push(
                    pressable(
                        s,
                        base("thread-details-previous-more")
                            .child(icon(&icons::PLUS, 16.).color(p.text_faint))
                            .child(label(
                                s,
                                format!("Show {} more", hidden.min(LINEAGE_PAGE)),
                                false,
                            )),
                    )
                    .on_press(|s: &mut State| {
                        if let Some(x) = side_mut(s) {
                            x.details.previous_shown += LINEAGE_PAGE;
                        }
                    }),
                );
            }
        }
    }
    sec.rows.push(list("thread-details-lineage-list", rows));
    Some(sec)
}

/// A side chat runs: the one open in the pane, or any the inbox says works.
fn side_chat_running(s: &State, id: &str) -> bool {
    let open = side(s)
        .and_then(|x| x.chat.as_ref())
        .is_some_and(|c| c.thread_id == id);
    (open && bridge::side_chat_running(s))
        || s.workspace
            .inbox
            .row(id)
            .is_some_and(|r| r.status == Some(InboxStatus::Working))
}

fn side_chats(c: &Ctx) -> Section {
    let (s, x) = (c.s, c.x);
    let p = &s.palette;
    let chats = session::chats(s);
    let running = chats.iter().filter(|id| side_chat_running(s, id)).count();
    let title = if running > 0 {
        format!("Side chats · {running} running")
    } else {
        "Side chats".into()
    };
    let mut sec = Section::new("thread-details-side-chats", title);
    let open_id = x.chat.as_ref().map(|c| c.thread_id.clone());
    // The open side chat's wash is one pill gliding between rows (rows
    // are ROW_H tall, no gap), not a fill that jumps.
    let shown_ix = chats
        .iter()
        .position(|id| open_id.as_deref() == Some(id.as_str()));
    let pill = kit::slide_pill_sized(
        s,
        "side-chats-pill",
        shown_ix.map(|ix| ix as f32 * ROW_H),
        ROW_H,
    )
    .map(|pill| pill.fill(p.active));
    let rows: Vec<El> = chats
        .into_iter()
        .map(|id| {
            let live = side_chat_running(s, &id);
            let name = bridge::title(s, &id).unwrap_or_else(|| "Side chat".into());
            let mut item = base(format!("session-side-chat-{id}"))
                .child(glyph(s, &icons::MESSAGES_SQUARE))
                .child(label(s, name.clone(), true));
            if live {
                item = item.child(crate::design::kit::Spinner::new().size(12.).build(c.k));
            }
            pressable(s, item)
                .label(format!("Open side chat {name}"))
                .on_press(move |s: &mut State| session::open(s, id.clone()))
        })
        .collect();
    if !rows.is_empty() {
        sec.rows.push(list(
            "thread-details-side-chats-list",
            pill.into_iter().chain(rows).collect(),
        ));
    }
    // ZLO's own: t3code only lists side chats; here they start from the card.
    let can_fork = !bridge::rows(s).is_empty();
    let busy = x.chat_busy;
    let ready = can_fork && !busy;
    let mut new_chat = base("session-new-side-chat")
        .child(glyph(s, &icons::PLUS))
        .child(label(s, "New side chat", ready))
        .label("New side chat");
    new_chat = if busy {
        new_chat.child(crate::design::kit::Spinner::new().size(12.).build(c.k))
    } else {
        new_chat.child(meta(s, "⌥⌘N"))
    };
    if ready {
        new_chat = pressable(s, new_chat).on_press(session::new_side_chat);
    }
    sec.row(new_chat);
    let mut fork = base("session-fork")
        .child(glyph(s, &icons::GIT_FORK))
        .child(label(s, "Fork chat", can_fork))
        .label("Fork chat");
    if can_fork {
        fork = pressable(s, fork).on_press(|s: &mut State| {
            close(s);
            bridge::fork(s);
        });
    }
    sec.row(fork);
    if !can_fork {
        sec.rows
            .push(el().px(10.).pb(4.).child(meta(s, "Send a message first.")));
        sec.fit += 20.;
    }
    sec
}

/// The product family a provider or runtime id belongs to, so a thread's
/// runtime finds its usage windows (`claude_cli_2` and `claude_code` are
/// both Claude).
fn family(id: &str) -> Option<&'static str> {
    let id = id.to_ascii_lowercase();
    [
        (&["claude", "anthropic"][..], "claude"),
        (&["codex", "openai", "gpt"][..], "codex"),
        (&["zai", "z.ai", "glm"][..], "zai"),
        (&["antigravity", "agy", "gemini"][..], "antigravity"),
        (&["muse"][..], "muse"),
    ]
    .into_iter()
    .find(|(keys, _)| keys.iter().any(|k| id.contains(k)))
    .map(|(_, f)| f)
}

/// The usage provider of this thread's runtime or model provider.
fn thread_provider(s: &State) -> Option<&zlo_client::usage::ProviderUsage> {
    let tel = s.workspace.telemetry.as_ref();
    let keys = [
        bridge::thread_runtime(s).map(str::to_string),
        tel.and_then(|t| t.provider.clone()),
        tel.map(|t| t.runtime_id.clone()),
        tel.and_then(|t| t.model.clone()),
    ];
    let want = keys.iter().flatten().find_map(|k| family(k))?;
    let report = crate::pages::usage::report(s)?;
    let mut matching = report
        .providers
        .iter()
        .filter(|p| family(&p.provider) == Some(want));
    let first = matching.next()?;
    Some(
        std::iter::once(first)
            .chain(matching)
            .find(|p| p.status == zlo_client::usage::UsageStatus::Ok)
            .unwrap_or(first),
    )
}

/// One label · bar · value row (t3code's `UsageRow`).
fn meter(s: &State, key: String, name: &str, fill: Option<f64>, tone: Color, value: String) -> El {
    let p = &s.palette;
    let mut r = row()
        .key(key)
        .w_full()
        .h(METER_H)
        .px(10.)
        .gap(12.)
        .items_center()
        .radius(8.)
        .child(ui::line(name.to_string(), xs(), p.text_muted).w(84.));
    match fill {
        Some(f) => {
            let f = f.clamp(0., 100.) as f32;
            r = r.child(
                row()
                    .grow(1.)
                    .h(4.)
                    .radius(2.)
                    .fill(p.border)
                    .child(
                        el().key("fill")
                            .grow(f.max(0.01))
                            .min_w(if f > 0. { 6. } else { 0. })
                            .motion(crate::design::motion::PILL)
                            .h(4.)
                            .radius(2.)
                            .fill(if f > 0. { tone } else { Color::TRANSPARENT }),
                    )
                    .child(
                        el().key("rest")
                            .grow((100. - f).max(0.01))
                            .motion(crate::design::motion::PILL),
                    ),
            );
        }
        None => r = r.child(ui::grow()),
    }
    r.child(ui::line(value, xs_med(), tone))
}

fn open_usage(s: &mut State) {
    close(s);
    crate::workspace::show(s, Page::Usage);
}

fn usage(c: &Ctx) -> Option<Section> {
    let s = c.s;
    let p = &s.palette;
    let provider = thread_provider(s)?;
    let mut sec = Section::new("thread-details-usage", "Usage limits");
    sec.heading = false;
    if provider.status != zlo_client::usage::UsageStatus::Ok || provider.windows.is_empty() {
        let note = provider
            .message
            .clone()
            .unwrap_or_else(|| "No limits reported.".into());
        sec.rows.push(
            pressable(
                s,
                meter(
                    s,
                    "thread-details-usage-none".into(),
                    "Limits",
                    None,
                    p.text_faint,
                    "Unavailable".into(),
                ),
            )
            .help(note)
            .on_press(open_usage),
        );
        sec.fit += METER_H;
        return Some(sec);
    }
    for w in &provider.windows {
        let left = (100. - w.used_percent).clamp(0., 100.).round();
        let tone = if left <= 10. {
            p.danger
        } else if left <= 20. {
            p.warning
        } else {
            p.text_muted
        };
        let mut tip = format!("{}: {left:.0}% left", w.label);
        if let Some(at) = w.resets_at {
            tip.push_str(&format!(
                " · Resets in {}",
                crate::pages::usage::countdown(at)
            ));
        }
        tip.push_str(" · Open usage");
        sec.rows.push(
            pressable(
                s,
                meter(
                    s,
                    format!("thread-details-usage-{}", w.id),
                    &w.label,
                    Some(left),
                    tone,
                    format!("{left:.0}%"),
                ),
            )
            .label(format!("{}: {left:.0}% left", w.label))
            .help(tip)
            .on_press(open_usage),
        );
        sec.fit += METER_H;
    }
    Some(sec)
}

fn context(c: &Ctx) -> Option<Section> {
    let s = c.s;
    let p = &s.palette;
    let tel = s.workspace.telemetry.as_ref()?;
    let used = tel.context_used?;
    let max = tel.context_max.filter(|m| *m > 0);
    let pct = max.map(|m| {
        tel.context_pct
            .unwrap_or(used as f64 * 100. / m as f64)
            .clamp(0., 100.)
    });
    let tone = match pct {
        Some(v) if v >= 90. => p.danger,
        Some(v) if v >= 80. => p.warning,
        _ => p.text_faint,
    };
    let short = crate::workspace::short_tokens;
    let value = match max {
        Some(m) => format!("{} / {}", short(used), short(m)),
        None => short(used),
    };
    let tip = match pct {
        Some(v) => format!("Context: {v:.0}% used"),
        None => format!("Context: {} tokens used", short(used)),
    };
    let mut sec = Section::new("thread-details-context", "Context window");
    sec.heading = false;
    sec.rows.push(
        meter(
            s,
            "thread-details-context-row".into(),
            "Context",
            pct,
            tone,
            value,
        )
        .help(tip),
    );
    sec.fit += METER_H;
    Some(sec)
}

/// The "Show in thread details" menu body, shared by the card's right-click
/// menu and its "Choose what to show" row.
fn items_menu(s: &State, mut menu: Menu) -> Menu {
    let v = visible(s);
    menu = menu.group_label("Show in thread details");
    for d in &ITEMS {
        let item = d.item;
        let on = v.get(item);
        menu = menu.item(
            MenuItem::new(d.label)
                .checked(on)
                .on_select(move |s: &mut State| {
                    s.update_prefs(|p| items::set(&mut p.thread_details_items, item, !on));
                    if !on {
                        opened(s);
                    }
                }),
        );
    }
    menu.separator().item(
        MenuItem::new("Reset to default")
            .disabled(v.is_default())
            .on_select(|s: &mut State| {
                s.update_prefs(|p| p.thread_details_items.clear());
                close_menus(s);
                opened(s);
            }),
    )
}

/// The open card, under the toggle keyed `anchor`: pushed into `layer` with
/// its menus after it. `window_h` is the window's height.
pub(super) fn card(s: &State, k: &Kit, layer: &mut Vec<El>, anchor: &str, window_h: f32) {
    let p = &s.palette;
    let Some(x) = side(s) else { return };
    let Some(thread) = thread_id(s) else { return };
    let available = (window_h - crate::design::TITLEBAR_HEIGHT - 6. - GAP).max(160.);
    let ctx = |density| Ctx {
        s,
        k,
        x,
        v: visible(s),
        density,
        thread: thread.clone(),
    };
    let fit = |density| {
        let mut scratch = Vec::new();
        let secs = sections(&ctx(density), &mut scratch);
        secs.iter().map(Section::height).sum::<f32>() + secs.len().saturating_sub(1) as f32
    };
    let density = items::density(available, fit(Density::Full), fit(Density::Compact));
    let mut menus = Vec::new();
    let secs = sections(&ctx(density), &mut menus);
    let mut body = col()
        .key(format!("thread-details-{}", density_key(density)))
        .w_full();
    let empty = secs.is_empty();
    for (ix, sec) in secs.into_iter().enumerate() {
        // Keyed by the section below it, so a section arriving above does
        // not hand its rule to another.
        if ix > 0 && sec.separated {
            body = body.child(
                el().key(format!("rule-{}", sec.key))
                    .w_full()
                    .h(1.)
                    .fill(p.border),
            );
        }
        body = body.child(sec.render(s));
    }
    if empty {
        // Shown only when nothing else is, so an emptied card can be refilled.
        let open = x.details.dropdown == Some(Dropdown::Choose);
        let trigger = pressable(
            s,
            base("thread-details-choose")
                .child(glyph(s, &icons::SLIDERS_HORIZONTAL))
                .child(label(s, "Choose what to show", true)),
        )
        .on_press(|s: &mut State| toggle_dropdown(s, Dropdown::Choose));
        let choose = items_menu(s, dropdown("thread-details-choose-menu", trigger, open));
        body = body.child(el().w_full().padding(8.).child(choose.build(k, &mut menus)));
    }
    if let Some(at) = x.details.menu_at {
        let menu = items_menu(
            s,
            Menu::new("thread-details-menu")
                .open(true)
                .at(at.x, at.y)
                .min_width(220.)
                .on_close(close_menus),
        );
        // The trigger it returns is a stand-in: the press point anchors it.
        let _ = menu.build(k, &mut menus);
    }
    let card = col()
        .key("side-session-card")
        .w(WIDTH)
        .max_h(available)
        .radius(16.)
        .fill(p.surface_overlay)
        .border(1., p.border)
        .shadow(
            8.,
            24.,
            Color::BLACK.alpha(if p.light { 0.12 } else { 0.35 }),
        )
        .anchored(anchor, Side::Bottom, Align::End, 6.)
        // Drops a few points from the toggle as it fades in, as the app's
        // popovers do.
        .transition(crate::design::motion::pop(-6.))
        .modal(true, close)
        .on_secondary_press(|s: &mut State, at, _| {
            if let Some(x) = side_mut(s) {
                x.details.dropdown = None;
                x.details.menu_at = Some(at);
            }
        })
        .child(
            col()
                .key("thread-details-scroll")
                .w_full()
                .min_h(0.)
                .scroll(ScrollSpec {
                    stick_bottom: false,
                })
                .child(body),
        );
    layer.push(card);
    layer.extend(menus);
}

/// Scene data the card reads from discovery and `gh`.
pub(super) fn fixture(x: &mut SideState, ports: &[(u16, &str)], pr: Option<(u64, &str)>) {
    x.details.servers = ports
        .iter()
        .map(|(port, process)| servers::Server {
            port: *port,
            pid: 0,
            process: (*process).into(),
        })
        .collect();
    x.details.pr = pr.map(|(number, title)| git::PullRequest {
        number,
        title: title.into(),
        state: "OPEN".into(),
        url: format!("https://github.com/you/zlo/pull/{number}"),
        draft: false,
    });
}

fn density_key(d: Density) -> &'static str {
    match d {
        Density::Full => "full",
        Density::Compact => "compact",
        Density::Essential => "essential",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn branch(upstream: bool, ahead: u32, behind: u32) -> git::Branch {
        git::Branch {
            name: "feature".into(),
            upstream: upstream.then(|| "origin/feature".into()),
            ahead,
            behind,
        }
    }

    #[test]
    fn quick_action_follows_t3code() {
        assert_eq!(
            quick_action(&branch(true, 0, 0), true),
            ("Commit & push", QuickKind::CommitPush)
        );
        assert_eq!(
            quick_action(&branch(true, 0, 2), true),
            ("Commit", QuickKind::Commit)
        );
        assert_eq!(quick_action(&branch(false, 0, 0), false).1, QuickKind::Push);
        assert_eq!(
            quick_action(&branch(true, 3, 0), false),
            ("Push", QuickKind::Push)
        );
        assert_eq!(
            quick_action(&branch(true, 0, 1), false),
            ("Pull", QuickKind::Pull)
        );
        assert!(matches!(
            quick_action(&branch(true, 1, 1), false),
            ("Sync branch", QuickKind::Hint(_))
        ));
        assert!(matches!(
            quick_action(&branch(true, 0, 0), false),
            ("Commit", QuickKind::Hint(_))
        ));
    }

    fn drawn(size: (f32, f32), edit: impl FnOnce(&mut State)) -> rune::Runtime<State> {
        let mut s = crate::chat::fixtures::test_state();
        crate::chat::side::fixtures::details(&mut s);
        edit(&mut s);
        let mut rt = rune::Runtime::new(s, crate::view, size, rune::Env::new(true, 1.));
        let mut t = 0.;
        for _ in 0..30 {
            t += 1. / 60.;
            rt.frame(t);
        }
        rt
    }

    /// t3code's sections in its order, each where it should be, inside a
    /// 280-point card under the toggle.
    #[test]
    fn the_card_lists_t3codes_sections_in_order() {
        let rt = drawn((1440., 1000.), |_| {});
        let (card, _) = rt.presented("side-session-card").expect("card drawn");
        assert_eq!(card[2], WIDTH);
        assert!(
            rt.presented("thread-details-full").is_some(),
            "full density"
        );
        let order = [
            "thread-details-environment-row",
            "thread-details-open-in-row",
            "thread-details-branch",
            "thread-details-pr",
            "thread-details-git-row",
            "thread-details-changes",
            "thread-details-server-5173",
            "thread-details-automation-s1",
            "thread-details-forked-from",
            "thread-details-agent-a1",
            "thread-details-previous-agents",
            "session-side-chat-sc1",
            "session-new-side-chat",
            "session-fork",
            "thread-details-usage-5h",
            "thread-details-context-row",
        ];
        let mut last = f32::MIN;
        for key in order {
            if key == "thread-details-open-in-row" && open_in::editors().is_empty() {
                continue;
            }
            let (r, _) = rt.presented(key).unwrap_or_else(|| panic!("{key} drawn"));
            assert!(r[1] >= last, "{key} sits below the row before it");
            last = r[1];
        }
        // Side chats list under Side chats, not again as forks.
        assert!(rt.presented("thread-details-fork-1").is_none());
        assert!(rt.presented("thread-details-fork-0").is_some());
    }

    /// A short window drops the full-only rows first, as t3code's densities.
    #[test]
    fn a_short_window_steps_the_density_down() {
        let rt = drawn((1440., 420.), |_| {});
        assert!(rt.presented("thread-details-full").is_none());
        assert!(rt.presented("thread-details-environment-row").is_none());
        assert!(rt.presented("thread-details-side-chats").is_none());
        assert!(
            rt.presented("thread-details-changes").is_some(),
            "changes stay"
        );
    }

    /// Hidden items leave the card; with nothing left it offers the menu.
    #[test]
    fn hidden_items_leave_and_an_empty_card_offers_the_menu() {
        let rt = drawn((1440., 1000.), |s| {
            items::set(&mut s.prefs.thread_details_items, Item::Lineage, false);
        });
        assert!(rt.presented("thread-details-lineage").is_none());
        assert!(rt.presented("thread-details-side-chats").is_some());
        let rt = drawn((1440., 1000.), |s| {
            for d in &ITEMS {
                items::set(&mut s.prefs.thread_details_items, d.item, false);
            }
        });
        assert!(rt.presented("thread-details-choose").is_some());
    }

    #[test]
    fn providers_match_by_family() {
        assert_eq!(family("claude_cli_2"), Some("claude"));
        assert_eq!(family("claude_code"), Some("claude"));
        assert_eq!(family("codex"), Some("codex"));
        assert_eq!(family("muse_code"), Some("muse"));
        assert_eq!(family("ironclaw"), None);
    }
}
