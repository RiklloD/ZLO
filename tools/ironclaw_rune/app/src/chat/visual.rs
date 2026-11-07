//! A reply's `visual` blocks drawn natively (GPUI `chat/visual.rs`): bar,
//! line, area and pie charts, a table and stat tiles, colored from the
//! theme's accent. The prose around them stays Markdown. The spec and its
//! checks live in `ironclaw_gpui_client::visual`.
//!
//! GPUI used gpui-component's charts; Rune has none, so the charts here are
//! drawn with Rune shapes (`canvas`): the same data, axes in the same
//! compact number format, and each value's exact figure as the mark's help
//! tooltip.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use ironclaw_gpui_client::visual::{self, Body, Segment, Series, Visual, compact, exact};
use rune::{Color, El, LineCap, Path, Shape, canvas, col, el, row, text};
use rune_kit::{Kit, Markdown, MarkdownStyle};

use super::{actions, file_links, flow};
use crate::design::Palette;
use crate::state::State;

const CHART_HEIGHT: f32 = 220.;
const PIE_HEIGHT: f32 = 180.;
/// Hue step between series, about 47°: six series stay apart.
const HUE_STEP: f32 = 0.13;
const CACHE_LIMIT: usize = 256;
/// The left gutter of a chart's value axis.
const AXIS_WIDTH: f32 = 44.;

thread_local! {
    /// Finished replies by text, so a view run doesn't parse the specs again.
    static CACHE: RefCell<HashMap<String, Rc<Vec<Segment>>>> = RefCell::default();
}

/// An assistant reply: Markdown, with each visual block drawn in its place.
pub(crate) fn reply(s: &State, thread: &str, key: &str, text: &str, streaming: bool, k: &Kit) -> El {
    let segments = segments(text, streaming);
    if let [Segment::Text(_)] = segments.as_slice() {
        return prose(s, thread, key, text, streaming, k);
    }
    let last = segments.len().saturating_sub(1);
    let mut out = col().key(format!("{key}-parts")).w_full().gap(12.);
    for (i, segment) in segments.iter().enumerate() {
        // The first part keeps the row's id, so streamed text stays put.
        let id = match i {
            0 => key.to_string(),
            _ => format!("{key}-v{i}"),
        };
        out = out.child(match segment {
            Segment::Text(text) => prose(s, thread, &id, text, streaming && i == last, k),
            Segment::Visual(v) => card(s, &id, v),
            Segment::Invalid { error, source } => invalid(s, thread, &id, error, source, k),
            Segment::Pending => pending(s, &id, k),
        });
    }
    out
}

fn segments(text: &str, streaming: bool) -> Rc<Vec<Segment>> {
    if streaming {
        return Rc::new(visual::split(text, true));
    }
    if let Some(hit) = CACHE.with(|c| c.borrow().get(text).cloned()) {
        return hit;
    }
    let parts = Rc::new(visual::split(text, false));
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= CACHE_LIMIT {
            c.clear();
        }
        c.insert(text.to_string(), parts.clone());
    });
    parts
}

/// The reply's Markdown style: Zeron's 14 px reading type on a 22 px line.
pub(crate) fn markdown_style(s: &State, k: &Kit) -> MarkdownStyle {
    MarkdownStyle::from_kit(k).body_font(flow::type_at(s, 14., 22., 400.))
}

/// Markdown that streams in place; links open in the app (⌘ outside it).
pub(crate) fn prose(s: &State, thread: &str, id: &str, text: &str, streaming: bool, k: &Kit) -> El {
    // Mid-stream text only grows; paths link once it settles.
    let text = if streaming {
        text.to_string()
    } else {
        file_links::link_paths(text)
    };
    let thread = thread.to_string();
    Markdown::new(text)
        .style(markdown_style(s, k))
        .streaming(streaming)
        .reveal(if streaming { rune::edge::WORD_FADE.word_fade } else { 0. })
        .selectable(true)
        .on_link_tx(move |s: &mut State, href: &str, tx: &mut rune::Tx| {
            let external = tx.mods().cmd;
            actions::open_link(s, &thread, href, external, tx);
        })
        .build(k)
        .key(id.to_string())
}

fn card_frame(s: &State, key: String) -> El {
    let p = &s.palette;
    col().key(key)
        .w_full()
        .gap(8.)
        .padding(12.)
        .radius(12.)
        .border(1., p.border)
        .fill(p.surface_card)
}

