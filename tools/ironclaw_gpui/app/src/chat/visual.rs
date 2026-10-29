//! A reply's `visual` blocks drawn natively: gpui-component's charts, a
//! table and stat tiles, colored from the theme's accent. The prose around
//! them stays Markdown. The spec and its checks live in
//! `ironclaw_gpui_client::visual`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use ironclaw_gpui_client::visual::{self, Body, Segment, Series, Visual, compact, exact};

use super::{ChatView, file_links, linked};
use crate::design::Palette;
use crate::ui::*;

const CHART_HEIGHT: f32 = 220.;
const PIE_HEIGHT: f32 = 180.;
/// Hue step between series, about 47°: six series stay apart.
const HUE_STEP: f32 = 0.13;
const CACHE_LIMIT: usize = 256;

thread_local! {
    /// Finished replies by text, so a frame doesn't parse the specs again.
    static CACHE: RefCell<HashMap<String, Rc<Vec<Segment>>>> = RefCell::default();
}

/// An assistant reply: Markdown, with each visual block drawn in its place.
pub(super) fn reply(
    key: &str,
    text: &str,
    streaming: bool,
    weak: &WeakEntity<ChatView>,
    cx: &mut App,
) -> AnyElement {
    let segments = segments(text, streaming);
    if let [Segment::Text(_)] = segments.as_slice() {
        return prose(key, text, streaming, weak, cx);
    }
    let last = segments.len().saturating_sub(1);
    v_flex()
        .w_full()
        .gap_3()
        .children(segments.iter().enumerate().map(|(i, segment)| {
            // The first part keeps the row's id, so streamed text stays put.
            let id = match i {
                0 => key.to_string(),
                _ => format!("{key}-v{i}"),
            };
            match segment {
                Segment::Text(text) => prose(&id, text, streaming && i == last, weak, cx),
                Segment::Visual(v) => card(&id, v, cx),
                Segment::Invalid { error, source } => invalid(&id, error, source, weak, cx),
                Segment::Pending => pending(cx),
            }
        }))
        .into_any_element()
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

fn prose(
    id: &str,
    text: &str,
    streaming: bool,
    weak: &WeakEntity<ChatView>,
    cx: &mut App,
) -> AnyElement {
    let text = SharedString::from(text.to_string());
    // Mid-stream text only grows; paths link once it settles.
    let text = match streaming {
        true => text,
        false => file_links::link_paths(&text),
    };
    linked(
        kept_markdown(id.to_string(), text, streaming, weak.entity_id(), cx),
        weak,
    )
    .into_any_element()
}

fn pending(cx: &App) -> AnyElement {
    let p = Palette::of(cx);
    crate::ui::card(cx)
        .child(
            h_flex()
                .gap_2()
                .text_xs()
                .text_color(p.text_muted)
                .child(Spinner::new().xsmall())
                .child("Drawing visual…"),
        )
        .into_any_element()
}

/// A spec that doesn't hold: why, then the block as the agent wrote it.
fn invalid(
    id: &str,
    error: &str,
    source: &str,
    weak: &WeakEntity<ChatView>,
    cx: &mut App,
) -> AnyElement {
    let p = Palette::of(cx);
    v_flex()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(p.warning)
                .child(format!("Couldn't draw this visual: {error}")),
        )
        .child(prose(
            id,
            &format!("```json\n{source}\n```"),
            false,
            weak,
            cx,
        ))
        .into_any_element()
}

fn card(id: &str, v: &Visual, cx: &App) -> AnyElement {
    let p = Palette::of(cx);
    crate::ui::card(cx)
        .w_full()
        .when_some(v.title.clone(), |this, title| {
            this.child(
                div()
                    .text_sm()
                    .font_semibold()
                    .text_color(p.text)
                    .child(title),
            )
        })
        .when_some(v.subtitle.clone(), |this, subtitle| {
            this.child(div().text_xs().text_color(p.text_muted).child(subtitle))
        })
        .child(body(id, v, p))
        .into_any_element()
}

