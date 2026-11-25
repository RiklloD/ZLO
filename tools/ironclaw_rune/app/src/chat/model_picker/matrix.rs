//! Signal Matrix model picker: plots the connected providers' models by
//! Artificial Analysis Intelligence Index (up) against list price (right),
//! with the Pareto frontier drawn through the best value picks. Each model
//! sits at the score it reaches at the Thinking level the slider underneath
//! shows (`view::thinking_row`), so dragging that slider moves the dots;
//! models without a thinking knob stay put. Press or drag anywhere on the
//! plot to scrub to the nearest model, and let go to pick it. Models with no
//! published score are listed under the plot rather than placed on a guess.
//! Enabled from Settings → General.

use std::collections::HashSet;

use rune::shape::{LineCap, Path, Shape};
use rune::{Color, Cursor, El, Phase, TextStyle, canvas, col, el, row, text};
use rune_kit::{Kit, RollingText};
use zlo_client::orchestration::RouteProvider;

use super::view::{FOLLOW, SETTLE, provider_mark, thinking_choices, thinking_row};
use super::{ModelPickerState, NATIVE, Slot, pick, pretty_model};
use crate::design::kit::compact::{composer_surface, panel_surface};
use crate::design::styles::rem;
use crate::design::{self, Palette};
use crate::state::State;

const MATRIX_WIDTH: f32 = 408.;
const PLOT_W: f32 = 384.;
const PLOT_H: f32 = 236.;
/// The data area inside the plot box, in points: room on the left for the
/// score ticks, a top strip for the captions, a bottom strip for the axis.
const AREA_L: f32 = 26.;
const AREA_R: f32 = PLOT_W - 22.;
const AREA_T: f32 = 16.;
const AREA_B: f32 = PLOT_H - 26.;
/// Marker diameters: a provider logo in a disc.
const MARK: f32 = 20.;
const ACTIVE_MARK: f32 = 26.;
/// The level a model without a thinking knob (or Auto) is rated at.
const DEFAULT_LEVEL: &str = "high";
const LABEL_H: f32 = 15.;
/// Three 28 pt rows of unscored models and their gaps.
const UNRATED_MAX_H: f32 = 3. * 28. + 2. * 4.;

/// Artificial Analysis Intelligence Index v5.0.1 scores per reasoning level
/// and list price (USD per 1M tokens), as published on 2026-11-24
/// (benchleader.com/benchmarks/aa_intelligence_index_v5). Patterns match the
/// normalized model id (lowercase, `.`/`_` → `-`); more specific first.
struct Bench {
    pattern: &'static str,
    price: f32,
    levels: &'static [(&'static str, f32)],
}

const BENCH: &[Bench] = &[
    Bench {
        pattern: "fable-5-2",
        price: 16.,
        levels: &[
            ("low", 53.9),
            ("medium", 56.3),
            ("high", 61.9),
            ("xhigh", 62.2),
            ("max", 64.8),
        ],
    },
    Bench {
        pattern: "fable-5-1",
        price: 20.,
        levels: &[
            ("low", 38.7),
            ("medium", 43.2),
            ("high", 47.2),
            ("xhigh", 50.0),
            ("max", 49.3),
        ],
    },
    Bench {
        pattern: "opus-5-5",
        price: 8.,
        levels: &[
            ("medium", 48.3),
            ("high", 51.8),
            ("xhigh", 57.5),
            ("max", 59.1),
        ],
    },
    Bench {
        pattern: "opus-5",
        price: 10.,
        levels: &[("max", 44.2)],
    },
    Bench {
        pattern: "sonnet-5-5",
        price: 4.,
        levels: &[("high", 41.5), ("xhigh", 49.9), ("max", 54.7)],
    },
    Bench {
        pattern: "gpt-6-2-astra",
        price: 16.,
        levels: &[
            ("low", 45.6),
            ("medium", 57.0),
            ("high", 60.3),
            ("xhigh", 61.8),
            ("max", 62.2),
        ],
    },
    Bench {
        pattern: "gpt-6-2-sol",
        price: 4.,
        levels: &[
            ("low", 40.5),
            ("medium", 52.4),
            ("high", 54.8),
            ("xhigh", 55.6),
            ("max", 57.3),
        ],
    },
    Bench {
        pattern: "gpt-6-1-sol",
        price: 4.,
        levels: &[
            ("medium", 46.4),
            ("high", 50.3),
            ("xhigh", 52.4),
            ("max", 54.7),
        ],
    },
    Bench {
        pattern: "gpt-6-1-terra",
        price: 2.,
        levels: &[
            ("medium", 38.1),
            ("high", 42.5),
            ("xhigh", 45.3),
            ("max", 51.4),
        ],
    },
    Bench {
        pattern: "gpt-6-astra",
        price: 20.,
        levels: &[
            ("low", 43.5),
            ("medium", 51.7),
            ("high", 54.9),
            ("xhigh", 55.4),
            ("max", 56.9),
        ],
    },
    Bench {
        pattern: "gpt-6-sol",
        price: 4.,
        levels: &[("max", 44.1)],
    },
    Bench {
        pattern: "gemini-4-neon",
        price: 6.,
        levels: &[
            ("low", 44.1),
            ("medium", 54.9),
            ("high", 59.8),
            ("xhigh", 61.5),
        ],
    },
    Bench {
        pattern: "gemini-4-carbon",
        price: 4.,
        levels: &[("low", 54.2), ("medium", 58.4), ("high", 60.1)],
    },
    Bench {
        pattern: "gemini-4-hydrogen",
        price: 3.,
        levels: &[("medium", 41.1), ("high", 51.0), ("xhigh", 54.1)],
    },
    Bench {
        pattern: "gemini-4-argon",
        price: 4.,
        levels: &[("medium", 47.7), ("high", 50.5)],
    },
    Bench {
        pattern: "kimi-k3-1-pro",
        price: 4.,
        levels: &[("high", 46.5)],
    },
    Bench {
        pattern: "qwen4-max",
        price: 2.25,
        levels: &[("high", 42.8)],
    },
];

