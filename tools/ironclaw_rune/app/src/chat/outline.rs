//! The prompt outline, after MonoCode's `transcript/outline.rs` (MIT) by way
//! of the GPUI app's `chat/outline.rs`: one short tick per message you sent,
//! stacked on the transcript's right edge. The tick for the part on screen
//! is lit. Pointing at the rail magnifies the ticks around the pointer like
//! the macOS Dock and shows the prompt with the start of its answer; a click
//! jumps there.
//!
//! GPUI observed the list's scroll position; here each prompt row reports
//! where it lies ([`saw`]) and the view re-runs only when the lit tick moves.

use std::collections::HashMap;

use ironclaw_gpui_client::transcript::{self, Row};
use rune::{El, Tx, col, el, row, text};

use super::{ChatState, Item, flow};
use crate::state::State;

const BAR_HEIGHT: f32 = 2.;
const BAR_WIDTH: f32 = 11.;
const BAR_WIDTH_LIFTED: f32 = 24.;
const BAR_OPACITY_IDLE: f32 = 0.18;
const BAR_OPACITY_LIT: f32 = 0.85;
const BAR_GAP: f32 = 10.;
const BAR_GAP_MIN: f32 = 1.;
const STACK_MAX: f32 = 330.;
/// Bars on each side of the pointer that lift with it.
const RIPPLE_SPAN: usize = 2;
const MIN_PROMPTS: usize = 2;
/// Narrower transcripts keep the reading column clear.
const MIN_WIDTH: f32 = 900.;
const CARD_WIDTH: f32 = 288.;

#[derive(Clone, PartialEq)]
pub(crate) struct Prompt {
    /// The user row's item key in the transcript list.
    pub key: String,
    pub label: String,
    pub reply: Option<String>,
}

#[derive(Default)]
pub(crate) struct OutlineState {
    prompts: Vec<Prompt>,
    hovered: Option<usize>,
    /// The prompt at the top of the viewport.
    active: Option<usize>,
    /// Where each prompt row was laid out last (window y), by key.
    tops: HashMap<String, f32>,
    /// The transcript viewport: its top (window y), width and height.
    pub(crate) viewport: [f32; 3],
}

impl OutlineState {
    pub(crate) fn set(&mut self, prompts: Vec<Prompt>) {
        if self.prompts != prompts {
            self.tops.retain(|k, _| prompts.iter().any(|p| p.key == *k));
            self.prompts = prompts;
            self.active = self.compute_active();
        }
    }

    fn compute_active(&self) -> Option<usize> {
        let top = self.viewport[0] + 40.;
        let mut active = None;
        for (ix, p) in self.prompts.iter().enumerate() {
            match self.tops.get(&p.key) {
                Some(y) if *y <= top => active = Some(ix),
                Some(_) => break,
                // Not laid out (virtualized away above): it is above.
                None if active.is_some() || ix == 0 => active = Some(ix),
                None => {}
            }
        }
        active
    }
}

/// A prompt row of `thread` was laid out at window `y`. Unless the lit tick
/// moved, the view does not run again.
pub(crate) fn saw(s: &mut State, thread: &str, key: &str, y: f32, tx: &mut Tx) {
    let Some(c) = super::chat_for(s, thread) else {
        tx.live_only();
        return;
    };
    c.outline.tops.insert(key.to_string(), y);
    let active = c.outline.compute_active();
    if active == c.outline.active {
        tx.live_only();
    } else {
        c.outline.active = active;
    }
}

/// Each sent message and the start of the reply under it.
pub(crate) fn prompts(items: &[Item]) -> Vec<Prompt> {
    let mut out: Vec<Prompt> = Vec::new();
    for item in items {
        match item {
            Item::Row(Row::User {
                key,
                text,
                attachments,
                ..
            }) => out.push(Prompt {
                key: key.clone(),
                label: first_line(&transcript::readable(text, attachments), 160),
                reply: None,
            }),
            Item::Row(Row::Assistant { text, .. }) => {
                if let Some(last) = out.last_mut()
                    && last.reply.is_none()
                    && !text.trim().is_empty()
                {
                    last.reply = Some(first_line(text, 120));
                }
            }
            _ => {}
        }
    }
    out
}

/// The first non-empty line, whitespace collapsed, at most `max` chars.
pub(crate) fn first_line(text: &str, max: usize) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("```"))
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut out: String = line.chars().take(max).collect();
    if line.chars().count() > max {
        out.push('…');
    }
    out
}

/// One bar per prompt while they fit; the gap shrinks first, then a window
/// slides to keep the active prompt in view, preferring the newest.
fn bar_stack(count: usize, active: Option<usize>, budget: f32) -> (usize, usize, f32) {
    let fit = (((budget + BAR_GAP_MIN) / (BAR_HEIGHT + BAR_GAP_MIN)).floor() as usize).max(1);
    let (start, end) = if count <= fit {
        (0, count)
    } else {
        let newest = count - fit;
        let start = active.map_or(newest, |a| a.min(newest));
        (start, start + fit)
    };
    let shown = end - start;
    let gap = if shown > 1 {
        ((budget - shown as f32 * BAR_HEIGHT) / (shown - 1) as f32)
            .floor()
            .clamp(BAR_GAP_MIN, BAR_GAP)
    } else {
        0.
    };
    (start, end, gap)
}

