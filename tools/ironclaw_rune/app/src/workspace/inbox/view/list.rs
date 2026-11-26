//! The inbox's scrolling list as Rune virtual rows: the view only works out
//! which lines there are (a few ids and flags each); a row's elements are
//! built when the virtual list mounts it, so 880 settled threads cost the
//! rows on screen, not the whole history.

use std::borrow::Cow;
use std::cell::Cell;
use std::rc::Rc;

use chrono::Utc;
use rune::{El, GrowthSpec, ScrollSpec, col, el};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit};

use super::super::merged::{self, Entry, RowId};
use super::super::subagents::Tree;
use super::super::{SETTLED_PAGE, shelves};
use super::rows::{RowMeta, card, slim};
use super::{fold, new_draft_card};
use crate::design;
use crate::design::motion;
use crate::icons;
use crate::state::State;
use crate::workspace::ui::{XS, t};

/// The scroll viewport's key.
pub(in crate::workspace) const SCROLL: &str = "inbox-scroll";
/// The growth stack's key.
const LIST: &str = "inbox-list";
/// The least room between the cards and the shelves.
const GAP: f32 = 8.;
/// Room under each line (the old list's gap) and under the last one.
const LINE_GAP: f32 = 4.;
const LIST_BOTTOM: f32 = 8.;
/// Estimated height of a line the list has not measured yet: most of a long
/// history is slim rows.
const ESTIMATE: f32 = design::SIDEBAR_SLIM_HEIGHT + LINE_GAP;

/// The spacer between the cards and the shelves. The design is a flex
/// column whose `flex_1` spacer sits the Working, Snoozed and Settled folds
/// at the sidebar's bottom while the cards are short; a virtual list must be
/// its viewport's only child, so the spacer is a row sized from layout: the
/// viewport's height less everything else in the list, never under [`GAP`].
#[derive(Debug, Default)]
pub(in crate::workspace) struct ShelfGap {
    /// The spacer's height, as the next view builds it.
    height: f32,
    /// The height the last view built it at (`list` runs with `&State`).
    built: Cell<f32>,
    /// The scroll viewport's height.
    viewport: f32,
    /// The list's height without the spacer (its line's bottom room kept).
    rest: f32,
}

impl ShelfGap {
    fn height(&self) -> f32 {
        self.height.max(GAP)
    }

    /// Re-aim the spacer at the measured heights; whether it changed.
    fn aim(&mut self) -> bool {
        if self.viewport <= 0. || self.rest <= 0. {
            return false;
        }
        let height = (self.viewport - self.rest).max(GAP);
        let moved = (height - self.height).abs() > 0.5;
        if moved {
            self.height = height;
        }
        moved
    }

    fn viewport(s: &mut State, height: f32) -> bool {
        let g = &mut s.workspace.inbox.shelf_gap;
        g.viewport = height;
        g.aim()
    }

    fn content(s: &mut State, height: f32) -> bool {
        let g = &mut s.workspace.inbox.shelf_gap;
        g.rest = height - g.built.get();
        g.aim()
    }
}

/// One line of the list, in draw order.
#[derive(Clone, Debug, PartialEq)]
enum Line {
    /// "Searching older chats…" while message text is indexed.
    Searching,
    Draft,
    DraftRule,
    /// A full card: `(computer_ix, row_ix)` into focused/peer rows, its
    /// stable `(computer, thread_id)` key, and its ⌘N badge.
    Card {
        computer_ix: usize,
        row_ix: usize,
        id: RowId,
        jump: Option<usize>,
    },
    Empty,
    /// The spacer that pushes the shelves down a short list.
    Gap,
    Fold {
        key: &'static str,
        label: &'static str,
        count: usize,
        open: bool,
    },
    /// A shelf's slim row.
    Slim {
        computer_ix: usize,
        row_ix: usize,
        id: RowId,
    },
    More,
}

/// What every row builder shares, made once per view.
struct Ctx {
    lines: Vec<Line>,
    tree: Tree,
    active: Option<RowId>,
    multi: bool,
    now: i64,
    kit: Kit,
}

