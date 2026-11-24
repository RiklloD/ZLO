//! The transcript as the list shows it: t3code's timeline. A turn is its
//! prompt, then the agent's words and its work in arrival order, then the
//! answer. Activity (reasoning, tool calls, skills, settled approvals)
//! between the words folds into one line per group: a single step's own
//! line, or the group's summary ("Ran 3 commands and edited 2 files"),
//! opening to the steps. While a run is live, "Working for …" sits under
//! its prompt, the trailing group's line follows the newest step and a
//! "Thinking" line closes the turn otherwise. Once the run settles,
//! everything before the answer folds under "Worked for …" (the work after
//! a steer under "Continued for …"). Sizes are at a 16 px base that the
//! font-size setting scales ([`ui`]).

use std::collections::HashMap;

use rune::{El, Icon, TextStyle, Transition, col, el, row, text};
use rune_kit::{Kit, RollingText};
use zlo_client::GateDecision;
use zlo_client::transcript::{CliBody, GateState, QuestionState, Row, ToolStatus};

use crate::icons;
use crate::state::State;

/// Zeron's spacing ladder.
pub const SPACE_SM: f32 = 8.;
pub const SPACE_MD: f32 = 12.;
pub const SPACE_LG: f32 = 16.;

/// A work line's height (t3code's `min-h-6`).
const TREE_ROW_HEIGHT: f32 = 24.;

#[path = "flow/timeline.rs"]
mod timeline;
/// The icon's column at the head of a work line.
const ICON_COLUMN: f32 = 20.;
const ICON_SIZE: f32 = 14.;
const TEXT_GAP: f32 = 6.;
const LABEL_SIZE: f32 = 13.;
const LABEL_LINE: f32 = 20.;
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
    /// Consecutive activity rows, keyed by the first one: one line, a tool
    /// call's own or the group's summary, that opens to the steps
    /// (t3code's work row and tool-group toggle).
    Activity {
        key: String,
        rows: Vec<Row>,
        /// The live turn's trailing group: its line follows the newest step.
        live: bool,
    },
    /// The live turn's line under its prompt: "Working for 1m 3s"
    /// (t3code's working row).
    Working,
    /// A settled turn's work, or the work after one of its steers, folded
    /// under one line: "Worked for 31m 12s" (t3code's turn fold). The
    /// answer stays below it. `hidden` lists the rows it holds while shut,
    /// so a search hit inside can open it.
    TurnFold {
        key: String,
        label: String,
        open: bool,
        hidden: Vec<String>,
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
            Item::Activity { key, .. } | Item::TurnFold { key, .. } => key,
            Item::Working => "\u{0}working",
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

/// A person's message that starts a turn (a steer joins the turn it steers).
fn is_prompt(item: &Item) -> bool {
    matches!(item, Item::Row(Row::User { steer: None, .. }))
}

/// What a fold reads besides the rows.
#[derive(Clone, Copy)]
pub struct Folding<'a> {
    /// Each settled prompt's worked time (`Transcript::turn_spans`).
    pub spans: &'a HashMap<String, i64>,
    /// The folds and groups the reader opened or shut.
    pub toggles: &'a HashMap<String, bool>,
    /// The running turn's prompt is among the rows
    /// (`Transcript::shows_running_prompt`): false while a run started
    /// elsewhere waits for its prompt, so the newest prompt shown is not
    /// taken for the live turn's.
    pub prompt_shown: bool,
}