/// Dock magnification: 1 on the hovered bar, tapering to 0 past the span.
fn bar_lift(index: usize, hover: Option<usize>) -> f32 {
    let Some(hover) = hover else { return 0. };
    let distance = index.abs_diff(hover);
    if distance > RIPPLE_SPAN {
        return 0.;
    }
    (RIPPLE_SPAN + 1 - distance) as f32 / (RIPPLE_SPAN + 1) as f32
}

/// The rail over the transcript's right edge, or nothing when there are
/// too few prompts or too little room.
pub(crate) fn view(s: &State, chat: &ChatState) -> Option<El> {
    let o = &chat.outline;
    let [_, width, height] = o.viewport;
    if o.prompts.len() < MIN_PROMPTS || width < MIN_WIDTH {
        return None;
    }
    let p = &s.palette;
    let budget = STACK_MAX.min((height * 0.6).floor()).max(BAR_HEIGHT);
    let (start, end, gap) = bar_stack(o.prompts.len(), o.active, budget);
    let hover = o.hovered.filter(|h| (start..end).contains(h));
    let ink = |a: f32| {
        if p.light {
            rune::Color::BLACK.alpha(a)
        } else {
            rune::Color::WHITE.alpha(a)
        }
    };
    let mut bars = col()
        .key("outline")
        .role("toolbar")
        .label("Your messages")
        .w(BAR_WIDTH_LIFTED)
        .py(8.);
    for ix in start..end {
        let prompt = &o.prompts[ix];
        let lift = bar_lift(ix, hover);
        let lit = match hover {
            Some(h) => ix == h,
            None => Some(ix) == o.active,
        };
        let thread = chat.thread_id.clone();
        let hover_thread = chat.thread_id.clone();
        let target = super::view::row_key(&prompt.key);
        let mut bar = row()
            .key(format!("outline-bar-{ix}"))
            .role("button")
            .label(prompt.label.clone())
            .w_full()
            .h(BAR_HEIGHT + gap)
            .center_y()
            .justify_end()
            .cursor(rune::Cursor::Pointer)
            .child(
                el().h(BAR_HEIGHT)
                    .w(BAR_WIDTH + (BAR_WIDTH_LIFTED - BAR_WIDTH) * lift)
                    .radius(1.)
                    .fill(ink(if lit {
                        BAR_OPACITY_LIT
                    } else {
                        BAR_OPACITY_IDLE
                    })),
            )
            .on_hover_change(move |s: &mut State, on: bool, _tx: &mut Tx| {
                if let Some(c) = super::chat_for(s, &hover_thread) {
                    if on {
                        c.outline.hovered = Some(ix);
                    } else if c.outline.hovered == Some(ix) {
                        c.outline.hovered = None;
                    }
                }
            })
            .on_press_tx(move |s: &mut State, tx| {
                if let Some(c) = super::chat_for(s, &thread) {
                    c.outline.hovered = None;
                }
                tx.scroll_into_view(target.clone());
            });
        if hover == Some(ix) {
            let mut card = col()
                .key("outline-card")
                .w(CARD_WIDTH)
                .gap(6.)
                .padding(12.)
                .radius(12.)
                .border(1., p.border)
                .fill(p.surface_overlay)
                .shadow(8., 24., rune::Color::BLACK.alpha(0.25))
                .transition(rune::Transition::opacity())
                .child(
                    text(prompt.label.clone())
                        .font(flow::type_at(s, 13., 18., 400.))
                        .color(p.text)
                        .wrap()
                        .max_lines(2),
                );
            if let Some(reply) = &prompt.reply {
                card = card.child(
                    text(reply.clone())
                        .font(flow::type_at(s, 13., 18., 400.))
                        .color(p.text_muted)
                        .wrap()
                        .max_lines(2),
                );
            }
            bar = bar.child(card.pin(None, Some(-24.), Some(BAR_WIDTH_LIFTED + 10.), None));
        }
        bars = bars.child(bar);
    }
    Some(
        col()
            .key("outline-rail")
            .pin(None, Some(44.), Some(14.), Some(150.))
            .justify_center()
            .child(bars),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stack_shrinks_the_gap_then_windows_on_the_newest() {
        let (s, e, gap) = bar_stack(5, None, 330.);
        assert_eq!((s, e, gap), (0, 5, BAR_GAP));
        let (s, e, _) = bar_stack(500, None, 330.);
        assert_eq!(e, 500);
        assert!(s > 0);
        let (s, _, _) = bar_stack(500, Some(3), 330.);
        assert_eq!(s, 3);
    }

    #[test]
    fn lift_tapers_across_the_ripple() {
        assert_eq!(bar_lift(4, Some(4)), 1.);
        assert!(bar_lift(5, Some(4)) > bar_lift(6, Some(4)));
        assert_eq!(bar_lift(8, Some(4)), 0.);
        assert_eq!(first_line("\n  hello   world \nsecond", 40), "hello world");
    }
}