fn pending(s: &State, id: &str, k: &Kit) -> El {
    let p = &s.palette;
    card_frame(s, format!("{id}-pending")).child(
        row()
            .gap(8.)
            .center_y()
            .child(rune_kit::Spinner::new().size(12.).build(k))
            .child(
                text("Drawing visual…")
                    .font(flow::type_at(s, 12., 16., 400.))
                    .color(p.text_muted),
            ),
    )
}

/// A spec that doesn't hold: why, then the block as the agent wrote it.
fn invalid(s: &State, thread: &str, id: &str, error: &str, source: &str, k: &Kit) -> El {
    let p = &s.palette;
    col().key(format!("{id}-invalid"))
        .gap(4.)
        .child(
            text(format!("Couldn't draw this visual: {error}"))
                .font(flow::type_at(s, 12., 16., 400.))
                .color(p.warning)
                .wrap(),
        )
        .child(prose(s, thread, id, &format!("```json\n{source}\n```"), false, k))
}

fn card(s: &State, id: &str, v: &Visual) -> El {
    let p = &s.palette;
    let mut frame = card_frame(s, format!("{id}-card"));
    if let Some(title) = &v.title {
        frame = frame.child(
            text(title.clone())
                .font(flow::type_at(s, 13., 18., 600.))
                .color(p.text),
        );
    }
    if let Some(subtitle) = &v.subtitle {
        frame = frame.child(
            text(subtitle.clone())
                .font(flow::type_at(s, 12., 16., 400.))
                .color(p.text_muted),
        );
    }
    frame.child(body(s, id, v))
}

/// Series `i`'s colour: the accent turned by `i` hue steps.
fn color(p: &Palette, i: usize) -> Color {
    let [l, a, b, alpha] = p.accent.to_oklab();
    let turn = i as f32 * HUE_STEP * std::f32::consts::TAU;
    let (sin, cos) = turn.sin_cos();
    Color::from_oklab([l, a * cos - b * sin, a * sin + b * cos, alpha])
}

fn body(s: &State, id: &str, v: &Visual) -> El {
    let p = &s.palette;
    let unit = v.unit.as_deref();
    match &v.body {
        Body::Bar { x, series } => bars(s, id, x, series, unit),
        Body::Line { x, series } if series.len() == 1 => plot(s, id, x, series, unit, false),
        Body::Line { x, series } => with_legend(s, plot(s, id, x, series, unit, false), series),
        Body::Area { x, series } => {
            let chart = plot(s, id, x, series, unit, true);
            match series.len() {
                1 => chart,
                _ => with_legend(s, chart, series),
            }
        }
        Body::Pie { x, series } => pie(s, id, x, series, unit),
        Body::Table { columns, rows } => table(s, columns, rows),
        Body::Stats(items) => {
            let mut tiles = row().w_full().flex_wrap().gap(8.);
            for (i, stat) in items.iter().enumerate() {
                let delta_color = match stat.delta_good() {
                    Some(true) => p.success,
                    Some(false) => p.danger,
                    None => p.text_muted,
                };
                let mut tile = col()
                    .key(format!("{id}-stat-{i}"))
                    .grow(1.)
                    .min_w(120.)
                    .gap(2.)
                    .padding(12.)
                    .radius(8.)
                    .fill(p.hover)
                    .child(
                        text(stat.label.clone())
                            .font(flow::type_at(s, 12., 16., 400.))
                            .color(p.text_muted),
                    )
                    .child(
                        text(stat.value.clone())
                            .font(flow::type_at(s, 20., 28., 600.))
                            .color(p.text),
                    );
                if let Some(delta) = &stat.delta {
                    tile = tile.child(
                        text(delta.clone())
                            .font(flow::type_at(s, 12., 16., 400.))
                            .color(delta_color),
                    );
                }
                tiles = tiles.child(tile);
            }
            tiles
        }
    }
}

/// The value range a chart's axis spans: from 0 (or the lowest value, when
/// negative) to the highest.
fn span(values: impl Iterator<Item = f64>) -> (f64, f64) {
    let (mut lo, mut hi) = (0f64, 0f64);
    for v in values.filter(|v| v.is_finite()) {
        lo = lo.min(v);
        hi = hi.max(v);
    }
    if (hi - lo).abs() < f64::EPSILON {
        hi = lo + 1.;
    }
    (lo, hi)
}