fn color(p: &Palette, i: usize) -> Hsla {
    let a = p.accent;
    Hsla {
        h: (a.h + i as f32 * HUE_STEP).fract(),
        ..a
    }
}

fn body(id: &str, v: &Visual, p: &Palette) -> AnyElement {
    let unit = v.unit.clone();
    let chart_id = SharedString::from(format!("{id}-chart"));
    match &v.body {
        Body::Bar { x, series } => {
            let data: Vec<(String, f64)> = points(x, series);
            let fill = color(p, 0);
            let (ticks, tips) = (unit.clone(), unit);
            plot(
                CHART_HEIGHT,
                BarChart::new(data)
                    .id(chart_id)
                    .appear(false)
                    .name(series.name.clone())
                    .band(|d: &(String, f64)| d.0.clone())
                    .value(|d| d.1)
                    .fill(move |_, _, _, _| fill)
                    .value_axis(true)
                    .value_tick_format(move |v| compact(v, ticks.as_deref()))
                    .tooltip_value(move |_, v| exact(v, tips.as_deref()).into()),
            )
        }
        Body::Line { x, series } if series.len() == 1 => {
            let series = &series[0];
            let (ticks, tips) = (unit.clone(), unit);
            plot(
                CHART_HEIGHT,
                LineChart::new(points(x, series))
                    .id(chart_id)
                    .appear(false)
                    .name(series.name.clone())
                    .x(|d: &(String, f64)| d.0.clone())
                    .y(|d| d.1)
                    .stroke(color(p, 0))
                    .dot()
                    .y_axis(true)
                    .y_tick_format(move |v| compact(v, ticks.as_deref()))
                    .tooltip_value(move |_, v| exact(v, tips.as_deref()).into()),
            )
        }
        Body::Line { x, series } => {
            with_legend(area(&chart_id, x, series, unit, false, p), series, p)
        }
        Body::Area { x, series } => {
            let chart = area(&chart_id, x, series, unit, true, p);
            match series.len() {
                1 => chart,
                _ => with_legend(chart, series, p),
            }
        }
        Body::Pie { x, series } => {
            let data: Vec<(String, f64, usize)> = x
                .iter()
                .cloned()
                .zip(series.values.iter().copied())
                .enumerate()
                .map(|(i, (label, value))| (label, value, i))
                .collect();
            let total: f64 = series.values.iter().sum();
            let colors: Vec<Hsla> = (0..data.len()).map(|i| color(p, i)).collect();
            let legend = data
                .iter()
                .map(|(label, value, i)| {
                    let share = match total > 0. {
                        true => format!("{}%", exact(value / total * 100., None)),
                        false => String::new(),
                    };
                    swatch(colors[*i], label.clone(), Some(share), p)
                })
                .collect::<Vec<_>>();
            let tips = unit;
            let chart = PieChart::new(data)
                .id(chart_id)
                .appear(false)
                .value(|d: &(String, f64, usize)| d.1 as f32)
                .color(move |d| colors[d.2])
                .inner_radius(PIE_HEIGHT * 0.25)
                .pad_angle(0.02)
                .tooltip_name(|d| d.0.clone().into())
                .tooltip_value(move |d, _, share| {
                    format!(
                        "{} ({}%)",
                        exact(d.1, tips.as_deref()),
                        exact(share as f64, None)
                    )
                    .into()
                });
            h_flex()
                .w_full()
                .gap_4()
                .items_center()
                .child(
                    div()
                        .w(px(PIE_HEIGHT))
                        .h(px(PIE_HEIGHT))
                        .flex_none()
                        .child(chart),
                )
                .child(v_flex().flex_1().min_w_0().gap_1().children(legend))
                .into_any_element()
        }
        Body::Table { columns, rows } => table(columns, rows, p),
        Body::Stats(items) => h_flex()
            .w_full()
            .flex_wrap()
            .gap_2()
            .children(items.iter().map(|s| {
                let delta_color = match s.delta_good() {
                    Some(true) => p.success,
                    Some(false) => p.danger,
                    None => p.text_muted,
                };
                v_flex()
                    .flex_1()
                    .min_w(px(120.))
                    .gap_0p5()
                    .p_3()
                    .rounded(px(8.))
                    .bg(p.hover)
                    .child(
                        div()
                            .text_xs()
                            .text_color(p.text_muted)
                            .child(s.label.clone()),
                    )
                    .child(
                        div()
                            .text_xl()
                            .font_semibold()
                            .text_color(p.text)
                            .child(s.value.clone()),
                    )
                    .when_some(s.delta.clone(), |this, delta| {
                        this.child(div().text_xs().text_color(delta_color).child(delta))
                    })
            }))
            .into_any_element(),
    }
}

