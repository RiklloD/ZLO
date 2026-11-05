//! The status bar's context meter. The fill is coloured by level (warning
//! from 80 % of the window, exceeded from 100 %); a click opens what fills
//! the window — a stacked bar, every category with tokens and share, the
//! largest tool owners, and whether the total is the provider's count or an
//! estimate. From 80 % a one-line banner over the thread offers "Compact
//! now" (the runtime's own compaction command, sent as a turn) and "New
//! thread from summary" (a fork seeded with a digest of the conversation).

use std::collections::HashSet;

use ironclaw_gpui_client::orchestration::ForkOptions;
use ironclaw_gpui_client::usage::{
    ContextCategory, ContextLevel, ContextSource, ContextToolOwner, ThreadContextReport,
    ThreadTelemetry,
};

use super::{Workspace, tokens};
use crate::design::{Palette, kit};
use crate::jobs;
use crate::ui::*;

/// Width of the popover's stacked bar.
const BAR_WIDTH: f32 = 288.;
/// Tool owners listed under the categories.
const TOP_OWNERS: usize = 4;

#[derive(Default)]
pub(super) struct ContextMeter {
    open: bool,
    /// The pointer is on the meter: a click there toggles, so the
    /// popover's click-outside must not close it first.
    hover: bool,
    report: Option<ThreadContextReport>,
    loading: bool,
    /// An action (compact, summary fork) is in flight.
    busy: bool,
    error: Option<SharedString>,
    /// Threads whose banner was closed, until their level changes.
    dismissed: HashSet<(String, u8)>,
}

/// The meter's level from the latest telemetry; `Normal` without a window.
pub(super) fn level_of(t: Option<&ThreadTelemetry>) -> ContextLevel {
    match t.and_then(|t| t.context_used.map(|used| (used, t.context_max))) {
        Some((used, max)) => ContextLevel::of(used, max),
        None => ContextLevel::Normal,
    }
}

fn level_key(level: ContextLevel) -> u8 {
    match level {
        ContextLevel::Normal => 0,
        ContextLevel::Warning => 1,
        ContextLevel::Exceeded => 2,
    }
}

pub(super) fn level_tint(level: ContextLevel, p: &Palette) -> Hsla {
    match level {
        ContextLevel::Normal => p.text_faint,
        ContextLevel::Warning => p.warning,
        ContextLevel::Exceeded => p.danger,
    }
}

pub(super) fn category_label(category: ContextCategory) -> &'static str {
    match category {
        ContextCategory::SystemPrompt => "System prompt",
        ContextCategory::Instructions => "Instructions & skills",
        ContextCategory::ToolDefinitions => "Tool definitions",
        ContextCategory::UserMessages => "Your messages",
        ContextCategory::AssistantMessages => "Replies",
        ContextCategory::ToolResults => "Tool results",
        ContextCategory::CompactionSummaries => "Compaction summaries",
        ContextCategory::Attachments => "Attachments",
        ContextCategory::Unattributed => "Agent built-ins (unmeasured)",
    }
}

/// One fixed hue per category, so a category keeps its colour across
/// threads and themes.
fn category_color(category: ContextCategory) -> Hsla {
    let hue = match category {
        ContextCategory::SystemPrompt => 0.62,
        ContextCategory::Instructions => 0.74,
        ContextCategory::ToolDefinitions => 0.08,
        ContextCategory::UserMessages => 0.52,
        ContextCategory::AssistantMessages => 0.38,
        ContextCategory::ToolResults => 0.13,
        ContextCategory::CompactionSummaries => 0.86,
        ContextCategory::Attachments => 0.96,
        ContextCategory::Unattributed => 0.,
    };
    let saturation = if category == ContextCategory::Unattributed {
        0.
    } else {
        0.55
    };
    gpui_kit::hsla(hue, saturation, 0.55, 1.)
}

pub(super) fn owner_label(owner: &ContextToolOwner) -> String {
    match owner {
        ContextToolOwner::Builtin => "IronClaw built-ins".into(),
        ContextToolOwner::Mcp(server) => format!("MCP · {server}"),
        ContextToolOwner::Extension(name) => name.clone(),
    }
}

pub(super) fn source_label(source: ContextSource) -> &'static str {
    match source {
        ContextSource::Provider => "Total exact from provider usage · split estimated",
        ContextSource::Estimated => "Estimated (no provider count yet)",
    }
}

fn percent(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.
    } else {
        part as f64 * 100. / whole as f64
    }
}

