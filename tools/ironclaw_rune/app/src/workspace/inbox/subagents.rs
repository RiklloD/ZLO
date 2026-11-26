//! Subagent threads fold under the thread that delegated them (on the same
//! computer). A parent card shows one quiet line ("3 subagents · 1 working")
//! and opens, on a click, into a nested list of its subagents to look in on or
//! steer; the list is closed by default. A subagent whose parent is not on the
//! list (filtered out, or past the first page) stays a row of its own.
//!
//! Settling a parent settles its subagents with it (the backend carries the
//! delegation tree); each subagent row settles or comes back on its own. A
//! subagent that finished its task settles by itself.

use std::collections::{HashMap, HashSet};

use rune::{Color, El, col, el, icon, row, text};
use rune_kit::Kit;
use zlo_client::inbox::{InboxAction, InboxRow, InboxState, InboxStatus};

use super::merged::{self, Entry, RowId};
use super::{mark_row, open_row, settle_blocker, settle_row, title_of, view};
use crate::design;
use crate::icons;
use crate::state::State;

/// Depth past which nested rows stop indenting further.
const MAX_INDENT: usize = 3;
const INDENT: f32 = 12.;

/// One subagent row stored in [`Tree`].
#[derive(Clone)]
pub(in crate::workspace) struct Kid {
    pub id: RowId,
    pub focused: bool,
    pub row: InboxRow,
}

/// The visible rows' delegation tree, keyed by `(computer, thread_id)` so
/// subagents only fold under parents on the same computer.
#[derive(Default)]
pub(in crate::workspace) struct Tree {
    /// Parent id → its subagents on the list, the ones needing you first.
    children: HashMap<RowId, Vec<Kid>>,
    /// Rows drawn under their parent instead of on their own.
    nested: HashSet<RowId>,
}

/// What a parent's folded line says about its subagents.
#[derive(Debug, Default, PartialEq, Eq)]
pub(in crate::workspace) struct Summary {
    pub total: usize,
    pub needs_you: usize,
    pub working: usize,
    pub failed: usize,
}

fn live(row: &InboxRow) -> bool {
    matches!(
        row.status,
        Some(InboxStatus::Working | InboxStatus::Background)
    )
}

fn failed(row: &InboxRow) -> bool {
    matches!(
        row.status,
        Some(InboxStatus::Failed | InboxStatus::Interrupted)
    )
}

/// Settled or snoozed: the row sits on a shelf that is closed by default.
fn parked(row: &InboxRow) -> bool {
    matches!(row.state, InboxState::Settled | InboxState::Snoozed)
}

/// Needs you, then working, then the rest; latest activity first in each.
fn rank(kid: &Kid) -> (u8, i64) {
    let row = &kid.row;
    let group = match row.status {
        Some(InboxStatus::NeedsInput) => 0,
        _ if live(row) => 1,
        _ if row.state == InboxState::Settled => 3,
        _ => 2,
    };
    (group, -row.last_activity_at)
}

