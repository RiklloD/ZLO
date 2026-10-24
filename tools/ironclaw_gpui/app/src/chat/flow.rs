//! The transcript as the list shows it, after Zeron's turn flow (see
//! UX_REFERENCE.md). Runs of activity (reasoning, tool calls, skills) fold
//! into one disclosure line that stays open while it is the live tail and
//! closes once the answer starts. A pending row trails the newest content
//! while a run is live, and a reserve row keeps room under a freshly sent
//! message so the answer streams down into it without the list scrolling.

use std::collections::HashMap;
use std::time::Instant;

use ironclaw_gpui_client::transcript::{Row, ToolStatus};

use crate::design::styles::Style;
use crate::design::{Palette, kit, motion};
use crate::ui::*;

use super::ChatView;

#[derive(Clone, PartialEq)]
pub(super) enum Item {
    Row(Row),
    /// Consecutive activity rows, keyed by the first one.
    Activity { key: String, rows: Vec<Row>, live: bool },
    Pending { stopping: bool },
    Reserve,
}

impl Item {
    pub(super) fn key(&self) -> &str {
        match self {
            Item::Row(row) => row.key(),
            Item::Activity { key, .. } => key,
            Item::Pending { .. } => "\u{0}pending",
            Item::Reserve => "\u{0}reserve",
        }
    }
}

fn is_activity(row: &Row) -> bool {
    matches!(
        row,
        Row::Thinking { .. } | Row::Tool { .. } | Row::Skills { .. } | Row::WorkSummary { .. }
    )
}

/// Fold `rows` into list items. `live` marks a run in flight; the trailing
/// activity group is then the one being written and opens by itself.
pub(super) fn fold(rows: &[Row], live: bool, stopping: bool, reserve: bool) -> Vec<Item> {
    let mut items: Vec<Item> = Vec::with_capacity(rows.len() + 2);
    for row in rows {
        if is_activity(row) {
            if let Some(Item::Activity { rows, .. }) = items.last_mut() {
                rows.push(row.clone());
                continue;
            }
            items.push(Item::Activity {
                key: format!("act-{}", row.key()),
                rows: vec![row.clone()],
                live: false,
            });
        } else {
            items.push(Item::Row(row.clone()));
        }
    }
    if live && let Some(Item::Activity { live, .. }) = items.last_mut() {
        *live = true;
    }
    if live || stopping {
        items.push(Item::Pending { stopping });
    }
    if reserve {
        items.push(Item::Reserve);
    }
    items
}

/// "Thought process · used 3 tools", Zeron's one-line summary of a group.
fn summary(rows: &[Row]) -> String {
    let thinking = rows.iter().any(|r| matches!(r, Row::Thinking { .. }));
    let tools = rows.iter().filter(|r| matches!(r, Row::Tool { .. })).count();
    let failed = rows
        .iter()
        .filter(|r| matches!(r, Row::Tool { status: ToolStatus::Failed, .. }))
        .count();
    let mut parts = Vec::new();
    if thinking {
        parts.push("Thought process".to_string());
    }
    if tools > 0 {
        let plural = if tools == 1 { "" } else { "s" };
        parts.push(format!("used {tools} tool{plural}"));
    }
    if failed > 0 {
        parts.push(format!("{failed} failed"));
    }
    if rows.iter().any(|r| matches!(r, Row::Skills { .. })) {
        parts.push("loaded skills".into());
    }
    for row in rows {
        if let Row::WorkSummary { text, .. } = row {
            parts.push(text.clone());
        }
    }
    let mut line = parts.join(" · ");
    if let Some(first) = line.get(..1) {
        line = first.to_uppercase() + &line[1..];
    }
    line
}

/// Whether a group is drawn open: the reader's own toggle wins, otherwise
/// it follows the run (open while live, closed after).
pub(super) fn is_open(key: &str, live: bool, toggles: &HashMap<String, bool>) -> bool {
    toggles.get(key).copied().unwrap_or(live)
}

