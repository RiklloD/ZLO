//! History: MonoCode Native's `history.rs` + `git_graph` (MIT) at pane size
//! (GPUI `chat/side/history.rs`): the commit log with its swimlane graph
//! drawn per row; a commit opens its own diff in the same viewer the
//! Changes tab uses (rune-kit `DiffView`).

use rune::shape::{LineCap, Shape};
use rune::{Color, El, canvas, col, el, row};
use rune_kit::Kit;
use rune_kit::diff::{DiffMode, DiffState, DiffView};

use super::ui::{self, sm_med, tiny, xs};
use super::{SideState, git, side, side_mut, side_of, thread_id};
use crate::design::Palette;
use crate::icons;
use crate::jobs;
use crate::state::State;

const LOG_LIMIT: usize = 200;
const ROW_H: f32 = 30.;
const LANE_W: f32 = 11.;
/// Lanes past this are not drawn (their commits still list).
const LANES_MAX: usize = 8;

/// Lane colours, MonoCode's soft graph palette (hues as turns).
const HUES: [f32; 6] = [0.58, 0.33, 0.08, 0.83, 0.47, 0.0];

/// One row's drawing: the commit's lane and the segments through the row.
#[derive(Clone, Default, Debug)]
pub(super) struct GraphRow {
    node: usize,
    /// Lanes entering from above that end at this commit.
    into: Vec<usize>,
    /// Lanes passing straight through.
    through: Vec<usize>,
    /// Lanes leaving to the parents below.
    out: Vec<usize>,
}

/// The lane layout of a topo-ordered log. Lanes keep their index while
/// open, so a passing lane is a straight line.
fn layout(commits: &[git::Commit]) -> Vec<GraphRow> {
    let mut lanes: Vec<Option<String>> = Vec::new();
    let mut rows = Vec::with_capacity(commits.len());
    for c in commits {
        let node = lanes
            .iter()
            .position(|l| l.as_deref() == Some(c.sha.as_str()))
            .unwrap_or_else(|| match lanes.iter().position(Option::is_none) {
                Some(free) => free,
                None => {
                    lanes.push(None);
                    lanes.len() - 1
                }
            });
        let into: Vec<usize> = lanes
            .iter()
            .enumerate()
            .filter(|(_, l)| l.as_deref() == Some(c.sha.as_str()))
            .map(|(i, _)| i)
            .collect();
        let through: Vec<usize> = lanes
            .iter()
            .enumerate()
            .filter(|(_, l)| l.as_ref().is_some_and(|s| *s != c.sha))
            .map(|(i, _)| i)
            .collect();
        for i in &into {
            lanes[*i] = None;
        }
        let mut out = Vec::new();
        for (k, parent) in c.parents.iter().enumerate() {
            let existing = lanes
                .iter()
                .position(|l| l.as_deref() == Some(parent.as_str()));
            let lane = match existing {
                Some(lane) => lane,
                None if k == 0 && lanes.get(node).is_none_or(Option::is_none) => node,
                None => match lanes.iter().position(Option::is_none) {
                    Some(free) => free,
                    None => {
                        lanes.push(None);
                        lanes.len() - 1
                    }
                },
            };
            while lanes.len() <= lane {
                lanes.push(None);
            }
            lanes[lane] = Some(parent.clone());
            out.push(lane);
        }
        while lanes.last().is_some_and(Option::is_none) {
            lanes.pop();
        }
        rows.push(GraphRow {
            node,
            into,
            through,
            out,
        });
    }
    rows
}

pub(super) struct Detail {
    commit: git::Commit,
    message: Option<String>,
    diff: Option<DiffState>,
    error: Option<String>,
}

#[derive(Default)]
pub(super) struct HistoryState {
    commits: Option<Result<Vec<git::Commit>, String>>,
    graph: Vec<GraphRow>,
    loading: bool,
    open: Option<Detail>,
}

fn lane_color(lane: usize, p: &Palette) -> Color {
    let hue = HUES[lane % HUES.len()];
    rune::oklch(if p.light { 0.55 } else { 0.74 }, 0.12, hue * 360.)
}

fn ago(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .map(jobs::ago)
        .unwrap_or_default()
}

/// The graph cell of one row.
fn graph_cell(row: &GraphRow, width: f32, p: &Palette) -> El {
    let x = |lane: usize| 6. + lane.min(LANES_MAX) as f32 * LANE_W;
    let mid = ROW_H / 2.;
    let mut shape = Shape::new();
    let line = |shape: Shape, a: [f32; 2], b: [f32; 2], lane: usize| {
        shape.line(a, b, 1.5, LineCap::Round, lane_color(lane, p))
    };
    for &lane in &row.through {
        shape = line(shape, [x(lane), 0.], [x(lane), ROW_H], lane);
    }
    for &lane in &row.into {
        shape = line(shape, [x(lane), 0.], [x(row.node), mid], lane);
    }
    for &lane in &row.out {
        shape = line(shape, [x(row.node), mid], [x(lane), ROW_H], lane);
    }
    // The node: a small filled circle.
    let (cx0, r) = (x(row.node), 3.5);
    let dot: Vec<[f32; 2]> = (0..16)
        .map(|i| {
            let a = i as f32 / 16. * std::f32::consts::TAU;
            [cx0 + r * a.cos(), mid + r * a.sin()]
        })
        .collect();
    shape = shape.fill(dot, lane_color(row.node, p));
    canvas(shape).w(width).h(ROW_H)
}