/// The value axis: the top, middle and bottom figures.
fn axis(s: &State, lo: f64, hi: f64, unit: Option<&str>, height: f32) -> El {
    let p = &s.palette;
    let label = |v: f64| {
        text(compact(v, unit))
            .font(flow::type_at(s, 10., 12., 400.))
            .color(p.text_faint)
    };
    col().w(AXIS_WIDTH)
        .h(height)
        .justify_between()
        .pr(6.)
        .items_end()
        .child(label(hi))
        .child(label((lo + hi) / 2.))
        .child(label(lo))
}

/// The x labels under a chart: first, middle and last.
fn x_labels(s: &State, x: &[String]) -> El {
    let p = &s.palette;
    let pick: Vec<&String> = match x.len() {
        0 => Vec::new(),
        1 | 2 => x.iter().collect(),
        n => vec![&x[0], &x[n / 2], &x[n - 1]],
    };
    let mut out = row().pl(AXIS_WIDTH).w_full().justify_between();
    for l in pick {
        out = out.child(
            text(l.clone())
                .font(flow::type_at(s, 10., 12., 400.))
                .color(p.text_faint)
                .truncate()
                .max_w(140.),
        );
    }
    out
}

fn bars(s: &State, id: &str, x: &[String], series: &Series, unit: Option<&str>) -> El {
    let p = &s.palette;
    let (lo, hi) = span(series.values.iter().copied());
    let fill = color(p, 0);
    let plot_h = CHART_HEIGHT - 20.;
    let mut columns = row().key(format!("{id}-bars")).grow(1.).h(plot_h).gap(4.).items_end();
    for (i, (label, value)) in x.iter().zip(series.values.iter()).enumerate() {
        let h = ((value.max(0.) - lo.max(0.)) / (hi - lo)) as f32 * plot_h;
        columns = columns.child(
            col().key(format!("{id}-bar-{i}"))
                .grow(1.)
                .h_full()
                .justify_end()
                .help(format!("{label} · {}: {}", series.name, exact(*value, unit)))
                .child(el().w_full().h(h.max(1.)).radius(3.).fill(fill)),
        );
    }
    col().w_full()
        .h(CHART_HEIGHT)
        .child(row().w_full().child(axis(s, lo, hi, unit, plot_h)).child(columns))
        .child(x_labels(s, x))
}

/// Line or area series over one x axis; several series share the axis.
fn plot(s: &State, id: &str, x: &[String], series: &[Series], unit: Option<&str>, filled: bool) -> El {
    let p = &s.palette;
    let (lo, hi) = span(series.iter().flat_map(|s| s.values.iter().copied()));
    let plot_h = CHART_HEIGHT - 20.;
    // Drawn in a 1000-wide view box, stretched to the chart's width.
    let width = 1000.;
    let n = x.len().max(1);
    let at = |i: usize, v: f64| {
        let px = if n == 1 {
            width / 2.
        } else {
            i as f32 / (n - 1) as f32 * width
        };
        let py = plot_h - ((v - lo) / (hi - lo)) as f32 * plot_h;
        [px, py]
    };
    let mut shape = Shape::new();
    for (si, s_) in series.iter().enumerate() {
        let c = color(p, si);
        let points: Vec<[f32; 2]> = s_.values.iter().enumerate().map(|(i, v)| at(i, *v)).collect();
        if filled && points.len() > 1 {
            let mut area = points.clone();
            area.push([points[points.len() - 1][0], plot_h]);
            area.push([points[0][0], plot_h]);
            shape = shape.fill(area, c.alpha(0.18));
        }
        if points.len() > 1 {
            let mut path = Path::new().move_to(points[0]);
            for pt in &points[1..] {
                path = path.line_to(*pt);
            }
            shape = shape.stroke_path(&path, 2., LineCap::Round, c);
        }
    }
    let line = canvas(shape.view_box(width, plot_h)).key(format!("{id}-plot")).grow(1.).h(plot_h);
    col().w_full()
        .h(CHART_HEIGHT)
        .help(
            series
                .iter()
                .map(|s_| {
                    let last = s_.values.last().copied().unwrap_or_default();
                    format!("{}: {}", s_.name, exact(last, unit))
                })
                .collect::<Vec<_>>()
                .join(" · "),
        )
        .child(row().w_full().child(axis(s, lo, hi, unit, plot_h)).child(line))
        .child(x_labels(s, x))
}