fn normalize(id: &str) -> String {
    id.rsplit('/')
        .next()
        .unwrap_or(id)
        .to_ascii_lowercase()
        .replace(['.', '_', ' '], "-")
}

fn bench_for(model: &str) -> Option<&'static Bench> {
    let id = normalize(model);
    BENCH.iter().find(|b| id.contains(b.pattern))
}

/// Position of a reasoning level on one shared ladder, so a level a model
/// doesn't take maps to its nearest one.
fn level_rank(level: &str) -> i32 {
    match level {
        "off" | "none" => 0,
        "minimal" | "instant" => 1,
        "low" => 2,
        "medium" => 3,
        "high" => 4,
        "xhigh" => 5,
        "max" => 6,
        _ => 4,
    }
}

fn nearest<'a>(levels: impl Iterator<Item = &'a str>, rank: i32) -> Option<&'a str> {
    levels.min_by_key(|l| ((level_rank(l) - rank).abs(), level_rank(l)))
}

impl Bench {
    fn score_at(&self, level: &str) -> f32 {
        let rank = level_rank(level);
        let at = nearest(self.levels.iter().map(|(l, _)| *l), rank);
        self.levels
            .iter()
            .find(|(l, _)| Some(*l) == at)
            .map_or(0., |(_, s)| *s)
    }

    fn range(&self) -> (f32, f32) {
        self.levels
            .iter()
            .fold((f32::MAX, f32::MIN), |(lo, hi), (_, s)| {
                (lo.min(*s), hi.max(*s))
            })
    }
}

/// The thread's pick, with IronClaw's profile `default` resolved to the
/// model that actually runs (its provider's first).
fn resolved_current(m: &ModelPickerState) -> Option<(String, String)> {
    let (pv, md) = m.current.as_ref()?;
    if md != "default" {
        return Some((pv.clone(), md.clone()));
    }
    let first = m
        .providers()
        .iter()
        .find(|p| &p.provider == pv)?
        .models
        .first()?;
    Some((pv.clone(), first.clone()))
}

/// The pair the chart highlights: the one under a drag, else the pick.
fn active_pair(m: &ModelPickerState) -> Option<(String, String)> {
    m.matrix_drag.clone().or_else(|| resolved_current(m))
}

/// The thinking level `model` would run at if picked now: the chart's level
/// mapped to the nearest one it takes. `None` keeps Auto, or the model has
/// no knob.
fn level_for(m: &ModelPickerState, provider: &str, model: &str) -> Option<String> {
    let level = chart_level(m)?;
    nearest(
        m.efforts_of(provider, model).iter().map(String::as_str),
        level_rank(&level),
    )
    .map(str::to_string)
}

/// Pick from the chart, carrying the level it previewed: Fable at Max picks
/// GPT 6.2 Sol at Extra High, the nearest it takes, rather than dropping to
/// Auto.
fn pick_at_level(s: &mut State, slot: &Slot, provider: String, model: String) {
    if let Some(m) = slot.get_mut(s)
        && !m.busy
    {
        let level = level_for(m, &provider, &model);
        if level.is_some() {
            m.effort = level;
        }
    }
    pick(s, slot.clone(), provider, model);
}

/// The level the chart shows: the one the Thinking slider is on, or the one
/// it would pick while it's being dragged. `None` is Auto.
fn chart_level(m: &ModelPickerState) -> Option<String> {
    let choices = thinking_choices(m);
    if let Some(frac) = m.thinking_drag
        && choices.len() >= 2
    {
        let ix = (frac.clamp(0., 1.) * (choices.len() - 1) as f32).round() as usize;
        return choices.get(ix).cloned().flatten();
    }
    m.effort.clone()
}

/// One plotted model.
#[derive(Clone, Debug)]
pub(crate) struct MatrixPoint {
    pub provider: String,
    pub runtime: String,
    pub brand: Option<String>,
    pub model: String,
    pub short: String,
    /// Intelligence Index at the shown level.
    pub score: f32,
    /// List price, USD per 1M tokens.
    pub price: f32,
    /// The model's own lowest/highest score across levels (equal when it
    /// has no thinking knob).
    pub span: (f32, f32),
    /// Position inside the plot box, in points.
    pub x: f32,
    pub y: f32,
    pub frontier: bool,
    /// Label box `(left, top, width)`; `None` when there's no room for it.
    label: Option<(f32, f32, f32)>,
}

/// A connected model with no published score.
#[derive(Clone, Debug)]
pub(crate) struct Unrated {
    pub provider: String,
    pub provider_name: String,
    pub runtime: String,
    pub brand: Option<String>,
    pub model: String,
}

pub(crate) struct Matrix {
    pub points: Vec<MatrixPoint>,
    pub unrated: Vec<Unrated>,
    /// Score axis bounds and tick step.
    pub y_lo: f32,
    pub y_hi: f32,
    pub y_step: f32,
    /// Price axis bounds, log10 of USD per 1M tokens.
    pub x_lo: f32,
    pub x_hi: f32,
}

/// Compact node label ("claude-opus-5-5" → "Opus 5.5", "gemini-4-carbon" →
/// "4 Carbon").
pub(crate) fn short_model(id: &str) -> String {
    let pretty = pretty_model(id);
    for prefix in ["Claude ", "Gemini "] {
        if let Some(rest) = pretty.strip_prefix(prefix)
            && !rest.is_empty()
        {
            return rest.to_string();
        }
    }
    pretty
}

fn label_w(label: &str) -> f32 {
    label.chars().count() as f32 * 6.3 + 6.
}

