//! The transcript as the list shows it, ported from Zeron's
//! `crates/ui/src/transcript.rs` (MIT). Activity (reasoning, tool calls,
//! skills, settled approvals) folds into one quiet summary line with a task
//! tree under it, in arrival order between the answer's text. The group is
//! open while it is the live tail and closes once the answer moves on. One
//! status line (a gradient glyph, a rotating verb, the seconds) trails the
//! newest content while a run is live. Sizes are Zeron's, in rems of a 16 px
//! base, so the font-size setting scales them.

use std::collections::HashMap;
use std::time::Instant;

use ironclaw_gpui_client::GateDecision;
use ironclaw_gpui_client::transcript::{GateState, Row, ToolStatus};

use crate::design::{Palette, kit, motion};
use crate::ui::*;

use super::ChatView;

/// Zeron's spacing ladder.
pub(super) const SPACE_SM: f32 = 8.;
pub(super) const SPACE_MD: f32 = 12.;
pub(super) const SPACE_LG: f32 = 16.;

const HEADER_HEIGHT: f32 = 26.;
const TREE_ROW_HEIGHT: f32 = 32.;
const GUTTER_WIDTH: f32 = 48.;
const TRUNK_X: f32 = 12.;
const BEND_RADIUS: f32 = 6.;
const BRANCH_END_X: f32 = 28.;
const ICON_LEFT: f32 = 32.;
const ICON_SIZE: f32 = 14.;
const TEXT_GAP: f32 = 8.;
const LABEL_LINE: f32 = 18.;

/// Sizes designed at Zeron's 16 px baseline.
pub(super) fn ui(px_at_16: f32) -> gpui_kit::Rems {
    gpui_kit::rems(px_at_16 / 16.)
}

#[derive(Clone, PartialEq)]
pub(super) enum Item {
    Row(Row),
    /// Consecutive activity rows, keyed by the first one.
    Activity {
        key: String,
        rows: Vec<Row>,
        live: bool,
    },
    Pending {
        stopping: bool,
    },
    /// Live subagents, the goal and queued turns, after the newest content.
    Tail,
    Reserve,
}

impl Item {
    pub(super) fn key(&self) -> &str {
        match self {
            Item::Row(row) => row.key(),
            Item::Activity { key, .. } => key,
            Item::Pending { .. } => "\u{0}pending",
            Item::Tail => "\u{0}tail",
            Item::Reserve => "\u{0}reserve",
        }
    }
}

/// Rows that fold into a group. A settled approval is part of the work it
/// unblocked; an open one stays a card, because it waits on the person.
fn is_activity(row: &Row) -> bool {
    matches!(
        row,
        Row::Thinking { .. }
            | Row::Tool { .. }
            | Row::Skills { .. }
            | Row::WorkSummary { .. }
            | Row::Gate {
                state: GateState::Closed(_),
                ..
            }
    )
}

/// Fold `rows` into list items. `live` marks a run in flight; the trailing
/// activity group is then the one being written and opens by itself.
pub(super) fn fold(
    rows: &[Row],
    live: bool,
    stopping: bool,
    tail: bool,
    reserve: bool,
) -> Vec<Item> {
    let mut items: Vec<Item> = Vec::with_capacity(rows.len() + 2);
    for row in rows {
        if is_activity(row) {
            if let Some(Item::Activity { rows, .. }) = items.last_mut() {
                rows.push(row.clone());
                continue;
            }
            items.push(Item::Activity {
                key: format!("act-{}", row.key()),
                rows: vec![row.clone()],
                live: false,
            });
        } else {
            items.push(Item::Row(row.clone()));
        }
    }
    if live && let Some(Item::Activity { live, .. }) = items.last_mut() {
        *live = true;
    }
    if live || stopping {
        items.push(Item::Pending { stopping });
    }
    if tail {
        items.push(Item::Tail);
    }
    if reserve {
        items.push(Item::Reserve);
    }
    items
}

/// Zeron's `top_gap_for`: a turn opens 16 above the person's message, a tool
/// group gets 12 on either side, everything else sits 8 apart.
pub(super) fn top_gap(prev: Option<&Item>, item: &Item) -> f32 {
    match (prev, item) {
        (None, _) => SPACE_LG,
        (_, Item::Reserve) => 0.,
        (_, Item::Row(Row::User { .. })) => SPACE_LG + SPACE_SM,
        (_, Item::Pending { .. }) => SPACE_SM,
        (_, Item::Tail) => SPACE_LG,
        (Some(Item::Activity { .. }), _) | (_, Item::Activity { .. }) => SPACE_MD,
        _ => SPACE_SM,
    }
}

