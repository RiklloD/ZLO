//! The gateway's in-memory log ring (`operator.logs`), newest first, with
//! level and target filters and paging to older lines.

use ironclaw_gpui_client::Backend;
use ironclaw_gpui_client::product_wire::{RebornLogEntry, RebornLogLevel, RebornOperatorLogsQuery};

use super::muted;
use crate::jobs;
use crate::pages::{empty_state, error_line};
use crate::ui::*;

const PAGE: u32 = 100;

const LEVELS: [(Option<RebornLogLevel>, &str); 6] = [
    (None, "All"),
    (Some(RebornLogLevel::Error), "Error"),
    (Some(RebornLogLevel::Warn), "Warn"),
    (Some(RebornLogLevel::Info), "Info"),
    (Some(RebornLogLevel::Debug), "Debug"),
    (Some(RebornLogLevel::Trace), "Trace"),
];

pub struct LogsPanel {
    backend: Backend,
    entries: Vec<RebornLogEntry>,
    next_cursor: Option<String>,
    source: Option<String>,
    level: Option<RebornLogLevel>,
    target: Entity<InputState>,
    loading: bool,
    loaded: bool,
    error: Option<SharedString>,
    _target_events: Subscription,
}

impl LogsPanel {
    pub fn new(backend: Backend, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let target = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Target contains, e.g. ironclaw_llm (Enter)")
        });
        let target_events = cx.subscribe(&target, |this, _, event, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.refresh(cx);
            }
        });
        let mut this = Self {
            backend,
            entries: Vec::new(),
            next_cursor: None,
            source: None,
            level: None,
            target,
            loading: false,
            loaded: false,
            error: None,
            _target_events: target_events,
        };
        this.refresh(cx);
        this
    }

    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.load(None, cx);
    }

    fn query(&self, cursor: Option<String>, cx: &App) -> RebornOperatorLogsQuery {
        let target = self.target.read(cx).value().trim().to_string();
        RebornOperatorLogsQuery {
            limit: Some(PAGE),
            cursor,
            level: self.level,
            target: (!target.is_empty()).then_some(target),
            ..Default::default()
        }
    }

    /// `cursor: None` reloads the newest page; `Some` appends older lines.
    fn load(&mut self, cursor: Option<String>, cx: &mut Context<Self>) {
        let older = cursor.is_some();
        let query = self.query(cursor, cx);
        self.loading = true;
        cx.notify();
        jobs::run(
            &self.backend,
            cx,
            move |c| async move { c.operator_logs(&query).await },
            move |this, result, cx| {
                this.loading = false;
                this.loaded = true;
                match result.map(|r| (r.logs, r.message)) {
                    Ok((Some(page), _)) => {
                        if !older {
                            this.entries.clear();
                        }
                        this.entries.extend(page.entries);
                        this.next_cursor = page.next_cursor;
                        this.source = Some(page.source);
                        this.error = None;
                    }
                    Ok((None, message)) => this.error = Some(message.into()),
                    Err(e) => this.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    fn set_level(&mut self, level: Option<RebornLogLevel>, cx: &mut Context<Self>) {
        self.level = level;
        self.refresh(cx);
    }
}

fn level_color(level: RebornLogLevel, cx: &App) -> gpui_kit::Hsla {
    let theme = cx.theme();
    match level {
        RebornLogLevel::Error => theme.danger,
        RebornLogLevel::Warn => theme.warning,
        RebornLogLevel::Info => theme.foreground,
        RebornLogLevel::Debug | RebornLogLevel::Trace => theme.muted_foreground,
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
pub(super) fn log_row(entry: &RebornLogEntry, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let time = entry
        .timestamp
        .with_timezone(&chrono::Local)
        .format("%H:%M:%S%.3f")
        .to_string();
    v_flex()
        .py_1()
        .border_b_1()
        .border_color(theme.border)
        .text_xs()
        .font_family(theme.mono_font_family.clone())
        .child(
            h_flex()
                .gap_2()
                .text_color(theme.muted_foreground)
                .child(time)
                .child(
                    div()
                        .w(px(44.))
                        .text_color(level_color(entry.level, cx))
                        .child(level_label(entry.level)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(entry.target.clone()),
                ),
        )
        .child(
            div()
                .whitespace_normal()
                .text_color(level_color(entry.level, cx))
                .child(entry.message.clone()),
        )
}

impl Render for LogsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let levels =
            h_flex()
                .gap_0p5()
                .children(LEVELS.iter().enumerate().map(|(ix, (level, label))| {
                    let level = *level;
                    Button::new(("log-level", ix))
                        .label(*label)
                        .xsmall()
                        .when(self.level == level, |b| b.primary())
                        .when(self.level != level, |b| b.ghost())
                        .on_click(cx.listener(move |this, _, _, cx| this.set_level(level, cx)))
                }));
        let rows: Vec<AnyElement> = self
            .entries
            .iter()
            .map(|e| log_row(e, cx).into_any_element())
            .collect();
        v_flex()
            .gap_2()
            .when_some(self.error.as_ref(), |d, e| d.child(error_line(e, cx)))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(levels)
                    .child(div().flex_1().child(Input::new(&self.target))),
            )
            .when_some(self.source.as_ref(), |d, s| {
                d.child(muted(
                    format!(
                        "Newest first. Source: {s}. The ring is in memory and starts empty when the gateway restarts."
                    ),
                    cx,
                ))
            })
            .when(!self.loaded, |d| d.child(Spinner::new()))
            .when(self.loaded && rows.is_empty() && self.error.is_none(), |d| {
                d.child(empty_state("No log lines match.", cx))
            })
            .child(v_flex().children(rows))
            .when_some(self.next_cursor.clone(), |d, cursor| {
                d.child(
                    Button::new("older-logs")
                        .label("Load older")
                        .ghost()
                        .small()
                        .loading(self.loading)
                        .disabled(self.loading)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.load(Some(cursor.clone()), cx)
                        })),
                )
            })
    }
}
