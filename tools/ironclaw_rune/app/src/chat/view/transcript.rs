//! The transcript list: a virtual growth stack in a scroll viewport that
//! follows its end. The view works out what every row shares (its run, the
//! reserve, the newest turn); a row's elements are built only when the
//! virtual list mounts it, so a 500-message thread costs the rows near the
//! viewport. Zeron's turn layout comes from the reserve: after a send, the
//! room under the newest prompt is exactly what keeps that prompt at the top
//! of the viewport, so following the end shows the prompt at the top while
//! its answer streams into the room below.

use std::borrow::Cow;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use rune::{El, GrowthSpec, ScrollSpec, Spring, SpringPair, Transition, col, el, memo, row};
use rune_kit::Kit;
use zlo_client::transcript::{CliBody, PREFIX_KEY, Row};

use super::super::{ChatState, Item, chat_for, chat_ref, flow, outline, question_form, rows};
use super::{row_key, transcript_key};
use crate::design::smooth_scroll::WheelGlide;
use crate::state::State;

/// Room each side of a transcript row's reading column.
const ROW_GUTTER: f32 = 16.;

/// Estimated height of a row the virtual list has not measured yet.
const ROW_ESTIMATE: f32 = 72.;

/// MonoCode's prompt rise, in seconds: the sent message's climb from the
/// composer, then the slide-in of the rows after it (GPUI `PROMPT_RISE`,
/// `PROMPT_FADE`, `PROMPT_RISE_TOTAL`).
const PROMPT_RISE: f32 = 0.56;
const PROMPT_FADE: f32 = 0.48;
const PROMPT_RISE_TOTAL: f32 = 0.88;
/// The send commit's curve (`actions::send`): the scroll that carries the
/// new prompt from the composer to the top of the list.
/// Growth rows take a commit's spring 15 % slower, so this lands the climb
/// at about [`PROMPT_RISE`].
pub(crate) const PROMPT_RISE_SPRING: Spring = Spring::new(PROMPT_RISE / 1.15, 0.);

/// The send's entrance for the rows that mount with it. The climb itself is
/// the layout: the reserve and the scroll to the end carry the prompt from
/// the composer to the top on [`PROMPT_RISE_SPRING`], so the row only fades
/// in on the way (no second travel stacked on it). The rows after it (the
/// working line, the answer's first rows) slide up 10 pt and fade in once
/// it lands. Reduced motion sets no `rise`.
fn prompt_rise(
    chat: &ChatState,
    ix: usize,
    turn_start: Option<usize>,
    item: &Item,
) -> Option<impl FnOnce(El) -> El> {
    let (key, at) = chat.rise.as_ref()?;
    let start = turn_start?;
    let elapsed = at.elapsed().as_secs_f32();
    if elapsed >= PROMPT_RISE_TOTAL || ix < start || matches!(item, Item::Reserve) {
        return None;
    }
    let (transition, delay) = if ix == start {
        if item.key() != key {
            return None;
        }
        (
            Transition::opacity().springs(SpringPair::with_exit_ratio(
                Spring::new(PROMPT_FADE, 0.),
                0.5,
            )),
            0.,
        )
    } else {
        (
            Transition::offset(0., 10.)
                .combined(Transition::opacity())
                .springs(SpringPair::with_exit_ratio(
                    Spring::new(PROMPT_RISE_TOTAL - PROMPT_RISE, 0.),
                    0.5,
                )),
            (PROMPT_RISE - elapsed).max(0.),
        )
    };
    Some(move |line: El| line.transition(transition).enter_delay(delay))
}

/// The reserve's height: while held, exactly what keeps the newest prompt
/// at the top of the list and no more. It stays after the run ends (the view
/// keeps its place, as in Zeron).
pub(super) fn reserve_height(chat: &ChatState) -> f32 {
    let Some(reserve) = &chat.reserve else {
        return 0.;
    };
    if !reserve.held {
        return 0.;
    }
    let turn: f32 = chat
        .items
        .iter()
        .rposition(|i| matches!(i, Item::Row(Row::User { .. })))
        .map(|start| {
            chat.items[start..]
                .iter()
                .filter(|i| !matches!(i, Item::Reserve))
                .filter_map(|i| chat.turn_heights.get(i.key()))
                .sum()
        })
        .unwrap_or(0.);
    // Under the reserve: the last row's own bottom padding.
    let below = flow::SPACE_LG + 24.;
    (chat.list_height - turn - below).max(0.)
}

