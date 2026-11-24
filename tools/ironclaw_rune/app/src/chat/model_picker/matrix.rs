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
use rune::{Color, Cursor, El, MotionIntent, Phase, TextStyle, canvas, col, el, row, text};
use rune_kit::{Kit, RollingText};
use zlo_client::orchestration::RouteProvider;

use super::view::{FOLLOW, SETTLE, provider_mark, thinking_choices, thinking_row};
use super::{ModelPickerState, NATIVE, Slot, effort_label, pick, pretty_model};
use crate::design::kit::compact::{XS, panel_surface, txt_medium};
use crate::design::styles::rem;
use crate::design::{self, Palette, rgb};
use crate::state::State;

const MATRIX_WIDTH: f32 = 408.;
const PLOT_W: f32 = 384.;
const PLOT_H: f32 = 264.;
/// The data area inside the plot box, in points: room on the left for the
/// score ticks, a top strip for the captions, a bottom strip for the axis.
const AREA_L: f32 = 30.;
const AREA_R: f32 = PLOT_W - 14.;
const AREA_T: f32 = 28.;
const AREA_B: f32 = PLOT_H - 24.;
/// The level a model without a thinking knob (or Auto) is rated at.
const DEFAULT_LEVEL: &str = "high";
const LABEL_H: f32 = 16.;

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
    pub model: String,
}

pub(crate) struct Matrix {
    pub points: Vec<MatrixPoint>,
    pub unrated: Vec<Unrated>,
    /// Score axis bounds and tick step.
    pub y_lo: f32,
    pub y_hi: f32,
    pub y_step: f32,
    pub level: Option<String>,
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
    label.chars().count() as f32 * 6.1 + 12.
}

