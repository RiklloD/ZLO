//! `sidebar.session_row.badge`: a small chip on each session row.
//!
//! Lazy and cheap. The sidebar is a virtual list, so only rows near the
//! viewport are built; building one reads the cache and, for a thread it
//! has no badge for (or one older than the row's last activity), notes it
//! in [`Badges::wanted`] and asks for one pump on the next turn of the UI
//! thread. The pump queues the requests and keeps at most [`IN_FLIGHT`]
//! renders running; each reply starts the next. The row never waits: it
//! draws without a chip until the badge lands, and an old chip stays up
//! while its refresh is on its way. Nothing is fetched on a timer.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};

use rune::El;
use zlo_client::inbox::InboxRow;
use zlo_client::plugin_ui::Tone;
use zlo_client::plugins::{SlotContext, SlotKind, UiReply};

use crate::jobs;
use crate::state::State;

/// Most badge renders running at once.
pub const IN_FLIGHT: usize = 4;
/// Most chips on one row.
const PER_ROW: usize = 2;
/// Most renders waiting; older wants beyond it are dropped (their rows
/// scrolled away long ago and ask again when they come back).
const QUEUE_MAX: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BadgeKey {
    pub extension_id: String,
    pub slot_id: String,
    pub thread_id: String,
}

#[derive(Clone, Debug, Default)]
pub struct Entry {
    /// The row's `last_activity_at` this badge was asked for.
    pub stamp: i64,
    pub badge: Option<(String, Tone)>,
    /// Queued or running.
    pub pending: bool,
}

#[derive(Default)]
pub struct Badges {
    pub cache: HashMap<BadgeKey, Entry>,
    queue: VecDeque<BadgeKey>,
    running: usize,
    /// Noted while building rows (the view has `&State` only); drained by
    /// [`pump`].
    wanted: RefCell<Vec<(BadgeKey, i64)>>,
    pump_asked: Cell<bool>,
    /// Bumped by [`Badges::clear`]: replies asked before are dropped.
    era: u64,
}

impl Badges {
    pub fn clear(&mut self) {
        self.cache.clear();
        self.queue.clear();
        self.wanted.borrow_mut().clear();
        // `running` stays: those requests are still on the wire and hold
        // their places until each one answers.
        self.era += 1;
    }

    #[cfg(test)]
    pub fn running(&self) -> usize {
        self.running
    }

    #[cfg(test)]
    pub fn queued(&self) -> usize {
        self.queue.len()
    }
}

/// The chips for a session row, building it. Notes the badges it lacks.
pub fn row_badges(s: &State, row: &InboxRow) -> Vec<El> {
    let p = &s.palette;
    let badges = &s.plugins.badges;
    let mut out = Vec::new();
    for slot in s.plugins.of(SlotKind::SessionBadge) {
        let key = BadgeKey {
            extension_id: slot.extension_id.clone(),
            slot_id: slot.slot_id.clone(),
            thread_id: row.thread_id.clone(),
        };
        let entry = badges.cache.get(&key);
        let fresh = entry.is_some_and(|e| e.pending || e.stamp == row.last_activity_at);
        if !fresh && !s.fixture {
            want(s, key.clone(), row.last_activity_at);
        }
        if out.len() < PER_ROW
            && let Some((label, tone)) = entry.and_then(|e| e.badge.as_ref())
        {
            out.push(
                super::render::chip(p, label, *tone, 11.)
                    .key(format!(
                        "badge-{}-{}-{}",
                        key.extension_id, key.slot_id, key.thread_id
                    ))
                    .max_w(96.)
                    .help(slot.title.clone())
                    // A badge lands from a background render, so it lands
                    // at once; where its row moves with the person (a
                    // filter, a reorder) it fades in and out instead of
                    // popping.
                    .transition(crate::design::motion::fade()),
            );
        }
    }
    out
}

fn want(s: &State, key: BadgeKey, stamp: i64) {
    let badges = &s.plugins.badges;
    let mut wanted = badges.wanted.borrow_mut();
    if wanted.iter().any(|(k, _)| *k == key) {
        return;
    }
    wanted.push((key, stamp));
    if !badges.pump_asked.replace(true) {
        s.rt.handle.update(|s, _| pump(s));
    }
}