/// Build the chart from connected (`authenticated`), non-hidden providers.
pub(crate) fn build(m: &ModelPickerState) -> Matrix {
    let level = chart_level(m);
    let rank = level_rank(level.as_deref().unwrap_or(DEFAULT_LEVEL));
    // The IronClaw copy rule keys on the pick itself, not on a drag, so the
    // plot doesn't change under the pointer while scrubbing.
    let current = resolved_current(m);
    let active = active_pair(m);

    let connected: Vec<&RouteProvider> = m
        .providers()
        .iter()
        .filter(|p| p.authenticated && !m.prefs.provider_hidden(&p.provider))
        .collect();
    // A model a dedicated provider offers isn't drawn again for IronClaw's
    // own copy, unless that's the one the thread runs on.
    let dedicated: HashSet<&str> = connected
        .iter()
        .filter(|p| p.provider != NATIVE)
        .flat_map(|p| {
            p.models
                .iter()
                .filter(|md| !m.prefs.model_hidden(&p.provider, md))
                .map(String::as_str)
        })
        .collect();

    let mut points = Vec::new();
    let mut unrated = Vec::new();
    for provider in connected {
        for model in &provider.models {
            if m.prefs.model_hidden(&provider.provider, model) {
                continue;
            }
            let is_current = current
                .as_ref()
                .is_some_and(|(pv, md)| pv == &provider.provider && md == model);
            if provider.provider == NATIVE && !is_current && dedicated.contains(model.as_str()) {
                continue;
            }
            let Some(b) = bench_for(model) else {
                // Each provider's copy is its own route, so each is listed.
                unrated.push(Unrated {
                    provider: provider.provider.clone(),
                    provider_name: provider.name.clone(),
                    runtime: provider.runtime.clone(),
                    brand: provider.brand.clone(),
                    model: model.clone(),
                });
                continue;
            };
            let knob = provider.efforts_for(model);
            // A model with levels runs at the nearest one it takes; one
            // without a knob always runs at its default.
            let score = if knob.is_empty() {
                b.score_at(DEFAULT_LEVEL)
            } else {
                let lvl = nearest(knob.iter().map(String::as_str), rank).unwrap_or(DEFAULT_LEVEL);
                b.score_at(lvl)
            };
            let span = if knob.is_empty() {
                (score, score)
            } else {
                b.range()
            };
            points.push(MatrixPoint {
                provider: provider.provider.clone(),
                runtime: provider.runtime.clone(),
                brand: provider.brand.clone(),
                model: model.clone(),
                short: short_model(model),
                score,
                price: b.price,
                span,
                x: 0.,
                y: 0.,
                frontier: false,
                label: None,
            });
        }
    }

    // Axes fit every level of every plotted model, so they hold still while
    // the Thinking slider moves the markers.
    let (lo, hi) = points.iter().fold((f32::MAX, f32::MIN), |(lo, hi), p| {
        (lo.min(p.span.0), hi.max(p.span.1))
    });
    let (lo, hi) = if points.is_empty() {
        (40., 60.)
    } else {
        (lo, hi)
    };
    let range = hi - lo;
    let y_step = if range > 30. {
        10.
    } else if range > 8. {
        5.
    } else if range > 3. {
        2.
    } else {
        1.
    };
    // At least one tick always falls inside.
    let y_lo = (lo - 1.).min((lo / y_step).floor() * y_step);
    let y_hi = (hi + 1.).max((hi / y_step).ceil() * y_step);
    let (plo, phi) = points.iter().fold((f32::MAX, f32::MIN), |(lo, hi), p| {
        (lo.min(p.price.log10()), hi.max(p.price.log10()))
    });
    let (plo, phi) = if points.is_empty() || phi - plo < 0.2 {
        let mid = if points.is_empty() {
            0.6
        } else {
            (plo + phi) * 0.5
        };
        (mid - 0.3, mid + 0.3)
    } else {
        (plo, phi)
    };
    let pad = (phi - plo) * 0.05;
    let (x_lo, x_hi) = (plo - pad, phi + pad);
    for p in &mut points {
        p.x = AREA_L + (p.price.log10() - x_lo) / (x_hi - x_lo) * (AREA_R - AREA_L);
        p.y = AREA_B - (p.score - y_lo) / (y_hi - y_lo) * (AREA_B - AREA_T);
    }
    spread_markers(&mut points);

    // Pareto frontier: no other model scores at least as well for no more.
    const EPS: f32 = 1e-4;
    for i in 0..points.len() {
        let b = &points[i];
        let dominated = points.iter().enumerate().any(|(j, a)| {
            i != j
                && a.score >= b.score - EPS
                && a.price <= b.price + EPS
                && (a.score > b.score + EPS || a.price < b.price - EPS)
        });
        points[i].frontier = !dominated;
    }

    place_labels(&mut points, active.as_ref());
    Matrix {
        points,
        unrated,
        y_lo,
        y_hi,
        y_step,
        x_lo,
        x_hi,
    }
}

/// Models at the same price and nearly the same score would sit on top of
/// each other: push such pairs apart sideways, just enough to show both.
fn spread_markers(points: &mut [MatrixPoint]) {
    let min = MARK + 3.;
    let (left, right) = (AREA_L + MARK * 0.5, PLOT_W - MARK * 0.5 - 2.);
    for _ in 0..16 {
        let mut moved = false;
        for i in 0..points.len() {
            for j in i + 1..points.len() {
                let (dx, dy) = (points[j].x - points[i].x, points[j].y - points[i].y);
                if dy.abs() >= min || dx.abs() >= min {
                    continue;
                }
                let need = (min * min - dy * dy).max(0.).sqrt();
                if dx.abs() >= need {
                    continue;
                }
                let push = (need - dx.abs()) * 0.5;
                // The higher scorer moves right, so order still reads.
                let dir = if dx.abs() > 0.01 {
                    dx.signum()
                } else if points[j].score >= points[i].score {
                    1.
                } else {
                    -1.
                };
                // A marker held by the plot's edge leaves the whole push to
                // the other one.
                let (xi, xj) = (points[i].x - push * dir, points[j].x + push * dir);
                let (ci, cj) = (xi.clamp(left, right), xj.clamp(left, right));
                points[i].x = ci - (xj - cj);
                points[j].x = cj - (xi - ci);
                points[i].x = points[i].x.clamp(left, right);
                points[j].x = points[j].x.clamp(left, right);
                moved = true;
            }
        }
        if !moved {
            break;
        }
    }
}