impl Ctx {
    fn key<'a>(&self, _s: &'a State, i: usize) -> Cow<'a, str> {
        match self.lines.get(i) {
            Some(Line::Searching) => "inbox-searching".into(),
            Some(Line::Draft) => "row-new-draft".into(),
            Some(Line::DraftRule) => "draft-rule".into(),
            Some(Line::Card { id, .. }) => format!("card-{}", id.key()).into(),
            Some(Line::Empty) => "inbox-empty".into(),
            Some(Line::Gap) | None => "shelf-gap".into(),
            Some(Line::Fold { key, .. }) => format!("fold-{key}").into(),
            Some(Line::Slim { id, .. }) => format!("slim-{}", id.key()).into(),
            Some(Line::More) => "inbox-more".into(),
        }
    }

    fn build(&self, s: &State, i: usize) -> El {
        let p = &s.palette;
        let k = &self.kit;
        let body = match self.lines.get(i) {
            // It breathes on the small loaders' 15 fps clock while the
            // index catches up (still under Reduce Motion).
            Some(Line::Searching) => el().px(12.).py(4.).child(
                crate::workspace::ui::side_t(
                    "Searching older chats…",
                    design::SIDEBAR_META_SIZE,
                    p.text_faint,
                )
                .key("inbox-searching-text")
                .cycle(design::kit::loops::breathe(
                    1.2,
                    0.,
                    &design::kit::loops::BREATH_55,
                )),
            ),
            Some(Line::Draft) => new_draft_card(s, k).unwrap_or_else(el),
            Some(Line::DraftRule) => el().mx(12.).my(6.).h(1.).fill(p.border),
            Some(Line::Card {
                computer_ix,
                row_ix,
                id,
                jump,
            }) => match merged::entry_at(s, *computer_ix, *row_ix) {
                Some(entry) => {
                    let meta = RowMeta {
                        active: self.active.as_ref() == Some(id),
                        jump: *jump,
                        show_computer: self.multi && !entry.focused,
                        now: self.now,
                    };
                    card(s, &self.tree, &entry, meta, k)
                }
                None => el(),
            },
            Some(Line::Empty) => {
                el().px(8.)
                    .py(12.)
                    .child(t("No conversations yet.", XS, p.text_faint))
            }
            Some(Line::Gap) | None => el().h(s.workspace.inbox.shelf_gap.height()),
            Some(Line::Fold {
                key,
                label,
                count,
                open,
            }) => fold(p, k, key, label, *count, *open),
            Some(Line::Slim {
                computer_ix,
                row_ix,
                id,
            }) => match merged::entry_at(s, *computer_ix, *row_ix) {
                Some(entry) => {
                    let active = self.active.as_ref() == Some(id);
                    let show_computer = self.multi && !entry.focused;
                    slim(s, &self.tree, &entry, active, show_computer, self.now, k)
                }
                None => el(),
            },
            // The next page grows in under the rows already shown.
            Some(Line::More) => Button::new("Show more")
                .variant(ButtonVariant::Ghost)
                .control_size(ControlSize::Small)
                .icon(&icons::CHEVRON_DOWN)
                .on_press_tx(|s: &mut State, tx| {
                    tx.keep_on_screen("fold-settled");
                    s.workspace.inbox.settled_limit += SETTLED_PAGE;
                })
                .build(k),
        };
        let last = i + 1 == self.lines.len();
        let mut line = col()
            .w_full()
            .px(8.)
            .pb(if last { LIST_BOTTOM } else { LINE_GAP })
            .child(body)
            .arrives_whole();
        // A resorted row glides to its new place.
        if matches!(
            self.lines.get(i),
            Some(Line::Card { .. } | Line::Slim { .. })
        ) {
            line = line.motion(motion::ROW_RESORT);
        }
        line
    }
}