/// Segment widths of the stacked bar: each category's share of `width`,
/// with every non-empty one at least a hairline wide.
pub(super) fn segments(report: &ThreadContextReport, width: f32) -> Vec<(ContextCategory, f32)> {
    let total = report.used.max(1) as f32;
    report
        .categories
        .iter()
        .filter(|c| c.tokens > 0)
        .map(|c| (c.category, (c.tokens as f32 / total * width).max(2.)))
        .collect()
}

impl ContextMeter {
    /// Thread switch: the popover belongs to the thread it was opened on.
    pub(super) fn close(&mut self) {
        self.open = false;
        self.error = None;
    }
}

impl Workspace {
    /// The report shown, when it belongs to the open thread.
    fn context_report(&self) -> Option<&ThreadContextReport> {
        let active = self.active.as_ref().map(|(id, _)| id.as_str())?;
        self.context
            .report
            .as_ref()
            .filter(|r| r.thread_id == active)
    }

    /// Fetch the open thread's report (cheap: the backend estimates once
    /// per turn).
    pub(super) fn load_context(&mut self, cx: &mut Context<Self>) {
        let (Some(backend), Some((id, _))) = (self.backend.clone(), self.active.as_ref()) else {
            return;
        };
        let id = id.clone();
        let asked = id.clone();
        self.context.loading = true;
        jobs::run(
            &backend,
            cx,
            move |c| async move { c.thread_context(&id).await },
            move |this, result, cx| {
                this.context.loading = false;
                if this.active.as_ref().map(|(id, _)| id.as_str()) != Some(asked.as_str()) {
                    return;
                }
                match result {
                    Ok(report) => {
                        this.context.report = report;
                        this.context.error = None;
                    }
                    Err(e) => this.context.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    /// After a telemetry change: keep an open popover current, and have the
    /// banner's actions ready once the window is filling up.
    pub(super) fn context_telemetry_changed(&mut self, cx: &mut Context<Self>) {
        let stale = self.context_report().map(|r| r.updated_at)
            != Some(self.telemetry.as_ref().and_then(|t| t.updated_at));
        let filling = level_of(self.telemetry.as_ref()) != ContextLevel::Normal;
        if stale && (self.context.open || filling) {
            self.load_context(cx);
        }
    }

    fn toggle_context(&mut self, cx: &mut Context<Self>) {
        self.context.open = !self.context.open;
        if self.context.open {
            self.load_context(cx);
        }
        cx.notify();
    }

    /// Send the runtime's compaction command as a turn of the open thread.
    fn compact_now(&mut self, cx: &mut Context<Self>) {
        let (Some(backend), Some((id, _)), Some(channel)) = (
            self.backend.clone(),
            self.active.as_ref(),
            self.session
                .as_ref()
                .and_then(|s| s.session_channel_extension_id.clone()),
        ) else {
            return;
        };
        let Some(prompt) = self.context_report().and_then(|r| r.compact_prompt.clone()) else {
            return;
        };
        let id = id.clone();
        self.context.busy = true;
        self.context.open = false;
        cx.notify();
        jobs::run(
            &backend,
            cx,
            move |c| async move { c.send_message(&channel, &id, &prompt, Vec::new()).await },
            |this, result, cx| {
                this.context.busy = false;
                if let Err(e) = result {
                    this.context.error = Some(e.to_string().into());
                }
                cx.notify();
            },
        );
    }

    /// Fork the open thread into a fresh one seeded with a digest, and open it.
    fn new_from_summary(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(backend), Some((id, _))) = (self.backend.clone(), self.active.as_ref()) else {
            return;
        };
        let id = id.clone();
        self.context.busy = true;
        self.context.open = false;
        cx.notify();
        jobs::run_in(
            &backend,
            window,
            cx,
            move |c| async move {
                let options = ForkOptions {
                    summary: true,
                    ..ForkOptions::default()
                };
                c.fork_thread(&id, &options).await
            },
            |this, result, window, cx| {
                this.context.busy = false;
                match result {
                    Ok(forked) => {
                        this.refresh_inbox(cx);
                        this.open_thread(forked.fork_thread_id, window, cx);
                    }
                    Err(e) => this.context.error = Some(e.to_string().into()),
                }
                cx.notify();
            },
        );
    }

    /// The status bar's "Context" metric, clickable, coloured by level.
    pub(super) fn render_context_meter(&self, text: String, cx: &mut Context<Self>) -> AnyElement {
        let p = *Palette::of(cx);
        let level = level_of(self.telemetry.as_ref());
        let tint = level_tint(level, &p);
        let meter = h_flex()
            .id("status-context")
            .role(Role::Button)
            .aria_label("What fills the context window")
            .on_hover(cx.listener(|this, h: &bool, _, _| this.context.hover = *h))
            .gap_1()
            .items_center()
            .cursor_pointer()
            .hover(|s| s.text_color(p.text))
            .when(level != ContextLevel::Normal, |d| d.text_color(tint))
            .when(self.context.open, |d| d.text_color(p.text))
            .child(div().size(px(6.)).rounded_full().bg(tint))
            .child("Context")
            .child(text)
            .on_click(cx.listener(|this, _, _, cx| this.toggle_context(cx)));
        let popover = self.context.open.then(|| {
            let card = div()
                .occlude()
                .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    if this.context.hover {
                        return;
                    }
                    this.context.open = false;
                    cx.notify();
                }))
                .child(self.render_context_popover(cx));
            gpui_kit::deferred(div().absolute().bottom(px(22.)).right_0().child(card))
                .with_priority(1)
        });
        div()
            .relative()
            .child(meter)
            .children(popover)
            .into_any_element()
    }

