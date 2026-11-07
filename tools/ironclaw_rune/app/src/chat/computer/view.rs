//! The screen share's stack, cards, status bar and enlarged view (GPUI
//! `ComputerShare::render`, `render_card`, `overlay`, `render_permissions`).

use std::time::Instant;

use ironclaw_gpui_client::computer_use::{ComputerApp, ComputerSession};
use rune::{Color, Cx, El, LoopSpec, Shape, TextStyle, canvas, col, el, row, text};
use rune_kit::{ButtonVariant, Kit};

use super::{
    Card, ComputerState, RIPPLE, TYPING_MS, click_card, close_big, now_ms, permissions, slot, stop,
    take_over_or_back, toggle_hidden,
};
use crate::chat::ChatState;
use crate::design::kit::compact::{XS, ghost_button, ghost_icon, txt, variant_button};
use crate::design::{Palette, rgb};
use crate::icons;
use crate::state::State;

const CARD_W: f32 = 300.;
const STAGE_H: f32 = 200.;
const DECK_W: f32 = 560.;
const INDIGO: u32 = 0x6366f1;
const INDIGO_SOFT: u32 = 0x818cf8;
const AMBER: u32 = 0xf59e0b;

struct Status {
    dot: u32,
    pulse: bool,
    text: String,
}

fn status_of(cu: &ComputerState, s: &ComputerSession) -> Status {
    let st = |dot, pulse, text: String| Status { dot, pulse, text };
    if cu.paused() {
        return st(AMBER, false, "You have control. The agent is paused".into());
    }
    if s.stopping {
        return st(AMBER, false, "Stopping…".into());
    }
    if s.ended {
        return match &s.last_failure {
            Some(f) => st(0xef4444, false, format!("Ended · last problem: {}", f.label)),
            None => st(0x777777, false, "Done".into()),
        };
    }
    if s.inflight > 0 {
        let text = if s.label.is_empty() { "Working…".into() } else { s.label.clone() };
        return st(0x22c55e, true, text);
    }
    if s.ok == Some(false) {
        let why = match s.status.as_deref() {
            Some("refused") => "Refused",
            Some("unknown") => "Outcome unknown",
            _ => "Failed",
        };
        return st(0xef4444, false, format!("{why}: {}", s.label));
    }
    let last = if s.label.is_empty() { "—" } else { s.label.as_str() };
    st(INDIGO_SOFT, true, format!("Waiting for the agent · last: {last}"))
}

fn age_text(app: &ComputerApp) -> Option<String> {
    let age = app.capture.frame_age?.round() as i64;
    let text = match age {
        a if a <= 2 => "live".to_string(),
        a if a < 60 => format!("{a}s old"),
        a => format!("{}m old", a / 60),
    };
    Some(if app.capture.method.as_deref() == Some("snapshot") { format!("{text} · agent view") } else { text })
}

/// The picture's rectangle inside a stage of `w`×`h` (contain, anchored
/// top-centre), as (x, y, w, h).
fn content_rect(natural: (u32, u32), w: f32, h: f32) -> (f32, f32, f32, f32) {
    let (nw, nh) = (natural.0 as f32, natural.1 as f32);
    if nw <= 0. || nh <= 0. {
        return (0., 0., w, h);
    }
    let k = (w / nw).min(h / nh);
    let (cw, ch) = (nw * k, nh * k);
    ((w - cw) / 2., 0., cw, ch)
}