/// Fold `rows` into list items, t3code's timeline: each turn is its
/// prompt, the agent's words and tool groups in arrival order, and the
/// answer. `live` marks a run in flight: its turn gets the working line
/// under the prompt and its trailing group follows the newest step. A
/// settled turn folds everything before its answer under "Worked for …",
/// shut unless the reader opened it.
pub fn fold(
    rows: &[Row],
    live: bool,
    stopping: bool,
    tail: bool,
    reserve: bool,
    ctx: &Folding,
) -> Vec<Item> {
    let mut grouped: Vec<Item> = Vec::with_capacity(rows.len() + 2);
    for row in rows {
        if is_activity(row) {
            if let Some(Item::Activity { rows, .. }) = grouped.last_mut() {
                rows.push(row.clone());
                continue;
            }
            grouped.push(Item::Activity {
                key: format!("act-{}", row.key()),
                rows: vec![row.clone()],
                live: false,
            });
        } else {
            grouped.push(Item::Row(row.clone()));
        }
    }
    if live && let Some(Item::Activity { live, .. }) = grouped.last_mut() {
        *live = true;
    }
    let working = live || stopping;
    let mut items = fold_turns(grouped, working, ctx);
    // The thinking line closes the live turn, unless its trailing group
    // already shows the step at work.
    if stopping || (live && !matches!(items.last(), Some(Item::Activity { live: true, .. }))) {
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

/// Split `items` into turns at each prompt: the live (newest) turn gets its
/// working line, every settled turn folds its work. A run whose prompt has
/// not loaded yet gets its working line at the end.
fn fold_turns(items: Vec<Item>, working: bool, ctx: &Folding) -> Vec<Item> {
    let starts: Vec<usize> = items
        .iter()
        .enumerate()
        .filter_map(|(ix, item)| is_prompt(item).then_some(ix))
        .collect();
    let Some(&first) = starts.first() else {
        let mut items = items;
        if working {
            items.push(Item::Working);
        }
        return items;
    };
    let mut out = Vec::with_capacity(items.len() + starts.len() + 1);
    out.extend_from_slice(&items[..first]);
    for (n, &start) in starts.iter().enumerate() {
        let end = starts.get(n + 1).copied().unwrap_or(items.len());
        let prompt = &items[start];
        out.push(prompt.clone());
        let body = &items[start + 1..end];
        if working && ctx.prompt_shown && n + 1 == starts.len() {
            out.push(Item::Working);
            out.extend_from_slice(body);
        } else {
            fold_turn(prompt.key(), body, ctx.spans, ctx.toggles, &mut out);
        }
    }
    if working && !ctx.prompt_shown {
        out.push(Item::Working);
    }
    out
}

/// One settled turn after its prompt. Its answer is its last reply; the
/// words and work before it fold, and so does finished work after it. The
/// work after each steer folds on its own, under "Continued for …", so the
/// steer stays between the work it split. A run that failed or was stopped
/// keeps everything in view, as do open cards and dividers.
fn fold_turn(
    prompt_key: &str,
    body: &[Item],
    spans: &HashMap<String, i64>,
    toggles: &HashMap<String, bool>,
    out: &mut Vec<Item>,
) {
    if body
        .iter()
        .any(|i| matches!(i, Item::Row(Row::RunNotice { .. })))
    {
        out.extend_from_slice(body);
        return;
    }
    let answer = body
        .iter()
        .rposition(|i| matches!(i, Item::Row(Row::Assistant { .. })));
    let hides = |ix: usize, item: &Item| {
        let after = answer.is_some_and(|a| ix > a);
        match item {
            // The live run's work is never folded into an older turn.
            Item::Activity { live: true, .. } => false,
            Item::Activity { rows, .. } => {
                !after
                    || rows.iter().all(|r| {
                        !matches!(
                            r,
                            Row::Tool {
                                status: ToolStatus::Running | ToolStatus::Failed,
                                ..
                            }
                        )
                    })
            }
            Item::Row(Row::Narration { .. }) => !after,
            Item::Row(Row::Assistant { .. }) => Some(ix) != answer && !after,
            _ => false,
        }
    };
    // Segments: the work before the first steer, then after each steer.
    let mut segment_start = 0;
    let mut label = spans.get(prompt_key).map_or_else(
        || "Worked".to_string(),
        |ms| format!("Worked for {}", duration(*ms)),
    );
    let mut fold_key = format!("fold-{prompt_key}");
    for ix in 0..=body.len() {
        let steer = body.get(ix).and_then(|item| match item {
            Item::Row(Row::User {
                key,
                steer: Some(mark),
                ..
            }) => Some((key, mark)),
            _ => None,
        });
        if ix < body.len() && steer.is_none() {
            continue;
        }
        let segment = segment_start..ix;
        let hidden: Vec<usize> = segment.clone().filter(|&i| hides(i, &body[i])).collect();
        if !hidden.is_empty() {
            let open = toggles.get(&fold_key).copied().unwrap_or(false);
            let keys = if open {
                Vec::new()
            } else {
                hidden.iter().flat_map(|&i| item_keys(&body[i])).collect()
            };
            // A shut turn's subagents sit above its fold (t3code), so each
            // opens without opening the turn.
            let hoisted = |i: usize| !open && matches!(body[i], Item::Row(Row::Subagent { .. }));
            out.extend(
                segment
                    .clone()
                    .filter(|&i| hoisted(i))
                    .map(|i| body[i].clone()),
            );
            out.push(Item::TurnFold {
                key: fold_key.clone(),
                label: label.clone(),
                open,
                hidden: keys,
            });
            for i in segment {
                if (open || !hidden.contains(&i)) && !hoisted(i) {
                    out.push(body[i].clone());
                }
            }
        } else {
            out.extend_from_slice(&body[segment]);
        }
        if let Some((key, mark)) = steer {
            out.push(body[ix].clone());
            segment_start = ix + 1;
            fold_key = format!("fold-{key}");
            label = super::dispatch::continued_label(mark.continued_ms);
        }
    }
}

/// The row keys an item holds, for a search hit inside a shut fold.
fn item_keys(item: &Item) -> Vec<String> {
    match item {
        Item::Activity { key, rows, .. } => std::iter::once(key.clone())
            .chain(rows.iter().map(|r| r.key().to_string()))
            .collect(),
        other => vec![other.key().to_string()],
    }
}

/// t3code's turn length: "45s", "31m 12s", "1h 2m 3s".
pub(crate) fn duration(ms: i64) -> String {
    let secs = (ms.max(0) + 500) / 1000;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    match (h, m) {
        (0, 0) => format!("{s}s"),
        (0, _) => format!("{m}m {s}s"),
        _ => format!("{h}h {m}m {s}s"),
    }
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
        // The working line and a turn's fold sit close under the prompt;
        // their hairline gets room below.
        (_, Item::Working | Item::TurnFold { .. }) => SPACE_SM,
        (Some(Item::Working | Item::TurnFold { .. }), _) => SPACE_MD,
        // Work lines stack like one list (t3code's work log).
        (Some(Item::Activity { .. }), Item::Activity { .. }) => 2.,
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

/// "a", "a and b", "a, b and c".
fn join_clauses(parts: &[String]) -> String {
    match parts {
        [] => String::new(),
        [one] => one.clone(),
        [head @ .., last] => format!("{} and {last}", head.join(", ")),
    }
}

/// A group's one line, t3code's tool-group summary: the two leading kinds
/// of work (commands and edits first), then a count of the rest — "Ran 3
/// commands and edited 2 files", "Read 4 files, searched 2 times and
/// performed 1 other action". Thoughts alone read "Thought".
pub(crate) fn summary(rows: &[Row]) -> String {
    let mut counts = [0usize; 6];
    let mut first = [usize::MAX; 6];
    let (mut thoughts, mut others) = (0, 0);
    for (ix, row) in rows.iter().enumerate() {
        match row {
            Row::Thinking { .. } => thoughts += 1,
            Row::Tool { title, .. } => {
                let kind = tool_kind(title) as usize;
                counts[kind] += 1;
                first[kind] = first[kind].min(ix);
            }
            // A run's restated status is not a step.
            Row::WorkSummary { .. } => {}
            _ => others += 1,
        }
    }
    if counts.iter().all(|n| *n == 0) && others == 0 {
        return match thoughts {
            0 => String::new(),
            1 => "Thought".into(),
            n => format!("Thought (×{n})"),
        };
    }
    let verbs = [
        ("Ran", "command", "commands"),
        ("Edited", "file", "files"),
        ("Read", "file", "files"),
        ("Searched", "time", "times"),
        ("Fetched", "page", "pages"),
        ("Called", "tool", "tools"),
    ];
    let mut kinds: Vec<usize> = (0..counts.len()).filter(|k| counts[*k] > 0).collect();
    // Commands and edits lead; ties go to whichever came first.
    kinds.sort_by_key(|k| (usize::from(*k > ToolKind::Edit as usize), first[*k]));
    kinds.truncate(2);
    kinds.sort_by_key(|k| first[*k]);
    let mut parts: Vec<String> = kinds
        .iter()
        .map(|k| {
            let (verb, one, many) = verbs[*k];
            format!("{verb} {}", plural(counts[*k], one, many))
        })
        .collect();
    let rest =
        counts.iter().sum::<usize>() - kinds.iter().map(|k| counts[*k]).sum::<usize>() + others;
    if rest > 0 {
        let verb = if parts.is_empty() {
            "Performed"
        } else {
            "performed"
        };
        let what = if parts.is_empty() {
            "action"
        } else {
            "other action"
        };
        parts.push(format!(
            "{verb} {}",
            plural(rest, what, &format!("{what}s"))
        ));
    }
    let lowered: Vec<String> = parts
        .iter()
        .enumerate()
        .map(|(ix, part)| {
            if ix == 0 {
                part.clone()
            } else {
                lower_first(part)
            }
        })
        .collect();
    join_clauses(&lowered)
}

/// "Ran 2 commands" → "ran 2 commands", to follow a lead-in.
fn lower_first(line: &str) -> String {
    let mut out = line.to_string();
    if let Some(first) = out.get_mut(..1) {
        first.make_ascii_lowercase();
    }
    out
}

/// A group's icon: its kind of work when there is one kind, else a mix.
fn group_icon(rows: &[Row]) -> &'static Icon {
    let mut kinds = rows.iter().filter_map(|r| match r {
        Row::Tool { title, .. } => Some(tool_kind(title)),
        _ => None,
    });
    match kinds.next() {
        None if rows.iter().all(|r| matches!(r, Row::Thinking { .. })) => &icons::BOT,
        Some(kind) if kinds.all(|k| k == kind) => kind_icon(kind),
        _ => &icons::ACTIVITY,
    }
}

fn kind_icon(kind: ToolKind) -> &'static Icon {
    match kind {
        ToolKind::Command => &icons::SQUARE_TERMINAL,
        ToolKind::Edit => &icons::SQUARE_PEN,
        ToolKind::Read => &icons::FILE_TEXT,
        ToolKind::Search => &icons::SEARCH,
        ToolKind::Fetch => &icons::GLOBE,
        ToolKind::Other => &icons::ZAP,
    }
}

/// Whether a group (or one of its steps) is drawn open: shut until the
/// reader opens it, live or not (t3code's work rows).
pub fn is_open(key: &str, toggles: &HashMap<String, bool>) -> bool {
    toggles.get(key).copied().unwrap_or(false)
}

/// A tree row: the label (verb), its detail, an icon and whether it failed.
#[derive(Clone)]
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
            let is_mcp = !mcp.is_empty();
            let verb = match kind {
                _ if is_mcp => mcp,
                ToolKind::Command => "Ran".into(),
                ToolKind::Edit => "Edited".into(),
                ToolKind::Read => "Read".into(),
                ToolKind::Search => "Searched".into(),
                ToolKind::Fetch => "Fetched".into(),
                ToolKind::Other => "Called".into(),
            };
            let arg = detail.as_deref().map(one_line).unwrap_or_default();
            // A command reads as the command itself, a file step as its
            // path; other tools by name.
            let plain = !is_mcp && kind != ToolKind::Other;
            let shown = if plain && !arg.is_empty() {
                arg.clone()
            } else if arg.is_empty() {
                name
            } else {
                format!("{name} · {arg}")
            };
            Leaf {
                icon: if is_mcp {
                    &icons::NETWORK
                } else {
                    kind_icon(kind)
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

/// Whether a finished group of `rows` folds on its own glide. A group long
/// enough to nest its tree as a virtual list (`timeline::NEST_AFTER`) opens
/// at once: gliding it open leaves the nested list committing on every
/// scroll through it afterwards, for a fold that shows only its first
/// screen anyway.
pub(crate) fn folds_on_glide(rows: usize) -> bool {
    rows <= timeline::NEST_AFTER
}

/// A settled turn's fold, t3code's: "Worked for 31m 12s" and a chevron over
/// a hairline. Opening it puts the turn's words and work back above the
/// answer; the line holds still under the pointer meanwhile.
pub fn render_turn_fold(s: &State, thread: &str, key: &str, label: &str, open: bool) -> El {
    let p = &s.palette;
    let font = type_at(s, LABEL_SIZE, LABEL_LINE, 400.);
    let head_key = format!("{key}-head");
    let (thread_id, toggle_key, keep) = (thread.to_string(), key.to_string(), head_key.clone());
    col()
        .key(format!("{key}-fold"))
        .w_full()
        .transition(rise(4.))
        .child(
            row()
                .key(head_key)
                .role("button")
                .label(label.to_string())
                .h(24.)
                .gap(4.)
                .px(2.)
                .center_y()
                .radius(design_radius())
                .cursor(rune::Cursor::Pointer)
                .child(text(label.to_string()).font(font).color(p.text_muted))
                .child(crate::design::kit::chevron(
                    "chevron",
                    open,
                    crate::design::kit::Turn::Disclose,
                    12.,
                    p.text_muted,
                ))
                .on_press_tx(move |s: &mut State, tx: &mut rune::Tx| {
                    let Some(c) = super::chat_for(s, &thread_id) else {
                        return;
                    };
                    let now = is_open(&toggle_key, &c.toggles);
                    c.toggles.insert(toggle_key.clone(), !now);
                    // The fold decides which rows exist: fold the list again.
                    c.folded = None;
                    super::fold_now(c, tx);
                    tx.keep_on_screen(keep.clone());
                    super::sync_items(s, &thread_id, tx);
                }),
        )
        .child(el().w_full().h(1.).mt(4.).fill(p.border))
}

/// A step's verb while it runs: "Ran" → "Running".
fn present_tense(verb: &str) -> String {
    match verb {
        "Ran" => "Running",
        "Edited" => "Editing",
        "Read" => "Reading",
        "Searched" => "Searching",
        "Fetched" => "Fetching",
        "Called" => "Calling",
        other => other,
    }
    .to_string()
}

/// An activity group, t3code's work row: a settled single step is its own
/// line; otherwise one line (the newest step while live, the summary after)
/// that opens to every step.
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
    let parts: Vec<(&Row, Leaf)> = rows
        .iter()
        .filter_map(|r| leaf(r).map(|l| (r, l)))
        .collect();
    let count = parts.len();
    let mut lines = Vec::with_capacity(count);
    for (ix, (row, mut leaf)) in parts.into_iter().enumerate() {
        let row_key = row.key();
        let open_key = format!("leaf:{row_key}");
        // The thought being written reads "Thinking"; its words show once
        // it ends, under "Thought".
        if live && ix + 1 == count && matches!(row, Row::Thinking { .. }) {
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
        lines.push(timeline::Line {
            key: row_key.to_string(),
            leaf,
            expanded,
            open_key,
        });
    }
    let group = col().key(format!("{key}-group")).w_full();
    // One settled step: the step is the line (t3code's single tool row).
    if !live
        && lines.len() == 1
        && let Some(line) = lines.pop()
    {
        return group.children(tree_row(
            s,
            thread,
            &line.key,
            line.leaf,
            line.expanded,
            line.open_key,
        ));
    }
    let newest = lines.last().map(|l| &l.leaf);
    let (icon, label, busy) = match newest {
        Some(leaf) if live => {
            let verb = if leaf.running {
                present_tense(&leaf.label)
            } else {
                leaf.label.clone()
            };
            let line = if leaf.detail.is_empty() {
                verb
            } else {
                format!("{verb} {}", leaf.detail)
            };
            (leaf.icon, line, true)
        }
        _ => (group_icon(rows), summary(rows), false),
    };
    // A failed step is red on its own line; the group's line stays quiet.
    let tint = p.text_muted;
    let font = type_at(s, LABEL_SIZE, LABEL_LINE, 400.);
    let title = if busy {
        // The live line breathes while the run works (t3code's shimmer).
        text(label.clone()).font(font).color(tint).truncate().cycle(
            crate::design::kit::loops::breathe(1.6, 0., &crate::design::kit::loops::BREATH_55),
        )
    } else {
        RollingText::new(format!("{key}-title"), label.clone())
            .font(font)
            .color(tint)
            .build(k)
    };
    let toggle_key = key.to_string();
    let head_key = format!("{key}-head");
    let thread_id = thread.to_string();
    let header = row()
        .key(head_key.clone())
        .role("button")
        .label(label)
        .w_full()
        .h(TREE_ROW_HEIGHT)
        .gap(TEXT_GAP)
        .center_y()
        .radius(design_radius())
        .cursor(rune::Cursor::Pointer)
        .transition(rise(4.))
        .child(el().w(ICON_COLUMN).h(TREE_ROW_HEIGHT).center().child(
            crate::design::kit::busy_glyph("head-glyph", icon, busy, ICON_SIZE, tint),
        ))
        .child(el().min_w(0.).shrink().child(title))
        // The header stays under the pointer while its rows open or shut.
        .on_press_tx(move |s: &mut State, tx: &mut rune::Tx| {
            let Some(c) = super::chat_for(s, &thread_id) else {
                return;
            };
            let now = c.activity_open(&toggle_key);
            c.toggles.insert(toggle_key.clone(), !now);
            super::fold_now(c, tx);
            tx.keep_on_screen(head_key.clone());
        });
    let group = group.child(header);
    if !open {
        return group;
    }
    let tree = col()
        .key(format!("{key}-tree"))
        .w_full()
        .pl(ICON_COLUMN / 2.)
        .transition(rise(4.));
    group.child(timeline::tree(s, thread, tree, lines))
}

/// One step: its icon, verb and subject on one line, then its opened text
/// while open. The caller's `{key}-step` column holds both, so opening a
/// step never moves `{key}-leaf` under a new parent (which cut its motion
/// short).
fn tree_row(
    s: &State,
    thread: &str,
    key: &str,
    leaf: Leaf,
    expanded: bool,
    open_key: String,
) -> Vec<El> {
    let p = &s.palette;
    let tint = if leaf.failed { p.danger } else { p.text_muted };
    let expandable = leaf.body.is_some() || leaf.output.is_some();
    let font = type_at(s, LABEL_SIZE, LABEL_LINE, 400.);
    let mut words = row()
        .min_w(0.)
        .grow(1.)
        .gap(5.)
        .center_y()
        .child(text(leaf.label.clone()).font(font).color(tint));
    if !leaf.detail.is_empty() {
        words = words.child(
            text(leaf.detail.clone())
                .font(font)
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
        .gap(TEXT_GAP)
        .center_y()
        .transition(rise(4.))
        .child(
            el().w(ICON_COLUMN)
                .h(TREE_ROW_HEIGHT)
                .center()
                // A running step's glyph breathes on the loaders' 15 fps
                // clock; its finish lands with the stream.
                .child(crate::design::kit::busy_glyph(
                    "leaf-glyph",
                    leaf.icon,
                    leaf.running,
                    ICON_SIZE,
                    tint,
                )),
        )
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
                super::fold_now(c, tx);
                tx.keep_on_screen(leaf_key.clone());
            });
    }
    if !expanded || !expandable {
        return vec![line];
    }
    // An opened step's text hangs under its words.
    let small = type_at(s, 12., 18., 400.);
    let mut body_col = col()
        .grow(1.)
        .min_w(0.)
        .pl(ICON_COLUMN + TEXT_GAP)
        .pb(8.)
        .gap(4.);
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
    vec![
        line,
        row()
            .key(format!("{key}-detail"))
            .w_full()
            .transition(rise(4.))
            .child(body_col),
    ]
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
    // The composer's own surface and rim, so a sent message reads as the
    // card it was typed in: opaque, one step off the canvas. A light theme
    // whose card is the canvas's own white takes a faint ink wash instead,
    // so the bubble still reads.
    let (card, rim) = super::composer::composer_surface(p, false);
    let wash = if p.light && card.to_oklab()[0] >= p.bg.to_oklab()[0] - 0.005 {
        p.bg.mix(p.text, 0.05)
    } else {
        card
    };
    row().w_full().justify_end().child(
        // `shrink` on the pill and its body: without it the row lays the
        // body out at its unwrapped width and long lines run past the pill.
        el().key(format!("{key}-pill"))
            .shrink()
            .max_w(column * 0.8)
            .fill(wash)
            .border(1., rim)
            .radius(crate::design::COMPOSER_RADIUS)
            .px(16.)
            .py(10.)
            .opacity(if pending { 0.65 } else { 1. })
            .transition(rise(6.))
            .child(body.shrink()),
    )
}