fn pie(s: &State, id: &str, x: &[String], series: &Series, unit: Option<&str>) -> El {
    let p = &s.palette;
    let total: f64 = series.values.iter().filter(|v| **v > 0.).sum();
    let (cx, cy, outer) = (PIE_HEIGHT / 2., PIE_HEIGHT / 2., PIE_HEIGHT / 2.);
    let inner = PIE_HEIGHT * 0.25;
    let mut shape = Shape::new();
    let mut start = -std::f32::consts::FRAC_PI_2;
    let mut legend = col().grow(1.).min_w(0.).gap(4.);
    for (i, (label, value)) in x.iter().zip(series.values.iter()).enumerate() {
        let share = if total > 0. { value.max(0.) / total } else { 0. };
        let sweep = share as f32 * std::f32::consts::TAU;
        // A small gap between wedges, as gpui-component's pad angle.
        let pad = if sweep > 0.04 { 0.01 } else { 0. };
        let (a0, a1) = (start + pad, start + sweep - pad);
        if a1 > a0 {
            let steps = ((a1 - a0) / 0.05).ceil().max(2.) as usize;
            let mut ring = Vec::with_capacity(steps * 2 + 2);
            for k in 0..=steps {
                let a = a0 + (a1 - a0) * k as f32 / steps as f32;
                ring.push([cx + outer * a.cos(), cy + outer * a.sin()]);
            }
            for k in (0..=steps).rev() {
                let a = a0 + (a1 - a0) * k as f32 / steps as f32;
                ring.push([cx + inner * a.cos(), cy + inner * a.sin()]);
            }
            shape = shape.fill(ring, color(p, i));
        }
        start += sweep;
        let note = (total > 0.).then(|| format!("{}%", exact(share * 100., None)));
        legend = legend.child(
            swatch(s, color(p, i), label.clone(), note)
                .key(format!("{id}-pie-{i}"))
                .help(format!("{label}: {}", exact(*value, unit))),
        );
    }
    row().w_full()
        .gap(16.)
        .center_y()
        .child(canvas(shape).key(format!("{id}-pie")).size(PIE_HEIGHT, PIE_HEIGHT))
        .child(legend)
}

fn with_legend(s: &State, chart: El, series: &[Series]) -> El {
    let p = &s.palette;
    let mut keys = row().flex_wrap().gap(12.);
    for (i, s_) in series.iter().enumerate() {
        keys = keys.child(swatch(s, color(p, i), s_.name.clone(), None).key(format!("legend-{i}")));
    }
    col().w_full().gap(8.).child(chart).child(keys)
}

fn swatch(s: &State, c: Color, label: String, note: Option<String>) -> El {
    let p = &s.palette;
    let small = flow::type_at(s, 12., 16., 400.);
    let mut out = row()
        .gap(6.)
        .center_y()
        .child(el().size(8., 8.).radius(4.).fill(c))
        .child(text(label).font(small).color(p.text).truncate().min_w(0.));
    if let Some(note) = note {
        out = out.child(text(note).font(small).color(p.text_muted));
    }
    out
}

fn table(s: &State, columns: &[String], rows: &[Vec<String>]) -> El {
    let p = &s.palette;
    let line = |key: String, cells: &[String], head: bool| {
        let font = flow::type_at(s, 12., 16., if head { 500. } else { 400. });
        let mut r = row()
            .key(key)
            .w_full()
            .py(6.)
            .border(0., p.border);
        for (i, c) in cells.iter().enumerate() {
            r = r.child(
                text(c.clone())
                    .key(format!("c{i}"))
                    .font(font)
                    .color(if head { p.text_muted } else { p.text })
                    .grow(1.)
                    .min_w(0.)
                    .px(8.)
                    .wrap()
                    .selectable(true),
            );
        }
        col().w_full()
            .child(r)
            .child(el().w_full().h(1.).fill(p.border))
    };
    let mut out = col().w_full().child(line("head".into(), columns, true));
    for (i, r) in rows.iter().enumerate() {
        out = out.child(line(format!("r{i}"), r, false));
    }
    out
}