/// What every row of one transcript shares, worked out once per view.
struct Rows {
    thread: String,
    items: Rc<Vec<Item>>,
    /// The run each row belongs to (1-based), for "fork from here".
    run_of: Vec<u32>,
    /// The newest prompt, while a send's reserve is in play.
    turn_start: Option<usize>,
    seed: u64,
    outputs: Arc<HashMap<String, CliBody>>,
    reserve_h: f32,
    /// The newest prompt's row key while the reserve keeps room under it.
    anchor: Option<String>,
    column: f32,
    kit: Kit,
    /// The palette, type size and column width, hashed: what every memoized
    /// row's look depends on beyond its own content.
    look: u64,
}

impl Rows {
    fn key(&self, ix: usize) -> String {
        self.items
            .get(ix)
            .map_or_else(String::new, |i| row_key(i.key()))
    }

    fn build(&self, s: &State, ix: usize) -> El {
        let (Some(chat), Some(item)) = (chat_ref(s, &self.thread), self.items.get(ix)) else {
            return el();
        };
        let (k, items, id) = (&self.kit, &self.items, &self.thread);
        let turn_start = self.turn_start;
        let top = flow::top_gap(ix.checked_sub(1).map(|i| &items[i]), item);
        let last = ix + 1 == items.len();
        let body = match item {
            Item::Row(
                row_ @ Row::Assistant {
                    key,
                    text,
                    streaming: false,
                },
            ) => {
                let fork = self
                    .run_of
                    .get(ix)
                    .copied()
                    .filter(|r| *r > 0 && !key.starts_with(PREFIX_KEY));
                let copied = chat.copied.as_ref().is_some_and(|(c, _)| c == key);
                // A finished reply only changes with its text, its copy
                // check, its run and the look: keep its parsed Markdown.
                let version = (text.as_str(), copied, fork, self.look);
                memo(format!("{key}-group"), version, || {
                    rows::with_actions(
                        chat,
                        key,
                        text,
                        false,
                        fork,
                        rows::render_row(s, chat, row_, k),
                        k,
                    )
                })
            }
            Item::Row(
                row_ @ Row::User {
                    key,
                    text,
                    pending: false,
                    ..
                },
            ) if !text.is_empty() => rows::with_actions(
                chat,
                key,
                text,
                true,
                None,
                rows::render_row(s, chat, row_, k),
                k,
            ),
            // The bubble's own key: text that turns into narration keeps its
            // element, so it neither re-parses nor fades in again.
            Item::Row(Row::Narration { text, .. }) => {
                super::super::visual::prose(s, id, item.key(), text, false, k)
            }
            Item::Row(Row::Question {
                key,
                run_id,
                gate_ref,
                form,
                state,
            }) => question_form::render_question(s, chat, key, run_id, gate_ref, form, *state, k),
            Item::Row(row_) => rows::render_row(s, chat, row_, k),
            Item::Activity {
                key,
                rows: group,
                live,
            } => {
                let open = chat.activity_open(key, *live);
                flow::render_activity(
                    s,
                    id,
                    key,
                    group,
                    *live,
                    open,
                    &chat.toggles,
                    &self.outputs,
                    k,
                )
            }
            Item::Pending { stopping } => {
                super::super::pending::render_pending(s, *stopping, chat.run_started, self.seed, k)
            }
            Item::Tail => super::super::orchestration::tail(s, chat, k).unwrap_or_else(el),
            Item::Reserve => el().h(self.reserve_h),
        };
        let key = item.key().to_string();
        // The reading column springs across with its row: a growth row's
        // children otherwise snap sideways while the row rides the panel's
        // edge (a sidebar slide), leaping the wrong way and back.
        let mut line = row()
            .w_full()
            .justify_center()
            .px(ROW_GUTTER)
            .pt(top)
            .animate_layout()
            .child(
                el().key("body")
                    .w_full()
                    .max_w(self.column)
                    .min_w(0.)
                    .child(body),
            );
        if last {
            line = line.pb(flow::SPACE_LG + 24.);
        }
        if let Some(enter) = prompt_rise(chat, ix, turn_start, item) {
            line = enter(line);
        }
        // The sent prompt and the room under it arrive whole: their growth
        // glides on the spring alone (not paced like streaming text), so
        // the climb to the top takes the prompt rise's time.
        if matches!(item, Item::Reserve) || turn_start == Some(ix) {
            line = line.arrives_whole();
        }
        // Rows of the newest turn report their height to size the reserve.
        if turn_start.is_some_and(|s| ix >= s) && !matches!(item, Item::Reserve) {
            let (thread, key, anchor) = (id.clone(), key.clone(), self.anchor.clone());
            line = line.on_layout_tx(move |s: &mut State, r, tx| {
                let h = r[3];
                let Some(c) = chat_for(s, &thread) else {
                    return;
                };
                let old = c.turn_heights.insert(key.clone(), h);
                if old.is_some_and(|o| (o - h).abs() <= 0.5) {
                    tx.live_only();
                    return;
                }
                // The reserve is sized from these laid-out heights while the
                // rows show their growing ones: hold the prompt where it is
                // drawn while both settle, so it never creeps. Once the
                // answer fills the room the list follows its end instead.
                match anchor.as_ref().filter(|_| reserve_height(c) > 0.) {
                    Some(anchor) => {
                        tx.keep_on_screen(anchor.clone());
                        c.held_prompt = true;
                    }
                    // The answer outgrew the room: back to following the end
                    // (holding the prompt took the list off it).
                    None if std::mem::take(&mut c.held_prompt) && !c.far_from_bottom => {
                        tx.scroll_to_bottom(transcript_key(&thread));
                    }
                    None => {}
                }
            });
        } else if let Item::Row(Row::User { key: user_key, .. }) = item {
            // Prompt rows tell the outline where they are.
            let (thread, user_key) = (id.clone(), user_key.clone());
            line = line.on_layout_tx(move |s: &mut State, r, tx| {
                outline::saw(s, &thread, &user_key, r[1], tx)
            });
        }
        line
    }
}