pub(super) fn refresh(s: &mut State) {
    let Some(thread) = thread_id(s) else { return };
    let Some(x) = side_mut(s) else { return };
    let Some(repo) = x.repo.clone() else { return };
    if x.history.loading {
        return;
    }
    x.history.loading = true;
    let generation = x.folder_gen;
    jobs::blocking(
        s,
        move || git::log(&repo, LOG_LIMIT),
        move |s, result, _| {
            let Some(x) = side_of(s, &thread).filter(|x| x.folder_gen == generation) else {
                return;
            };
            let state = &mut x.history;
            state.loading = false;
            state.graph = result.as_ref().map(|c| layout(c)).unwrap_or_default();
            state.commits = Some(result);
        },
    );
}

/// A commit was made or the tree moved: History reloads (GPUI reloaded
/// when next drawn; a Rune view can't start work, so it reloads now when
/// the tab shows, else on its next visit).
pub(super) fn stale(s: &mut State) {
    let Some(x) = side_mut(s) else { return };
    x.history.commits = None;
    if super::tab_shown(s, super::Tab::History) {
        refresh(s);
    }
}

fn open(s: &mut State, commit: git::Commit) {
    let Some(thread) = thread_id(s) else { return };
    let Some(x) = side_mut(s) else { return };
    let Some(repo) = x.repo.clone() else { return };
    let sha = commit.sha.clone();
    x.history.open = Some(Detail {
        commit,
        message: None,
        diff: None,
        error: None,
    });
    jobs::blocking(
        s,
        move || {
            let message = git::message(&repo, &sha).ok();
            (message, git::show(&repo, &sha), sha)
        },
        move |s, (message, patch, sha), _| {
            let Some(x) = side_of(s, &thread) else { return };
            let Some(detail) = x.history.open.as_mut().filter(|d| d.commit.sha == sha) else {
                return;
            };
            detail.message = message;
            match patch {
                Ok(patch) => {
                    let mut diff = DiffState::new(rune_kit::diff::parse(&patch));
                    diff.set_mode(DiffMode::Unified);
                    detail.diff = Some(diff);
                }
                Err(e) => detail.error = Some(e),
            }
        },
    );
}

fn detail_diff(s: &State) -> &DiffState {
    side(s)
        .and_then(|x| x.history.open.as_ref())
        .and_then(|d| d.diff.as_ref())
        .unwrap_or_else(|| ui::empty_diff())
}

fn detail_diff_mut(s: &mut State) -> &mut DiffState {
    match side_mut(s)
        .and_then(|x| x.history.open.as_mut())
        .and_then(|d| d.diff.as_mut())
    {
        Some(d) => d,
        None => ui::spare_diff(),
    }
}

pub(super) fn view(s: &State, k: &Kit) -> El {
    let Some(x) = side(s) else {
        return ui::loading(k);
    };
    if x.repo.is_none() {
        return ui::empty(s, "This folder is not a git repository.");
    }
    if let Some(detail) = x.history.open.as_ref() {
        return commit_view(s, k, detail);
    }
    let commits = match &x.history.commits {
        None => return ui::loading(k),
        Some(Err(e)) => return ui::error_line(s, e.clone()),
        Some(Ok(c)) => c,
    };
    if commits.is_empty() {
        return ui::empty(s, "No commits yet.");
    }
    let lanes = lanes(x);
    let graph_w = 12. + lanes as f32 * LANE_W;
    let list = ui::virtual_scroll(
        "history-list",
        commits.len(),
        ROW_H,
        |s, i| {
            let sha = commits_of(s).and_then(|c| c.get(i)).map(|c| c.sha.as_str());
            format!("commit-{}", sha.unwrap_or_default())
        },
        move |s, i| commit_row(s, i, graph_w),
    );
    ui::fill_col()
        .child(
            row().w_full().px(8.).justify_end().child(
                ui::ibtn(&icons::REFRESH_CW, "Refresh")
                    .label("Refresh history")
                    .on_press(refresh)
                    .build(k)
                    .key("history-refresh"),
            ),
        )
        .child(list)
}

/// The loaded commits, when there are any.
fn commits_of(s: &State) -> Option<&Vec<git::Commit>> {
    side(s)?.history.commits.as_ref()?.as_ref().ok()
}

