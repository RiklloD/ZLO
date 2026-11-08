//! The transcript as the list shows it, ported from Zeron's
//! `crates/ui/src/transcript.rs` (MIT) by way of the GPUI app's
//! `chat/flow.rs`. Activity (reasoning, tool calls, skills, settled
//! approvals) folds into one quiet summary line with a task tree under it,
//! in arrival order between the answer's text. The group is open while it is
//! the live tail and closes once the answer moves on. One status line (a
//! glyph, a rotating verb, the seconds) trails the newest content while a
//! run is live. Sizes are Zeron's, at a 16 px base that the font-size
//! setting scales ([`ui`]).

use std::collections::HashMap;

use zlo_client::GateDecision;
use zlo_client::transcript::{CliBody, GateState, QuestionState, Row, ToolStatus};
use rune::{
    Color, El, Icon, LineCap, Path, Shape, TextStyle, Transition, canvas, col, el, icon, row, text,
};
use rune_kit::{Kit, RollingText};

use crate::design::Palette;
use crate::icons;
use crate::state::State;

/// Zeron's spacing ladder.
pub const SPACE_SM: f32 = 8.;
pub const SPACE_MD: f32 = 12.;
pub const SPACE_LG: f32 = 16.;

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
/// Lines of a command's output tail a row shows.
const OUTPUT_LINES: usize = 14;

/// A size designed at Zeron's 16 px baseline, scaled by the font-size
/// setting (Appearance → Font size; 0 is the default 16).
pub fn ui(s: &State, px_at_16: f32) -> f32 {
    let base = if s.prefs.ui_font_size > 0. {
        s.prefs.ui_font_size
    } else {
        16.
    };
    px_at_16 * base / 16.
}

/// Text at `size` (at 16 px) on a `line` line, both scaled.
pub fn type_at(s: &State, size: f32, line: f32, weight: f32) -> TextStyle {
    TextStyle::new(ui(s, size), weight).lh(ui(s, line))
}

/// An entering row's motion: it rises `y` points while it fades in.
pub fn rise(y: f32) -> Transition {
    let mut t = Transition::offset(0., y);
    t.insertion.opacity = 0.;
    t.removal.opacity = 0.;
    t
}

#[derive(Clone, PartialEq)]
pub enum Item {
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
    pub fn key(&self) -> &str {
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
            | Row::Plan { .. }
            | Row::Question {
                state: QuestionState::Closed(_),
                ..
            }
    )
}

