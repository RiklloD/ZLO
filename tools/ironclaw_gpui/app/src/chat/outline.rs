//! The prompt outline, after MonoCode's `transcript/outline.rs` (MIT, see
//! `THIRD_PARTY_NOTICES.md`): one short tick per message you sent, stacked
//! on the transcript's right edge. The tick for the part on screen is lit.
//! Pointing at the rail magnifies the ticks around the pointer like the
//! macOS Dock and shows the prompt with the start of its answer; a click
//! jumps there. Its own view, so scrolling redraws the ticks, not the
//! transcript.

use std::rc::Rc;

use gpui_kit::{Render, Subscription};

use crate::design::{Palette, motion};
use crate::ui::*;

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
/// Narrower windows keep the reading column clear.
const MIN_WIDTH: f32 = 900.;
const CARD_WIDTH: f32 = 288.;
/// MonoCode's bar transition: 200 ms ease-out.
const BAR: motion::MotionSpec = motion::MotionSpec::new(200, motion::EASE_OUT);

#[derive(Clone, PartialEq)]
pub(super) struct Prompt {
    /// The user row's index in the transcript list.
    pub item: usize,
    pub label: SharedString,
    pub reply: Option<SharedString>,
}

pub(super) struct Outline {
    id: u64,
    scroller: Entity<MessageScrollerState>,
    prompts: Rc<Vec<Prompt>>,
    hovered: Option<usize>,
    _scroll: Subscription,
}

impl Outline {
    pub(super) fn new(scroller: Entity<MessageScrollerState>, cx: &mut Context<Self>) -> Self {
        let scroll = cx.observe(&scroller, |_, _, cx| cx.notify());
        Self {
            id: cx.entity_id().as_u64(),
            scroller,
            prompts: Rc::new(Vec::new()),
            hovered: None,
            _scroll: scroll,
        }
    }

    pub(super) fn set(&mut self, prompts: Vec<Prompt>, cx: &mut Context<Self>) {
        if *self.prompts != prompts {
            self.prompts = Rc::new(prompts);
            cx.notify();
        }
    }
}

/// The first non-empty line, whitespace collapsed, at most `max` chars.
pub(super) fn first_line(text: &str, max: usize) -> SharedString {
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
    out.into()
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

impl Render for Outline {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = window.viewport_size();
        if self.prompts.len() < MIN_PROMPTS || f32::from(viewport.width) < MIN_WIDTH {
            return div().into_any_element();
        }
        let p = *Palette::of(cx);
        let reduced = cx.reduce_motion();
        let top = self.scroller.read(cx).logical_scroll_top().item_ix;
        let active = self.prompts.iter().rposition(|pr| pr.item <= top + 1);
        let budget = STACK_MAX.min((f32::from(viewport.height) * 0.6).floor());
        let (start, end, gap) = bar_stack(self.prompts.len(), active, budget);
        let hover = self.hovered.filter(|h| (start..end).contains(h));
        let ink = |a: f32| {
            if p.light {
                gpui_kit::black().opacity(a)
            } else {
                gpui_kit::white().opacity(a)
            }
        };

        let bars = (start..end).map(|ix| {
            let prompt = self.prompts[ix].clone();
            let lift = bar_lift(ix, hover);
            let lit = match hover {
                Some(h) => ix == h,
                None => Some(ix) == active,
            };
            let key = format!("outline:{}:{ix}", self.id);
            let width = motion::glide(
                &format!("{key}:w"),
                BAR_WIDTH + (BAR_WIDTH_LIFTED - BAR_WIDTH) * lift,
                BAR,
                reduced,
            );
            let alpha = motion::glide(
                &format!("{key}:a"),
                if lit {
                    BAR_OPACITY_LIT
                } else {
                    BAR_OPACITY_IDLE
                },
                BAR,
                reduced,
            );
            let item = prompt.item;
            let scroller = self.scroller.clone();
            div()
                .id(("outline-bar", ix))
                .role(Role::Button)
                .aria_label(prompt.label.clone())
                .relative()
                .flex()
                .w_full()
                .flex_none()
                .items_center()
                .justify_end()
                .h(px(BAR_HEIGHT + gap))
                .cursor_pointer()
                .child(
                    div()
                        .h(px(BAR_HEIGHT))
                        .w(px(width))
                        .rounded_full()
                        .bg(ink(alpha)),
                )
                .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                    if *hovered && this.hovered != Some(ix) {
                        this.hovered = Some(ix);
                        cx.notify();
                    }
                }))
                .on_click(move |_, _, cx| {
                    scroller.update(cx, |s, cx| {
                        s.scroll_to_item(item, cx);
                    });
                })
                .when(hover == Some(ix), |d| {
                    let card = v_flex()
                        .w(px(CARD_WIDTH))
                        .gap(px(6.))
                        .p(px(12.))
                        .rounded(px(12.))
                        .border_1()
                        .border_color(p.border)
                        .bg(p.surface_overlay)
                        .shadow_lg()
                        .text_size(gpui_kit::rems(13. / 16.))
                        .line_height(px(18.))
                        .child(
                            div()
                                .line_clamp(2)
                                .text_color(p.text)
                                .child(prompt.label.clone()),
                        )
                        .when_some(prompt.reply.clone(), |d, reply| {
                            d.child(div().line_clamp(2).text_color(p.text_muted).child(reply))
                        });
                    d.child(
                        div()
                            .absolute()
                            .right(px(BAR_WIDTH_LIFTED + 10.))
                            .top(px(-24.))
                            .child(motion::fade_in(
                                card,
                                SharedString::from(format!("{key}:card")),
                                motion::FADE_QUICK,
                                0.,
                            )),
                    )
                })
        });

        div()
            .absolute()
            .top(px(44.))
            .bottom(px(150.))
            .right(px(14.))
            .flex()
            .items_center()
            .child(
                v_flex()
                    .id("outline")
                    .role(Role::Toolbar)
                    .aria_label("Your messages")
                    .w(px(BAR_WIDTH_LIFTED))
                    .py_2()
                    .items_end()
                    .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                        if !*hovered && this.hovered.is_some() {
                            this.hovered = None;
                            cx.notify();
                        }
                    }))
                    .children(bars),
            )
            .into_any_element()
    }
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
        assert_eq!(
            first_line("\n  hello   world \nsecond", 40).as_ref(),
            "hello world"
        );
    }
}