    fn render_context_popover(&self, cx: &mut Context<Self>) -> AnyElement {
        let p = *Palette::of(cx);
        let pop = kit::popover("context-pop", cx);
        let head = div()
            .px_2()
            .pt_1()
            .pb_1()
            .text_xs()
            .font_medium()
            .text_color(p.text_faint)
            .child("Context window");
        let Some(report) = self.context_report() else {
            let note = match (&self.context.error, self.context.loading) {
                (Some(e), _) => e.clone(),
                (None, true) => "Measuring…".into(),
                (None, false) => "Nothing measured for this thread yet.".into(),
            };
            return pop(v_flex()
                .w(px(BAR_WIDTH + 24.))
                .p_2()
                .child(head)
                .child(div().px_2().text_xs().text_color(p.text_muted).child(note)));
        };
        let total = match report.max {
            Some(max) => format!(
                "{} / {} · {:.0}%",
                tokens(report.used),
                tokens(max),
                percent(report.used, max)
            ),
            None => tokens(report.used),
        };
        let bar = h_flex()
            .mx_2()
            .h(px(8.))
            .w(px(BAR_WIDTH))
            .rounded(px(4.))
            .overflow_hidden()
            .bg(p.hover)
            .children(
                segments(report, BAR_WIDTH)
                    .into_iter()
                    .map(|(category, w)| div().h_full().w(px(w)).bg(category_color(category))),
            );
        let rows = report.categories.iter().map(|c| {
            h_flex()
                .px_2()
                .gap_2()
                .items_center()
                .text_xs()
                .child(
                    div()
                        .size(px(8.))
                        .rounded(px(2.))
                        .bg(category_color(c.category)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(p.text_muted)
                        .child(category_label(c.category)),
                )
                .child(div().child(tokens(c.tokens)))
                .child(
                    div()
                        .w(px(40.))
                        .text_right()
                        .text_color(p.text_faint)
                        .child(format!("{:.0}%", percent(c.tokens, report.used))),
                )
        });
        let owners: Vec<_> = report.tool_owners.iter().take(TOP_OWNERS).collect();
        let owner_rows = owners.into_iter().map(|o| {
            h_flex()
                .px_2()
                .gap_2()
                .text_xs()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(p.text_muted)
                        .child(owner_label(&o.owner)),
                )
                .child(
                    div()
                        .text_color(p.text_faint)
                        .child(format!("{} tools", o.tools)),
                )
                .child(div().w(px(48.)).text_right().child(tokens(o.tokens)))
        });
        let has_owners = !report.tool_owners.is_empty();
        let hint = if report.compact_prompt.is_none() && report.auto_compacts {
            Some("This thread compacts by itself near the limit.")
        } else {
            None
        };
        pop(v_flex()
            .w(px(BAR_WIDTH + 24.))
            .py_1()
            .gap_1p5()
            .child(
                h_flex().justify_between().child(head).child(
                    div()
                        .px_2()
                        .text_xs()
                        .text_color(level_tint(report.level, &p))
                        .child(total),
                ),
            )
            .child(bar)
            .children(rows)
            .when(has_owners, |d| {
                d.child(
                    div()
                        .px_2()
                        .pt_1()
                        .text_xs()
                        .font_medium()
                        .text_color(p.text_faint)
                        .child("Largest tool owners"),
                )
                .children(owner_rows)
            })
            .child(
                div()
                    .mt_1()
                    .px_2()
                    .pt_1p5()
                    .border_t_1()
                    .border_color(p.border)
                    .text_xs()
                    .text_color(p.text_faint)
                    .child(source_label(report.source)),
            )
            .children(hint.map(|h| div().px_2().text_xs().text_color(p.text_faint).child(h))))
    }

