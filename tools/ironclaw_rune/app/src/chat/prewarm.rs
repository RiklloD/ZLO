//! A thread made ready while the pointer rests on its row (sidebar card,
//! Working-shelf row, subagent row), so the click that follows opens it at
//! once. After [`REST`] on one row its timeline is fetched into a warm
//! [`ChatState`] (kept apart from the open chat: no stream, no extras, no
//! handler finds it), and once its rows are in, Rune lays that chat's column
//! out into the window's text cache ahead (`Tx::prepare_text`, #1104), so the
//! switch's first frame finds its text shaped. [`super::open`] then adopts
//! the warm rows: the transcript shows on the click's own frame, not one
//! timeline round-trip later, and the open's own timeline load refreshes it.
//!
//! Bounded: at most [`WARM_MAX`] threads are warm, oldest out first; a row
//! the pointer leaves before [`REST`] (a pass over the list, a scroll past)
//! warms nothing, and a fetch still in flight when it leaves is dropped as
//! it lands. Nothing here changes what the window shows, so the hover
//! handlers and the landings are live-only: no rebuild.

use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{Duration, Instant};

use rune::{Cx, El, Tx, el};
use zlo_client::transcript::Row;

use super::{ChatState, outline, question_form};
use crate::state::State;

/// How long the pointer rests on a row before its thread warms.
pub const REST: Duration = Duration::from_millis(80);
/// The most threads kept warm at once.
pub const WARM_MAX: usize = 3;
/// A warm thread older than this is fetched again before it is laid out.
const FRESH_FOR: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct Prewarm {
    /// The row under the pointer and when the pointer came to it.
    hover: Option<(String, Instant)>,
    /// The hovered thread its warm pass already ran for.
    ran: Option<String>,
    /// Warm threads, oldest first.
    warm: VecDeque<Warm>,
    /// The generation the next warm entry gets: a fetch lands only in the
    /// entry it was started for, never in a later one for the same thread.
    next_gen: u64,
}

struct Warm {
    chat: ChatState,
    /// Its rows are in (false while the fetch is in flight).
    loaded: bool,
    asked: Instant,
    generation: u64,
}

impl Prewarm {
    fn find(&self, id: &str) -> Option<&Warm> {
        self.warm.iter().find(|w| w.chat.thread_id == id)
    }

    fn find_mut(&mut self, id: &str) -> Option<&mut Warm> {
        self.warm.iter_mut().find(|w| w.chat.thread_id == id)
    }

    fn drop_thread(&mut self, id: &str) {
        self.warm.retain(|w| w.chat.thread_id != id);
    }

    fn hovered(&self) -> Option<&str> {
        self.hover.as_ref().map(|(id, _)| id.as_str())
    }
}

/// A thread row's `on_hover_change`: the pointer came to (`inside`) or left
/// `thread`'s row. Live-only: nothing on screen depends on it.
pub fn hover(s: &mut State, thread: &str, inside: bool, tx: &mut Tx) {
    tx.live_only();
    let p = &mut s.prewarm;
    if inside {
        if p.hovered() != Some(thread) {
            p.hover = Some((thread.to_string(), Instant::now()));
            p.ran = None;
        }
        tx.wake_after(REST);
        return;
    }
    if p.hovered() != Some(thread) {
        return;
    }
    p.hover = None;
    p.ran = None;
    // Cancel: a fetch still in flight for it lands nowhere.
    if p.find(thread).is_some_and(|w| !w.loaded) {
        p.drop_thread(thread);
    }
}

/// Before every frame: warms the hovered thread once the pointer has rested
/// on its row for [`REST`]. Never changes the view.
pub fn poll(s: &mut State, tx: &mut Tx) {
    let Some((id, at)) = s.prewarm.hover.clone() else {
        return;
    };
    if s.prewarm.ran.as_deref() == Some(id.as_str()) {
        return;
    }
    let rested = at.elapsed();
    if rested < REST {
        tx.wake_after(REST - rested);
        return;
    }
    s.prewarm.ran = Some(id.clone());
    warm(s, &id, tx);
}

