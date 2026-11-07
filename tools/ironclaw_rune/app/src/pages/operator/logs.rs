//! Operator → Logs: the gateway's in-memory log ring (`operator.logs`),
//! newest first, with level and target filters and paging to older lines.
//! GPUI `pages/operator/logs.rs`.

use ironclaw_gpui_client::product_wire::{RebornLogEntry, RebornLogLevel, RebornOperatorLogsQuery};
use rune::{Color, El, TextEdit, col, row, text};
use rune_kit::Kit;

use crate::design::Palette;
use crate::jobs::{self, Generation};
use crate::pages::c_widgets::{
    Look, MONO_XS, Size, button, empty_state, error_line, input, loading, muted, rule, spinner,
};
use crate::state::State;

const PAGE: u32 = 100;

const LEVELS: [(Option<RebornLogLevel>, &str); 6] = [
    (None, "All"),
    (Some(RebornLogLevel::Error), "Error"),
    (Some(RebornLogLevel::Warn), "Warn"),
    (Some(RebornLogLevel::Info), "Info"),
    (Some(RebornLogLevel::Debug), "Debug"),
    (Some(RebornLogLevel::Trace), "Trace"),
];

#[derive(Default)]
pub struct LogsPanel {
    pub entries: Vec<RebornLogEntry>,
    pub next_cursor: Option<String>,
    pub source: Option<String>,
    pub level: Option<RebornLogLevel>,
    /// Only this run's lines (a failed run's Logs button).
    pub run: Option<String>,
    pub target: TextEdit,
    /// Bumped per load so a slower, older answer can't overwrite a newer one.
    pub generation: Generation,
    pub loading: bool,
    pub loaded: bool,
    pub error: Option<String>,
}

fn st(s: &mut State) -> Option<&mut LogsPanel> {
    s.pages.operator.logs.as_mut()
}

fn target_edit(s: &mut State) -> &mut TextEdit {
    let o = &mut s.pages.operator;
    match o.logs.as_mut() {
        Some(p) => &mut p.target,
        None => &mut o.status_scratch,
    }
}

pub fn refresh(s: &mut State) {
    load(s, None);
}

fn query(p: &LogsPanel, cursor: Option<String>) -> RebornOperatorLogsQuery {
    let target = p.target.text.trim().to_string();
    RebornOperatorLogsQuery {
        limit: Some(PAGE),
        cursor,
        level: p.level,
        target: (!target.is_empty()).then_some(target),
        run_id: p.run.clone(),
        ..Default::default()
    }
}

/// `cursor: None` reloads the newest page; `Some` appends older lines.
fn load(s: &mut State, cursor: Option<String>) {
    let Some(p) = st(s) else { return };
    let older = cursor.is_some();
    let query = query(p, cursor);
    let generation = p.generation.bump();
    p.loading = true;
    jobs::run(
        s,
        "operator",
        move |c| async move { c.operator_logs(&query).await },
        move |s, result, _| {
            let Some(p) = st(s) else { return };
            if p.generation != generation {
                return;
            }
            p.loading = false;
            p.loaded = true;
            match result.map(|r| (r.logs, r.message)) {
                Ok((Some(page), _)) => {
                    if !older {
                        p.entries.clear();
                    }
                    p.entries.extend(page.entries);
                    p.next_cursor = page.next_cursor;
                    p.source = Some(page.source);
                    p.error = None;
                }
                Ok((None, message)) => p.error = Some(message),
                Err(e) => p.error = Some(e.to_string()),
            }
        },
    );
}

pub fn set_run(s: &mut State, run: Option<String>) {
    if let Some(p) = st(s) {
        p.run = run;
    }
    refresh(s);
}

fn set_level(s: &mut State, level: Option<RebornLogLevel>) {
    if let Some(p) = st(s) {
        p.level = level;
    }
    refresh(s);
}

fn level_color(p: &Palette, level: RebornLogLevel) -> Color {
    match level {
        RebornLogLevel::Error => p.danger,
        RebornLogLevel::Warn => p.warning,
        RebornLogLevel::Info => p.text,
        RebornLogLevel::Debug | RebornLogLevel::Trace => p.text_muted,
    }
}