    /// One line over the thread from 80 %: why, and what to do about it.
    pub(super) fn render_context_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let p = *Palette::of(cx);
        let (id, _) = self.active.as_ref()?;
        let level = level_of(self.telemetry.as_ref());
        if level == ContextLevel::Normal
            || self
                .context
                .dismissed
                .contains(&(id.clone(), level_key(level)))
        {
            return None;
        }
        let t = self.telemetry.as_ref()?;
        let pct = match (t.context_used, t.context_max) {
            (Some(used), Some(max)) => percent(used, max),
            _ => return None,
        };
        let tint = level_tint(level, &p);
        let text = match level {
            ContextLevel::Exceeded => format!("Context window full ({pct:.0}%)."),
            _ => format!("Context window {pct:.0}% full."),
        };
        let compact = self
            .context_report()
            .and_then(|r| r.compact_prompt.as_ref())
            .is_some();
        let busy = self.context.busy;
        let dismiss_key = (id.clone(), level_key(level));
        Some(
            h_flex()
                .id("context-banner")
                .flex_none()
                .w_full()
                .px_3()
                .py_1()
                .gap_2()
                .items_center()
                .text_xs()
                .bg(tint.opacity(0.10))
                .border_b_1()
                .border_color(tint.opacity(0.35))
                .child(div().size(px(6.)).rounded_full().bg(tint))
                .child(div().text_color(p.text).child(text))
                .children(
                    self.context
                        .error
                        .clone()
                        .map(|e| div().min_w_0().truncate().text_color(p.danger).child(e)),
                )
                .child(div().flex_1())
                .when(busy, |d| d.child(Spinner::new().xsmall()))
                .when(compact, |d| {
                    d.child(
                        Button::new("context-compact")
                            .label("Compact now")
                            .xsmall()
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, _, cx| this.compact_now(cx))),
                    )
                })
                .child(
                    Button::new("context-summary")
                        .label("New thread from summary")
                        .xsmall()
                        .disabled(busy)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.new_from_summary(window, cx)),
                        ),
                )
                .child(
                    Button::new("context-dismiss")
                        .icon(IconName::Close)
                        .ghost()
                        .xsmall()
                        .tooltip("Hide until it fills further")
                        .accessibility_label("Hide")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.context.dismissed.insert(dismiss_key.clone());
                            cx.notify();
                        })),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use ironclaw_gpui_client::usage::ContextCategory;

    use super::*;

    fn telemetry(used: Option<u64>, max: Option<u64>) -> ThreadTelemetry {
        ThreadTelemetry {
            context_used: used,
            context_max: max,
            ..ThreadTelemetry::default()
        }
    }

    #[test]
    fn the_meter_warns_at_eighty_percent_and_flags_a_full_window() {
        assert_eq!(level_of(None), ContextLevel::Normal);
        assert_eq!(
            level_of(Some(&telemetry(Some(79), Some(100)))),
            ContextLevel::Normal
        );
        assert_eq!(
            level_of(Some(&telemetry(Some(80), Some(100)))),
            ContextLevel::Warning
        );
        assert_eq!(
            level_of(Some(&telemetry(Some(100), Some(100)))),
            ContextLevel::Exceeded
        );
        // No window known (an old gateway): fill alone, never a warning.
        assert_eq!(
            level_of(Some(&telemetry(Some(10_000_000), None))),
            ContextLevel::Normal
        );
    }

    #[test]
    fn estimates_and_exact_totals_are_labelled_apart() {
        assert!(source_label(ContextSource::Provider).contains("exact"));
        assert!(source_label(ContextSource::Estimated).starts_with("Estimated"));
        assert_eq!(
            owner_label(&ContextToolOwner::Mcp("notion".into())),
            "MCP · notion"
        );
    }

    #[test]
    fn the_stacked_bar_splits_by_share_and_keeps_tiny_categories_visible() {
        let report: ThreadContextReport = serde_json::from_value(serde_json::json!({
            "threadId": "t", "runtimeId": "r", "used": 1000, "max": 2000,
            "level": "normal", "source": "provider",
            "categories": [
                {"category": "system_prompt", "tokens": 750},
                {"category": "user_messages", "tokens": 249},
                {"category": "attachments", "tokens": 1}
            ],
            "toolOwners": [], "compactPrompt": null, "autoCompacts": true, "updatedAt": null
        }))
        .expect("report");
        let s = segments(&report, 100.);
        assert_eq!(s[0], (ContextCategory::SystemPrompt, 75.));
        assert!((s[1].1 - 24.9).abs() < 0.01);
        assert_eq!(s[2], (ContextCategory::Attachments, 2.));
    }
}
