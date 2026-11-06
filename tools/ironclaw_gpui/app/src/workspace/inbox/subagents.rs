//! Subagent threads fold under the thread that delegated them. A parent
//! card shows one quiet line ("3 subagents · 1 working") and opens, on a
//! click, into a nested list of its subagents to look in on or steer; the
//! list is closed by default. A subagent whose parent is not on the list
//! (filtered out, or past the first page) stays a row of its own.
//!
//! Settling a parent settles its subagents with it (the backend carries the
//! delegation tree); each subagent row settles or comes back on its own. A
//! subagent that finished its task settles by itself.

use std::collections::{HashMap, HashSet};

use ironclaw_gpui_client::inbox::{InboxAction, InboxRow, InboxState, InboxStatus};

use super::{Workspace, settle_blocker, sidebar_row, status_label, title_of};
use crate::design::{self, Palette};
use crate::ui::*;

/// Depth past which nested rows stop indenting further.
const MAX_INDENT: usize = 3;
const INDENT: f32 = 12.;

/// The visible rows' delegation tree, rebuilt every render.
#[derive(Default)]
pub(in crate::workspace) struct Tree {
    /// Parent id → its subagents on the list, the ones needing you first.
    children: HashMap<String, Vec<InboxRow>>,
    /// Rows drawn under their parent instead of on their own.
    nested: HashSet<String>,
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
fn rank(row: &InboxRow) -> (u8, i64) {
    let group = match row.status {
        Some(InboxStatus::NeedsInput) => 0,
        _ if live(row) => 1,
        _ if row.state == InboxState::Settled => 3,
        _ => 2,
    };
    (group, -row.last_activity_at)
}

impl Tree {
    pub(in crate::workspace) fn new(rows: &[InboxRow]) -> Self {
        let by_id: HashMap<&str, &InboxRow> =
            rows.iter().map(|r| (r.thread_id.as_str(), r)).collect();
        let mut tree = Self::default();
        for row in rows {
            let Some(parent) = row.parent_thread_id.as_deref() else {
                continue;
            };
            let Some(parent_row) = by_id.get(parent).filter(|_| parent != row.thread_id) else {
                continue;
            };
            // A subagent still open (un-settled by hand, working, waiting on
            // you) never hides under a parked parent's closed shelf.
            if parked(parent_row) && !parked(row) {
                continue;
            }
            tree.nested.insert(row.thread_id.clone());
            tree.children
                .entry(parent.to_string())
                .or_default()
                .push(row.clone());
        }
        for kids in tree.children.values_mut() {
            kids.sort_by_key(rank);
        }
        // A delegation cycle would leave every member nested with no top
        // row to hang from; such rows stand on their own instead.
        let orphaned: Vec<String> = tree
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

    /// The top row a nested row hangs from, `None` inside a cycle.
    fn root_of(&self, id: &str) -> Option<String> {
        let parent_of: HashMap<&str, &str> = self
            .children
            .iter()
            .flat_map(|(p, kids)| kids.iter().map(move |k| (k.thread_id.as_str(), p.as_str())))
            .collect();
        let mut at = id;
        let mut seen = HashSet::new();
        while let Some(parent) = parent_of.get(at) {
            if !seen.insert(*parent) {
                return None;
            }
            at = parent;
        }
        (at != id).then(|| at.to_string())
    }

    pub(in crate::workspace) fn is_nested(&self, id: &str) -> bool {
        self.nested.contains(id)
    }

    /// Every subagent under `root`, depth first, with its depth (1 = direct).
    pub(in crate::workspace) fn descendants(&self, root: &str) -> Vec<(usize, &InboxRow)> {
        let mut out = Vec::new();
        let mut seen = HashSet::from([root.to_string()]);
        let mut stack: Vec<(usize, &InboxRow)> = self
            .children
            .get(root)
            .into_iter()
            .flatten()
            .rev()
            .map(|r| (1, r))
            .collect();
        while let Some((depth, row)) = stack.pop() {
            if !seen.insert(row.thread_id.clone()) {
                continue;
            }
            out.push((depth, row));
            if let Some(kids) = self.children.get(&row.thread_id) {
                stack.extend(kids.iter().rev().map(|k| (depth + 1, k)));
            }
        }
        out
    }

    pub(in crate::workspace) fn summary(&self, root: &str) -> Summary {
        self.descendants(root)
            .into_iter()
            .fold(Summary::default(), |mut s, (_, row)| {
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

    /// The thread ids above `id`, nearest first.
    pub(in crate::workspace) fn ancestors(&self, id: &str) -> Vec<String> {
        let parent_of: HashMap<&str, &str> = self
            .children
            .iter()
            .flat_map(|(p, kids)| kids.iter().map(move |k| (k.thread_id.as_str(), p.as_str())))
            .collect();
        let mut out = Vec::new();
        let mut at = id;
        while let Some(parent) = parent_of.get(at) {
            if out.iter().any(|o: &String| o == parent) {
                break;
            }
            out.push(parent.to_string());
            at = parent;
        }
        out
    }
}

impl Workspace {
    /// Open the subagent lists above the thread on screen, once per
    /// thread opened, so the selection is visible and can still be folded.
    pub(super) fn reveal_active_subagent(&mut self, active: Option<&str>) {
        if self.inbox.revealed.as_deref() == active {
            return;
        }
        self.inbox.revealed = active.map(str::to_string);
        if let Some(id) = active {
            let above = self.inbox.tree.ancestors(id);
            self.inbox.expanded.extend(above);
        }
    }

    fn subagents_open(&self, root: &str) -> bool {
        self.inbox.unfold_subagents || self.inbox.expanded.contains(root)
    }

    fn toggle_subagents(&mut self, root: &str, cx: &mut Context<Self>) {
        if !self.inbox.expanded.remove(root) {
            self.inbox.expanded.insert(root.to_string());
        }
        cx.notify();
    }

    /// The folded line under a parent's title: count, then what needs a
    /// look. `None` for a thread that delegated nothing.
    pub(super) fn render_subagent_line(
        &self,
        row: &InboxRow,
        compact: bool,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let s = self.inbox.tree.summary(&row.thread_id);
        if s.total == 0 {
            return None;
        }
        let p = *Palette::of(cx);
        let open = self.subagents_open(&row.thread_id);
        let root = row.thread_id.clone();
        let noun = if s.total == 1 {
            "subagent"
        } else {
            "subagents"
        };
        let mut parts: Vec<(String, gpui_kit::Hsla)> = Vec::new();
        if s.needs_you > 0 {
            parts.push((format!("{} needs you", s.needs_you), p.warning));
        }
        if s.working > 0 {
            parts.push((format!("{} working", s.working), p.accent));
        }
        if s.failed > 0 {
            parts.push((format!("{} failed", s.failed), p.danger));
        }
        if parts.is_empty() && !compact {
            parts.push(("all done".into(), p.text_faint));
        }
        let label = format!("{} {noun}", s.total);
        Some(
            h_flex()
                .id(SharedString::from(format!("subagents-{root}")))
                .role(Role::Button)
                .aria_label(if open {
                    format!("Hide {label}")
                } else {
                    format!("Show {label}")
                })
                .flex_none()
                .gap(px(5.))
                .items_center()
                .min_w_0()
                .text_size(px(design::SIDEBAR_META_SIZE - 1.))
                .text_color(p.text_muted)
                .cursor_pointer()
                .hover(|d| d.text_color(p.text))
                .child(
                    Icon::new(if open {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .xsmall()
                    .text_color(p.text_faint),
                )
                .child(Icon::new(IconName::Bot).xsmall())
                .child(
                    div()
                        .flex_none()
                        .child(if compact { s.total.to_string() } else { label }),
                )
                .children(parts.into_iter().map(|(text, color)| {
                    h_flex()
                        .flex_none()
                        .gap(px(5.))
                        .child(div().text_color(p.text_faint).child("·"))
                        .child(div().text_color(color).child(text))
                }))
                .on_click(cx.listener(move |this, _, _, cx| {
                    // The line sits inside the card: opening the list must
                    // not also open the parent thread.
                    cx.stop_propagation();
                    this.toggle_subagents(&root, cx);
                }))
                .into_any_element(),
        )
    }

    /// The unfolded list under a parent: one slim row per subagent,
    /// indented by depth, each with its own settle.
    pub(super) fn render_subagent_list(
        &self,
        row: &InboxRow,
        active: Option<&str>,
        now: i64,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.subagents_open(&row.thread_id) {
            return None;
        }
        let rows: Vec<(usize, InboxRow)> = self
            .inbox
            .tree
            .descendants(&row.thread_id)
            .into_iter()
            .map(|(depth, r)| (depth, r.clone()))
            .collect();
        if rows.is_empty() {
            return None;
        }
        let p = *Palette::of(cx);
        let items: Vec<AnyElement> = rows
            .iter()
            .map(|(depth, kid)| {
                let is_active = active == Some(kid.thread_id.as_str());
                self.render_subagent_row(kid, *depth, is_active, now, cx)
            })
            .collect();
        Some(
            v_flex()
                .id(SharedString::from(format!(
                    "subagent-list-{}",
                    row.thread_id
                )))
                .ml(px(18.))
                .mr(px(4.))
                .mb(px(4.))
                .pl(px(6.))
                .gap_px()
                .border_l_1()
                .border_color(p.border)
                .children(items)
                .into_any_element(),
        )
    }

    fn render_subagent_row(
        &self,
        row: &InboxRow,
        depth: usize,
        active: bool,
        now: i64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let p = *Palette::of(cx);
        let id = row.thread_id.clone();
        let group: SharedString = format!("subagent-{id}").into();
        let settled = row.state == InboxState::Settled;
        let blocker = settle_blocker(row);
        let dot = match row.status {
            Some(InboxStatus::NeedsInput) => p.warning,
            _ if live(row) => p.accent,
            _ if failed(row) => p.danger,
            _ if settled => p.text_faint.opacity(0.5),
            _ => p.success,
        };
        let (open_id, mark_id) = (id.clone(), id.clone());
        let action = if settled {
            Button::new(SharedString::from(format!("subagent-unsettle-{id}")))
                .icon(IconName::Undo2)
                .ghost()
                .xsmall()
                .tooltip("Un-settle subagent")
                .accessibility_label("Un-settle subagent")
                .on_click(cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.inbox_mark(mark_id.clone(), InboxAction::Unsettle, None, cx)
                }))
        } else {
            Button::new(SharedString::from(format!("subagent-settle-{id}")))
                .icon(IconName::Check)
                .ghost()
                .xsmall()
                .disabled(blocker.is_some())
                .tooltip(match blocker {
                    Some(why) => format!("Can't settle: it {why}"),
                    None => "Settle subagent".into(),
                })
                .accessibility_label("Settle subagent")
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.settle(mark_id.clone(), window, cx)
                }))
        };
        let end: AnyElement = if settled {
            h_flex()
                .flex_none()
                .gap_1()
                .items_center()
                .text_xs()
                .text_color(p.text_faint)
                .child(Icon::new(IconName::Check).xsmall())
                .child("Settled")
                .into_any_element()
        } else {
            status_label(row, active, now, cx)
        };
        sidebar_row(SharedString::from(format!("subagent-row-{id}")), active, cx)
            .min_h(px(30.))
            .pl(px(6. + INDENT * (depth.min(MAX_INDENT) - 1) as f32))
            .text_size(px(design::SIDEBAR_META_SIZE))
            .group(group.clone())
            .aria_label(format!("Subagent: {}", title_of(row)))
            .on_click(
                cx.listener(move |this, _, window, cx| {
                    this.open_thread(open_id.clone(), window, cx)
                }),
            )
            .child(div().flex_none().size(px(6.)).rounded_full().bg(dot))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(if settled { p.text_faint } else { p.text_muted })
                    .child(title_of(row).to_string()),
            )
            .child(
                div()
                    .relative()
                    .flex_none()
                    .child(
                        h_flex()
                            .invisible()
                            .group_hover(group.clone(), |s| s.visible())
                            .child(action),
                    )
                    .child(
                        h_flex()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .right_0()
                            .items_center()
                            .group_hover(group, |s| s.invisible())
                            .child(end),
                    ),
            )
            .context_menu(self.row_menu(row, cx))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let tree = Tree::new(&rows);
        assert!(tree.is_nested("a") && tree.is_nested("b") && tree.is_nested("a1"));
        assert!(!tree.is_nested("root"));
        assert!(
            !tree.is_nested("stray"),
            "a subagent whose parent is not listed stands alone"
        );
        let order: Vec<(usize, &str)> = tree
            .descendants("root")
            .into_iter()
            .map(|(d, r)| (d, r.thread_id.as_str()))
            .collect();
        assert_eq!(
            order,
            [(1, "b"), (1, "a"), (2, "a1")],
            "needs-you first, nested depth first"
        );
        assert_eq!(
            tree.summary("root"),
            Summary {
                total: 3,
                needs_you: 1,
                working: 1,
                failed: 1
            }
        );
        assert_eq!(tree.ancestors("a1"), ["a", "root"]);
        assert_eq!(tree.summary("stray").total, 0);
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
        let tree = Tree::new(&rows);
        assert!(
            tree.is_nested("done"),
            "settled stays with its settled parent"
        );
        assert!(
            !tree.is_nested("back"),
            "un-settled by hand: a row of its own"
        );
        assert!(
            !tree.is_nested("busy"),
            "working under a snoozed parent stays visible"
        );
        assert!(tree.is_nested("finished"));
        assert_eq!(tree.summary("root").total, 1);
    }

    #[test]
    fn a_delegation_cycle_never_hides_its_rows() {
        let rows = vec![row("x", Some("y"), None), row("y", Some("x"), None)];
        let tree = Tree::new(&rows);
        assert!(!tree.is_nested("x") && !tree.is_nested("y"));
        assert_eq!(tree.descendants("x").len(), 1, "the walk ends at the cycle");
    }
}