pub(super) fn render_activity(
    key: &str,
    rows: &[Row],
    live: bool,
    open: bool,
    weak: &WeakEntity<ChatView>,
    cx: &App,
) -> AnyElement {
    let p = *Palette::of(cx);
    let s = *Style::of(cx);
    let turn = motion::state_t(&format!("{key}:chev"), open, s.slide, cx.reduce_motion());
    let label = summary(rows);
    let toggle_key = key.to_string();
    let weak = weak.clone();
    let header = h_flex()
        .id(SharedString::from(format!("{key}-head")))
        .role(Role::Button)
        .aria_label(label.clone())
        .gap_1p5()
        .py_0p5()
        .items_center()
        .text_sm()
        .text_color(p.text_muted)
        .cursor_pointer()
        .hover(|d| d.text_color(p.text))
        .child(
            Icon::new(IconName::ChevronRight)
                .xsmall()
                .rotate(gpui_kit::radians(turn * std::f32::consts::FRAC_PI_2)),
        )
        .child(if live {
            kit::shimmer_text(SharedString::from(format!("{key}-shimmer")), label, cx)
                .into_any_element()
        } else {
            div().child(label).into_any_element()
        })
        .on_click(move |_, _, cx| {
            let key = toggle_key.clone();
            let _ = weak.update(cx, |this, cx| {
                let now = this.activity_open(&key, live);
                this.toggles.insert(key, !now);
                cx.notify();
            });
        });
    let body = open.then(|| {
        let children = v_flex()
            .ml(px(7.))
            .pl(px(14.))
            .border_l_1()
            .border_color(p.border)
            .gap_2()
            .py_1()
            .children(rows.iter().map(|row| activity_child(row, cx)));
        motion::fade_in(children, SharedString::from(format!("{key}-body")), s.enter, 4.)
    });
    v_flex()
        .w_full()
        .gap_1()
        .child(header)
        .children(body)
        .into_any_element()
}

fn activity_child(row: &Row, cx: &App) -> AnyElement {
    let p = Palette::of(cx);
    let line = |icon: IconName, title: String, detail: Option<String>| {
        v_flex()
            .gap_0p5()
            .text_xs()
            .child(
                h_flex()
                    .gap_1p5()
                    .items_center()
                    .text_color(p.text_muted)
                    .child(Icon::new(icon).xsmall().flex_none())
                    .child(div().min_w_0().truncate().child(title)),
            )
            .when_some(detail, |d, detail| {
                let folded = detail.split_whitespace().collect::<Vec<_>>().join(" ");
                d.child(
                    div()
                        .pl(px(18.))
                        .line_clamp(3)
                        .text_color(p.text_faint)
                        .child(folded),
                )
            })
            .into_any_element()
    };
    match row {
        // Reasoning reads as prose: muted, full length, no icon tile.
        Row::Thinking { text, .. } => div()
            .text_xs()
            .line_height(px(18.))
            .text_color(p.text_muted)
            .whitespace_normal()
            .child(text.clone())
            .into_any_element(),
        Row::Tool {
            title,
            detail,
            status,
            ..
        } => {
            let icon = match status {
                ToolStatus::Running => IconName::LoaderCircle,
                ToolStatus::Completed => IconName::CircleCheck,
                ToolStatus::Failed => IconName::CircleX,
            };
            line(icon, title.clone(), detail.clone())
        }
        Row::Skills { names, .. } => line(
            IconName::BookOpen,
            format!("Using skills: {}", names.join(", ")),
            None,
        ),
        Row::WorkSummary { text, .. } => line(IconName::FileText, text.clone(), None),
        _ => div().into_any_element(),
    }
}

/// Zeron's pending row: breathing dots, a shimmering label and the seconds
/// since the run began. It loops only while a run is live.
pub(super) fn render_pending(stopping: bool, since: Option<Instant>, cx: &App) -> AnyElement {
    let p = *Palette::of(cx);
    let secs = since.map(|t| t.elapsed().as_secs()).unwrap_or(0);
    h_flex()
        .gap_2()
        .items_center()
        .text_sm()
        .child(kit::pulse_dots("working-dots", p.accent))
        .child(kit::shimmer_text(
            "working-label",
            if stopping { "Stopping…" } else { "Working…" },
            cx,
        ))
        .child(div().text_xs().text_color(p.text_faint).child(format!("{secs}s")))
        .into_any_element()
}