/// The cursor, ripples and drag trail over one card's picture, in the
/// card's own points.
fn overlay(card: &Card, rect: (f32, f32, f32, f32), scale: f32) -> Shape {
    let at = |f: (f32, f32)| [rect.0 + f.0 * rect.2, rect.1 + f.1 * rect.3];
    let mut shape = Shape::new();
    if let Some((a, b, _)) = card.trail {
        // A 4-on, 3-off dashed line.
        let (pa, pb) = (at(a), at(b));
        let len = ((pb[0] - pa[0]).powi(2) + (pb[1] - pa[1]).powi(2)).sqrt();
        if len > 0. {
            let dir = [(pb[0] - pa[0]) / len, (pb[1] - pa[1]) / len];
            let mut d = 0.;
            while d < len {
                let e = (d + 4.).min(len);
                let p0 = [pa[0] + dir[0] * d, pa[1] + dir[1] * d];
                let p1 = [pa[0] + dir[0] * e, pa[1] + dir[1] * e];
                shape = shape.stroke(vec![p0, p1], 2., rgb(INDIGO_SOFT));
                d += 7.;
            }
        }
    }
    let now = Instant::now();
    for r in card.ripples.iter().filter(|r| now >= r.starts) {
        let t = (now - r.starts).as_secs_f32() / RIPPLE.as_secs_f32();
        let radius = 15. * (0.3 + 1.4 * t.min(1.)) * scale;
        let alpha = (1. - t).clamp(0., 1.);
        let c = at(r.at);
        let steps = 24;
        let ring: Vec<[f32; 2]> = (0..=steps)
            .map(|i| {
                let a = i as f32 / steps as f32 * std::f32::consts::TAU;
                [c[0] + radius * a.cos(), c[1] + radius * a.sin()]
            })
            .collect();
        let color = rgb(if r.right { AMBER } else { INDIGO_SOFT }).alpha(alpha);
        shape = shape.stroke(ring, 2., color);
    }
    if let Some(g) = card.glide {
        let tip = at(g.at().0);
        let k = 0.9 * scale.max(0.6);
        let pt = |x: f32, y: f32| [tip[0] + (x - 4.) * k, tip[1] + (y - 2.5) * k];
        let arrow: Vec<[f32; 2]> =
            [(4., 2.5), (19.5, 11.5), (12.7, 13.), (9.1, 19.5)].iter().map(|(x, y)| pt(*x, *y)).collect();
        let mut outline = arrow.clone();
        outline.push(arrow[0]);
        shape = shape.fill(arrow, rgb(INDIGO)).stroke(outline, 1.7, Color::WHITE);
    }
    shape
}

#[derive(Clone, Copy)]
struct CardGeometry {
    w: f32,
    h: f32,
    scale: f32,
}

