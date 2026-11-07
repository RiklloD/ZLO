//! The inbox's scrolling list as Rune virtual rows: the view only works out
//! which lines there are (a few ids and flags each); a row's elements are
//! built when the virtual list mounts it, so 880 settled threads cost the
//! rows on screen, not the whole history.

use std::borrow::Cow;
use std::collections::HashMap;
use std::rc::Rc;

use chrono::Utc;
use rune::{El, GrowthSpec, ScrollSpec, col, el};
use rune_kit::{Button, ButtonVariant, ControlSize, Kit};

use super::super::subagents::Tree;
use super::super::{SETTLED_PAGE, shelves};
use super::rows::{card, slim};
use super::{fold, new_draft_card};
use crate::design;
use crate::design::motion;
use crate::design::smooth_scroll::WheelGlide;
use crate::icons;
use crate::state::{Page, State};
use crate::workspace::active_id;
use crate::workspace::ui::{XS, t};

/// The scroll viewport's key.
pub(in crate::workspace) const SCROLL: &str = "inbox-scroll";
/// The growth stack's key.
const LIST: &str = "inbox-list";
/// The room between the cards and the shelves.
const GAP: f32 = 8.;
/// Room under each line (the old list's gap) and under the last one.
const LINE_GAP: f32 = 4.;
const LIST_BOTTOM: f32 = 8.;
/// Estimated height of a line the list has not measured yet: most of a long
/// history is slim rows.
const ESTIMATE: f32 = design::SIDEBAR_SLIM_HEIGHT + LINE_GAP;

/// One line of the list, in draw order.
#[derive(Clone, Debug, PartialEq)]
enum Line {
    /// "Searching older chats…" while message text is indexed.
    Searching,
    Draft,
    DraftRule,
    /// A full card: index into `inbox.list.rows`, its ⌘N badge.
    Card {
        ix: usize,
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
    /// A shelf's slim row: index into `inbox.list.rows`.
    Slim {
        ix: usize,
    },
    More,
}

/// What every row builder shares, made once per view.
struct Ctx {
    lines: Vec<Line>,
    tree: Tree,
    active: Option<String>,
    now: i64,
    kit: Kit,
}

impl Ctx {
    fn key<'a>(&self, s: &'a State, i: usize) -> Cow<'a, str> {
        let rows = &s.workspace.inbox.list.rows;
        let id = |ix: usize| rows.get(ix).map_or("", |r| r.thread_id.as_str());
        match self.lines.get(i) {
            Some(Line::Searching) => "inbox-searching".into(),
            Some(Line::Draft) => "row-new-draft".into(),
            Some(Line::DraftRule) => "draft-rule".into(),
            Some(Line::Card { ix, .. }) => format!("card-{}", id(*ix)).into(),
            Some(Line::Empty) => "inbox-empty".into(),
            Some(Line::Gap) | None => "shelf-gap".into(),
            Some(Line::Fold { key, .. }) => format!("fold-{key}").into(),
            Some(Line::Slim { ix }) => format!("slim-{}", id(*ix)).into(),
            Some(Line::More) => "inbox-more".into(),
        }
    }

    fn build(&self, s: &State, i: usize) -> El {
        let p = &s.palette;
        let k = &self.kit;
        let rows = &s.workspace.inbox.list.rows;
        let is_active =
            |ix: usize| self.active.as_deref() == rows.get(ix).map(|r| r.thread_id.as_str());
        let body = match self.lines.get(i) {
            Some(Line::Searching) => el().px(12.).py(4.).child(t(
                "Searching older chats…",
                design::SIDEBAR_META_SIZE,
                p.text_faint,
            )),
            Some(Line::Draft) => new_draft_card(s, k).unwrap_or_else(el),
            Some(Line::DraftRule) => el().mx(12.).my(6.).h(1.).fill(p.border),
            Some(Line::Card { ix, jump }) => match rows.get(*ix) {
                Some(r) => card(s, &self.tree, r, is_active(*ix), *jump, self.now, k),
                None => el(),
            },
            Some(Line::Empty) => {
                el().px(8.)
                    .py(12.)
                    .child(t("No conversations yet.", XS, p.text_faint))
            }
            Some(Line::Gap) | None => el().h(GAP),
            Some(Line::Fold {
                key,
                label,
                count,
                open,
            }) => fold(p, key, label, *count, *open),
            Some(Line::Slim { ix }) => match rows.get(*ix) {
                Some(r) => slim(s, &self.tree, r, is_active(*ix), self.now, k),
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
        // A resorted row glides to its new place (GPUI `flip::ROW_SECS`).
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
    let rows = &inbox.list.rows;
    let index: HashMap<&str, usize> = rows
        .iter()
        .enumerate()
        .map(|(ix, r)| (r.thread_id.as_str(), ix))
        .collect();
    let ix_of = |id: &str| index.get(id).copied().unwrap_or(usize::MAX);
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
    let card_line = |id: &str| Line::Card {
        ix: ix_of(id),
        jump: inbox
            .cmd_held
            .then(|| sh.jump.iter().position(|j| j == id))
            .flatten(),
    };
    for row in sh.pinned.iter().chain(sh.main.iter()) {
        lines.push(card_line(&row.thread_id));
    }
    if sh.main.is_empty() && sh.pinned.is_empty() && sh.working.is_empty() && rows.is_empty() {
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
            for row in &sh.working {
                lines.push(card_line(&row.thread_id));
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
            lines.extend(items.iter().take(limit).map(|r| Line::Slim {
                ix: ix_of(&r.thread_id),
            }));
            if items.len() > limit {
                lines.push(Line::More);
            }
        }
    }
    let count = lines.len();
    let ctx = Rc::new(Ctx {
        lines,
        tree: sh.tree,
        active: (s.page == Page::Chat)
            .then(|| active_id(s))
            .flatten()
            .map(str::to_string),
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
        .wheel_glide(SCROLL)
        .child(
            col()
                .key(LIST)
                .w_full()
                .min_h(0.0)
                .growth(GrowthSpec::default())
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