fn warm(s: &mut State, id: &str, tx: &mut Tx) {
    // Already on screen: the open chat or a side chat in its pane.
    if super::chat_ref(s, id).is_some() {
        return;
    }
    match s.prewarm.find(id) {
        Some(w) if w.loaded && w.asked.elapsed() < FRESH_FOR => {
            prepare(tx);
            return;
        }
        // In flight.
        Some(w) if !w.loaded => return,
        Some(_) => s.prewarm.drop_thread(id),
        None => {}
    }
    let chat = super::build(s, id.to_string(), false);
    let p = &mut s.prewarm;
    let generation = p.next_gen;
    p.next_gen += 1;
    p.warm.push_back(Warm {
        chat,
        loaded: false,
        asked: Instant::now(),
        generation,
    });
    while p.warm.len() > WARM_MAX {
        p.warm.pop_front();
    }
    // A bench scene's gateway is its feed.
    if let Some(rows) = crate::bench::timeline(s, id) {
        if let Some(w) = s.prewarm.find_mut(id) {
            super::fixtures::fill(&mut w.chat, rows, false);
            w.loaded = true;
        }
        prepare(tx);
        return;
    }
    let (req, thread) = (id.to_string(), id.to_string());
    crate::jobs::run(
        s,
        "prewarm",
        move |c| async move { c.timeline(&req, super::TIMELINE_LIMIT).await },
        move |s, result, tx| {
            tx.live_only();
            let hovered = s.prewarm.hovered() == Some(thread.as_str());
            // Only the entry this fetch was started for: a cancelled
            // fetch landing after a newer one must not overwrite it.
            let Some(w) = s
                .prewarm
                .warm
                .iter_mut()
                .find(|w| w.chat.thread_id == thread && w.generation == generation)
            else {
                return;
            };
            match result {
                Ok(page) => {
                    w.chat.transcript.set_timeline(page);
                    fold(&mut w.chat);
                    w.loaded = true;
                    if hovered {
                        prepare(tx);
                    }
                }
                Err(e) => {
                    tracing::debug!(thread_id = %thread, error = %e, "prewarm: timeline failed");
                    s.prewarm
                        .warm
                        .retain(|w| !(w.chat.thread_id == thread && w.generation == generation));
                }
            }
        },
    );
}

/// The warm chat's rows and list items from its transcript, as
/// `refresh_rows` + `sync_items` fold them for a chat at rest (no pacer, no
/// reserve: the open's own load folds it again with both).
fn fold(c: &mut ChatState) {
    let rows: Rc<Vec<Row>> = Rc::new(c.transcript.rows());
    let live = c.running_run().is_some();
    let stopping = c.transcript.is_stopping();
    c.spans = Rc::new(c.transcript.turn_spans());
    c.items = Rc::new(super::flow::fold(
        &rows, live, stopping, false, false, &c.spans, &c.toggles,
    ));
    c.outline.set(outline::prompts(&c.items));
    c.rows = rows;
    c.rows_gen += 1;
    question_form::sync_drafts(c);
}

/// Ask Rune to lay the hovered warm chat's column out ahead, at the open
/// chat's size.
fn prepare(tx: &mut Tx) {
    tx.prepare_text::<State>(prepared, None);
}

/// The view `Tx::prepare_text` measures: the hovered warm chat's column
/// where the open chat's is drawn (its size, so every row wraps at the
/// width it will be shown at).
fn prepared(s: &State, cx: &mut Cx<State>) -> El {
    let Some(w) = s
        .prewarm
        .hovered()
        .and_then(|id| s.prewarm.find(id))
        .filter(|w| w.loaded)
    else {
        return el();
    };
    let k = crate::design::kit(&s.palette);
    let mut layer = Vec::new();
    let column = super::view::column(s, &w.chat, cx, &k, &mut layer);
    let size = crate::workspace::active_id(s)
        .and_then(|id| crate::workspace::thread_drag::chat_rect(s, id))
        .map(|r| (r[2], r[3]));
    match size {
        Some((w, h)) => el().w(w).h(h).child(column),
        None => el().w_full().h_full().child(column),
    }
}