/// One card. `big` is the enlarged view: clicking it closes it.
fn card_view(
    id: &str,
    cu: &ComputerState,
    app: &ComputerApp,
    session: &ComputerSession,
    geometry: CardGeometry,
    front: bool,
    big: bool,
) -> El {
    let CardGeometry { w, h, scale } = geometry;
    let key = if big { "cu-big".to_string() } else { format!("cu-card-{}", app.window_id) };
    let card = cu.cards.get(&app.window_id);
    let natural = card.map_or((0, 0), |c| c.natural);
    let rect = content_rect(natural, w, h);
    let has_image = card.is_some_and(|c| c.has_image);
    let note = if cu.paused() {
        Some("Paused while you have control".to_string())
    } else if !has_image {
        card.and_then(|c| c.refusal.clone())
            .or_else(|| app.capture.error.clone().map(|e| format!("No picture: {e}")))
            .or_else(|| Some("Connecting…".into()))
    } else {
        None
    };
    let typing = session
        .typing
        .as_ref()
        .filter(|t| t.window_id == Some(app.window_id) && now_ms() - cu.skew_ms - t.at < TYPING_MS);
    let cursor = card.and_then(|c| c.glide).map(|g| g.at().0);
    let window = app.window_id;
    let tag = if app.app.is_empty() { app.title.clone() } else { app.app.clone() };
    let old = app.capture.frame_age.is_some_and(|a| a > 4.) && session.inflight > 0;
    let running = front && session.inflight > 0;
    let radius = 10. * scale.max(0.7);

    let mut stage = el().w(w).h(h);
    if let Some(card) = card {
        if card.has_image {
            stage = stage.child(
                el().pin(Some(rect.0), Some(rect.1), None, None).size(rect.2, rect.3).pixels(&card.pixels),
            );
        }
        stage = stage.child(canvas(overlay(card, rect, scale)).pin(Some(0.), Some(0.), None, None).size(w, h));
    }
    if let Some(note) = note {
        stage = stage.child(
            el().pin(Some(0.), Some(0.), Some(0.), Some(0.))
                .center()
                .padding(12.)
                .fill(Color::BLACK.alpha(0.55))
                .child(txt(note, XS, rgb(0xaaaaaa)).wrap().align(rune::Align::Center)),
        );
    }
    stage = stage.child(
        el().pin(Some(6.), Some(6.), None, None)
            .px(7.)
            .radius(99.)
            .fill(if front { rgb(INDIGO).alpha(0.92) } else { rgb(0x3c3c3c).alpha(0.9) })
            .max_w(w * 0.7)
            .child(text(tag).font(TextStyle::new(10.5, 600.)).color(Color::WHITE).truncate()),
    );
    if let Some(age) = age_text(app).filter(|_| front) {
        stage = stage.child(
            el().pin(None, Some(6.), Some(6.), None)
                .px(6.)
                .radius(99.)
                .fill(Color::BLACK.alpha(0.6))
                .child(txt(age, 10., if old { rgb(AMBER) } else { rgb(0xcccccc) })),
        );
    }
    if let Some(t) = typing {
        // On the pointer when there is one, else along the bottom.
        let (x, y) = match cursor {
            Some(c) => (rect.0 + c.0 * rect.2 + 14., rect.1 + c.1 * rect.3 + 20.),
            None => (rect.0 + 8., rect.1 + rect.3 - 28.),
        };
        stage = stage.child(
            el().pin(Some(x.min(w - 60.)), Some(y.min(h - 20.)), None, None)
                .px(7.)
                .py(2.)
                .radius(6.)
                .fill(Color::BLACK.alpha(0.8))
                .max_w(240.)
                .child(txt(format!("⌨ {}", t.text), 10.5, Color::WHITE).truncate()),
        );
    }
    let mut out = el()
        .key(key)
        .w(w)
        .h(h)
        .radius(radius)
        .clip_rounded(radius)
        .fill(rgb(0x0d0d0d))
        .border(1., if running { rgb(INDIGO).alpha(0.75) } else { Color::WHITE.alpha(0.14) })
        .shadow(10., 30., Color::BLACK.alpha(0.35))
        .cursor(rune::Cursor::Pointer)
        .role("button")
        .label(format!("{} window", app.app))
        .on_press({
            let id = id.to_string();
            move |s: &mut State| if big { close_big(s, &id) } else { click_card(s, &id, window, front) }
        });
    if front {
        out = out.child(stage);
    } else {
        out = out.hover_group().child(el().group_opacity(0.7, 0.95).child(stage));
    }
    out
}

/// Computer use cannot act until ZLO has both permissions; say so before
/// the agent tries, with one button that asks macOS.
fn permissions_card(p: &Palette, id: &str, cu: &ComputerState, k: &Kit) -> El {
    let (allow_id, later_id) = (id.to_string(), id.to_string());
    let names: Vec<&str> = cu.missing.iter().map(|m| m.label()).collect();
    let screen = cu.missing.contains(&permissions::Permission::ScreenRecording);
    col()
        .key("cu-permissions")
        .pin(None, Some(12.), Some(12.), None)
        .w(CARD_W + 40.)
        .gap(8.)
        .padding(12.)
        .radius(8.)
        .fill(p.surface_overlay)
        .border(1., p.border)
        .child(txt(format!("ZLO needs {} to use apps for you.", names.join(" and ")), XS, p.text).wrap())
        .child(
            txt(
                if screen {
                    "Click Allow, switch ZLO Rune on in System Settings, then reopen ZLO Rune."
                } else {
                    "Click Allow, then switch ZLO Rune on in System Settings."
                },
                XS,
                p.text_faint,
            )
            .wrap(),
        )
        .child(
            row()
                .gap(8.)
                .child(variant_button(k, "cu-allow", "Allow", None, ButtonVariant::Submit, false, move |s| {
                    if let Some(cu) = slot(s, &allow_id) {
                        permissions::request(&cu.missing);
                    }
                }))
                .child(ghost_button(k, "cu-later", "Later", None, false, move |s| {
                    if let Some(cu) = slot(s, &later_id) {
                        cu.permissions_hidden = true;
                    }
                })),
        )
}