/// What a tool did, for the summary and the row label: Zeron's verbs.
#[derive(Clone, Copy, PartialEq)]
enum ToolKind {
    Command,
    Edit,
    Read,
    Search,
    Fetch,
    Other,
}

fn tool_kind(title: &str) -> ToolKind {
    let name = title
        .split_whitespace()
        .next()
        .unwrap_or(title)
        .to_ascii_lowercase();
    let words: Vec<&str> = name.split(|c: char| !c.is_ascii_alphanumeric()).collect();
    let has = |list: &[&str]| words.iter().any(|w| list.contains(w));
    // An MCP server's tools are "called", whatever they are named.
    if words.first() == Some(&"mcp") {
        ToolKind::Other
    } else if has(&[
        "shell", "exec", "bash", "zsh", "command", "terminal", "process", "run",
    ]) {
        ToolKind::Command
    } else if has(&[
        "write", "edit", "patch", "apply", "create", "delete", "move",
    ]) {
        ToolKind::Edit
    } else if has(&["read", "list", "ls", "cat", "view", "get", "open"]) {
        ToolKind::Read
    } else if has(&["search", "grep", "glob", "find", "query", "recall"]) {
        ToolKind::Search
    } else if has(&["fetch", "http", "web", "browse", "url", "download"]) {
        ToolKind::Fetch
    } else {
        ToolKind::Other
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// "Thought process · ran 2 commands · called 2 tools", Zeron's one line
/// for a group, capitalized at its first letter only.
fn summary(rows: &[Row]) -> String {
    let mut counts = [0usize; 6];
    let (mut thoughts, mut failed, mut approvals, mut skills) = (0, 0, 0, 0);
    let mut extra = Vec::new();
    for row in rows {
        match row {
            Row::Thinking { .. } => thoughts += 1,
            Row::Tool { title, status, .. } => {
                counts[tool_kind(title) as usize] += 1;
                failed += usize::from(*status == ToolStatus::Failed);
            }
            Row::Gate { .. } => approvals += 1,
            Row::Skills { .. } => skills += 1,
            Row::WorkSummary { text, .. } => extra.push(text.clone()),
            _ => {}
        }
    }
    let mut parts = Vec::new();
    match thoughts {
        0 => {}
        1 => parts.push("thought process".to_string()),
        n => parts.push(format!("thought {n} times")),
    }
    let verbs = [
        ("ran", "command", "commands"),
        ("edited", "file", "files"),
        ("read", "file", "files"),
        ("searched", "time", "times"),
        ("fetched", "page", "pages"),
        ("called", "tool", "tools"),
    ];
    for (n, (verb, one, many)) in counts.iter().zip(verbs) {
        if *n > 0 {
            parts.push(format!("{verb} {}", plural(*n, one, many)));
        }
    }
    if skills > 0 {
        parts.push("loaded skills".into());
    }
    if approvals > 0 {
        parts.push(plural(approvals, "approval", "approvals"));
    }
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    parts.extend(extra);
    let mut line = parts.join(" · ");
    if let Some(first) = line.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    line
}

/// Whether a group is drawn open: the reader's own toggle wins, otherwise
/// it follows the run (open while live, closed after).
pub(super) fn is_open(key: &str, live: bool, toggles: &HashMap<String, bool>) -> bool {
    toggles.get(key).copied().unwrap_or(live)
}

/// A tree row: the label (verb), its detail, an icon and whether it failed.
struct Leaf {
    icon: IconName,
    label: String,
    detail: String,
    /// The full text the row opens to, when there is more than one line.
    body: Option<String>,
    failed: bool,
    running: bool,
}

/// How many finished paragraphs a thought in progress shows.
const LIVE_THOUGHT_PARAGRAPHS: usize = 3;

/// The finished paragraphs of a thought still being written, newest last:
/// a paragraph shows once a blank line closes it, never word by word.
fn finished_paragraphs(text: &str) -> Option<String> {
    let (done, _open) = text.rsplit_once("\n\n")?;
    let paragraphs: Vec<&str> = done
        .split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    let skip = paragraphs.len().saturating_sub(LIVE_THOUGHT_PARAGRAPHS);
    let shown = paragraphs[skip..].join("\n\n");
    (!shown.is_empty()).then_some(shown)
}

/// The tail thought of a live group: open under a "Thinking" label.
fn live_thought(text: &str) -> Leaf {
    Leaf {
        icon: IconName::Bot,
        label: "Thinking".into(),
        detail: String::new(),
        body: finished_paragraphs(text),
        failed: false,
        running: true,
    }
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// "ironclaw.shell" → "Shell"; "mcp.yab-browser.open" → "yab-browser · open".
fn tool_name(title: &str) -> (String, String) {
    let (name, rest) = title.split_once(' ').unwrap_or((title, ""));
    let mut parts: Vec<&str> = name
        .split(['.', ':', '/'])
        .filter(|p| !p.is_empty())
        .collect();
    if parts
        .first()
        .is_some_and(|p| *p == "ironclaw" || *p == "builtin")
        && parts.len() > 1
    {
        parts.remove(0);
    }
    let mcp = parts.first().is_some_and(|p| p.eq_ignore_ascii_case("mcp"));
    if mcp {
        parts.remove(0);
    }
    let shown = parts.join(" · ").replace('_', " ");
    let label = if mcp {
        "MCP".to_string()
    } else {
        String::new()
    };
    let detail = [shown, rest.trim().to_string()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    (label, detail)
}

fn leaf(row: &Row) -> Option<Leaf> {
    Some(match row {
        Row::Thinking { text, .. } => Leaf {
            icon: IconName::Bot,
            label: "Thought".into(),
            detail: one_line(text),
            body: (text.trim().len() > 80).then(|| text.trim().to_string()),
            failed: false,
            running: false,
        },
        Row::Tool {
            title,
            detail,
            status,
            reason,
            ..
        } => {
            let kind = tool_kind(title);
            let (mcp, name) = tool_name(title);
            let verb = match kind {
                _ if !mcp.is_empty() => mcp,
                ToolKind::Command => "Ran".into(),
                ToolKind::Edit => "Edited".into(),
                ToolKind::Read => "Read".into(),
                ToolKind::Search => "Searched".into(),
                ToolKind::Fetch => "Fetched".into(),
                ToolKind::Other => "Called".into(),
            };
            let arg = detail.as_deref().map(one_line).unwrap_or_default();
            // A command reads as the command itself; other tools by name.
            let shown = if kind == ToolKind::Command && !arg.is_empty() {
                arg.clone()
            } else if arg.is_empty() {
                name
            } else {
                format!("{name} · {arg}")
            };
            Leaf {
                icon: match kind {
                    ToolKind::Command => IconName::SquareTerminal,
                    ToolKind::Edit => IconName::FileText,
                    ToolKind::Read => IconName::File,
                    ToolKind::Search => IconName::Search,
                    ToolKind::Fetch => IconName::Globe,
                    ToolKind::Other => IconName::Network,
                },
                label: verb,
                detail: shown,
                body: super::approval_reason::leaf_body(
                    reason.as_deref(),
                    detail
                        .as_deref()
                        .map(str::trim)
                        .filter(|d| d.len() > 90 || d.contains('\n'))
                        .map(str::to_string),
                ),
                failed: *status == ToolStatus::Failed,
                running: *status == ToolStatus::Running,
            }
        }
        Row::Skills { names, .. } => Leaf {
            icon: IconName::BookOpen,
            label: "Loaded".into(),
            detail: names.join(", "),
            body: None,
            failed: false,
            running: false,
        },
        Row::WorkSummary { text, .. } => Leaf {
            icon: IconName::FileText,
            label: "Noted".into(),
            detail: one_line(text),
            body: None,
            failed: false,
            running: false,
        },
        Row::Gate {
            headline,
            body,
            state: GateState::Closed(decision),
            reason,
            ..
        } => {
            let (icon, verdict, declined) = match decision {
                Some(GateDecision::Approve { always: true }) => {
                    (IconName::CircleCheck, "Always allowed", false)
                }
                Some(GateDecision::Approve { always: false }) => {
                    (IconName::CircleCheck, "Approved", false)
                }
                Some(GateDecision::Decline) => (IconName::CircleX, "Declined", true),
                None => (IconName::Check, "Answered", false),
            };
            let text = body.clone().unwrap_or_else(|| headline.clone());
            Leaf {
                icon,
                label: verdict.into(),
                detail: one_line(&text),
                body: super::approval_reason::leaf_body(
                    reason.as_deref(),
                    (text.len() > 90).then_some(text),
                ),
                failed: declined,
                running: false,
            }
        }
        _ => return None,
    })
}

/// The tree's line color: opaque, so the trunk and an elbow overlapping it
/// never darken the joint the way two translucent strokes would.
fn rail_color(p: &Palette) -> gpui_kit::Hsla {
    motion::mix(p.bg, p.text, if p.light { 0.16 } else { 0.14 })
}

/// The gutter for one tree row: the trunk coming down from above, a rounded
/// elbow out to the icon, and the trunk carrying on when a row follows. The
/// elbow draws after the trunk reaches it, as in Zeron's reveal.
fn rail(leaf: &Leaf, last: bool, reveal: f32, p: &Palette) -> impl IntoElement {
    let color = rail_color(p);
    let mid = TREE_ROW_HEIGHT / 2.;
    let incoming = (reveal / 0.62).clamp(0., 1.);
    let branch = ((reveal - 0.58) / 0.42).clamp(0., 1.);
    let tint = if leaf.failed { p.danger } else { p.text_muted };
    div()
        .relative()
        .w(px(GUTTER_WIDTH))
        .h(px(TREE_ROW_HEIGHT))
        .flex_none()
        .child(
            div()
                .absolute()
                .left(px(TRUNK_X))
                .top_0()
                .w(px(1.))
                .h(px(if last {
                    (mid - BEND_RADIUS) * incoming
                } else {
                    TREE_ROW_HEIGHT * incoming
                }))
                .bg(color),
        )
        .when(branch > 0., |d| {
            d.child(
                div()
                    .absolute()
                    .left(px(TRUNK_X))
                    .top(px(mid - BEND_RADIUS))
                    .w(px((BRANCH_END_X - TRUNK_X) * branch))
                    .h(px(BEND_RADIUS + 1.))
                    .border_l_1()
                    .border_b_1()
                    .rounded_bl(px(BEND_RADIUS))
                    .border_color(color),
            )
        })
        .child(
            div()
                .absolute()
                .left(px(ICON_LEFT))
                .top(px(mid - ICON_SIZE / 2.))
                .opacity(branch)
                .text_color(tint)
                .child(
                    Icon::new(leaf.icon.clone())
                        .size(px(ICON_SIZE))
                        .text_color(tint),
                ),
        )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn render_activity(
    key: &str,
    rows: &[Row],
    live: bool,
    open: bool,
    toggles: &HashMap<String, bool>,
    weak: &WeakEntity<ChatView>,
    cx: &App,
) -> AnyElement {
    let p = *Palette::of(cx);
    let reduced = cx.reduce_motion();
    let fold = motion::state_t(&format!("{key}:chevron"), open, motion::TOOL_FOLD, reduced);
    let label = summary(rows);
    let toggle_key = key.to_string();
    let toggle_weak = weak.clone();
    let title = motion::roll_text(
        SharedString::from(format!("{key}-title")),
        label.clone().into(),
        reduced,
    );
    let header = h_flex()
        .id(SharedString::from(format!("{key}-head")))
        .role(Role::Button)
        .aria_label(label.clone())
        .h(px(HEADER_HEIGHT))
        .gap(px(6.))
        .pr_1()
        .items_center()
        .text_size(ui(12.))
        .line_height(px(LABEL_LINE))
        .text_color(p.text_muted)
        .cursor_pointer()
        .hover(|d| d.text_color(p.text))
        .child(
            div().w(px(22.)).h(px(18.)).flex_none().relative().child(
                div().absolute().left(px(TRUNK_X - 6.5)).top(px(2.5)).child(
                    Icon::new(IconName::ChevronDown)
                        .size(px(13.))
                        .rotate(gpui_kit::radians(
                            -std::f32::consts::FRAC_PI_2 * (1. - fold),
                        )),
                ),
            ),
        )
        .child(
            div()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .child(if live {
                    kit::shimmer_text(SharedString::from(format!("{key}-shimmer")), label, cx)
                        .into_any_element()
                } else {
                    title
                }),
        )
        .on_click(move |_, _, cx| {
            let key = toggle_key.clone();
            let _ = toggle_weak.update(cx, |this, cx| {
                let now = this.activity_open(&key, live);
                this.toggles.insert(key, !now);
                cx.notify();
            });
        });

    // The fold morphs to the tree's measured height.
    let parts: Vec<(&Row, Leaf)> = rows
        .iter()
        .filter_map(|r| leaf(r).map(|l| (r, l)))
        .collect();
    let count = parts.len();
    let mut leaves = Vec::with_capacity(count);
    for (ix, (row, mut leaf)) in parts.into_iter().enumerate() {
        let row_key = row.key();
        let open_key = format!("leaf:{row_key}");
        // The thought being written stays open; it folds once it ends.
        let thinking_now = live && ix + 1 == count && matches!(row, Row::Thinking { .. });
        if let Row::Thinking { text, .. } = row
            && thinking_now
        {
            leaf = live_thought(text);
        }
        let expanded = leaf.body.is_some()
            && (thinking_now || toggles.get(&open_key).copied().unwrap_or(false));
        if !live {
            motion::settle(row_key);
        }
        let reveal = motion::reveal(row_key, motion::TOOL_ROW_REVEAL, 0, reduced);
        let last = ix + 1 == count;
        leaves.push(tree_row(
            row_key, leaf, last, reveal, expanded, open_key, weak, &p, reduced,
        ));
    }

    let body = motion::collapse(
        &format!("{key}:fold"),
        open,
        motion::TOOL_FOLD,
        reduced,
        || v_flex().w_full().children(leaves).into_any_element(),
    );
    v_flex()
        .w_full()
        .child(header)
        .children(body)
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn tree_row(
    key: &str,
    leaf: Leaf,
    last: bool,
    reveal: f32,
    expanded: bool,
    open_key: String,
    weak: &WeakEntity<ChatView>,
    p: &Palette,
    reduced: bool,
) -> AnyElement {
    let tint = if leaf.failed { p.danger } else { p.text_muted };
    let expandable = leaf.body.is_some();
    let body = leaf.body.clone();
    let weak = weak.clone();
    let line = h_flex()
        .id(SharedString::from(format!("{key}-leaf")))
        .w_full()
        .h(px(TREE_ROW_HEIGHT))
        .flex_none()
        .group(SharedString::from(format!("{key}-leafg")))
        .child(rail(&leaf, last && !expanded, reveal, p))
        .child(
            h_flex()
                .ml(px(TEXT_GAP))
                .min_w_0()
                .flex_1()
                .gap(px(8.))
                .items_center()
                .relative()
                .top(px(4. * (1. - reveal)))
                .opacity(reveal)
                .text_size(ui(12.))
                .line_height(px(LABEL_LINE))
                .child(
                    div()
                        .flex_none()
                        .text_color(tint)
                        .when(leaf.running, |d| d.text_color(p.text))
                        .child(leaf.label),
                )
                .when(!leaf.detail.is_empty(), |d| {
                    d.child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(if leaf.failed { p.danger } else { p.text_faint })
                            .when(expandable, |d| {
                                d.group_hover(SharedString::from(format!("{key}-leafg")), |s| {
                                    s.text_color(p.text_muted)
                                })
                            })
                            .child(leaf.detail),
                    )
                }),
        )
        .when(expandable, |d| {
            d.cursor_pointer().on_click(move |_, _, cx| {
                let key = open_key.clone();
                let _ = weak.update(cx, |this, cx| {
                    let on = this.toggles.get(&key).copied().unwrap_or(false);
                    this.toggles.insert(key, !on);
                    cx.notify();
                });
            })
        });
    let Some(body) = body else {
        return line.into_any_element();
    };
    // An opened row's full text hangs under its label, the trunk carrying
    // on beside it unless this is the last row.
    let p = *p;
    let detail = motion::collapse(
        &format!("{key}:body"),
        expanded,
        motion::COLLAPSE,
        reduced,
        move || {
            h_flex()
                .w_full()
                .child(
                    div()
                        .w(px(GUTTER_WIDTH + TEXT_GAP))
                        .flex_none()
                        .relative()
                        .self_stretch()
                        .when(!last, |d| {
                            d.child(
                                div()
                                    .absolute()
                                    .left(px(TRUNK_X))
                                    .top_0()
                                    .bottom_0()
                                    .w(px(1.))
                                    .bg(rail_color(&p)),
                            )
                        }),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .pb_2()
                        .max_h(px(360.))
                        .overflow_hidden()
                        .text_size(ui(12.))
                        .line_height(px(LABEL_LINE))
                        .text_color(p.text_muted)
                        .whitespace_normal()
                        .child(body),
                )
                .into_any_element()
        },
    );
    v_flex()
        .w_full()
        .child(line)
        .children(detail)
        .into_any_element()
}

/// The person's message: Zeron's pill, right-aligned, at most 80 % of the
/// column, a faint wash rather than a colored bubble.
pub(super) fn user_pill(key: &str, body: impl IntoElement, pending: bool, cx: &App) -> AnyElement {
    let p = *Palette::of(cx);
    // An opaque step off the canvas, so the bubble reads the same over
    // a wallpaper, a solid canvas and either scheme.
    let wash = motion::mix(p.bg, p.text, if p.light { 0.06 } else { 0.11 });
    if !pending {
        motion::settle(key);
    }
    let t = motion::reveal(key, motion::TOOL_ROW_REVEAL, 0, cx.reduce_motion());
    h_flex()
        .w_full()
        .justify_end()
        .child(
            div()
                .min_w_0()
                .max_w(gpui_kit::relative(0.8))
                .bg(wash)
                .rounded(px(16.))
                .px(px(16.))
                .py(px(10.))
                .text_size(ui(14.))
                .line_height(ui(22.))
                .text_color(p.text)
                .whitespace_normal()
                .relative()
                .top(px(6. * (1. - t)))
                .opacity(if pending { 0.65 * t.max(0.3) } else { t })
                .child(body),
        )
        .into_any_element()
}

/// Zeron's working line: the gradient glyph, a verb that rolls every 7 s
/// and the elapsed seconds, rolling as they tick.
pub(super) fn render_pending(
    stopping: bool,
    since: Option<Instant>,
    seed: u64,
    cx: &App,
) -> AnyElement {
    let p = *Palette::of(cx);
    let reduced = cx.reduce_motion();
    let secs = since.map(|t| t.elapsed().as_secs()).unwrap_or(0);
    let word = if stopping {
        "Stopping".to_string()
    } else {
        motion::flavour_word(seed, secs).to_string()
    };
    h_flex()
        .gap(px(SPACE_SM))
        .items_center()
        .h(px(24.))
        .child(kit::gradient_spinner("working-glyph", 2.5))
        .child(
            div()
                .text_size(ui(12.))
                .text_color(p.text_muted)
                .child(motion::roll_text(
                    "working-word",
                    format!("{word}…").into(),
                    reduced,
                )),
        )
        .child(
            div()
                .relative()
                .top(px(1.))
                .text_size(ui(11.))
                .text_color(p.text_faint)
                .child(motion::roll_text(
                    "working-timer",
                    motion::format_elapsed(secs).into(),
                    reduced,
                )),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(key: &str, title: &str) -> Row {
        Row::Tool {
            key: key.into(),
            title: title.into(),
            detail: None,
            status: ToolStatus::Completed,
            reason: None,
        }
    }

    #[test]
    fn a_live_thought_shows_only_its_finished_paragraphs() {
        assert_eq!(finished_paragraphs("Reading the config"), None);
        assert_eq!(
            finished_paragraphs("One.\n\nTwo.\n\nThree is still go"),
            Some("One.\n\nTwo.".into())
        );
        let long = "a\n\nb\n\nc\n\nd\n\ne";
        assert_eq!(finished_paragraphs(long), Some("b\n\nc\n\nd".into()));
    }

    #[test]
    fn summary_reads_like_zerons() {
        let rows = vec![
            Row::Thinking {
                key: "t".into(),
                text: "hmm".into(),
            },
            tool("a", "ironclaw.shell"),
            tool("b", "ironclaw.shell"),
            tool("c", "mcp.yab-browser.open"),
        ];
        assert_eq!(
            summary(&rows),
            "Thought process · ran 2 commands · called 1 tool"
        );
    }

    #[test]
    fn settled_gates_fold_into_the_work_and_open_ones_do_not() {
        let gate = |state| Row::Gate {
            key: "g".into(),
            run_id: "r".into(),
            gate_ref: "g".into(),
            kind: ironclaw_gpui_client::contract::ProductGateKind::Approval,
            headline: "Run zsh".into(),
            body: None,
            allow_always: true,
            state,
            reason: None,
        };
        let rows = vec![
            tool("a", "ironclaw.shell"),
            gate(GateState::Closed(Some(GateDecision::Approve {
                always: false,
            }))),
            tool("b", "ironclaw.shell"),
        ];
        assert_eq!(fold(&rows, false, false, false, false).len(), 1);
        let rows = vec![tool("a", "ironclaw.shell"), gate(GateState::Open)];
        assert_eq!(fold(&rows, true, false, false, false).len(), 3);
        // The tail follows the working line and stays above the reserve.
        let items = fold(&rows, true, false, true, true);
        assert!(matches!(
            items[items.len() - 3..],
            [Item::Pending { .. }, Item::Tail, Item::Reserve]
        ));
    }
}
