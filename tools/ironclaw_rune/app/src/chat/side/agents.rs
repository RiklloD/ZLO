//! Agents: the whole delegation family of the open conversation — the main
//! agent at the top and every subagent under it, nested — whichever member
//! is open. A subagent's chat shows its parent and siblings too, so the tab
//! is never empty just because the open thread delegated nothing itself.
//!
//! Two sources, joined per subagent:
//! - the sidebar inbox (`GET /inbox`, polled while anything works): which
//!   threads exist, who delegated them, and whether they run now. It is the
//!   same live list the sidebar shows, so a subagent appears here the moment
//!   it appears there;
//! - the orchestrator's tasks (`/tasks`): titles, engines, summaries and
//!   errors. The open thread's come from its chat (`chat::orchestration`);
//!   the other parents' (the main agent, when a subagent is open) are read
//!   here, and kept across thread switches.
//!
//! While the tab is shown it re-reads both every few seconds (with the
//! window in front), so it updates while agents work, not only when they
//! finish. Opening an agent from the tab leaves a trail: a Back row returns
//! to where the person came from, step by step.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::Utc;
use rune::{Color, El, col, el, icon, row};
use rune_kit::Kit;
use zlo_client::ClientError;
use zlo_client::inbox::{InboxRow, InboxStatus};
use zlo_client::orchestration::{TaskStatus, TaskView};

use super::bridge;
use super::ui::{self, sm, sm_med, tiny, xs};
use super::{Dock, Tab};
use crate::design::Palette;
use crate::icons;
use crate::jobs;
use crate::state::State;

/// Refresh period while a family member works.
const LIVE_POLL: Duration = Duration::from_secs(3);
/// Refresh period while nothing does (a new delegation still shows up).
const IDLE_POLL: Duration = Duration::from_secs(15);
/// Delegation depth followed before giving up (cycles are guarded too).
const MAX_DEPTH: usize = 12;
/// Parents whose tasks one refresh reads.
const MAX_FETCH: usize = 8;
/// Steps the Back trail remembers.
const MAX_TRAIL: usize = 32;
/// How the backend titles a delegated child before it has a real title.
const CHILD_PROMPT_TITLE: &str = "You are a delegated child agent";

/// The tab's memory across thread switches (`s.workspace.agents`).
#[derive(Default)]
pub struct AgentsNav {
    /// Threads left by opening an agent from this tab, oldest first.
    trail: Vec<String>,
    /// The thread the last tab click opened: the trail holds while it is
    /// the open one, and resets once the person goes elsewhere.
    landed: Option<String>,
    /// Tasks of family parents other than the open thread, by parent id.
    tasks: HashMap<String, Vec<TaskView>>,
    loading: bool,
    /// Another refresh was asked for while one was in flight.
    stale: bool,
    polling: bool,
    /// Turns the bounded parent reads so every parent gets read in time.
    turn: usize,
}

impl AgentsNav {
    /// A computer switch: the old computer's tasks and trail mean nothing
    /// there, and a read in flight will never land (`jobs::run` drops it).
    /// The refresh loop runs on; it stops by itself once the tab hides.
    pub(crate) fn reset_for_switch(&mut self) {
        let polling = self.polling;
        *self = Self {
            polling,
            ..Self::default()
        };
    }
}

// ── family model ──

/// One agent in the family tree.
struct Member<'a> {
    /// Its conversation; `None` for a task that has not started one yet.
    thread: Option<String>,
    key: String,
    depth: usize,
    row: Option<&'a InboxRow>,
    task: Option<&'a TaskView>,
}

/// A subagent before it is placed: its conversation, inbox row and task.
type Child<'a> = (Option<String>, Option<&'a InboxRow>, Option<&'a TaskView>);

fn open_id(s: &State) -> Option<&str> {
    s.chat.as_ref().map(|c| c.thread_id.as_str())
}

/// Tasks `thread` delegated, from its chat when open, else from the tab's reads.
fn tasks_of<'a>(s: &'a State, thread: &str) -> &'a [TaskView] {
    if open_id(s) == Some(thread) {
        return bridge::tasks(s);
    }
    s.workspace
        .agents
        .tasks
        .get(thread)
        .map_or(&[], Vec::as_slice)
}