/// The stack, pinned to the top right of its parent; the enlarged card
/// goes into `layer`.
pub fn view(s: &State, chat: &ChatState, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> Option<El> {
    let cu = &chat.computer;
    let id = chat.thread_id.as_str();
    let p = &s.palette;
    if !cu.missing.is_empty() && !cu.permissions_hidden {
        return Some(permissions_card(p, id, cu, k));
    }
    let session = cu.session.as_ref().filter(|_| !cu.hidden)?;
    let status = status_of(cu, session);
    let apps = cu.apps();
    let mut deck = el().key("cu-deck").w(DECK_W).h(STAGE_H + 30.);
    for (ix, app) in apps.iter().enumerate().rev() {
        let scale = 0.82f32.powi(ix as i32);
        let (w, h) = (CARD_W * scale, STAGE_H * scale);
        let right = ix as f32 * 110.;
        let top = STAGE_H / 2. + ix as f32 * 10. - h / 2.;
        let geometry = CardGeometry { w, h, scale };
        deck = deck.child(
            card_view(id, cu, app, session, geometry, ix == 0, false).pin(None, Some(top), Some(right), None),
        );
    }
    let mut dot = el().size(7., 7.).radius(3.5).fill(rgb(status.dot));
    if status.pulse {
        dot = dot.looping(LoopSpec::opacity(1.1, 0., 0.3, 1.));
    }
    let held_elsewhere = session.human_has_control && !cu.mine;
    let mut bar = row()
        .key("cu-bar")
        .self_end()
        .w(CARD_W)
        .mt(8.)
        .gap(6.)
        .center_y()
        .pl(10.)
        .pr(6.)
        .py(5.)
        .radius(8.)
        .fill(p.surface_overlay)
        .border(1., p.border)
        .child(dot)
        .child(txt(cu.note.clone().unwrap_or(status.text), XS, p.text).truncate().grow(1.).min_w(0.));
    if !session.ended {
        let stop_id = id.to_string();
        bar = bar.child(
            ghost_button(k, "cu-stop", "Stop", Some(&icons::SQUARE), false, move |s| stop(s, &stop_id))
                .help("Stop the agent now (also stops this conversation's turn)"),
        );
    }
    if !held_elsewhere {
        let take_id = id.to_string();
        let take = ghost_button(k, "cu-take", if cu.mine { "Hand back" } else { "Take over" }, None, false, move |s| {
            take_over_or_back(s, &take_id)
        })
            .help(if cu.mine {
                "Give the computer back to the agent"
            } else {
                "Pause the agent and use the computer yourself until you hand it back"
            });
        bar = bar.child(if cu.mine { el().radius(6.).fill(p.active).child(take) } else { take });
    }
    let hide_id = id.to_string();
    bar = bar.child(ghost_icon(k, "cu-hide", &icons::CLOSE, "Hide the screen share (stops capturing)", move |s| {
        toggle_hidden(s, &hide_id)
    }));

    if let Some(app) = cu.big.and_then(|w| apps.iter().find(|a| a.window_id == w)) {
        let [vw, vh] = cx.layout_size("kit-root").unwrap_or([1280., 820.]);
        let w = (vw * 0.8).min(1100.);
        let h = (vh * 0.75).min(760.);
        let geometry = CardGeometry { w, h, scale: w / CARD_W };
        layer.push(
            el().key("cu-big-veil")
                .absolute(0., 0.)
                .w_full()
                .h_full()
                .center()
                .fill(Color::BLACK.alpha(0.6))
                .on_press({
                    let id = id.to_string();
                    move |s: &mut State| close_big(s, &id)
                })
                .child(card_view(id, cu, app, session, geometry, true, true)),
        );
    }
    Some(
        col()
            .key("cu-stack")
            .pin(None, Some(52.), Some(16.), None)
            .w(DECK_W)
            .child(deck)
            .child(bar),
    )
}