/// Move what the rows asked for into the queue and start renders up to
/// [`IN_FLIGHT`].
pub fn pump(s: &mut State) {
    let badges = &mut s.plugins.badges;
    badges.pump_asked.set(false);
    let wanted: Vec<_> = badges.wanted.borrow_mut().drain(..).collect();
    for (key, stamp) in wanted {
        let entry = badges.cache.entry(key.clone()).or_default();
        if entry.pending {
            continue;
        }
        entry.pending = true;
        entry.stamp = stamp;
        badges.queue.push_back(key);
    }
    while badges.queue.len() > QUEUE_MAX {
        if let Some(old) = badges.queue.pop_front()
            && let Some(e) = badges.cache.get_mut(&old)
        {
            e.pending = false;
            // Asked again when its row is built again.
            e.stamp = i64::MIN;
        }
    }
    start(s);
}

fn start(s: &mut State) {
    while s.plugins.badges.running < IN_FLIGHT {
        let Some(key) = s.plugins.badges.queue.pop_front() else {
            return;
        };
        s.plugins.badges.running += 1;
        let era = s.plugins.badges.era;
        let job_key = key.clone();
        jobs::run(
            s,
            "plugin badge",
            move |c| async move {
                let ctx = SlotContext {
                    thread_id: Some(job_key.thread_id.clone()),
                };
                c.plugin_ui_render(&job_key.extension_id, &job_key.slot_id, &ctx)
                    .await
            },
            move |s, result, _| landed(s, era, &key, result.ok()),
        );
    }
}

fn landed(s: &mut State, era: u64, key: &BadgeKey, reply: Option<UiReply>) {
    let badges = &mut s.plugins.badges;
    // Its place is free whichever era asked.
    badges.running = badges.running.saturating_sub(1);
    if badges.era != era {
        start(s);
        return;
    }
    if let Some(e) = badges.cache.get_mut(key) {
        e.pending = false;
        // A failed render keeps no chip; the plugin's reason is not shown
        // on a row.
        e.badge = match reply {
            Some(UiReply::Tree(node)) => node.as_badge().map(|(t, tone)| (t.to_string(), tone)),
            _ => None,
        };
    }
    start(s);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: usize) -> BadgeKey {
        BadgeKey {
            extension_id: "linear".into(),
            slot_id: "state".into(),
            thread_id: format!("t{n}"),
        }
    }

    /// Ten rows asking at once start only [`IN_FLIGHT`] renders; each reply
    /// starts the next, and a stale era's reply changes nothing.
    #[test]
    fn renders_are_capped_and_each_reply_starts_the_next() {
        let mut s = crate::chat::fixtures::test_state();
        s.fixture = false;
        for n in 0..10 {
            s.plugins.badges.wanted.borrow_mut().push((key(n), 1));
        }
        pump(&mut s);
        assert_eq!(s.plugins.badges.running(), IN_FLIGHT);
        assert_eq!(s.plugins.badges.queued(), 10 - IN_FLIGHT);
        // Pumping again with the same wants queues nothing twice.
        s.plugins.badges.wanted.borrow_mut().push((key(0), 1));
        pump(&mut s);
        assert_eq!(s.plugins.badges.queued(), 10 - IN_FLIGHT);

        let era = s.plugins.badges.era;
        landed(&mut s, era, &key(0), None);
        assert_eq!(s.plugins.badges.running(), IN_FLIGHT);
        assert_eq!(s.plugins.badges.queued(), 10 - IN_FLIGHT - 1);
        assert!(!s.plugins.badges.cache[&key(0)].pending);

        // Clearing (the slot list was replaced) frees no place: the four
        // requests are still running, so new wants wait behind them.
        s.plugins.badges.clear();
        assert_eq!(s.plugins.badges.running(), IN_FLIGHT);
        for n in 20..23 {
            s.plugins.badges.wanted.borrow_mut().push((key(n), 1));
        }
        pump(&mut s);
        assert_eq!(s.plugins.badges.running(), IN_FLIGHT);
        assert_eq!(s.plugins.badges.queued(), 3);
        // An earlier era's reply writes nothing but hands its place on.
        landed(&mut s, era, &key(1), None);
        assert_eq!(s.plugins.badges.running(), IN_FLIGHT);
        assert_eq!(s.plugins.badges.queued(), 2);
        assert!(!s.plugins.badges.cache.contains_key(&key(1)));
    }
}