/// Who delegated `thread`: the inbox, the chat's lineage, or a task read here.
fn parent_of(s: &State, thread: &str) -> Option<String> {
    if let Some(p) = s
        .workspace
        .inbox
        .row(thread)
        .and_then(|r| r.parent_thread_id.clone())
    {
        return Some(p);
    }
    if let Some(task) = crate::chat::orchestration::delegation(thread)
        && !task.parent_thread_id.is_empty()
    {
        return Some(task.parent_thread_id);
    }
    s.workspace.agents.tasks.iter().find_map(|(parent, tasks)| {
        tasks
            .iter()
            .any(|t| t.child_thread_id.as_deref() == Some(thread))
            .then(|| parent.clone())
    })
}

/// `thread` and its delegators up to the main agent, innermost first.
fn ancestry(s: &State, thread: &str) -> Vec<String> {
    let mut chain = vec![thread.to_string()];
    let mut seen = HashSet::from([thread.to_string()]);
    while chain.len() <= MAX_DEPTH {
        let Some(parent) = chain.last().and_then(|t| parent_of(s, t)) else {
            break;
        };
        if !seen.insert(parent.clone()) {
            break;
        }
        chain.push(parent);
    }
    chain
}

/// Whether the inbox or a task says this agent runs now.
fn is_live(s: &State, row: Option<&InboxRow>, task: Option<&TaskView>) -> bool {
    row.is_some_and(|r| {
        matches!(
            s.workspace.inbox.shown_status(r),
            Some(InboxStatus::Working | InboxStatus::Background | InboxStatus::NeedsInput)
        )
    }) || task.is_some_and(|t| t.status.is_live())
}

/// `parent`'s direct subagents: its tasks joined with the inbox rows that
/// name it as parent (a row without a task yet still shows). Live first,
/// newest first.
fn children_of<'a>(s: &'a State, parent: &str) -> Vec<Child<'a>> {
    let inbox = &s.workspace.inbox;
    let mut out: Vec<Child<'_>> = Vec::new();
    let mut threads = HashSet::new();
    for task in tasks_of(s, parent) {
        let thread = task.child_thread_id.clone();
        if let Some(t) = &thread
            && !threads.insert(t.clone())
        {
            continue;
        }
        let row = thread.as_deref().and_then(|t| inbox.row(t));
        out.push((thread, row, Some(task)));
    }
    for row in &inbox.list.rows {
        if row.parent_thread_id.as_deref() == Some(parent)
            && row.thread_id != parent
            && threads.insert(row.thread_id.clone())
        {
            out.push((Some(row.thread_id.clone()), Some(row), None));
        }
    }
    let born = |row: Option<&InboxRow>, task: Option<&TaskView>| {
        task.map(|t| t.created_at)
            .or_else(|| row.and_then(|r| r.working_since))
            .or_else(|| row.map(|r| r.last_activity_at))
            .unwrap_or(0)
    };
    out.sort_by_key(|(_, row, task)| (!is_live(s, *row, *task), -born(*row, *task)));
    out
}

/// The family of the open thread: the main agent (depth 0) and every
/// subagent under it, depth first. Empty when the open thread neither
/// delegated nor was delegated.
fn family(s: &State) -> Vec<Member<'_>> {
    let Some(open) = open_id(s) else {
        return Vec::new();
    };
    let root = ancestry(s, open).pop().unwrap_or_else(|| open.to_string());
    let mut out = vec![Member {
        thread: Some(root.clone()),
        key: root.clone(),
        depth: 0,
        row: s.workspace.inbox.row(&root),
        task: None,
    }];
    let mut seen = HashSet::from([root.clone()]);
    let mut stack: Vec<(usize, Child<'_>)> = children_of(s, &root)
        .into_iter()
        .rev()
        .map(|c| (1, c))
        .collect();
    while let Some((depth, (thread, row, task))) = stack.pop() {
        let key = match (&thread, task) {
            (Some(t), _) => t.clone(),
            (None, Some(task)) => format!("task-{}", task.task_id),
            (None, None) => continue,
        };
        if !seen.insert(key.clone()) {
            continue;
        }
        if let Some(t) = &thread
            && depth < MAX_DEPTH
        {
            stack.extend(children_of(s, t).into_iter().rev().map(|c| (depth + 1, c)));
        }
        out.push(Member {
            thread,
            key,
            depth,
            row,
            task,
        });
    }
    if out.len() == 1 && open == root {
        // Nobody delegated anything: no family to show.
        out.clear();
    }
    out
}

