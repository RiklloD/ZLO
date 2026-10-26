//! History: MonoCode Native's `history.rs` + `git_graph` (MIT) at pane
//! size: the commit log with its swimlane graph painted per row; a commit
//! opens its own diff in the same viewer the Changes tab uses.

use gpui_kit::component::diff::{Diff, DiffFile, DiffMode, DiffState};

use super::{ChatView, empty, error_line, git, loading, scroll};
use crate::design::{Palette, kit};
use crate::ui::*;

const LOG_LIMIT: usize = 200;
const ROW_H: f32 = 30.;
const LANE_W: f32 = 11.;
/// Lanes past this are not drawn (their commits still list).
const LANES_MAX: usize = 8;

/// Lane colors, MonoCode's soft graph palette (HSL hues).
const HUES: [f32; 6] = [0.58, 0.33, 0.08, 0.83, 0.47, 0.0];

/// One row's drawing: the commit's lane and the segments through the row,
/// as (lane at the top or middle, lane at the middle or bottom).
#[derive(Clone, Default)]
struct GraphRow {
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
                None if k == 0 && lanes[node].is_none() => node,
                None => match lanes.iter().position(Option::is_none) {
                    Some(free) => free,
                    None => {
                        lanes.push(None);
                        lanes.len() - 1
                    }
                },
            };
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
    diff: Option<Entity<DiffState>>,
    error: Option<String>,
}

#[derive(Default)]
pub(super) struct State {
    commits: Option<Result<Vec<git::Commit>, String>>,
    graph: Vec<GraphRow>,
    loading: bool,
    open: Option<Detail>,
}

fn lane_color(lane: usize, p: &Palette) -> gpui_kit::Hsla {
    let hue = HUES[lane % HUES.len()];
    gpui_kit::hsla(hue, 0.55, if p.light { 0.45 } else { 0.62 }, 1.)
}

fn ago(unix: i64) -> String {
    chrono::DateTime::from_timestamp(unix, 0)
        .map(crate::jobs::ago)
        .unwrap_or_default()
}

/// The graph cell of one row.
fn graph_cell(row: GraphRow, width: f32, p: Palette) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds: Bounds<Pixels>, _, window, _| {
            let left = f32::from(bounds.left()) + 6.;
            let top = f32::from(bounds.top());
            let x = |lane: usize| px(left + lane.min(LANES_MAX) as f32 * LANE_W);
            let mid = px(top + ROW_H / 2.);
            let (y0, y1) = (px(top), px(top + ROW_H));
            let line = |a: gpui_kit::Point<Pixels>,
                        b: gpui_kit::Point<Pixels>,
                        lane: usize,
                        window: &mut Window| {
                let mut path = PathBuilder::stroke(px(1.5));
                path.move_to(a);
                path.line_to(b);
                if let Ok(path) = path.build() {
                    window.paint_path(path, lane_color(lane, &p));
                }
            };
            for &lane in &row.through {
                line(point(x(lane), y0), point(x(lane), y1), lane, window);
            }
            for &lane in &row.into {
                line(point(x(lane), y0), point(x(row.node), mid), lane, window);
            }
            for &lane in &row.out {
                line(point(x(row.node), mid), point(x(lane), y1), lane, window);
            }
            // The node: a small filled circle.
            let mut dot = PathBuilder::fill();
            let (cx0, r) = (f32::from(x(row.node)), 3.5);
            for i in 0..=16 {
                let a = i as f32 / 16. * std::f32::consts::TAU;
                let pt = point(px(cx0 + r * a.cos()), px(f32::from(mid) + r * a.sin()));
                if i == 0 {
                    dot.move_to(pt);
                } else {
                    dot.line_to(pt);
                }
            }
            if let Ok(path) = dot.build() {
                window.paint_path(path, lane_color(row.node, &p));
            }
        },
    )
    .w(px(width))
    .h(px(ROW_H))
    .flex_none()
}