fn points(x: &[String], series: &Series) -> Vec<(String, f64)> {
    x.iter()
        .cloned()
        .zip(series.values.iter().copied())
        .collect()
}

/// Several series over one x axis. A line chart draws them unfilled.
fn area(
    id: &SharedString,
    x: &[String],
    series: &[Series],
    unit: Option<String>,
    filled: bool,
    p: &Palette,
) -> AnyElement {
    let data: Vec<(String, Vec<f64>)> = x
        .iter()
        .enumerate()
        .map(|(i, label)| (label.clone(), series.iter().map(|s| s.values[i]).collect()))
        .collect();
    let (ticks, tips) = (unit.clone(), unit);
    let mut chart = AreaChart::new(data)
        .id(id.clone())
        .appear(false)
        .x(|d: &(String, Vec<f64>)| d.0.clone())
        .y_axis(true)
        .y_tick_format(move |v| compact(v, ticks.as_deref()))
        .tooltip_value(move |_, _, v| exact(v, tips.as_deref()).into());
    for (i, s) in series.iter().enumerate() {
        let c = color(p, i);
        chart = chart
            .y(move |d| d.1[i])
            .stroke(c)
            .fill(match filled {
                true => c.opacity(0.18),
                false => c.opacity(0.),
            })
            .name(s.name.clone());
    }
    plot(CHART_HEIGHT, chart)
}

fn plot(height: f32, chart: impl IntoElement) -> AnyElement {
    div().w_full().h(px(height)).child(chart).into_any_element()
}

fn with_legend(chart: AnyElement, series: &[Series], p: &Palette) -> AnyElement {
    v_flex()
        .w_full()
        .gap_2()
        .child(chart)
        .child(
            h_flex().flex_wrap().gap_3().children(
                series
                    .iter()
                    .enumerate()
                    .map(|(i, s)| swatch(color(p, i), s.name.clone(), None, p)),
            ),
        )
        .into_any_element()
}

fn swatch(c: Hsla, label: String, note: Option<String>, p: &Palette) -> impl IntoElement {
    h_flex()
        .gap_1p5()
        .text_xs()
        .text_color(p.text)
        .child(div().flex_none().size(px(8.)).rounded_full().bg(c))
        .child(div().min_w_0().truncate().child(label))
        .when_some(note, |this, note| {
            this.child(div().text_color(p.text_muted).child(note))
        })
}

fn table(columns: &[String], rows: &[Vec<String>], p: &Palette) -> AnyElement {
    let line = |cells: &[String], head: bool| {
        h_flex()
            .w_full()
            .py_1p5()
            .border_b_1()
            .border_color(p.border)
            .when(head, |this| this.font_medium().text_color(p.text_muted))
            .children(
                cells
                    .iter()
                    .map(|c| div().flex_1().min_w_0().px_2().child(c.clone())),
            )
    };
    v_flex()
        .w_full()
        .text_xs()
        .text_color(p.text)
        .child(line(columns, true))
        .children(rows.iter().map(|r| line(r, false)))
        .into_any_element()
}