/// (working, total) subagents in the open thread's family, for the tab's
/// pill, badge and status line.
pub(super) fn summary(s: &State) -> (usize, usize) {
    let fam = family(s);
    let subs = fam.iter().filter(|m| m.depth > 0);
    let live = subs.clone().filter(|m| is_live(s, m.row, m.task)).count();
    (live, subs.count())
}

// ── navigation ──

impl AgentsNav {
    /// The person goes from `from` to `target` through the tab.
    fn record(&mut self, from: Option<&str>, target: &str) {
        if let Some(from) = from.filter(|f| *f != target) {
            if self.landed.as_deref() != Some(from) {
                // The person went elsewhere since the last tab click.
                self.trail.clear();
            }
            match self.trail.iter().position(|t| t == target) {
                // Back to a thread already on the trail: drop the steps after it.
                Some(i) => self.trail.truncate(i),
                None => {
                    self.trail.push(from.to_string());
                    if self.trail.len() > MAX_TRAIL {
                        self.trail.remove(0);
                    }
                }
            }
        }
        self.landed = Some(target.to_string());
    }
}

/// Open `target` from the tab, remembering where the person was.
fn open_tracked(s: &mut State, target: String) {
    let from = open_id(s).map(str::to_string);
    s.workspace.agents.record(from.as_deref(), &target);
    crate::workspace::open_thread(s, target);
    ensure_poll(s);
}

/// One step back along the trail.
fn back(s: &mut State) {
    let nav = &mut s.workspace.agents;
    let Some(prev) = nav.trail.pop() else { return };
    nav.landed = Some(prev.clone());
    crate::workspace::open_thread(s, prev);
    ensure_poll(s);
}

/// Where Back goes, while the trail holds.
fn back_target(s: &State) -> Option<&str> {
    let nav = &s.workspace.agents;
    let open = open_id(s)?;
    (nav.landed.as_deref() == Some(open))
        .then(|| nav.trail.last().map(String::as_str))
        .flatten()
}

// ── refresh ──

/// The trail holds only while the tab's last click is the open thread:
/// any other way of changing threads (sidebar, search) drops it, so a
/// later return there does not bring an old Back row back. Cheap: the
/// app's poll calls it.
pub(crate) fn sync_trail(s: &mut State) {
    let open = open_id(s).map(str::to_string);
    let nav = &mut s.workspace.agents;
    if nav.landed.is_some() && nav.landed != open {
        nav.landed = None;
        nav.trail.clear();
    }
}

/// The tab is on screen for an open chat, with the window in front and
/// the battery saver off: only then does it keep itself fresh.
fn visible(s: &State) -> bool {
    s.chat.is_some()
        && s.page == crate::state::Page::Chat
        && crate::workspace::window_active(s)
        && !crate::design::look::saving(s)
        && super::dock_open(s, Dock::Side)
        && super::dock_tab(s, Dock::Side) == Tab::Agents
}

/// The tab was shown: read now, and keep reading while it shows.
pub(super) fn load(s: &mut State) {
    if s.workspace.agents.polling {
        crate::workspace::refresh_inbox(s);
        refresh(s);
    } else {
        // Starting the loop reads at once.
        ensure_poll(s);
    }
}

/// Start the tab's refresh loop if it shows and none runs. It stops once
/// the tab hides, the window goes behind or the battery saver holds; the
/// app's poll calls this, so coming back restarts it with a read at once.
pub(crate) fn ensure_poll(s: &mut State) {
    if s.workspace.agents.polling || !visible(s) {
        return;
    }
    s.workspace.agents.polling = true;
    crate::workspace::refresh_inbox(s);
    refresh(s);
    schedule_tick(s);
}

fn schedule_tick(s: &State) {
    let live = summary(s).0 > 0
        || open_id(s).is_some_and(|id| {
            s.workspace
                .inbox
                .row(id)
                .is_some_and(|r| is_live(s, Some(r), None))
        });
    let delay = if live { LIVE_POLL } else { IDLE_POLL };
    jobs::after_if(s, delay, |s, _| {
        if !visible(s) {
            s.workspace.agents.polling = false;
            return false;
        }
        // The inbox's own poll stops when nothing it lists works; a
        // sibling started elsewhere must still show up here.
        crate::workspace::refresh_inbox(s);
        refresh(s);
        schedule_tick(s);
        false
    });
}