impl ChatView {
    pub(super) fn history_refresh(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.side.repo.clone() else {
            return;
        };
        if self.side.history.loading {
            return;
        }
        self.side.history.loading = true;
        let task = cx.background_spawn(async move { git::log(&repo, LOG_LIMIT) });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                let state = &mut this.side.history;
                state.loading = false;
                state.graph = result.as_ref().map(|c| layout(c)).unwrap_or_default();
                state.commits = Some(result);
                cx.notify();
            });
        })
        .detach();
    }

    /// A commit was made or the tree moved: History reloads when next drawn.
    pub(super) fn history_stale(&mut self) {
        self.side.history.commits = None;
    }

    fn history_open(&mut self, commit: git::Commit, cx: &mut Context<Self>) {
        let Some(repo) = self.side.repo.clone() else {
            return;
        };
        let sha = commit.sha.clone();
        self.side.history.open = Some(Detail {
            commit,
            message: None,
            diff: None,
            error: None,
        });
        cx.notify();
        let task = cx.background_spawn(async move {
            let message = git::message(&repo, &sha).ok();
            (message, git::show(&repo, &sha))
        });
        cx.spawn(async move |this, cx| {
            let (message, patch) = task.await;
            let _ = this.update(cx, |this, cx| {
                let files = patch
                    .as_ref()
                    .map(|p| DiffFile::parse(p).unwrap_or_default());
                let diff = files.as_ref().ok().map(|files| {
                    cx.new(|cx| DiffState::new(files.clone(), cx).with_mode(DiffMode::Unified))
                });
                if let Some(detail) = this.side.history.open.as_mut() {
                    detail.message = message;
                    detail.diff = diff;
                    detail.error = patch.err();
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn render_history(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = *Palette::of(cx);
        if self.side.repo.is_none() {
            return empty("This folder is not a git repository.", cx);
        }
        if let Some(detail) = self.side.history.open.as_ref() {
            return self.render_commit(detail, cx);
        }
        let commits = match &self.side.history.commits {
            None => {
                if !self.side.history.loading {
                    // Stale after a commit: the next frame shows the reload.
                    let weak = cx.weak_entity();
                    cx.defer(move |cx| {
                        let _ = weak.update(cx, |this, cx| this.history_refresh(cx));
                    });
                }
                return loading();
            }
            Some(Err(e)) => return error_line(e.clone(), cx),
            Some(Ok(c)) => c.clone(),
        };
        if commits.is_empty() {
            return empty("No commits yet.", cx);
        }
        let lanes = self
            .side
            .history
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
            .min(LANES_MAX);
        let graph_w = 12. + lanes as f32 * LANE_W;
        let graph = self.side.history.graph.clone();
        let list = v_flex().children(commits.into_iter().zip(graph).enumerate().map(
            |(ix, (commit, row))| {
                let refs: Vec<String> = commit.refs.iter().take(2).cloned().collect();
                let meta = format!(
                    "{} · {} · {}",
                    commit.short(),
                    commit.author,
                    ago(commit.time)
                );
                let open = commit.clone();
                h_flex()
                    .id(SharedString::from(format!("commit-{ix}")))
                    .h(px(ROW_H))
                    .pr_2()
                    .gap_1()
                    .items_center()
                    .cursor_pointer()
                    .hover(|s| s.bg(kit::hover_wash(&p)))
                    .child(graph_cell(row, graph_w, p))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                h_flex()
                                    .gap_1()
                                    .items_center()
                                    .children(refs.into_iter().map(|r| {
                                        div()
                                            .flex_none()
                                            .px_1()
                                            .rounded(px(4.))
                                            .bg(p.accent_wash)
                                            .text_color(p.accent)
                                            .text_size(px(10.))
                                            .max_w(px(110.))
                                            .truncate()
                                            .child(r)
                                    }))
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_xs()
                                            .child(commit.subject.clone()),
                                    ),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_size(px(10.))
                                    .text_color(p.text_faint)
                                    .child(meta),
                            ),
                    )
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.history_open(open.clone(), cx)),
                    )
            },
        ));
        v_flex()
            .flex_1()
            .min_h_0()
            .child(
                h_flex().px_2().justify_end().child(
                    Button::new("history-refresh")
                        .icon(IconName::RefreshCw)
                        .ghost()
                        .xsmall()
                        .tooltip("Refresh")
                        .accessibility_label("Refresh history")
                        .on_click(cx.listener(|this, _, _, cx| this.history_refresh(cx))),
                ),
            )
            .child(scroll("history-list").child(list))
            .into_any_element()
    }

    fn render_commit(&self, detail: &Detail, cx: &mut Context<Self>) -> AnyElement {
        let p = *Palette::of(cx);
        let c = &detail.commit;
        let sha = c.sha.clone();
        let body = detail
            .message
            .as_deref()
            .and_then(|m| m.split_once('\n').map(|(_, rest)| rest.trim().to_string()))
            .filter(|b| !b.is_empty());
        v_flex()
            .flex_1()
            .min_h_0()
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .items_center()
                    .border_b_1()
                    .border_color(p.border)
                    .child(
                        Button::new("commit-back")
                            .icon(IconName::ChevronLeft)
                            .ghost()
                            .xsmall()
                            .tooltip("Back")
                            .accessibility_label("Back")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.side.history.open = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                            .child(c.subject.clone()),
                    )
                    .child(
                        Button::new("commit-copy")
                            .icon(IconName::Copy)
                            .ghost()
                            .xsmall()
                            .tooltip("Copy commit id")
                            .accessibility_label("Copy commit id")
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(sha.clone()))
                            }),
                    ),
            )
            .child(
                v_flex()
                    .px_3()
                    .py_2()
                    .gap_1()
                    .text_xs()
                    .text_color(p.text_muted)
                    .child(format!("{} · {} · {}", c.short(), c.author, ago(c.time)))
                    .when_some(body, |d, body| {
                        d.child(div().whitespace_normal().text_color(p.text).child(body))
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .border_t_1()
                    .border_color(p.border)
                    .child(match (&detail.diff, &detail.error) {
                        (_, Some(e)) => error_line(e.clone(), cx),
                        (Some(diff), None) => Diff::new(diff).size_full().into_any_element(),
                        (None, None) => loading(),
                    }),
            )
            .into_any_element()
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