/// History row `i`: the graph cell, refs, subject and meta line.
fn commit_row(s: &State, i: usize, graph_w: f32) -> El {
    let p = &s.palette;
    let (Some(commit), Some(row_)) = (
        commits_of(s).and_then(|c| c.get(i)),
        side(s).and_then(|x| x.history.graph.get(i)),
    ) else {
        return el();
    };
    let meta = format!(
        "{} · {} · {}",
        commit.short(),
        commit.author,
        ago(commit.time)
    );
    let mut subject = row().gap(4.).items_center();
    for r in commit.refs.iter().take(2) {
        subject = subject.child(
            el().px(4.)
                .radius(4.)
                .fill(p.accent_wash)
                .max_w(110.)
                .child(ui::line(r.clone(), tiny(), p.accent)),
        );
    }
    subject = subject.child(
        ui::line(commit.subject.clone(), xs(), p.text)
            .shrink()
            .min_w(0.),
    );
    let to_open = commit.clone();
    row()
        .w_full()
        .h(ROW_H)
        .pr(8.)
        .gap(4.)
        .items_center()
        .hover(p.hover)
        .cursor(rune::Cursor::Pointer)
        .child(graph_cell(row_, graph_w, p))
        .child(
            col()
                .grow(1.)
                .min_w(0.)
                .child(subject)
                .child(ui::line(meta, tiny(), p.text_faint)),
        )
        .on_press(move |s: &mut State| open(s, to_open.clone()))
}

fn lanes(x: &SideState) -> usize {
    x.history
        .graph
        .iter()
        .flat_map(|r| {
            r.through
                .iter()
                .chain(&r.out)
                .chain(&r.into)
                .copied()
                .chain([r.node])
        })
        .max()
        .unwrap_or(0)
        .min(LANES_MAX)
}

fn commit_view(s: &State, k: &Kit, detail: &Detail) -> El {
    let p = &s.palette;
    let c = &detail.commit;
    let sha = c.sha.clone();
    let body = detail
        .message
        .as_deref()
        .and_then(|m| m.split_once('\n').map(|(_, rest)| rest.trim().to_string()))
        .filter(|b| !b.is_empty());
    let mut info = col().w_full().px(12.).py(8.).gap(4.).child(ui::line(
        format!("{} · {} · {}", c.short(), c.author, ago(c.time)),
        xs(),
        p.text_muted,
    ));
    if let Some(body) = body {
        info = info.child(ui::para(body, xs(), p.text));
    }
    let diff = match (&detail.diff, &detail.error) {
        (_, Some(e)) => ui::error_line(s, e.clone()),
        (Some(_), None) => DiffView::new(detail_diff, detail_diff_mut)
            .key("history-diff")
            .build(s, k),
        (None, None) => ui::loading(k),
    };
    ui::fill_col()
        .child(
            row()
                .w_full()
                .px(8.)
                .py(4.)
                .gap(4.)
                .items_center()
                .child(
                    ui::ibtn(&icons::CHEVRON_LEFT, "Back")
                        .on_press(|s: &mut State| {
                            if let Some(x) = side_mut(s) {
                                x.history.open = None;
                            }
                        })
                        .build(k)
                        .key("commit-back"),
                )
                .child(
                    ui::line(c.subject.clone(), sm_med(), p.text)
                        .grow(1.)
                        .min_w(0.),
                )
                .child(
                    ui::ibtn(&icons::COPY, "Copy commit id")
                        .on_press_tx(move |_: &mut State, tx| tx.copy(sha.clone()))
                        .build(k)
                        .key("commit-copy"),
                ),
        )
        .child(el().w_full().h(1.).fill(p.border))
        .child(info)
        .child(el().w_full().h(1.).fill(p.border))
        .child(ui::fill_col().child(diff))
}

/// Fill the log from a real repository synchronously (scenes only).
pub(super) fn fixture_load(x: &mut SideState) {
    if let Some(repo) = &x.repo {
        let result = git::log(repo, 40);
        x.history.graph = result.as_ref().map(|c| layout(c)).unwrap_or_default();
        x.history.commits = Some(result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commit(sha: &str, parents: &[&str]) -> git::Commit {
        git::Commit {
            sha: sha.into(),
            parents: parents.iter().map(|p| p.to_string()).collect(),
            author: String::new(),
            time: 0,
            refs: Vec::new(),
            subject: String::new(),
        }
    }

    #[test]
    fn a_merge_opens_a_second_lane_that_joins_back() {
        // m merges b into a; both descend from r.
        let log = [
            commit("m", &["a", "b"]),
            commit("b", &["r"]),
            commit("a", &["r"]),
            commit("r", &[]),
        ];
        let rows = layout(&log);
        assert_eq!(rows[0].node, 0);
        assert_eq!(rows[0].out, vec![0, 1]);
        assert_eq!(rows[1].node, 1);
        // a's parent already has a lane: it bends over, its own lane ends.
        assert_eq!(rows[2].node, 0);
        assert_eq!(rows[2].out, vec![1]);
        assert_eq!(rows[3].node, 1);
    }
}