/// Re-read the open thread's orchestration and the other family parents'
/// tasks. Coalesces like the chat's refresh.
pub(crate) fn refresh(s: &mut State) {
    let Some(open) = open_id(s).map(str::to_string) else {
        return;
    };
    // The open chat polls its own tasks while anything moves; read them
    // here only when it does not, so the two never double up.
    if !crate::chat::orchestration::polling(s, &open) {
        crate::chat::orchestration::refresh(s, &open);
    }
    // Parents to read: the open thread's delegators, and every family
    // member the inbox says delegated something — except the open thread,
    // whose chat reads its own.
    let mut parents: Vec<String> = ancestry(s, &open).into_iter().skip(1).collect();
    for m in family(s) {
        let Some(t) = m.thread else { continue };
        let delegated = s
            .workspace
            .inbox
            .list
            .rows
            .iter()
            .any(|r| r.parent_thread_id.as_deref() == Some(t.as_str()));
        if delegated && t != open && !parents.contains(&t) {
            parents.push(t);
        }
    }
    let nav = &mut s.workspace.agents;
    if parents.is_empty() {
        return;
    }
    // Bounded: parents never read come first, then the rest in turns, so
    // a large family's every parent is read within a few ticks.
    let (mut fresh, mut known): (Vec<String>, Vec<String>) = parents
        .into_iter()
        .partition(|p| !nav.tasks.contains_key(p));
    if !known.is_empty() {
        let at = nav.turn % known.len();
        known.rotate_left(at);
    }
    nav.turn = nav.turn.wrapping_add(MAX_FETCH);
    fresh.append(&mut known);
    let mut parents = fresh;
    parents.truncate(MAX_FETCH);
    if nav.loading {
        nav.stale = true;
        return;
    }
    nav.loading = true;
    jobs::run(
        s,
        "agents",
        move |c| async move {
            let reads = parents.iter().map(|p| c.thread_tasks(p));
            let results = futures::future::join_all(reads).await;
            Ok::<_, ClientError>(parents.into_iter().zip(results).collect::<Vec<_>>())
        },
        |s, result, _| {
            let nav = &mut s.workspace.agents;
            nav.loading = false;
            if let Ok(results) = result {
                for (parent, read) in results {
                    match read {
                        Ok(t) => {
                            nav.tasks.insert(parent, t.tasks);
                        }
                        // silent-ok: a thread the orchestrator never saw
                        // delegated nothing.
                        Err(e) if e.is_not_found() => {
                            nav.tasks.remove(&parent);
                        }
                        Err(e) => tracing::debug!(error = %e, "family tasks read failed"),
                    }
                }
            }
            if std::mem::take(&mut nav.stale) {
                refresh(s);
            }
            // Fetch titles of members the inbox has not titled.
            let missing: Vec<String> = family(s)
                .into_iter()
                .filter_map(|m| m.thread)
                .filter(|t| s.workspace.inbox.row(t).is_none())
                .collect();
            crate::workspace::titles_ensure(s, missing);
        },
    );
}

/// Cancel a live task. The open thread's goes through its chat; another
/// parent's is cancelled here and re-read.
fn cancel(s: &mut State, parent: String, task_id: String) {
    if open_id(s) == Some(parent.as_str()) {
        bridge::cancel_task(s, task_id);
        return;
    }
    jobs::run(
        s,
        "agents",
        move |c| async move { c.cancel_task(&task_id).await },
        |s, result, tx| {
            tx.animated();
            if let Err(e) = result
                && let Some(open) = open_id(s).map(str::to_string)
            {
                crate::chat::report_error(s, &open, format!("Couldn't cancel the subagent: {e}"));
            }
            refresh(s);
        },
    );
}

// ── view ──

fn task_color(status: TaskStatus, p: &Palette) -> Color {
    match status {
        TaskStatus::Completed => p.success,
        TaskStatus::Failed | TaskStatus::Interrupted => p.danger,
        TaskStatus::Cancelled | TaskStatus::Unknown => p.text_faint,
        _ => p.accent,
    }
}