fn overlaps(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> bool {
    a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3
}

/// Put each label beside its marker where it covers no other label or
/// marker: the active model first, then the frontier, then by score. A
/// label with no free spot is left off (the marker's logo still shows, and
/// scrubbing names it in the header).
fn place_labels(points: &mut [MatrixPoint], active: Option<&(String, String)>) {
    let mut order: Vec<usize> = (0..points.len()).collect();
    let is_act =
        |p: &MatrixPoint| active.is_some_and(|(pv, md)| pv == &p.provider && md == &p.model);
    order.sort_by(|&a, &b| {
        let (pa, pb) = (&points[a], &points[b]);
        (is_act(pb), pb.frontier)
            .cmp(&(is_act(pa), pa.frontier))
            .then(pb.score.total_cmp(&pa.score))
    });
    let marks: Vec<(f32, f32, f32, f32)> = points
        .iter()
        .map(|p| {
            let r = if is_act(p) { ACTIVE_MARK } else { MARK } * 0.5 + 1.;
            (p.x - r, p.y - r, r * 2., r * 2.)
        })
        .collect();
    let mut placed: Vec<(f32, f32, f32, f32)> = Vec::new();
    for i in order {
        let (x, y) = (points[i].x, points[i].y);
        let act = is_act(&points[i]);
        let w = label_w(&points[i].short) + if act { 4. } else { 0. };
        let gap = if act { ACTIVE_MARK } else { MARK } * 0.5 + 4.;
        let h = LABEL_H;
        let candidates = [
            (x + gap, y - h * 0.5),
            (x - gap - w, y - h * 0.5),
            (x - w * 0.5, y - gap - h + 2.),
            (x - w * 0.5, y + gap - 2.),
            (x + gap - 4., y - h - 4.),
            (x + gap - 4., y + 4.),
            (x - gap - w + 4., y - h - 4.),
            (x - gap - w + 4., y + 4.),
        ];
        let fits = |&(lx, ly): &(f32, f32)| {
            let r = (lx, ly, w, h);
            lx >= AREA_L
                && lx + w <= PLOT_W - 4.
                && ly >= 4.
                && ly + h <= AREA_B
                && !placed.iter().any(|o| overlaps(r, *o))
                && !marks
                    .iter()
                    .enumerate()
                    .any(|(j, d)| j != i && overlaps(r, *d))
        };
        if let Some(&(lx, ly)) = candidates.iter().find(|c| fits(c)) {
            placed.push((lx, ly, w, h));
            points[i].label = Some((lx, ly, w));
        }
    }
}

/// Frontier points, cheapest first.
fn frontier_ordered(points: &[MatrixPoint]) -> Vec<&MatrixPoint> {
    let mut out: Vec<&MatrixPoint> = points.iter().filter(|p| p.frontier).collect();
    out.sort_by(|a, b| {
        a.price
            .total_cmp(&b.price)
            .then(a.score.total_cmp(&b.score))
    });
    out
}

/// The model nearest a point on the plot, in points.
fn nearest_point(points: &[MatrixPoint], x: f32, y: f32) -> Option<(String, String)> {
    points
        .iter()
        .min_by(|a, b| {
            let da = (a.x - x).hypot(a.y - y);
            let db = (b.x - x).hypot(b.y - y);
            da.total_cmp(&db)
        })
        .map(|p| (p.provider.clone(), p.model.clone()))
}

fn y_of(score: f32, mx: &Matrix) -> f32 {
    AREA_B - (score - mx.y_lo) / (mx.y_hi - mx.y_lo) * (AREA_B - AREA_T)
}

fn x_of(price: f32, mx: &Matrix) -> f32 {
    AREA_L + (price.log10() - mx.x_lo) / (mx.x_hi - mx.x_lo) * (AREA_R - AREA_L)
}

/// Score ticks inside the axis.
fn y_ticks(mx: &Matrix) -> Vec<f32> {
    let mut out = Vec::new();
    let mut t = (mx.y_lo / mx.y_step).ceil() * mx.y_step;
    while t <= mx.y_hi {
        out.push(t);
        t += mx.y_step;
    }
    out
}

/// Price ticks (powers of two dollars) inside the axis.
fn x_ticks(mx: &Matrix) -> Vec<f32> {
    [0.25_f32, 0.5, 1., 2., 4., 8., 16., 32., 64.]
        .into_iter()
        .filter(|p| (mx.x_lo..=mx.x_hi).contains(&p.log10()))
        .collect()
}

/// The plot's drawing: hairline score rules and the frontier. Everything
/// else is a real element on top.
fn matrix_canvas_shape(p: &Palette, mx: &Matrix) -> Shape {
    let mut shape = Shape::new();
    let rule = p.border.alpha(if p.light { 0.4 } else { 0.45 });
    for t in y_ticks(mx) {
        let y = y_of(t, mx).round() + 0.5;
        shape = shape.line([AREA_L, y], [AREA_R + 6., y], 1., LineCap::Butt, rule);
    }
    let frontier = frontier_ordered(&mx.points);
    if frontier.len() >= 2 {
        let mut line = Path::new().move_to([frontier[0].x, frontier[0].y]);
        for pt in &frontier[1..] {
            line = line.line_to([pt.x, pt.y]);
        }
        shape = shape.stroke_path(&line, 1.5, LineCap::Round, p.accent.alpha(0.55));
    }
    shape
}

/// The Signal Matrix popover: header, the plot, unscored models, and the
/// Thinking slider.
pub(super) fn matrix_popover(s: &State, m: &ModelPickerState, slot: &Slot, k: &Kit) -> El {
    let p = &s.palette;
    let mx = build(m);
    let active_pair = active_pair(m);
    let active_point = active_pair.as_ref().and_then(|(pv, md)| {
        mx.points
            .iter()
            .find(|pt| &pt.provider == pv && &pt.model == md)
    });

    let (head_name, head_provider, head_runtime, head_brand) = match active_point {
        Some(pt) => (
            pretty_model(&pt.model),
            pt.provider.clone(),
            pt.runtime.clone(),
            pt.brand.clone(),
        ),
        None => {
            let (lbl, pv) = m.label();
            let pv = pv.unwrap_or_else(|| NATIVE.into());
            let found = m.providers().iter().find(|r| r.provider == pv);
            (
                lbl,
                pv.clone(),
                found.map_or_else(|| NATIVE.into(), |r| r.runtime.clone()),
                found.and_then(|r| r.brand.clone()),
            )
        }
    };
    // What the active model scores and costs, beside its name.
    // Auto runs at the provider's own default, which the chart can only
    // estimate (as High).
    let approx = if chart_level(m).is_none() { "≈" } else { "" };
    let stats = active_point.map(|pt| {
        format!(
            "Index {approx}{:.1} · ${}/M",
            pt.score,
            price_text(pt.price)
        )
    });

    let mut name_row = row().gap(6.).center_y().child(provider_mark(
        s,
        head_brand.as_deref(),
        &head_runtime,
        &head_provider,
        14.,
    ));
    name_row = name_row.child(
        RollingText::new("matrix-active-model", head_name)
            .font(TextStyle::new(rem(13.), 500.))
            .color(p.text)
            .spring(design::motion::ROLL)
            .trailing()
            .build(k),
    );
    let mut header = row()
        .key("matrix-header")
        .px(14.)
        .pt(12.)
        .pb(10.)
        .gap(8.)
        .center_y()
        .child(name_row.grow(1.));
    if let Some(stats) = stats {
        header = header.child(
            RollingText::new("matrix-active-stats", stats)
                .font(TextStyle::new(rem(11.5), 450.))
                .color(p.text_muted)
                .spring(design::motion::ROLL)
                .trailing()
                .build(k),
        );
    }

    let pad_body = if mx.points.is_empty() {
        let msg = if m.route.is_none() {
            "Loading connected models…"
        } else {
            "No benchmarked models connected."
        };
        el().key("matrix-pad").h(64.).center().child(
            text(msg)
                .font(TextStyle::new(rem(11.5), 400.))
                .color(p.text_faint),
        )
    } else {
        plot(s, m, &mx, active_point, slot)
    };

    let mut body = col()
        .key("model-popover")
        .role("dialog")
        .label("Models")
        .preserve_focus_on_press(true)
        .w(MATRIX_WIDTH)
        .items_stretch()
        .clip_rounded(design::PANEL_RADIUS)
        .child(header)
        .child(el().px(12.).child(pad_body));

    if !mx.unrated.is_empty() {
        body = body
            .child(el().h(1.).w_full().fill(p.border))
            .child(unrated_list(s, &mx.unrated, active_pair.as_ref(), slot));
    }

    if let Some(bar) = thinking_row(s, m, slot, k) {
        body = body.child(el().h(1.).w_full().fill(p.border)).child(bar);
    }

    panel_surface(p, body)
}

/// "16", "2.25", "0.5".
fn price_text(price: f32) -> String {
    if (price - price.round()).abs() < 0.01 {
        format!("{price:.0}")
    } else {
        format!("{price:.2}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    }
}

fn axis_text(t: impl Into<String>, color: Color) -> El {
    text(t.into())
        .font(TextStyle::new(rem(10.), 450.))
        .color(color)
        .single_line()
}

fn plot(
    s: &State,
    m: &ModelPickerState,
    mx: &Matrix,
    active_point: Option<&MatrixPoint>,
    slot: &Slot,
) -> El {
    let p = &s.palette;
    // The popover's own fill, so markers and label backs hide the lines
    // under them without showing as boxes.
    let panel = composer_surface(p).0;
    let spring = if m.matrix_drag.is_some() || m.thinking_drag.is_some() {
        FOLLOW
    } else {
        SETTLE
    };
    let ink = p.text_faint;

    let mut pad = el()
        .key("matrix-pad")
        .role("group")
        .label("Models by intelligence and price: drag to pick")
        .w(PLOT_W)
        .h(PLOT_H)
        .cursor(Cursor::Crosshair)
        .child(
            canvas(matrix_canvas_shape(p, mx).view_box(PLOT_W, PLOT_H))
                .key("matrix-canvas")
                .pin(Some(0.), Some(0.), Some(0.), Some(0.))
                .a11y_hidden()
                .inert(),
        );

    // Score ticks on the left, price ticks along the bottom.
    for t in y_ticks(mx) {
        pad = pad.child(
            row()
                .absolute(0., y_of(t, mx) - 7.)
                .w(AREA_L - 8.)
                .h(14.)
                .justify_end()
                .center_y()
                .inert()
                .a11y_hidden()
                .child(axis_text(format!("{t:.0}"), ink)),
        );
    }
    let ticks = x_ticks(mx);
    for (ix, t) in ticks.iter().enumerate() {
        let label = if ix + 1 == ticks.len() {
            format!("${}/M", price_text(*t))
        } else {
            format!("${}", price_text(*t))
        };
        let w = 48.;
        pad = pad.child(
            row()
                .absolute((x_of(*t, mx) - w * 0.5).min(PLOT_W - w), AREA_B + 8.)
                .w(w)
                .h(14.)
                .justify_center()
                .center_y()
                .inert()
                .a11y_hidden()
                .child(axis_text(label, ink)),
        );
    }

    // A ring that glides to whichever model is active or under the drag.
    if let Some(act) = active_point {
        let d = ACTIVE_MARK + 10.;
        pad = pad.child(
            el().key("matrix-halo")
                .absolute(act.x - d * 0.5, act.y - d * 0.5)
                .size(d, d)
                .radius(d * 0.5)
                .border(1.5, p.accent.alpha(0.35))
                .a11y_hidden()
                .inert()
                .motion(spring),
        );
    }

    // Markers and labels are drawn only: the plot itself takes presses and
    // drags, so a drag can start on a marker.
    for pt in &mx.points {
        let is_act = active_point.is_some_and(|a| a.provider == pt.provider && a.model == pt.model);
        let size = if is_act { ACTIVE_MARK } else { MARK };
        let ring = if is_act {
            p.accent
        } else if pt.frontier {
            p.accent.alpha(0.6)
        } else {
            p.border
        };
        pad = pad.child(
            el().key(format!("matrix-node-{}-{}", pt.provider, pt.model))
                .absolute(pt.x - size * 0.5, pt.y - size * 0.5)
                .size(size, size)
                .radius(size * 0.5)
                .center()
                .fill(panel)
                .border(if is_act { 2. } else { 1. }, ring)
                .inert()
                .a11y_hidden()
                .motion(spring)
                .child(provider_mark(
                    s,
                    pt.brand.as_deref(),
                    &pt.runtime,
                    &pt.provider,
                    if is_act { 14. } else { 11. },
                )),
        );
        if let Some((lx, ly, w)) = pt.label {
            let (color, weight) = if is_act {
                (p.text, 600.)
            } else if pt.frontier {
                (p.text, 500.)
            } else {
                (p.text_muted, 450.)
            };
            pad = pad.child(
                row()
                    .key(format!("matrix-lbl-{}-{}", pt.provider, pt.model))
                    .absolute(lx, ly)
                    .w(w)
                    .h(LABEL_H)
                    .center()
                    .radius(3.)
                    .fill(panel)
                    .inert()
                    .a11y_hidden()
                    .motion(spring)
                    .child(
                        text(pt.short.clone())
                            .font(TextStyle::new(rem(11.), weight))
                            .color(color)
                            .single_line(),
                    ),
            );
        }
    }

    let drag_slot = slot.clone();
    pad.on_drag(move |s: &mut State, d, _| matrix_drag(s, &drag_slot, d))
}

/// Connected models with no published score: plain two-column rows under
/// the plot, picked with a press.
fn unrated_list(
    s: &State,
    unrated: &[Unrated],
    active: Option<&(String, String)>,
    slot: &Slot,
) -> El {
    let p = &s.palette;
    let cell_w = (MATRIX_WIDTH - 16. - 4.) / 2.;
    let mut grid = row().key("matrix-unrated").flex_wrap().gap(4.);
    for u in unrated {
        let on = active.is_some_and(|(pv, md)| pv == &u.provider && md == &u.model);
        let (pv, md) = (u.provider.clone(), u.model.clone());
        let pick_slot = slot.clone();
        grid = grid.child(
            row()
                .key(format!("matrix-unrated-{}-{}", u.provider, u.model))
                .role("button")
                .label(format!("{} ({})", pretty_model(&u.model), u.provider_name))
                .help(format!("{} · no published score", u.provider_name))
                .w(cell_w)
                .h(28.)
                .px(8.)
                .gap(8.)
                .center_y()
                .radius(6.)
                .continuous_corners()
                .fill(if on { p.active } else { Color::TRANSPARENT })
                .hover(if on { p.active } else { p.hover })
                .cursor(Cursor::Pointer)
                .on_press(move |s: &mut State| {
                    pick_at_level(s, &pick_slot, pv.clone(), md.clone());
                })
                .child(provider_mark(
                    s,
                    u.brand.as_deref(),
                    &u.runtime,
                    &u.provider,
                    13.,
                ))
                .child(
                    text(pretty_model(&u.model))
                        .font(TextStyle::new(rem(12.), if on { 500. } else { 400. }))
                        .color(if on { p.text } else { p.text_muted })
                        .single_line()
                        .truncate()
                        .grow(1.)
                        .min_w(0.),
                ),
        );
    }
    col()
        .px(8.)
        .pt(8.)
        .pb(8.)
        .gap(2.)
        .child(
            el().px(8.).pb(2.).child(
                text("No published score")
                    .font(TextStyle::new(rem(11.), 450.))
                    .color(p.text_faint)
                    .single_line(),
            ),
        )
        .child(
            // Three rows show; a longer list scrolls, so the Thinking
            // slider under it stays in the panel.
            col()
                .key("matrix-unrated-scroll")
                .max_h(UNRATED_MAX_H)
                .min_h(0.)
                .scroll(rune::ScrollSpec {
                    stick_bottom: false,
                })
                .scroll_edge_fade(12.)
                .child(grid),
        )
}

/// A press or drag on the plot scrubs to the nearest model; letting go
/// picks it. Escape (a cancelled drag) leaves the pick as it was.
fn matrix_drag(s: &mut State, slot: &Slot, d: &mut rune::Drag) {
    let Some(m) = slot.get(s) else { return };
    // The plot's layout box is fixed, so map the pointer into its points.
    let x = d.x / d.w.max(1.) * PLOT_W;
    let y = d.y / d.h.max(1.) * PLOT_H;
    let snapped = nearest_point(&build(m).points, x, y);
    match d.phase {
        Phase::Down | Phase::Move => {
            if let Some(m) = slot.get_mut(s) {
                m.matrix_drag = snapped;
            }
        }
        Phase::Up => {
            let cached = slot.get_mut(s).and_then(|m| m.matrix_drag.take());
            if d.cancelled() {
                return;
            }
            // A catalog refresh may have hidden or dropped the model the
            // drag was on: only a model still plotted is picked.
            let Some(m) = slot.get(s) else { return };
            let plotted = build(m).points;
            let target = cached
                .filter(|(pv, md)| plotted.iter().any(|p| &p.provider == pv && &p.model == md))
                .or(snapped);
            if let Some((pv, md)) = target {
                pick_at_level(s, slot, pv, md);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::model_picker::{Slot, fixture_matrix, scene_matrix};

    fn find<'a>(mx: &'a Matrix, provider: &str, model: &str) -> &'a MatrixPoint {
        mx.points
            .iter()
            .find(|p| p.provider == provider && p.model == model)
            .unwrap_or_else(|| panic!("{provider}/{model} plotted"))
    }

    #[test]
    fn only_connected_visible_models_plot_and_unscored_ones_are_listed() {
        let mx = build(&fixture_matrix());
        assert!(!mx.points.iter().any(|pt| pt.provider == "openrouter"));
        // A hidden provider and hidden models stay out.
        assert!(!mx.points.iter().any(|pt| pt.provider == "codex"));
        assert!(!mx.points.iter().any(|pt| pt.model == "claude-sonnet-5-5"));
        assert!(!mx.points.iter().any(|pt| pt.model == "gpt-6.1-terra"));
        assert!(
            !mx.points
                .iter()
                .any(|pt| pt.provider == "antigravity" && pt.model == "claude-opus-5-5")
        );
        find(&mx, "claude-cli", "claude-fable-5-2");
        find(&mx, "antigravity", "gemini-4-carbon");
        // With Codex hidden, IronClaw's own GPT copies are the ones shown.
        find(&mx, NATIVE, "gpt-6.2-sol");
        // No published score: listed, never placed on a guess.
        assert!(!mx.points.iter().any(|pt| pt.model == "glm-5.4"));
        for model in [
            "glm-5.4",
            "glm-5.3-flash",
            "mercury-3",
            "gemini-pro-orchestrator-argon",
        ] {
            assert!(
                mx.unrated.iter().any(|u| u.model == model),
                "{model} listed"
            );
        }
        assert!(
            !mx.unrated.iter().any(|u| u.model == "claude-haiku-5-5"),
            "hidden stays hidden"
        );
    }

    #[test]
    fn fable_outscores_opus_at_every_level_and_dots_follow_the_slider() {
        let mut m = fixture_matrix();
        let mut last_fable_y = f32::MAX;
        for level in ["low", "medium", "high", "xhigh", "max"] {
            m.effort = Some(level.into());
            let mx = build(&m);
            let fable = find(&mx, "claude-cli", "claude-fable-5-2");
            let opus = find(&mx, "claude-cli", "claude-opus-5-5");
            assert!(fable.score > opus.score, "Fable above Opus at {level}");
            assert!(fable.y < opus.y, "drawn higher at {level}");
            assert!(fable.y <= last_fable_y, "more thinking never drops the dot");
            last_fable_y = fable.y;
        }
        m.effort = Some("max".into());
        assert_eq!(
            find(&build(&m), "claude-cli", "claude-fable-5-2").score,
            64.8
        );
        m.effort = Some("low".into());
        assert_eq!(
            find(&build(&m), "claude-cli", "claude-fable-5-2").score,
            53.9
        );
        // Carbon is rated up to high: past it, it holds its high score.
        m.effort = Some("max".into());
        assert_eq!(
            find(&build(&m), "antigravity", "gemini-4-carbon").score,
            60.1
        );
    }

    #[test]
    fn a_model_without_a_thinking_knob_holds_still() {
        let mut m = fixture_matrix();
        // IronClaw's GPT 6.2 Sol is rated at five levels; take its knob away.
        if let Some(route) = m.route.as_mut()
            && let Some(native) = route.providers.iter_mut().find(|p| p.provider == NATIVE)
        {
            native.efforts.clear();
        }
        let mut seen = Vec::new();
        for level in ["low", "medium", "high", "xhigh", "max"] {
            m.effort = Some(level.into());
            let mx = build(&m);
            let sol = find(&mx, NATIVE, "gpt-6.2-sol");
            // Its height and score hold; it may only step aside for a
            // neighbour that moves onto it.
            seen.push((sol.score, sol.y));
        }
        assert!(seen.windows(2).all(|w| w[0] == w[1]), "{seen:?}");
        assert_eq!(seen[0].0, 54.8, "held at its default level");
    }

    fn providers_mut(m: &mut ModelPickerState) -> &mut Vec<RouteProvider> {
        &mut m.route.as_mut().expect("fixture route").providers
    }

    #[test]
    fn each_providers_unscored_copy_is_its_own_row() {
        let mut m = fixture_matrix();
        let inception = providers_mut(&mut m)
            .iter_mut()
            .find(|p| p.provider == "inception")
            .expect("inception");
        inception.models.push("glm-5.4".into());
        let mx = build(&m);
        let glm: Vec<_> = mx.unrated.iter().filter(|u| u.model == "glm-5.4").collect();
        assert_eq!(glm.len(), 2, "GLM and Inception both offer it");
    }

    #[test]
    fn a_profile_default_pick_highlights_the_model_that_runs() {
        let mut m = fixture_matrix();
        m.current = Some((NATIVE.into(), "default".into()));
        assert_eq!(active_pair(&m), Some((NATIVE.into(), "gpt-6.2-sol".into())));
        find(&build(&m), NATIVE, "gpt-6.2-sol");
    }

    #[test]
    fn one_rated_model_still_gets_score_ticks() {
        let mut m = fixture_matrix();
        providers_mut(&mut m).retain(|p| p.provider == "kimi");
        m.current = Some(("kimi".into(), "kimi-k3.1-pro".into()));
        let mx = build(&m);
        assert_eq!(mx.points.len(), 1);
        assert!(!y_ticks(&mx).is_empty());
        assert!(!x_ticks(&mx).is_empty() || mx.x_hi > mx.x_lo);
    }

    #[test]
    fn same_price_copies_stay_apart_and_pickable_at_the_edge() {
        let mut m = fixture_matrix();
        let template = providers_mut(&mut m)
            .iter()
            .find(|p| p.provider == "antigravity")
            .cloned()
            .expect("antigravity");
        providers_mut(&mut m).retain(|p| p.provider == "claude-cli");
        for ix in 0..4 {
            let mut copy = template.clone();
            copy.provider = format!("copy{ix}");
            copy.models = vec!["gemini-4-carbon".into()];
            providers_mut(&mut m).push(copy);
        }
        let mx = build(&m);
        let carbons: Vec<_> = mx
            .points
            .iter()
            .filter(|p| p.model == "gemini-4-carbon")
            .collect();
        assert_eq!(carbons.len(), 4);
        for (i, a) in carbons.iter().enumerate() {
            for b in &carbons[i + 1..] {
                assert!((a.x - b.x).hypot(a.y - b.y) >= MARK, "{a:?} on {b:?}");
            }
            assert_eq!(
                nearest_point(&mx.points, a.x, a.y),
                Some((a.provider.clone(), a.model.clone())),
                "each copy is reachable"
            );
        }
    }

    #[test]
    fn frontier_is_non_dominated_and_labels_never_collide() {
        let mx = build(&fixture_matrix());
        let frontier: Vec<_> = mx.points.iter().filter(|p| p.frontier).collect();
        assert!(frontier.len() >= 2);
        for f in &frontier {
            assert!(!mx.points.iter().any(|o| {
                o.model != f.model
                    && o.score >= f.score
                    && o.price <= f.price
                    && (o.score > f.score || o.price < f.price)
            }));
        }
        let boxes: Vec<_> = mx
            .points
            .iter()
            .filter_map(|p| p.label.map(|(x, y, w)| (x, y, w, LABEL_H)))
            .collect();
        for (i, a) in boxes.iter().enumerate() {
            assert!(a.0 >= 0. && a.0 + a.2 <= PLOT_W, "label inside the plot");
            for b in &boxes[i + 1..] {
                assert!(!overlaps(*a, *b), "labels {a:?} and {b:?} overlap");
            }
        }
        // Dots use the plot's full width and height.
        let xs = mx.points.iter().map(|p| p.x);
        let (xmin, xmax) = xs.fold((f32::MAX, f32::MIN), |(a, b), x| (a.min(x), b.max(x)));
        assert!(xmin < AREA_L + 30. && xmax > AREA_R - 30.);
    }

    #[test]
    fn renders_beside_the_thinking_slider_and_a_drag_from_a_dot_picks() {
        use rune::{Env, Runtime};

        let mut s_list = crate::chat::fixtures::test_state();
        crate::chat::model_picker::scene(&mut s_list, true);
        assert!(!s_list.prefs.signal_matrix_picker);
        let mut rt_list = Runtime::new(s_list, crate::view, (1440., 900.), Env::new(true, 1.));
        for i in 0..30 {
            rt_list.frame(i as f64 / 60.);
        }
        assert!(rt_list.presented("matrix-pad").is_none());

        let mut s = crate::chat::fixtures::test_state();
        scene_matrix(&mut s);
        let mut rt = Runtime::new(s, crate::view, (1440., 900.), Env::new(true, 1.));
        for i in 0..60 {
            rt.frame(i as f64 / 60.);
        }
        let (popover, _) = rt.presented("model-popover").expect("popover drawn");
        let (card, _) = rt.presented("composer:t1").expect("composer drawn");
        assert!(popover[1] + popover[3] <= card[1], "clears the composer");
        assert!(rt.presented("thinking-track").is_some(), "slider kept");
        assert!(rt.presented("matrix-unrated").is_some(), "unscored listed");

        // At Max, press on Opus, drag onto Carbon (rated up to Extra High),
        // let go.
        let slot = Slot::chat("t1");
        if let Some(m) = slot.get_mut(&mut rt.state) {
            m.effort = Some("max".into());
        }
        rt.frame(1.05);
        let (opus, _) = rt
            .presented("matrix-node-claude-cli-claude-opus-5-5")
            .expect("opus dot");
        let (carbon, _) = rt
            .presented("matrix-node-antigravity-gemini-4-carbon")
            .expect("carbon dot");
        let (ox, oy) = (opus[0] + opus[2] * 0.5, opus[1] + opus[3] * 0.5);
        let (cx, cy) = (carbon[0] + carbon[2] * 0.5, carbon[1] + carbon[3] * 0.5);
        rt.pointer_move(ox, oy);
        rt.pointer_down(ox, oy);
        rt.frame(1.1);
        assert_eq!(
            slot.get(&rt.state).and_then(|m| m.matrix_drag.clone()),
            Some(("claude-cli".into(), "claude-opus-5-5".into())),
            "press on a dot scrubs to it"
        );
        for step in 1..=8 {
            let t = step as f32 / 8.;
            rt.pointer_move(ox + (cx - ox) * t, oy + (cy - oy) * t);
            rt.frame(1.1 + f64::from(step) / 60.);
        }
        assert_eq!(
            slot.get(&rt.state).and_then(|m| m.matrix_drag.clone()),
            Some(("antigravity".into(), "gemini-4-carbon".into())),
            "drag follows to the dot under the pointer"
        );
        rt.pointer_up(cx, cy);
        rt.frame(1.4);
        let m = slot.get(&rt.state).expect("picker");
        assert!(!m.is_open(), "release picks and closes");
        assert_eq!(
            rt.state.prefs.recent_models.first(),
            Some(&("antigravity".to_string(), "gemini-4-carbon".to_string())),
            "the dot under the pointer was picked"
        );
        assert_eq!(
            slot.get(&rt.state)
                .and_then(|m| m.effort.clone())
                .as_deref(),
            Some("xhigh"),
            "the pick carries the level the chart showed, not Auto"
        );
    }
}