/// The warm rows of `thread`, moved into the chat `super::open` just built
/// for it. False when it was not warm.
pub(super) fn adopt(s: &mut State, chat: &mut ChatState) -> bool {
    let p = &mut s.prewarm;
    let Some(ix) = p
        .warm
        .iter()
        .position(|w| w.chat.thread_id == chat.thread_id)
    else {
        return false;
    };
    let Some(w) = p.warm.remove(ix) else {
        return false;
    };
    // Unfinished or stale rows are not adopted: the open's own load shows
    // the thread instead.
    if !w.loaded || w.asked.elapsed() >= FRESH_FOR {
        return false;
    }
    let warm = w.chat;
    chat.transcript = warm.transcript;
    chat.rows = warm.rows;
    chat.spans = warm.spans;
    chat.rows_gen = warm.rows_gen;
    chat.items = warm.items;
    chat.outline = warm.outline;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn switch_scene() -> State {
        let mut s = super::super::fixtures::test_state();
        crate::bench::switch(&mut s);
        s
    }

    fn hovered(s: &mut State, id: &str, inside: bool) {
        let mut tx = Tx::default();
        hover(s, id, inside, &mut tx);
    }

    #[test]
    fn a_pass_over_a_row_warms_nothing() {
        let mut s = switch_scene();
        hovered(&mut s, "t2", true);
        hovered(&mut s, "t2", false);
        poll(&mut s, &mut Tx::default());
        assert!(s.prewarm.warm.is_empty());
    }

    #[test]
    fn a_rested_row_warms_its_thread_and_the_click_adopts_it() {
        let mut s = switch_scene();
        hovered(&mut s, "t2", true);
        s.prewarm.hover.as_mut().unwrap().1 -= REST;
        poll(&mut s, &mut Tx::default());
        assert!(s.prewarm.find("t2").is_some_and(|w| w.loaded));
        crate::workspace::open_thread(&mut s, "t2".into());
        let c = s.chat.as_ref().unwrap();
        assert_eq!(c.thread_id, "t2");
        assert!(!c.items.is_empty(), "the click shows the warm rows at once");
        assert!(s.prewarm.warm.is_empty());
    }

    fn warm_t2(s: &mut State) {
        hovered(s, "t2", true);
        s.prewarm.hover.as_mut().unwrap().1 -= REST;
        poll(s, &mut Tx::default());
        hovered(s, "t2", false);
        assert!(s.prewarm.find("t2").is_some_and(|w| w.loaded));
    }

    #[test]
    fn stale_warm_rows_are_not_adopted() {
        let mut s = switch_scene();
        warm_t2(&mut s);
        s.prewarm.find_mut("t2").unwrap().asked -= FRESH_FOR;
        let mut chat = super::super::build(&s, "t2".into(), false);
        assert!(
            !adopt(&mut s, &mut chat),
            "rows older than FRESH_FOR wait for the open's load"
        );
        assert!(chat.items.is_empty());
    }

    #[test]
    fn each_warm_entry_gets_its_own_generation() {
        let mut s = switch_scene();
        warm_t2(&mut s);
        let first = s.prewarm.find("t2").unwrap().generation;
        s.prewarm.drop_thread("t2");
        warm_t2(&mut s);
        assert_ne!(s.prewarm.find("t2").unwrap().generation, first);
    }

    #[test]
    fn switching_computers_drops_warm_threads() {
        let mut s = switch_scene();
        warm_t2(&mut s);
        crate::remote::connect::reset_for_switch(&mut s);
        assert!(s.prewarm.warm.is_empty());
    }

    #[test]
    fn at_most_a_few_threads_stay_warm() {
        let mut s = switch_scene();
        for id in ["t2", "t3", "t4", "t5", "t6"] {
            hovered(&mut s, id, true);
            s.prewarm.hover.as_mut().unwrap().1 -= REST;
            poll(&mut s, &mut Tx::default());
            hovered(&mut s, id, false);
        }
        assert!(s.prewarm.warm.len() <= WARM_MAX);
    }
}