impl Tree {
    pub(in crate::workspace) fn new<'r>(
        rows: impl IntoIterator<Item = (&'r RowId, bool, &'r InboxRow)>,
    ) -> Self {
        let rows: Vec<(&RowId, bool, &InboxRow)> = rows.into_iter().collect();
        let by_id: HashMap<&RowId, &InboxRow> = rows.iter().map(|&(id, _, r)| (id, r)).collect();
        let mut tree = Self::default();
        for &(id, focused, row) in &rows {
            let Some(parent_tid) = row.parent_thread_id.as_deref() else {
                continue;
            };
            if parent_tid == row.thread_id {
                continue;
            }
            let parent_id = RowId::new(&id.computer, parent_tid);
            let Some(parent_row) = by_id.get(&parent_id) else {
                continue;
            };
            // A subagent still open (un-settled by hand, working, waiting on
            // you) never hides under a parked parent's closed shelf.
            if parked(parent_row) && !parked(row) {
                continue;
            }
            tree.nested.insert(id.clone());
            tree.children.entry(parent_id).or_default().push(Kid {
                id: id.clone(),
                focused,
                row: row.clone(),
            });
        }
        for kids in tree.children.values_mut() {
            kids.sort_by_key(rank);
        }
        // A delegation cycle would leave every member nested with no top
        // row to hang from; such rows stand on their own instead.
        let orphaned: Vec<RowId> = tree
            .nested
            .iter()
            .filter(|id| tree.root_of(id).is_none())
            .cloned()
            .collect();
        for id in orphaned {
            tree.nested.remove(&id);
        }
        tree
    }

    /// Convenience builder for one computer's rows (used by `reveal_active_subagent`).
    pub(in crate::workspace) fn for_computer(computer: &str, rows: &[InboxRow]) -> Self {
        let owned: Vec<(RowId, &InboxRow)> = rows
            .iter()
            .map(|r| (RowId::new(computer, &r.thread_id), r))
            .collect();
        Self::new(owned.iter().map(|(id, r)| (id, true, *r)))
    }

    /// The top row a nested row hangs from, `None` inside a cycle.
    fn root_of(&self, id: &RowId) -> Option<RowId> {
        let parent_of: HashMap<&RowId, &RowId> = self
            .children
            .iter()
            .flat_map(|(p, kids)| kids.iter().map(move |k| (&k.id, p)))
            .collect();
        let mut at = id;
        let mut seen = HashSet::new();
        while let Some(parent) = parent_of.get(at) {
            if !seen.insert(*parent) {
                return None;
            }
            at = parent;
        }
        (at != id).then(|| at.clone())
    }

    pub(in crate::workspace) fn is_nested(&self, id: &RowId) -> bool {
        self.nested.contains(id)
    }

    /// Every subagent under `root`, depth first, with its depth (1 = direct).
    pub(in crate::workspace) fn descendants(&self, root: &RowId) -> Vec<(usize, &Kid)> {
        let mut out = Vec::new();
        let mut seen = HashSet::from([root.clone()]);
        let mut stack: Vec<(usize, &Kid)> = self
            .children
            .get(root)
            .into_iter()
            .flatten()
            .rev()
            .map(|k| (1, k))
            .collect();
        while let Some((depth, kid)) = stack.pop() {
            if !seen.insert(kid.id.clone()) {
                continue;
            }
            out.push((depth, kid));
            if let Some(kids) = self.children.get(&kid.id) {
                stack.extend(kids.iter().rev().map(|k| (depth + 1, k)));
            }
        }
        out
    }

    pub(in crate::workspace) fn summary(&self, root: &RowId) -> Summary {
        self.descendants(root)
            .into_iter()
            .fold(Summary::default(), |mut s, (_, kid)| {
                let row = &kid.row;
                s.total += 1;
                if row.status == Some(InboxStatus::NeedsInput) {
                    s.needs_you += 1;
                } else if live(row) {
                    s.working += 1;
                } else if failed(row) {
                    s.failed += 1;
                }
                s
            })
    }

    /// The row ids above `id`, nearest first.
    pub(in crate::workspace) fn ancestors(&self, id: &RowId) -> Vec<RowId> {
        let parent_of: HashMap<&RowId, &RowId> = self
            .children
            .iter()
            .flat_map(|(p, kids)| kids.iter().map(move |k| (&k.id, p)))
            .collect();
        let mut out = Vec::new();
        let mut at = id;
        while let Some(parent) = parent_of.get(at) {
            if out.iter().any(|o: &RowId| o == *parent) {
                break;
            }
            out.push((*parent).clone());
            at = parent;
        }
        out
    }
}