/// Build the chart from connected (`authenticated`), non-hidden providers.
pub(crate) fn build(m: &ModelPickerState) -> Matrix {
    let level = chart_level(m);
    let rank = level_rank(level.as_deref().unwrap_or(DEFAULT_LEVEL));
    let active_pair = m.matrix_drag.as_ref().or(m.current.as_ref());

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
        .flat_map(|p| p.models.iter().map(String::as_str))
        .collect();

    let mut points = Vec::new();
    let mut unrated = Vec::new();
    let mut seen_unrated: HashSet<&str> = HashSet::new();
    for provider in connected {
        for model in &provider.models {
            if m.prefs.model_hidden(&provider.provider, model) {
                continue;
            }
            let is_active =
                active_pair.is_some_and(|(pv, md)| pv == &provider.provider && md == model);
            if provider.provider == NATIVE && !is_active && dedicated.contains(model.as_str()) {
                continue;
            }
            let Some(b) = bench_for(model) else {
                if seen_unrated.insert(model.as_str()) || is_active {
                    unrated.push(Unrated {
                        provider: provider.provider.clone(),
                        provider_name: provider.name.clone(),
                        model: model.clone(),
                    });
                }
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
    // the Thinking slider moves the dots.
    let (lo, hi) = points.iter().fold((f32::MAX, f32::MIN), |(lo, hi), p| {
        (lo.min(p.span.0), hi.max(p.span.1))
    });
    let (lo, hi) = if points.is_empty() {
        (40., 60.)
    } else {
        (lo, hi)
    };
    let y_step = if hi - lo > 30. { 10. } else { 5. };
    let y_lo = lo - 1.5;
    let y_hi = hi + 1.5;
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
    // Room past the outermost dots for their labels.
    let (x_lo, x_hi) = (plo - (phi - plo) * 0.06, phi + (phi - plo) * 0.06);
    for p in &mut points {
        p.x = AREA_L + (p.price.log10() - x_lo) / (x_hi - x_lo) * (AREA_R - AREA_L);
        p.y = AREA_B - (p.score - y_lo) / (y_hi - y_lo) * (AREA_B - AREA_T);
    }

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

    place_labels(&mut points, active_pair);
    Matrix {
        points,
        unrated,
        y_lo,
        y_hi,
        y_step,
        level,
    }
}

fn overlaps(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> bool {
    a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3
}

/// Put each label beside its dot where it covers no other label or dot: the
/// active model first, then the frontier, then by score. A label with no
/// free spot is left off (the dot still shows, and scrubbing names it).
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
    let dots: Vec<(f32, f32, f32, f32)> = points
        .iter()
        .map(|p| (p.x - 5., p.y - 5., 10., 10.))
        .collect();
    let mut placed: Vec<(f32, f32, f32, f32)> = Vec::new();
    for i in order {
        let (x, y) = (points[i].x, points[i].y);
        let act = is_act(&points[i]);
        let w = label_w(&points[i].short) + if act { 2. } else { 0. };
        let gap = if act { 13. } else { 8. };
        let h = LABEL_H;
        let candidates = [
            (x + gap, y - h * 0.5),
            (x - gap - w, y - h * 0.5),
            (x - w * 0.5, y - gap - h),
            (x - w * 0.5, y + gap),
            (x + gap, y - h - 2.),
            (x + gap, y + 2.),
            (x - gap - w, y - h - 2.),
            (x - gap - w, y + 2.),
        ];
        let fits = |&(lx, ly): &(f32, f32)| {
            let r = (lx, ly, w, h);
            lx >= AREA_L - 2.
                && lx + w <= PLOT_W - 2.
                && ly >= AREA_T - 10.
                && ly + h <= AREA_B + 8.
                && !placed.iter().any(|o| overlaps(r, *o))
                && !dots
                    .iter()
                    .enumerate()
                    .any(|(j, d)| j != i && overlaps(r, *d))
        };
        let spot = candidates.iter().find(|c| fits(c)).copied().or_else(|| {
            act.then(|| {
                let (lx, ly) = candidates[0];
                (lx.min(PLOT_W - w - 2.), ly)
            })
        });
        if let Some((lx, ly)) = spot {
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

/// The plot's drawing: dot field, dotted score rules, each model's level
/// range, the frontier with its wash and scanlines, and the active crosshair.
fn matrix_canvas_shape(p: &Palette, mx: &Matrix, active: Option<&MatrixPoint>) -> Shape {
    let mut shape = Shape::new();

    // Dot field (the Thinking slider's texture).
    let dot_col = p.text_faint.alpha(if p.light { 0.12 } else { 0.11 });
    let mut gx = AREA_L + 4.;
    while gx < PLOT_W - 6. {
        let mut gy = AREA_T;
        while gy < AREA_B {
            shape = shape.line([gx, gy], [gx, gy + 1.5], 1.5, LineCap::Butt, dot_col);
            gy += 16.;
        }
        gx += 16.;
    }

    // Dotted rules at each score tick.
    let rule = p.text_faint.alpha(if p.light { 0.32 } else { 0.28 });
    let mut t = (mx.y_lo / mx.y_step).ceil() * mx.y_step;
    while t <= mx.y_hi {
        let y = y_of(t, mx);
        let mut x = AREA_L;
        while x < AREA_R {
            shape = shape.line([x, y], [x + 1.5, y], 1., LineCap::Butt, rule);
            x += 5.;
        }
        t += mx.y_step;
    }

    // Each thinking model's range across its levels: the dot slides on it.
    let range_col = p.text_faint.alpha(if p.light { 0.3 } else { 0.26 });
    for pt in &mx.points {
        if pt.span.1 - pt.span.0 < 0.5 {
            continue;
        }
        let (y0, y1) = (y_of(pt.span.1, mx), y_of(pt.span.0, mx));
        shape = shape
            .line([pt.x, y0], [pt.x, y1], 1.5, LineCap::Round, range_col)
            .line(
                [pt.x - 3., y0],
                [pt.x + 3., y0],
                1.,
                LineCap::Butt,
                range_col,
            )
            .line(
                [pt.x - 3., y1],
                [pt.x + 3., y1],
                1.,
                LineCap::Butt,
                range_col,
            );
    }

    // Frontier: a stepped line (a cheaper model holds until a pricier one
    // scores higher), its wash, and fine scanlines under it.
    let frontier = frontier_ordered(&mx.points);
    if let Some(first) = frontier.first() {
        let mut steps: Vec<[f32; 2]> = vec![[first.x, first.y]];
        for win in frontier.windows(2) {
            let (a, b) = (win[0], win[1]);
            steps.push([b.x, a.y]);
            steps.push([b.x, b.y]);
        }
        let last = *steps.last().unwrap_or(&[first.x, first.y]);
        let mut line = Path::new().move_to(steps[0]);
        let mut area = Path::new().move_to([steps[0][0], AREA_B]).line_to(steps[0]);
        for s in &steps[1..] {
            line = line.line_to(*s);
            area = area.line_to(*s);
        }
        area = area.line_to([last[0], AREA_B]).close();
        shape = shape.fill_path(&area, p.accent.alpha(if p.light { 0.07 } else { 0.12 }));
        let scan = p.accent.alpha(if p.light { 0.08 } else { 0.1 });
        let mut sx = (steps[0][0] / 6.).ceil() * 6.;
        while sx <= last[0] {
            let top = steps
                .windows(2)
                .find(|w| sx >= w[0][0] && sx <= w[1][0] && (w[0][0] - w[1][0]).abs() > 0.01)
                .map_or(steps[0][1], |w| w[0][1]);
            shape = shape.line([sx, top + 2.], [sx, AREA_B], 1., LineCap::Butt, scan);
            sx += 6.;
        }
        if steps.len() >= 2 {
            shape = shape.stroke_path(&line, 1.75, LineCap::Round, p.accent.alpha(0.9));
        }
    }

    if let Some(act) = active {
        let ch = p.accent.alpha(if p.light { 0.22 } else { 0.2 });
        shape = shape
            .line([AREA_L, act.y], [AREA_R, act.y], 1., LineCap::Butt, ch)
            .line([act.x, AREA_T - 6.], [act.x, AREA_B], 1., LineCap::Butt, ch);
    }

    shape
}

/// The Signal Matrix popover: header, the plot, unrated models, and the
/// Thinking slider.
pub(super) fn matrix_popover(s: &State, m: &ModelPickerState, slot: &Slot, k: &Kit) -> El {
    let p = &s.palette;
    let mx = build(m);
    let active_pair = m.matrix_drag.as_ref().or(m.current.as_ref());
    let active_point = active_pair.and_then(|(pv, md)| {
        mx.points
            .iter()
            .find(|pt| &pt.provider == pv && &pt.model == md)
    });

    let (head_name, head_provider, head_runtime, head_brand) = match (active_point, active_pair) {
        (Some(pt), _) => (
            pretty_model(&pt.model),
            pt.provider.clone(),
            pt.runtime.clone(),
            pt.brand.clone(),
        ),
        _ => {
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

    let header = row()
        .key("matrix-header")
        .px(12.)
        .pt(12.)
        .pb(8.)
        .center_y()
        .child(txt_medium("Model", XS, p.text_faint).grow(1.))
        .child(
            row()
                .gap(6.)
                .center_y()
                .child(provider_mark(
                    s,
                    head_brand.as_deref(),
                    &head_runtime,
                    &head_provider,
                    14.,
                ))
                .child(
                    RollingText::new("matrix-active-model", head_name)
                        .font(TextStyle::new(rem(13.), 500.))
                        .color(p.text)
                        .spring(design::motion::ROLL)
                        .trailing()
                        .build(k),
                ),
        );

    let pad_body = if mx.points.is_empty() {
        let msg = if m.route.is_none() {
            "Loading connected models…"
        } else {
            "No benchmarked models connected."
        };
        el().key("matrix-pad")
            .h(72.)
            .center()
            .radius(8.)
            .fill(p.bg)
            .border(1., p.border)
            .child(
                text(msg)
                    .font(TextStyle::new(rem(11.), 400.))
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
        .child(el().px(12.).pb(10.).child(pad_body));

    if !mx.unrated.is_empty() {
        body = body.child(unrated_row(p, &mx.unrated, active_pair, slot));
    }

    if let Some(bar) = thinking_row(s, m, slot, k) {
        body = body.child(el().h(1.).w_full().fill(p.border)).child(bar);
    }

    panel_surface(p, body)
}

fn caption(t: impl Into<String>, color: Color) -> El {
    text(t.into())
        .font(TextStyle::new(rem(9.5), 500.))
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
    let spring = if m.matrix_drag.is_some() || m.thinking_drag.is_some() {
        FOLLOW
    } else {
        SETTLE
    };
    let ink = p.text_faint.alpha(0.75);
    let level = mx
        .level
        .as_deref()
        .map_or_else(|| "AUTO".to_string(), |l| effort_label(l).to_uppercase());

    let mut pad = el()
        .key("matrix-pad")
        .role("group")
        .label("Signal Matrix: drag to pick a model")
        .w(PLOT_W)
        .h(PLOT_H)
        .radius(8.)
        .continuous_corners()
        .clip_rounded(8.)
        .fill(p.bg)
        .border(1., p.border)
        .cursor(Cursor::Crosshair)
        .child(
            canvas(matrix_canvas_shape(p, mx, active_point).view_box(PLOT_W, PLOT_H))
                .key("matrix-canvas")
                .pin(Some(0.), Some(0.), Some(0.), Some(0.))
                .a11y_hidden()
                .inert(),
        )
        .child(
            el().pin(Some(10.), Some(8.), None, None)
                .inert()
                .a11y_hidden()
                .child(caption("INTELLIGENCE INDEX ↑", ink)),
        )
        .child(
            el().key("matrix-level")
                .pin(None, Some(8.), Some(10.), None)
                .inert()
                .a11y_hidden()
                .child(caption(format!("THINKING · {level}"), p.accent.alpha(0.85))),
        )
        .child(
            el().pin(Some(AREA_L), None, None, Some(7.))
                .inert()
                .a11y_hidden()
                .child(caption("CHEAPER", ink)),
        )
        .child(
            el().pin(None, None, Some(10.), Some(7.))
                .inert()
                .a11y_hidden()
                .child(caption("PRICIER →", ink)),
        );

    // Score ticks down the left edge.
    let mut t = (mx.y_lo / mx.y_step).ceil() * mx.y_step;
    while t <= mx.y_hi {
        let y = y_of(t, mx);
        pad = pad.child(
            row()
                .absolute(0., y - 6.)
                .w(AREA_L - 6.)
                .h(12.)
                .justify_end()
                .center_y()
                .inert()
                .a11y_hidden()
                .child(caption(format!("{t:.0}"), ink)),
        );
        t += mx.y_step;
    }

    if let Some(act) = active_point {
        let d = 26.;
        pad = pad.child(
            el().key("matrix-halo")
                .absolute(act.x - d * 0.5, act.y - d * 0.5)
                .size(d, d)
                .radius(d * 0.5)
                .fill(p.accent.alpha(if p.light { 0.14 } else { 0.2 }))
                .border(1.5, p.accent.alpha(0.5))
                .a11y_hidden()
                .inert()
                .motion(spring),
        );
    }

    // Dots and labels are drawn only: the plot itself takes presses and
    // drags, so a drag can start on a dot.
    for pt in &mx.points {
        let is_act = active_point.is_some_and(|a| a.provider == pt.provider && a.model == pt.model);
        let size = if pt.frontier { 9. } else { 7. };
        let dot = if is_act {
            el().size(size, size)
        } else if pt.frontier {
            el().size(size, size)
                .radius(size * 0.5)
                .fill(p.accent)
                .border(1.5, p.bg)
        } else {
            el().size(size, size)
                .radius(size * 0.5)
                .fill(p.bg)
                .border(1.5, p.text_muted.alpha(0.7))
        };
        pad = pad.child(
            el().key(format!("matrix-node-{}-{}", pt.provider, pt.model))
                .absolute(pt.x - size * 0.5, pt.y - size * 0.5)
                .size(size, size)
                .inert()
                .a11y_hidden()
                .motion(spring)
                .child(dot),
        );
        if let Some((lx, ly, w)) = pt.label {
            let color = if is_act {
                p.text
            } else if pt.frontier {
                p.text.alpha(0.9)
            } else {
                p.text_muted
            };
            let mut chip = row()
                .key(format!("matrix-lbl-{}-{}", pt.provider, pt.model))
                .absolute(lx, ly)
                .w(w)
                .h(LABEL_H)
                .center()
                .radius(4.)
                .fill(if is_act {
                    p.accent.alpha(if p.light { 0.2 } else { 0.3 })
                } else {
                    p.bg.alpha(0.9)
                })
                .inert()
                .a11y_hidden()
                .motion(spring)
                .child(
                    text(pt.short.clone())
                        .font(TextStyle::new(rem(10.5), if is_act { 600. } else { 500. }))
                        .color(color)
                        .single_line(),
                );
            if is_act {
                chip = chip.border(1., p.accent.alpha(0.55));
            }
            pad = pad.child(chip);
        }
    }

    if let Some(act) = active_point {
        pad = pad.child(
            el().key("matrix-puck")
                .absolute(act.x - 8., act.y - 8.)
                .size(16., 16.)
                .radius(8.)
                .fill(if p.light { p.text } else { rgb(0xedf0f5) })
                .border(
                    1.5,
                    if p.light {
                        p.bg
                    } else {
                        Color::WHITE.alpha(0.5)
                    },
                )
                .shadow(2., 8., Color::BLACK.alpha(0.45))
                .a11y_hidden()
                .inert()
                .motion(spring)
                .motion_intent(MotionIntent::OVERSHOOT),
        );
    }

    let drag_slot = slot.clone();
    pad.on_drag(move |s: &mut State, d, _| matrix_drag(s, &drag_slot, d))
}

/// Connected models with no published score, as plain chips under the plot.
fn unrated_row(
    p: &Palette,
    unrated: &[Unrated],
    active: Option<&(String, String)>,
    slot: &Slot,
) -> El {
    let mut chips = row().key("matrix-unrated").flex_wrap().gap(4.);
    for u in unrated {
        let on = active.is_some_and(|(pv, md)| pv == &u.provider && md == &u.model);
        let (pv, md) = (u.provider.clone(), u.model.clone());
        let pick_slot = slot.clone();
        chips = chips.child(
            el().key(format!("matrix-unrated-{}-{}", u.provider, u.model))
                .role("button")
                .label(format!("{} ({})", pretty_model(&u.model), u.provider_name))
                .help(format!("{} · no published score", u.provider_name))
                .px(8.)
                .h(22.)
                .center()
                .radius(6.)
                .continuous_corners()
                .fill(if on { p.active } else { Color::TRANSPARENT })
                .border(1., p.border)
                .hover(p.hover)
                .cursor(Cursor::Pointer)
                .on_press(move |s: &mut State| pick(s, pick_slot.clone(), pv.clone(), md.clone()))
                .child(
                    text(pretty_model(&u.model))
                        .font(TextStyle::new(rem(11.), if on { 500. } else { 400. }))
                        .color(if on { p.text } else { p.text_muted })
                        .single_line(),
                ),
        );
    }
    col()
        .px(12.)
        .pb(10.)
        .gap(6.)
        .child(caption("NOT BENCHMARKED", p.text_faint.alpha(0.75)))
        .child(chips)
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
            let target = slot
                .get_mut(s)
                .and_then(|m| m.matrix_drag.take())
                .or(snapped);
            if d.cancelled() {
                return;
            }
            if let Some((pv, md)) = target {
                pick(s, slot.clone(), pv, md);
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
        assert!(!mx.points.iter().any(|pt| pt.model == "gpt-6.1-mini"));
        find(&mx, "anthropic", "claude-fable-5-2");
        find(&mx, "gemini", "gemini-4-carbon");
        // No published score: listed, never placed on a guess.
        assert!(!mx.points.iter().any(|pt| pt.model == "glm-5.4"));
        assert!(mx.unrated.iter().any(|u| u.model == "glm-5.4"));
        assert!(mx.unrated.iter().any(|u| u.model == "claude-haiku-5-5"));
    }

    #[test]
    fn fable_outscores_opus_at_every_level_and_dots_follow_the_slider() {
        let mut m = fixture_matrix();
        let mut last_fable_y = f32::MAX;
        for level in ["low", "medium", "high", "xhigh", "max"] {
            m.effort = Some(level.into());
            let mx = build(&m);
            let fable = find(&mx, "anthropic", "claude-fable-5-2");
            let opus = find(&mx, "anthropic", "claude-opus-5-5");
            assert!(fable.score > opus.score, "Fable above Opus at {level}");
            assert!(fable.y < opus.y, "drawn higher at {level}");
            assert!(fable.y <= last_fable_y, "more thinking never drops the dot");
            last_fable_y = fable.y;
        }
        m.effort = Some("max".into());
        assert_eq!(
            find(&build(&m), "anthropic", "claude-fable-5-2").score,
            64.8
        );
        m.effort = Some("low".into());
        assert_eq!(
            find(&build(&m), "anthropic", "claude-fable-5-2").score,
            53.9
        );
        // Gemini tops out at high: the slider past it keeps it at high.
        m.effort = Some("max".into());
        assert_eq!(find(&build(&m), "gemini", "gemini-4-carbon").score, 60.1);
    }

    #[test]
    fn a_model_without_a_thinking_knob_holds_still() {
        let mut m = fixture_matrix();
        let mut seen = Vec::new();
        for level in ["low", "medium", "high", "xhigh", "max"] {
            m.effort = Some(level.into());
            let mx = build(&m);
            let terra = find(&mx, NATIVE, "gpt-6.1-terra");
            seen.push((terra.score, terra.x, terra.y));
        }
        assert!(seen.windows(2).all(|w| w[0] == w[1]), "{seen:?}");
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

        // Press on a dot (Opus), drag onto another (Carbon), let go.
        let slot = Slot::chat("t1");
        let (opus, _) = rt
            .presented("matrix-node-anthropic-claude-opus-5-5")
            .expect("opus dot");
        let (carbon, _) = rt
            .presented("matrix-node-gemini-gemini-4-carbon")
            .expect("carbon dot");
        let (ox, oy) = (opus[0] + opus[2] * 0.5, opus[1] + opus[3] * 0.5);
        let (cx, cy) = (carbon[0] + carbon[2] * 0.5, carbon[1] + carbon[3] * 0.5);
        rt.pointer_move(ox, oy);
        rt.pointer_down(ox, oy);
        rt.frame(1.1);
        assert_eq!(
            slot.get(&rt.state).and_then(|m| m.matrix_drag.clone()),
            Some(("anthropic".into(), "claude-opus-5-5".into())),
            "press on a dot scrubs to it"
        );
        for step in 1..=8 {
            let t = step as f32 / 8.;
            rt.pointer_move(ox + (cx - ox) * t, oy + (cy - oy) * t);
            rt.frame(1.1 + f64::from(step) / 60.);
        }
        assert_eq!(
            slot.get(&rt.state).and_then(|m| m.matrix_drag.clone()),
            Some(("gemini".into(), "gemini-4-carbon".into())),
            "drag follows to the dot under the pointer"
        );
        rt.pointer_up(cx, cy);
        rt.frame(1.4);
        let m = slot.get(&rt.state).expect("picker");
        assert!(!m.is_open(), "release picks and closes");
        assert_eq!(
            rt.state.prefs.recent_models.first(),
            Some(&("gemini".to_string(), "gemini-4-carbon".to_string())),
            "the dot under the pointer was picked"
        );
    }
}