/// What an agent is doing now: label, dot colour, and whether it breathes.
/// The inbox wins while it says the thread runs (it is the fresher of the
/// two); otherwise the task's outcome; otherwise the inbox's.
fn status(
    s: &State,
    row: Option<&InboxRow>,
    task: Option<&TaskView>,
) -> (&'static str, Color, bool) {
    let p = &s.palette;
    // The inbox says what the thread is doing now (a follow-up in a
    // subagent's chat can fail after its task completed); the task's
    // outcome speaks only while the inbox has nothing to say.
    let shown = row.and_then(|r| s.workspace.inbox.shown_status(r));
    match (shown, task) {
        (Some(InboxStatus::NeedsInput), _) => ("Needs you", p.warning, true),
        (Some(InboxStatus::Working), _) => ("Working", p.accent, true),
        (Some(InboxStatus::Background), _) => ("Waiting on its subagents", p.accent, true),
        (Some(InboxStatus::Limited), _) => ("Usage limit", p.warning, false),
        (Some(InboxStatus::Interrupted), _) => ("Interrupted", p.danger, false),
        (Some(InboxStatus::Failed), _) => ("Failed", p.danger, false),
        (Some(InboxStatus::Stopped), _) => ("Stopped", p.text_faint, false),
        (_, Some(t)) => (
            t.status.label(),
            task_color(t.status, p),
            t.status.is_live(),
        ),
        _ => ("Idle", p.text_faint, false),
    }
}

/// "3 min" from Unix milliseconds to now (or to the finish).
pub(super) fn elapsed(task: &TaskView) -> Option<String> {
    let start = task.started_at.unwrap_or(task.created_at);
    if start <= 0 {
        return None;
    }
    let end = task
        .finished_at
        .unwrap_or_else(|| Utc::now().timestamp_millis());
    Some(span((end - start) / 1000))
}

fn span(secs: i64) -> String {
    match secs.max(0) {
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{} min", s / 60),
        s => format!("{} h {} min", s / 3600, (s % 3600) / 60),
    }
}

/// A row's own title, unless it is still the delegation prompt.
fn row_title(row: Option<&InboxRow>) -> Option<String> {
    row.and_then(|r| r.title.clone())
        .filter(|t| !t.trim().is_empty() && !t.starts_with(CHILD_PROMPT_TITLE))
}

fn title(s: &State, m: &Member<'_>) -> String {
    let task_title = m
        .task
        .map(|t| t.title.trim().to_string())
        .filter(|t| !t.is_empty() && !t.starts_with(CHILD_PROMPT_TITLE));
    let fallback = if m.depth == 0 {
        "Main agent"
    } else {
        "Subagent"
    };
    // A subagent's task title says what it was asked; the main agent's
    // conversation title says what the thread is about.
    let picked = if m.depth == 0 {
        row_title(m.row).or(task_title)
    } else {
        task_title.or_else(|| row_title(m.row))
    };
    picked
        .or_else(|| {
            m.thread
                .as_deref()
                .and_then(|t| crate::workspace::title_of(s, t))
                .filter(|t| !t.starts_with(CHILD_PROMPT_TITLE))
        })
        .unwrap_or_else(|| fallback.into())
}

/// The status dot: a soft pulse while the agent is live (GPUI `kit::status_dot`).
fn status_dot(color: Color, live: bool) -> El {
    let d = ui::dot(color, 8.);
    if live {
        d.cycle(crate::design::kit::loops::breathe(
            1.2,
            0.,
            &crate::design::kit::loops::BREATH_45,
        ))
    } else {
        d
    }
}

fn pill(s: &State, label: &str) -> El {
    let p = &s.palette;
    el().h(16.)
        .px(5.)
        .radius(5.)
        .center()
        .fill(p.accent_wash)
        .child(ui::line(label.to_string(), tiny(), p.accent))
}

/// The Agents tab's fork button, for the dock's title row.
pub(super) fn actions(k: &Kit) -> Vec<El> {
    vec![
        ui::ibtn(&icons::GIT_FORK, "Fork this conversation")
            .on_press(bridge::fork)
            .build(k)
            .key("agents-fork"),
    ]
}