/// The scrolling list under the inbox's header.
pub(super) fn list(s: &State, k: &Kit) -> El {
    let inbox = &s.workspace.inbox;
    let query = inbox.query();
    let sh = shelves(s);
    let any_rows = !inbox.list.rows.is_empty() || merged::has_peer_rows(s);
    let mut lines = Vec::new();
    // Message text is searched too; say so while older chats are indexed.
    if !query.is_empty() && s.workspace.message_search.indexing(&query) {
        lines.push(Line::Searching);
    }
    // A new-thread draft sits on top until it is sent, then a rule.
    if crate::drafts::preview(s, crate::drafts::NEW_THREAD).is_some() {
        lines.push(Line::Draft);
        lines.push(Line::DraftRule);
    }
    let card_line = |entry: &Entry<'_>| Line::Card {
        computer_ix: entry.computer_ix,
        row_ix: entry.row_ix,
        id: entry.id.clone(),
        // The open profile menu numbers its own rows on ⌘.
        jump: (inbox.cmd_held && !crate::workspace::inbox::picker::is_open(s))
            .then(|| sh.jump.iter().position(|j| j == &entry.id))
            .flatten(),
    };
    for entry in sh.pinned.iter().chain(sh.main.iter()) {
        lines.push(card_line(entry));
    }
    if sh.main.is_empty() && sh.pinned.is_empty() && sh.working.is_empty() && !any_rows {
        lines.push(Line::Empty);
    }
    // Like t3code: Working, Snoozed and Settled sit at the bottom of the
    // sidebar and unfold downward, scrolling with the list. Working keeps
    // full cards; the parked shelves use slim rows.
    let mut pushed = false;
    if !sh.working.is_empty() {
        pushed = true;
        lines.push(Line::Gap);
        lines.push(Line::Fold {
            key: "working",
            label: "Working",
            count: sh.working.len(),
            open: sh.working_open,
        });
        if sh.working_open {
            for entry in &sh.working {
                lines.push(card_line(entry));
            }
        }
    }
    let sections = [
        (
            "snoozed",
            "Snoozed",
            &sh.snoozed,
            inbox.show_snoozed || sh.unfold,
            usize::MAX,
        ),
        (
            "settled",
            "Settled",
            &sh.settled,
            inbox.show_settled || sh.unfold,
            inbox.settled_limit,
        ),
    ];
    for (key, label, items, open, limit) in sections {
        if items.is_empty() {
            continue;
        }
        if !std::mem::replace(&mut pushed, true) {
            lines.push(Line::Gap);
        }
        lines.push(Line::Fold {
            key,
            label,
            count: items.len(),
            open,
        });
        if open {
            lines.extend(items.iter().take(limit).map(|e| Line::Slim {
                computer_ix: e.computer_ix,
                row_ix: e.row_ix,
                id: e.id.clone(),
            }));
            if items.len() > limit {
                lines.push(Line::More);
            }
        }
    }
    let count = lines.len();
    let gap = &inbox.shelf_gap;
    gap.built.set(if lines.contains(&Line::Gap) {
        gap.height()
    } else {
        0.
    });
    let ctx = Rc::new(Ctx {
        lines,
        tree: sh.tree,
        active: merged::active_row(s),
        multi: merged::multi_connected(s),
        now: Utc::now().timestamp_millis(),
        kit: k.clone(),
    });
    let keys = ctx.clone();
    col()
        .key(SCROLL)
        .w_full()
        .grow(1.0)
        .min_h(0.0)
        .scroll(ScrollSpec {
            stick_bottom: false,
        })
        .on_layout_tx(|s: &mut State, r: [f32; 4], tx: &mut rune::Tx| {
            if !ShelfGap::viewport(s, r[3]) {
                tx.live_only();
            }
        })
        .child(
            col()
                .key(LIST)
                .w_full()
                .min_h(0.0)
                .growth(GrowthSpec::default())
                .whole_rows()
                .on_layout_tx(|s: &mut State, r: [f32; 4], tx: &mut rune::Tx| {
                    if !ShelfGap::content(s, r[3]) {
                        tx.live_only();
                    }
                })
                .virtual_rows(
                    count,
                    ESTIMATE,
                    move |s: &State, i| keys.key(s, i),
                    move |s: &State, i| ctx.build(s, i),
                ),
        )
}

#[cfg(test)]
#[path = "list_tests.rs"]
mod tests;