/// The folded line under a parent's title: count, then what needs a look.
/// `None` for a thread that delegated nothing.
pub(super) fn line(s: &State, tree: &Tree, entry: &Entry<'_>, compact: bool) -> Option<El> {
    let sum = tree.summary(&entry.id);
    if sum.total == 0 {
        return None;
    }
    let p = &s.palette;
    let open = open(s, &entry.id);
    let root = entry.id.clone();
    let root_key = root.key();
    let noun = if sum.total == 1 {
        "subagent"
    } else {
        "subagents"
    };
    let mut parts: Vec<(String, Color)> = Vec::new();
    if sum.needs_you > 0 {
        parts.push((format!("{} needs you", sum.needs_you), p.warning));
    }
    if sum.working > 0 {
        parts.push((format!("{} working", sum.working), p.accent));
    }
    if sum.failed > 0 {
        parts.push((format!("{} failed", sum.failed), p.danger));
    }
    if parts.is_empty() && !compact {
        parts.push(("all done".into(), p.text_faint));
    }
    let label = format!("{} {noun}", sum.total);
    let size = design::SIDEBAR_META_SIZE - 1.;
    let mut line = row()
        .key(format!("subagents-{root_key}"))
        .role("button")
        .label(if open {
            format!("Hide {label}")
        } else {
            format!("Show {label}")
        })
        .gap(5.)
        .center_y()
        .min_w(0.0)
        .flex_wrap()
        .cursor(rune::Cursor::Pointer)
        .hover_group()
        .child(design::kit::chevron(
            &format!("subagents-{root_key}-chevron"),
            open,
            design::kit::Turn::Disclose,
            12.,
            p.text_faint,
        ))
        .child(icon(&icons::BOT, 12.).color(p.text_muted))
        .child(
            text(if compact {
                sum.total.to_string()
            } else {
                label
            })
            .font(crate::workspace::ui::sidebar_style(size, 400.))
            .color(p.text_muted)
            .group_color(p.text),
        );
    for (part, color) in parts {
        line = line
            .child(
                text("·")
                    .font(crate::workspace::ui::sidebar_style(size, 400.))
                    .color(p.text_faint),
            )
            .child(
                text(part)
                    .font(crate::workspace::ui::sidebar_style(size, 400.))
                    .color(color),
            );
    }
    // The line sits inside the card: opening the list must not also open
    // the parent thread (the innermost press wins).
    Some(line.on_press(move |s: &mut State| super::toggle_subagents(s, &root)))
}

fn open(s: &State, root: &RowId) -> bool {
    // A search is on: every subagent list is unfolded.
    !s.workspace.inbox.query().is_empty() || s.workspace.inbox.expanded.contains(root)
}

/// The unfolded list under a parent: one slim row per subagent, indented by
/// depth, each with its own settle.
pub(super) fn list(s: &State, tree: &Tree, entry: &Entry<'_>, now: i64, k: &Kit) -> Option<El> {
    if !open(s, &entry.id) {
        return None;
    }
    let rows = tree.descendants(&entry.id);
    if rows.is_empty() {
        return None;
    }
    let p = &s.palette;
    let active = merged::active_row(s);
    let items: Vec<El> = rows
        .iter()
        .map(|(depth, kid)| {
            let is_active = active.as_ref() == Some(&kid.id);
            subagent_row(s, kid, *depth, is_active, now, k)
        })
        .collect();
    Some(
        // Unfolds from the parent's edge and folds back into it.
        row()
            .key(format!("subagent-list-{}", entry.id.key()))
            .transition(rune::Transition::reveal(rune::Edge::Top))
            .ml(18.)
            .mr(4.)
            .mb(4.)
            .child(el().w(1.).fill(p.border).self_stretch())
            .child(col().grow(1.0).min_w(0.0).pl(6.).gap(1.).children(items)),
    )
}