/// Fold `rows` into list items. `live` marks a run in flight; the trailing
/// activity group is then the one being written and opens by itself.
pub fn fold(rows: &[Row], live: bool, stopping: bool, tail: bool, reserve: bool) -> Vec<Item> {
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
pub fn top_gap(prev: Option<&Item>, item: &Item) -> f32 {
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
    let mut words: Vec<&str> = name.split(|c: char| !c.is_ascii_alphanumeric()).collect();
    // A CLI agent's calls arrive as `cli.<tool>`.
    if words.len() > 1 && words[0] == "cli" {
        words.remove(0);
    }
    let has = |list: &[&str]| words.iter().any(|w| list.contains(w));
    // An MCP server's tools are "called", whatever they are named.
    if words.first() == Some(&"mcp") {
        ToolKind::Other
    } else if has(&[
        "shell", "exec", "bash", "zsh", "command", "terminal", "process", "run",
    ]) {
        ToolKind::Command
    } else if has(&[
        "write",
        "edit",
        "multiedit",
        "notebookedit",
        "patch",
        "apply",
        "create",
        "delete",
        "move",
    ]) {
        ToolKind::Edit
    } else if has(&["read", "list", "ls", "cat", "view", "get", "open"]) {
        ToolKind::Read
    } else if has(&[
        "search",
        "grep",
        "glob",
        "find",
        "query",
        "recall",
        "websearch",
    ]) {
        ToolKind::Search
    } else if has(&[
        "fetch", "webfetch", "http", "web", "browse", "url", "download",
    ]) {
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
pub(crate) fn summary(rows: &[Row]) -> String {
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
            Row::Gate { .. } | Row::Question { .. } => approvals += 1,
            Row::Skills { .. } => skills += 1,
            // A run restates its status as it goes; only the latest counts.
            Row::WorkSummary { text, .. } => extra = vec![text.clone()],
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
pub fn is_open(key: &str, live: bool, toggles: &HashMap<String, bool>) -> bool {
    toggles.get(key).copied().unwrap_or(live)
}

/// A tree row: the label (verb), its detail, an icon and whether it failed.
struct Leaf {
    icon: &'static Icon,
    label: String,
    detail: String,
    /// The full text the row opens to, when there is more than one line.
    body: Option<String>,
    /// The streamed tail of a command's output (Codex), shown under the
    /// row while it runs and when opened after.
    output: Option<CliBody>,
    failed: bool,
    running: bool,
}

/// The tail thought of a live group, as Zeron draws it: a "Thinking" label
/// and nothing more. Its words show once it ends, folded under "Thought".
fn live_thought() -> Leaf {
    Leaf {
        icon: &icons::BOT,
        label: "Thinking".into(),
        detail: String::new(),
        body: None,
        output: None,
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
        .is_some_and(|p| *p == "ironclaw" || *p == "builtin" || *p == "cli")
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
            icon: &icons::BOT,
            label: "Thought".into(),
            detail: one_line(text),
            body: (text.trim().len() > 80).then(|| text.trim().to_string()),
            output: None,
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
                    ToolKind::Command => &icons::SQUARE_TERMINAL,
                    ToolKind::Edit => &icons::FILE_TEXT,
                    ToolKind::Read => &icons::FILE,
                    ToolKind::Search => &icons::SEARCH,
                    ToolKind::Fetch => &icons::GLOBE,
                    ToolKind::Other => &icons::NETWORK,
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
                output: None,
                failed: *status == ToolStatus::Failed,
                running: *status == ToolStatus::Running,
            }
        }
        Row::Plan { plan, .. } => Leaf {
            icon: &icons::CIRCLE_CHECK,
            label: "Updated plan".into(),
            detail: super::plan_strip::plan_summary(plan),
            body: None,
            output: None,
            failed: false,
            running: false,
        },
        Row::Question {
            form,
            state: QuestionState::Closed(sent),
            ..
        } => Leaf {
            icon: if *sent == Some(false) {
                &icons::CIRCLE_X
            } else {
                &icons::CHECK
            },
            label: match sent {
                Some(true) => "Answered",
                Some(false) => "Dismissed",
                None => "Asked",
            }
            .into(),
            detail: form
                .fields
                .first()
                .map(|f| one_line(&f.label))
                .unwrap_or_default(),
            body: None,
            output: None,
            failed: false,
            running: false,
        },
        Row::Skills { names, .. } => Leaf {
            icon: &icons::BOOK_OPEN,
            label: "Loaded".into(),
            detail: names.join(", "),
            body: None,
            output: None,
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
            let (icon, verdict, declined): (&'static Icon, &str, bool) = match decision {
                Some(GateDecision::Approve { always: true }) => {
                    (&icons::CIRCLE_CHECK, "Always allowed", false)
                }
                Some(GateDecision::Approve { always: false }) => {
                    (&icons::CIRCLE_CHECK, "Approved", false)
                }
                Some(GateDecision::Decline) => (&icons::CIRCLE_X, "Declined", true),
                None => (&icons::CHECK, "Answered", false),
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
                output: None,
                failed: declined,
                running: false,
            }
        }
        _ => return None,
    })
}

/// The tree's line color: opaque, so the trunk and an elbow overlapping it
/// never darken the joint the way two translucent strokes would.
fn rail_color(p: &Palette) -> Color {
    p.bg.mix(p.text, if p.light { 0.16 } else { 0.14 })
}

/// The gutter for one tree row: the trunk coming down from above, a rounded
/// elbow out to the icon, and the trunk carrying on when a row follows.
fn rail(leaf: &Leaf, last: bool, p: &Palette) -> El {
    let color = rail_color(p);
    let mid = TREE_ROW_HEIGHT / 2.;
    let tint = if leaf.failed { p.danger } else { p.text_muted };
    let x = TRUNK_X + 0.5;
    let trunk_end = if last {
        mid - BEND_RADIUS
    } else {
        TREE_ROW_HEIGHT
    };
    let shape = Shape::new()
        .line([x, 0.], [x, trunk_end], 1., LineCap::Butt, color)
        .stroke_path(
            &Path::new()
                .move_to([x, mid - BEND_RADIUS])
                .quad_to([x, mid + 0.5], [x + BEND_RADIUS, mid + 0.5])
                .line_to([BRANCH_END_X, mid + 0.5]),
            1.,
            LineCap::Butt,
            color,
        );
    el().w(GUTTER_WIDTH)
        .h(TREE_ROW_HEIGHT)
        .child(
            canvas(shape)
                .size(GUTTER_WIDTH, TREE_ROW_HEIGHT)
                .absolute(0., 0.),
        )
        .child(
            icon(leaf.icon, ICON_SIZE)
                .color(tint)
                .absolute(ICON_LEFT, mid - ICON_SIZE / 2.),
        )
}

/// The activity group: a summary line that opens to the tree.
#[allow(clippy::too_many_arguments)]
pub fn render_activity(
    s: &State,
    thread: &str,
    key: &str,
    rows: &[Row],
    live: bool,
    open: bool,
    toggles: &HashMap<String, bool>,
    outputs: &HashMap<String, CliBody>,
    k: &Kit,
) -> El {
    let p = &s.palette;
    let label = summary(rows);
    let small = type_at(s, 12., LABEL_LINE, 400.);
    let toggle_key = key.to_string();
    let head_key = format!("{key}-head");
    let thread_id = thread.to_string();
    let title = if live {
        // The live group's line breathes while the run works.
        text(label.clone())
            .font(small)
            .color(p.text_muted)
            .truncate()
            .looping(rune::LoopSpec {
                period: 1.6,
                delay: 0.,
                kind: rune::LoopKind::Opacity {
                    low: 0.55,
                    high: 1.,
                },
            })
    } else {
        RollingText::new(format!("{key}-title"), label.clone())
            .font(small)
            .color(p.text_muted)
            .build(k)
    };
    let chevron = icon(&icons::CHEVRON_DOWN, 13.)
        .color(p.text_muted)
        .rotate(if open {
            0.
        } else {
            -std::f32::consts::FRAC_PI_2
        });
    let header = row()
        .key(format!("{key}-head"))
        .role("button")
        .label(label.clone())
        .h(HEADER_HEIGHT)
        .gap(6.)
        .pr(4.)
        .center_y()
        .cursor(rune::Cursor::Pointer)
        .transition(rise(4.))
        .child(
            el().w(22.)
                .h(18.)
                .child(chevron.absolute(TRUNK_X - 6.5, 2.5)),
        )
        .child(el().min_w(0.).shrink().child(title))
        // The header stays under the pointer while its rows open or shut.
        .on_press_tx(move |s: &mut State, tx: &mut rune::Tx| {
            let Some(c) = super::chat_for(s, &thread_id) else {
                return;
            };
            let now = c.activity_open(&toggle_key, live);
            c.toggles.insert(toggle_key.clone(), !now);
            tx.keep_on_screen(head_key.clone());
        });

    let mut group = col().key(format!("{key}-group")).w_full().child(header);
    if !open {
        return group;
    }
    let parts: Vec<(&Row, Leaf)> = rows
        .iter()
        .filter_map(|r| leaf(r).map(|l| (r, l)))
        .collect();
    let count = parts.len();
    let mut tree = col()
        .key(format!("{key}-tree"))
        .w_full()
        .transition(rise(4.));
    for (ix, (row, mut leaf)) in parts.into_iter().enumerate() {
        let row_key = row.key();
        let open_key = format!("leaf:{row_key}");
        // The thought being written stays open; it folds once it ends.
        let thinking_now = live && ix + 1 == count && matches!(row, Row::Thinking { .. });
        if thinking_now {
            leaf = live_thought();
        }
        if matches!(row, Row::Tool { .. }) {
            leaf.output = outputs
                .get(row_key)
                .filter(|o| !o.body.trim().is_empty())
                .cloned();
        }
        let opened = toggles.get(&open_key).copied();
        // A running command shows its output until closed; a finished one
        // folds it away until opened.
        let expanded = match (&leaf.output, opened) {
            (Some(_), Some(on)) => on,
            (Some(_), None) => leaf.running,
            (None, on) => leaf.body.is_some() && on.unwrap_or(false),
        };
        let last = ix + 1 == count;
        tree = tree.child(tree_row(s, thread, row_key, leaf, last, expanded, open_key));
    }
    group = group.child(tree);
    group
}

fn tree_row(
    s: &State,
    thread: &str,
    key: &str,
    leaf: Leaf,
    last: bool,
    expanded: bool,
    open_key: String,
) -> El {
    let p = &s.palette;
    let tint = if leaf.failed { p.danger } else { p.text_muted };
    let expandable = leaf.body.is_some() || leaf.output.is_some();
    let small = type_at(s, 12., LABEL_LINE, 400.);
    let mut words = row()
        .ml(TEXT_GAP)
        .min_w(0.)
        .grow(1.)
        .gap(8.)
        .center_y()
        .child(text(leaf.label.clone()).font(small).color(if leaf.running {
            p.text
        } else {
            tint
        }));
    // An opened row shows its text once, below; the label line keeps only
    // the verb.
    if !leaf.detail.is_empty() && !expanded {
        words = words.child(
            text(leaf.detail.clone())
                .font(small)
                .color(if leaf.failed { p.danger } else { p.text_faint })
                .truncate()
                .min_w(0.)
                .shrink(),
        );
    }
    let mut line = row()
        .key(format!("{key}-leaf"))
        .w_full()
        .h(TREE_ROW_HEIGHT)
        .transition(rise(4.))
        .child(rail(&leaf, last && !expanded, p))
        .child(words);
    if expandable {
        let thread_id = thread.to_string();
        let leaf_key = format!("{key}-leaf");
        line = line
            .cursor(rune::Cursor::Pointer)
            .hover(p.hover)
            .radius(design_radius())
            .on_press_tx(move |s: &mut State, tx: &mut rune::Tx| {
                let Some(c) = super::chat_for(s, &thread_id) else {
                    return;
                };
                let on = c.toggles.get(&open_key).copied().unwrap_or(expanded);
                c.toggles.insert(open_key.clone(), !on);
                tx.keep_on_screen(leaf_key.clone());
            });
    }
    if !expanded || (leaf.body.is_none() && leaf.output.is_none()) {
        return line;
    }
    // An opened row's full text hangs under its label, the trunk carrying on
    // beside it unless this is the last row.
    let mut gutter = el().w(GUTTER_WIDTH + TEXT_GAP).self_stretch();
    if !last {
        gutter = gutter.child(el().w(1.).fill(rail_color(p)).pin(
            Some(TRUNK_X),
            Some(0.),
            None,
            Some(0.),
        ));
    }
    let mut body_col = col().grow(1.).min_w(0.).pb(8.).gap(4.);
    if let Some(body) = &leaf.body {
        body_col = body_col.child(
            text(body.clone())
                .font(small)
                .color(p.text_muted)
                .wrap()
                .max_lines(20)
                .selectable(true),
        );
    }
    if let Some(output) = &leaf.output {
        body_col = body_col.child(output_tail(s, output, leaf.running));
    }
    col()
        .key(format!("{key}-leafbox"))
        .w_full()
        .child(line)
        .child(
            row()
                .key(format!("{key}-detail"))
                .w_full()
                .transition(rise(4.))
                .child(gutter)
                .child(body_col),
        )
}

fn design_radius() -> f32 {
    crate::design::CONTROL_RADIUS
}

/// A command's streamed output: its newest lines in a monospace well.
fn output_tail(s: &State, output: &CliBody, running: bool) -> El {
    let p = &s.palette;
    let lines: Vec<&str> = output.body.trim_end_matches('\n').lines().collect();
    let skip = lines.len().saturating_sub(OUTPUT_LINES);
    let cut = output.truncated || skip > 0;
    let mono = type_at(s, 11., 15., 400.).mono();
    let mut well = col()
        .w_full()
        .px(8.)
        .py(4.)
        .radius(6.)
        .fill(p.text.alpha(0.04));
    if cut {
        well = well.child(text("…").font(mono).color(p.text_faint));
    }
    for (i, l) in lines[skip..].iter().enumerate() {
        well = well.child(
            text(l.to_string())
                .key(format!("out-{i}"))
                .font(mono)
                .color(p.text_muted)
                .truncate()
                .min_w(0.),
        );
    }
    if running {
        well = well.child(text("▍").font(mono).color(p.text_faint));
    }
    well
}

/// The person's message: Zeron's pill, right-aligned, at most 80 % of the
/// column, a faint wash rather than a colored bubble.
pub fn user_pill(s: &State, key: &str, body: El, pending: bool, column: f32) -> El {
    let p = &s.palette;
    // An opaque step off the canvas, so the bubble reads the same over a
    // wallpaper, a solid canvas and either scheme.
    let wash = p.bg.mix(p.text, if p.light { 0.06 } else { 0.11 });
    row().w_full().justify_end().child(
        el().key(format!("{key}-pill"))
            .min_w(0.)
            .max_w(column * 0.8)
            .fill(wash)
            .radius(crate::design::BUBBLE_RADIUS)
            .px(16.)
            .py(10.)
            .opacity(if pending { 0.65 } else { 1. })
            .transition(rise(6.))
            .child(body),
    )
}