pub(super) fn transcript(s: &State, chat: &ChatState, k: &Kit) -> El {
    let items = chat.items.clone();
    let id = chat.thread_id.clone();
    let turn_start = items
        .iter()
        .rposition(|i| matches!(i, Item::Row(Row::User { .. })))
        .filter(|_| chat.reserve.is_some());
    let run_of: Vec<u32> = items
        .iter()
        .scan(0u32, |run, item| {
            if let Item::Row(Row::User { key, .. }) = item
                && !key.starts_with(PREFIX_KEY)
            {
                *run += 1;
            }
            Some(*run)
        })
        .collect();
    let reserve_h = reserve_height(chat);
    // While the reserve keeps room under it, the newest prompt is the turn's
    // anchor at the top of the list; once the answer fills the room the list
    // follows its end again.
    let anchor = turn_start
        .filter(|_| reserve_h > 0.)
        .map(|ix| row_key(items[ix].key()));
    let column = super::super::content_width(s);
    let count = items.len();
    let rows = Rc::new(Rows {
        thread: id.clone(),
        items,
        run_of,
        turn_start,
        seed: super::super::pending::seed(&id),
        outputs: chat.transcript.tool_outputs(),
        reserve_h,
        anchor,
        column,
        kit: k.clone(),
        look: look(s, column),
    });
    let keys = rows.clone();
    // The stack is the reading column (plus its gutters), centred in the
    // viewport: when the panel's width changes (a sidebar slide) the stack
    // travels as one element and its rows never shift sideways inside it.
    let stack = col()
        .key("transcript-rows")
        .w_full()
        .max_w(column + 2. * ROW_GUTTER)
        .growth(GrowthSpec::default())
        .virtual_rows(
            count,
            ROW_ESTIMATE,
            move |_: &State, ix| Cow::Owned(keys.key(ix)),
            move |s: &State, ix| rows.build(s, ix),
        );
    let (layout_id, far_id) = (id.clone(), id.clone());
    col()
        .key(transcript_key(&id))
        .w_full()
        .h_full()
        .min_h(0.)
        .items_center()
        .scroll(ScrollSpec { stick_bottom: true })
        .wheel_glide(transcript_key(&id))
        .scroll_edge_fade(24.)
        .on_layout_tx(move |s: &mut State, r, tx| {
            let Some(c) = chat_for(s, &layout_id) else {
                return;
            };
            let [_, y, w, h] = r;
            if (c.list_height - h).abs() <= 0.5 && c.outline.viewport == [y, w, h] {
                tx.live_only();
                return;
            }
            c.list_height = h;
            c.outline.viewport = [y, w, h];
        })
        .on_far_from_bottom(160., move |s: &mut State, far| {
            if let Some(c) = chat_for(s, &far_id) {
                c.far_from_bottom = far;
            }
        })
        .child(stack)
}

/// A fingerprint of what finished rows look like beyond their own content.
fn look(s: &State, column: f32) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    format!("{:?}", s.palette).hash(&mut h);
    s.prefs.ui_font_size.to_bits().hash(&mut h);
    column.to_bits().hash(&mut h);
    h.finish()
}