/// One agent's card. The whole card opens its conversation.
fn card(s: &State, k: &Kit, m: &Member<'_>, open: Option<&str>) -> El {
    let p = &s.palette;
    let (word, color, live) = status(s, m.row, m.task);
    let here = m.thread.as_deref().is_some_and(|t| Some(t) == open);
    let mut meta = vec![word.to_string()];
    if let Some(task) = m.task {
        meta.extend(Some(task.engine_label()).filter(|e| !e.is_empty()));
        meta.extend(elapsed(task));
    } else if live && let Some(ms) = m.row.and_then(|r| r.working_since) {
        meta.push(span((Utc::now().timestamp_millis() - ms) / 1000));
    }
    let name = title(s, m);
    let mut head = row()
        .gap(8.)
        .items_center()
        .child(status_dot(color, live))
        .child(
            ui::line(
                name.clone(),
                if m.depth == 0 { sm_med() } else { sm() },
                p.text,
            )
            .shrink()
            .min_w(0.),
        );
    if m.depth == 0 {
        head = head.child(pill(s, "Main"));
    }
    if here {
        head = head.child(pill(s, "Viewing"));
    }
    head = head.child(ui::grow());
    if let Some(task) = m.task
        && task.status.is_live()
        && !task.cancel_requested
    {
        let (parent, id) = (task.parent_thread_id.clone(), task.task_id.clone());
        head = head.child(
            ui::ibtn(&icons::SQUARE, "Cancel")
                .label("Cancel this subagent")
                .on_press(move |s: &mut State| cancel(s, parent.clone(), id.clone()))
                .build(k)
                .key(format!("agent-cancel-{}", task.task_id)),
        );
    }
    let opens = m.thread.clone().filter(|_| !here);
    if opens.is_some() {
        head = head.child(icon(&icons::ARROW_RIGHT, 13.).color(p.text_faint));
    }
    // Live-sorted: a resort glides, an arrival wipes in, in commits that
    // animate.
    let indent = 8. + 14. * (m.depth.saturating_sub(1).min(4) as f32);
    let mut card = col()
        .key(format!("agent-{}", m.key))
        .motion(crate::design::motion::ROW_RESORT)
        .transition(rune::Transition::reveal(rune::Edge::Top))
        .ml(indent)
        .mr(8.)
        .px(8.)
        .py(8.)
        .gap(4.)
        .radius(8.)
        .child(head)
        .child(ui::line(meta.join(" · "), xs(), p.text_faint).pl(16.));
    if here {
        card = card.fill(p.hover).border(1., p.border_strong);
    }
    if let Some(task) = m.task
        && let Some(note) = task
            .error
            .clone()
            .or(task.summary.clone())
            .filter(|n| !n.trim().is_empty())
    {
        let ink = if task.status == TaskStatus::Failed {
            p.danger
        } else {
            p.text_muted
        };
        card = card.child(ui::para(note, xs(), ink).max_lines(3).pl(16.));
    }
    match opens {
        Some(thread) => card
            .role("button")
            .label(format!("Open {name}"))
            .focusable(true)
            .hover(p.hover)
            .press(p.hover)
            .press_scale(crate::design::motion::PRESS_CARD)
            .cursor(rune::Cursor::Pointer)
            .on_press(move |s: &mut State| open_tracked(s, thread.clone())),
        None => card,
    }
}