fn subagent_row(s: &State, kid: &Kid, depth: usize, active: bool, now: i64, k: &Kit) -> El {
    let p = &s.palette;
    let row_data = &kid.row;
    let id = kid.id.clone();
    let row_key = id.key();
    let settled = row_data.state == InboxState::Settled;
    let blocker = settle_blocker(row_data);
    let dot = match row_data.status {
        Some(InboxStatus::NeedsInput) => p.warning,
        _ if live(row_data) => p.accent,
        _ if failed(row_data) => p.danger,
        _ if settled => p.text_faint.alpha(0.5),
        _ => p.success,
    };
    let (open_id, mark_id, menu_id) = (id.clone(), id.clone(), id.clone());
    let warm_id = row_data.thread_id.clone();
    let focused = kid.focused;
    let action = if settled {
        crate::workspace::ui::ghost_icon(
            k,
            &icons::UNDO_2,
            "Un-settle subagent",
            rune_kit::ControlSize::Mini,
            move |s| mark_row(s, mark_id.clone(), InboxAction::Unsettle, None),
        )
    } else {
        rune_kit::IconButton::new(&icons::CHECK)
            .variant(rune_kit::IconButtonVariant::Ghost)
            .control_size(rune_kit::ControlSize::Mini)
            .disabled(blocker.is_some())
            .label("Settle subagent")
            .help(match blocker {
                Some(why) => format!("Can't settle: it {why}"),
                None => "Settle subagent".into(),
            })
            .on_press(move |s: &mut State| settle_row(s, mark_id.clone()))
            .build(k)
    };
    let end = if settled {
        row()
            .gap(4.)
            .center_y()
            .child(icon(&icons::CHECK, 12.).color(p.text_faint))
            .child(
                text("Settled")
                    .font(rune::TextStyle::new(crate::design::styles::rem(12.), 400.))
                    .color(p.text_faint),
            )
    } else {
        view::status_label(s, &id, focused, row_data, active, now)
    };
    row()
        .key(format!("subagent-row-{row_key}"))
        // A subagent that moves in its parent's list glides there, as the
        // inbox's rows do.
        .motion(crate::design::motion::ROW_RESORT)
        .role("button")
        .label(format!("Subagent: {}", title_of(row_data)))
        .w_full()
        .min_h(30.)
        .pl(6. + INDENT * (depth.min(MAX_INDENT) - 1) as f32)
        .pr(8.)
        .gap(8.)
        .center_y()
        .radius(design::ROW_RADIUS)
        .cursor(rune::Cursor::Pointer)
        .fill(if active {
            crate::workspace::ui::selected_wash(p)
        } else {
            Color::TRANSPARENT
        })
        .hover(crate::workspace::ui::hover_wash(p))
        .hover_group()
        .hover_motion(crate::design::motion::HOVER, crate::design::motion::HOVER)
        .press_scale(crate::design::motion::PRESS_CARD)
        .on_press(move |s: &mut State| open_row(s, &open_id))
        .on_secondary_press(move |s: &mut State, at, tx| super::open_row_menu(s, &menu_id, at, tx))
        // Resting on it warms the thread for the click (`chat::prewarm`, focused only).
        .on_hover_change(move |s: &mut State, inside, tx| {
            if focused {
                crate::chat::prewarm::hover(s, &warm_id, inside, tx);
            }
        })
        .child(el().size(6., 6.).radius(3.).fill(dot))
        .child(
            el().grow(1.0).min_w(0.0).child(
                text(title_of(row_data).to_string())
                    .font(crate::workspace::ui::sidebar_style(
                        design::SIDEBAR_META_SIZE,
                        400.,
                    ))
                    .color(if settled { p.text_faint } else { p.text_muted })
                    .truncate(),
            ),
        )
        .child(
            el().child(row().center_y().group_opacity(1., 0.).child(end))
                .child(
                    row()
                        .pin(None, Some(0.), Some(0.), Some(0.))
                        .center_y()
                        .group_opacity(0., 1.)
                        .child(action),
                ),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rid(id: &str) -> RowId {
        RowId::new("this-mac", id)
    }

    fn row(id: &str, parent: Option<&str>, status: Option<InboxStatus>) -> InboxRow {
        InboxRow {
            thread_id: id.into(),
            title: None,
            profile_id: "default".into(),
            state: InboxState::Inbox,
            status,
            engines: vec![],
            last_activity_at: 0,
            snooze_until: None,
            settled_at: None,
            limit_reset_at: None,
            working_since: None,
            parent_thread_id: parent.map(str::to_string),
            automation: false,
            worktree_branch: None,
            title_pending: false,
            imported: None,
        }
    }

    #[test]
    fn subagents_fold_under_the_parent_on_the_list_only() {
        let rows = vec![
            row("root", None, Some(InboxStatus::Background)),
            row("a", Some("root"), Some(InboxStatus::Working)),
            row("b", Some("root"), Some(InboxStatus::NeedsInput)),
            row("a1", Some("a"), Some(InboxStatus::Failed)),
            row("stray", Some("gone"), None),
        ];
        let tree = Tree::for_computer("this-mac", &rows);
        assert!(
            tree.is_nested(&rid("a")) && tree.is_nested(&rid("b")) && tree.is_nested(&rid("a1"))
        );
        assert!(!tree.is_nested(&rid("root")));
        assert!(
            !tree.is_nested(&rid("stray")),
            "a subagent whose parent is not listed stands alone"
        );
        let order: Vec<(usize, &str)> = tree
            .descendants(&rid("root"))
            .into_iter()
            .map(|(d, k)| (d, k.row.thread_id.as_str()))
            .collect();
        assert_eq!(
            order,
            [(1, "b"), (1, "a"), (2, "a1")],
            "needs-you first, nested depth first"
        );
        assert_eq!(
            tree.summary(&rid("root")),
            Summary {
                total: 3,
                needs_you: 1,
                working: 1,
                failed: 1
            }
        );
        assert_eq!(tree.ancestors(&rid("a1")), [rid("a"), rid("root")]);
        assert_eq!(tree.summary(&rid("stray")).total, 0);
    }

    #[test]
    fn an_open_subagent_never_hides_under_a_parked_parent() {
        let parked_as = |id: &str, parent: Option<&str>, state: InboxState| InboxRow {
            state,
            ..row(id, parent, None)
        };
        let rows = vec![
            parked_as("root", None, InboxState::Settled),
            // Settled with the parent, then one brought back by hand.
            parked_as("done", Some("root"), InboxState::Settled),
            parked_as("back", Some("root"), InboxState::Inbox),
            parked_as("nap", None, InboxState::Snoozed),
            InboxRow {
                state: InboxState::Working,
                ..row("busy", Some("nap"), Some(InboxStatus::Working))
            },
            // An open parent keeps its settled subagents, dimmed, folded.
            parked_as("open", None, InboxState::Inbox),
            parked_as("finished", Some("open"), InboxState::Settled),
        ];
        let tree = Tree::for_computer("this-mac", &rows);
        assert!(
            tree.is_nested(&rid("done")),
            "settled stays with its settled parent"
        );
        assert!(
            !tree.is_nested(&rid("back")),
            "un-settled by hand: a row of its own"
        );
        assert!(
            !tree.is_nested(&rid("busy")),
            "working under a snoozed parent stays visible"
        );
        assert!(tree.is_nested(&rid("finished")));
        assert_eq!(tree.summary(&rid("root")).total, 1);
    }

    #[test]
    fn a_delegation_cycle_never_hides_its_rows() {
        let rows = vec![row("x", Some("y"), None), row("y", Some("x"), None)];
        let tree = Tree::for_computer("this-mac", &rows);
        assert!(!tree.is_nested(&rid("x")) && !tree.is_nested(&rid("y")));
        assert_eq!(
            tree.descendants(&rid("x")).len(),
            1,
            "the walk ends at the cycle"
        );
    }
}