fn level_label(level: RebornLogLevel) -> &'static str {
    match level {
        RebornLogLevel::Error => "ERROR",
        RebornLogLevel::Warn => "WARN",
        RebornLogLevel::Info => "INFO",
        RebornLogLevel::Debug => "DEBUG",
        RebornLogLevel::Trace => "TRACE",
    }
}

/// One log line. Shared with the inspector's thread logs.
pub(super) fn log_row(p: &Palette, entry: &RebornLogEntry) -> El {
    let time = entry
        .timestamp
        .with_timezone(&chrono::Local)
        .format("%H:%M:%S%.3f")
        .to_string();
    let color = level_color(p, entry.level);
    col()
        .key(format!("log-{}", entry.id))
        .w_full()
        .child(
            col()
                .w_full()
                .py(4.)
                .child(
                    row()
                        .w_full()
                        .gap(8.)
                        .child(text(time).font(MONO_XS).color(p.text_muted))
                        .child(
                            text(level_label(entry.level))
                                .font(MONO_XS)
                                .color(color)
                                .w(44.),
                        )
                        .child(
                            text(entry.target.clone())
                                .font(MONO_XS)
                                .color(p.text_muted)
                                .truncate()
                                .grow(1.)
                                .min_w(0.),
                        ),
                )
                .child(
                    text(entry.message.clone())
                        .font(MONO_XS)
                        .color(color)
                        .wrap()
                        .selectable(true),
                ),
        )
        .child(rule(p))
}

pub fn view(s: &State, k: &Kit, panel: &LogsPanel) -> El {
    let p = &s.palette;
    let levels = row().gap(2.).children(LEVELS.iter().map(|(level, label)| {
        let level = *level;
        let look = if panel.level == level {
            Look::Primary
        } else {
            Look::Ghost
        };
        button(*label, look, Size::XSmall)
            .on_press(move |s: &mut State| set_level(s, level))
            .build(k)
            .key(format!("log-level-{label}"))
    }));
    let mut c = col().w_full().gap(8.);
    if let Some(e) = &panel.error {
        c = c.child(error_line(p, e));
    }
    c = c.child(
        row().w_full().gap(8.).center_y().child(levels).child(
            col().grow(1.).min_w(0.).child(
                input(&panel.target)
                    .placeholder("Target contains, e.g. ironclaw_llm (Enter)")
                    .on_edit(target_edit)
                    .on_submit(|s: &mut State, _| refresh(s))
                    .build(k)
                    .key("log-target"),
            ),
        ),
    );
    if let Some(run) = &panel.run {
        let short: String = run.chars().take(8).collect();
        c = c.child(
            row()
                .gap(8.)
                .center_y()
                .child(muted(p, format!("Run {short}…")))
                .child(
                    button("All runs", Look::Ghost, Size::XSmall)
                        .on_press(|s: &mut State| set_run(s, None))
                        .build(k)
                        .key("clear-run"),
                ),
        );
    }
    if let Some(src) = &panel.source {
        c = c.child(muted(
            p,
            format!(
                "Newest first. Source: {src}. The ring is in memory and starts empty when the gateway restarts."
            ),
        ));
    }
    if !panel.loaded {
        c = c.child(spinner(k));
    }
    if panel.loaded && panel.entries.is_empty() && panel.error.is_none() {
        c = c.child(empty_state(p, "No log lines match."));
    }
    c = c.child(
        col()
            .w_full()
            .children(panel.entries.iter().map(|e| log_row(p, e))),
    );
    if let Some(cursor) = panel.next_cursor.clone() {
        c = c.child(
            row().child(
                loading(
                    button("Load older", Look::Ghost, Size::Small)
                        .on_press(move |s: &mut State| load(s, Some(cursor.clone()))),
                    panel.loading,
                )
                .build(k)
                .key("older-logs"),
            ),
        );
    }
    c
}