pub(super) fn view(s: &State, k: &Kit) -> El {
    let p = &s.palette;
    let open = open_id(s);
    let mut list = col().w_full().pt(4.).gap(2.);

    if let Some(prev) = back_target(s) {
        let name = crate::workspace::title_of(s, prev)
            .or_else(|| row_title(s.workspace.inbox.row(prev)))
            .filter(|t| !t.starts_with(CHILD_PROMPT_TITLE))
            .unwrap_or_else(|| "previous agent".into());
        list = list.child(
            ui::hover_row(s, "agents-back")
                .mx(8.)
                .child(icon(&icons::ARROW_LEFT, 13.).color(p.text_muted))
                .child(
                    ui::line(format!("Back to {name}"), sm(), p.text_muted)
                        .grow(1.)
                        .min_w(0.),
                )
                .on_press(back),
        );
    }

    let fam = family(s);
    if fam.is_empty() {
        list = list.child(ui::empty_card(
            s,
            &icons::BOT,
            "No subagents yet",
            // Forking lives on the dock's title row (`actions`); side chats
            // open from their own place, not here.
            "Delegated agents and conversation forks show up here with live status and summaries.",
            None,
        ));
    } else {
        let (live, total) = summary(s);
        let mut members = fam.iter();
        if let Some(root) = members.next() {
            list = list.child(card(s, k, root, open));
        }
        let label = match (live, total) {
            (_, 0) => "Subagents".to_string(),
            (0, n) => format!("Subagents · {n}"),
            (l, n) => format!("Subagents · {l} of {n} working"),
        };
        list = list.child(ui::heading(s, label));
        if total == 0 {
            list = list.child(el().w_full().px(16.).py(4.).child(ui::line(
                "Nothing delegated yet.",
                xs(),
                p.text_faint,
            )));
        }
        for m in members {
            list = list.child(card(s, k, m, open));
        }
    }

    let mut related: Vec<(&'static str, String)> = Vec::new();
    if let Some(lineage) = bridge::lineage(s) {
        if let Some(from) = &lineage.forked_from {
            related.push(("Forked from", from.source_thread.clone()));
        }
        for fork in &lineage.forks {
            if let Some(target) = &fork.target_thread {
                related.push(("Fork", target.clone()));
            }
        }
    }
    if !related.is_empty() {
        list = list.child(ui::heading(s, "Forks"));
        for (label, thread) in related {
            let title = bridge::title(s, &thread).unwrap_or_else(|| "Conversation".into());
            list = list.child(
                ui::hover_row(s, format!("agent-fork-{thread}"))
                    .mx(8.)
                    .child(icon(&icons::GIT_FORK, 12.).color(p.text_faint))
                    .child(ui::line(label, xs(), p.text_faint))
                    .child(ui::line(title, sm(), p.text).grow(1.).min_w(0.))
                    .on_press(move |s: &mut State| open_tracked(s, thread.clone())),
            );
        }
    }
    ui::scroll("agents-list", list)
}

#[cfg(test)]
mod tests {
    use zlo_client::inbox::{InboxState, InboxStatus};

    use super::*;
    use crate::state::State;

    fn task(id: &str, parent: &str, child: Option<&str>, status: &str, title: &str) -> TaskView {
        serde_json::from_value(serde_json::json!({
            "taskId": id, "title": title, "status": status, "parentThreadId": parent,
            "childThreadId": child, "createdAt": 1_000,
        }))
        .expect("task json")
    }

    fn row(id: &str, parent: Option<&str>, status: Option<InboxStatus>) -> InboxRow {
        let mut r = crate::workspace::fixtures::row(
            id,
            "You are a delegated child agent (T3 orchestrator task task_1",
            "default",
            InboxState::Inbox,
            1,
        );
        r.parent_thread_id = parent.map(Into::into);
        r.status = status;
        r
    }

    /// `open` is the open chat; the inbox lists `main` working on two
    /// subagents, one of which (`b`) has no task read yet.
    fn state(open: &str) -> State {
        let mut s = crate::chat::fixtures::test_state();
        let mut main = row("main", None, Some(InboxStatus::Background));
        main.title = Some("Subagents running in parallel".into());
        s.workspace.inbox.list.rows = vec![
            main,
            row("a", Some("main"), Some(InboxStatus::Working)),
            row("b", Some("main"), Some(InboxStatus::Working)),
        ];
        s.chat = Some(crate::chat::ChatState::new(open.into()));
        s
    }

    fn keys(s: &State) -> Vec<(String, usize)> {
        family(s).into_iter().map(|m| (m.key, m.depth)).collect()
    }

    #[test]
    fn a_subagents_chat_shows_its_whole_family() {
        let mut s = state("a");
        // Before any task read the inbox alone gives the tree.
        let fam = keys(&s);
        assert_eq!(fam[0], ("main".into(), 0));
        assert!(fam.contains(&("a".into(), 1)) && fam.contains(&("b".into(), 1)));
        assert_eq!(summary(&s), (2, 2), "both subagents work");

        // The main agent's tasks, read by the tab, title them.
        s.workspace.agents.tasks.insert(
            "main".into(),
            vec![task(
                "t1",
                "main",
                Some("a"),
                "running",
                "River-crossing designer",
            )],
        );
        let fam = family(&s);
        let a = fam.iter().find(|m| m.key == "a").expect("a listed");
        assert_eq!(title(&s, a), "River-crossing designer");
        let b = fam.iter().find(|m| m.key == "b").expect("b listed");
        assert_eq!(
            title(&s, b),
            "Subagent",
            "the delegation prompt is never a title"
        );
        assert_eq!(title(&s, &fam[0]), "Subagents running in parallel");
    }

    #[test]
    fn the_main_agent_lists_subagents_before_its_tasks_are_read() {
        let s = state("main");
        assert_eq!(summary(&s), (2, 2));
    }

    #[test]
    fn a_thread_that_delegated_nothing_has_no_family() {
        let mut s = state("solo");
        s.workspace.inbox.list.rows.push(row("solo", None, None));
        assert!(family(&s).is_empty());
        assert_eq!(summary(&s), (0, 0));
    }

    #[test]
    fn a_finished_task_outranks_an_idle_inbox_row() {
        let mut s = state("main");
        s.workspace.inbox.list.rows[1].status = None;
        let t = task("t1", "main", Some("a"), "completed", "Done thing");
        let (word, _, live) = status(&s, s.workspace.inbox.row("a"), Some(&t));
        assert_eq!((word, live), ("Done", false));
        // While the inbox says it works, that wins over a stale task.
        let (word, _, live) = status(&s, s.workspace.inbox.row("b"), Some(&t));
        assert_eq!((word, live), ("Working", true));
    }

    #[test]
    fn nested_subagents_hang_under_their_parent() {
        let mut s = state("main");
        s.workspace
            .inbox
            .list
            .rows
            .push(row("a1", Some("a"), Some(InboxStatus::Working)));
        let fam = keys(&s);
        let a = fam.iter().position(|m| m.0 == "a").expect("a");
        assert_eq!(fam[a + 1], ("a1".into(), 2), "a's own subagent follows it");
    }

    #[test]
    fn a_delegation_cycle_ends() {
        let mut s = state("a");
        s.workspace.inbox.list.rows[0].parent_thread_id = Some("a".into());
        // Terminates, and lists each thread once.
        let fam = keys(&s);
        let mut seen = HashSet::new();
        assert!(fam.iter().all(|(k, _)| seen.insert(k.clone())));
    }

    /// [`open_tracked`] without the chat machinery: the open thread is
    /// swapped by hand.
    fn go(s: &mut State, target: &str) {
        let from = open_id(s).map(str::to_string);
        s.workspace.agents.record(from.as_deref(), target);
        s.chat = Some(crate::chat::ChatState::new(target.into()));
    }

    #[test]
    fn the_trail_leads_back_step_by_step() {
        let mut s = state("main");
        assert_eq!(back_target(&s), None);
        go(&mut s, "a");
        assert_eq!(back_target(&s), Some("main"));
        go(&mut s, "b");
        assert_eq!(back_target(&s), Some("a"));
        // Clicking the main agent's card is going back: no loop.
        go(&mut s, "main");
        assert_eq!(back_target(&s), None);
        assert!(s.workspace.agents.trail.is_empty());
    }

    #[test]
    fn a_failed_follow_up_outranks_a_completed_task() {
        let mut s = state("main");
        s.workspace.inbox.list.rows[1].status = Some(InboxStatus::Failed);
        let t = task("t1", "main", Some("a"), "completed", "Done thing");
        let (word, _, live) = status(&s, s.workspace.inbox.row("a"), Some(&t));
        assert_eq!((word, live), ("Failed", false));
    }

    #[test]
    fn a_sidebar_detour_does_not_revive_the_trail() {
        let mut s = state("main");
        go(&mut s, "a");
        // The sidebar goes to b, then back to a.
        s.chat = Some(crate::chat::ChatState::new("b".into()));
        sync_trail(&mut s);
        s.chat = Some(crate::chat::ChatState::new("a".into()));
        sync_trail(&mut s);
        assert_eq!(back_target(&s), None);
    }

    #[test]
    fn a_computer_switch_forgets_tasks_and_unblocks_reads() {
        let mut s = state("a");
        go(&mut s, "b");
        let nav = &mut s.workspace.agents;
        nav.loading = true;
        nav.polling = true;
        nav.tasks.insert("main".into(), Vec::new());
        nav.reset_for_switch();
        assert!(!nav.loading && nav.tasks.is_empty() && nav.trail.is_empty());
        assert!(nav.polling, "the live loop keeps its flag and ends itself");
    }

    #[test]
    fn going_elsewhere_drops_the_trail() {
        let mut s = state("main");
        go(&mut s, "a");
        // The sidebar opened another thread, not the tab.
        s.chat = Some(crate::chat::ChatState::new("b".into()));
        assert_eq!(
            back_target(&s),
            None,
            "Back only shows where the tab took you"
        );
        go(&mut s, "a");
        assert_eq!(s.workspace.agents.trail, vec!["b".to_string()]);
    }
}
