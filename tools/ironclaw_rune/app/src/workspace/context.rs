//! The status bar's context meter (GPUI `workspace/context.rs`). The fill is
//! coloured by level (warning from 80 % of the window, exceeded from 100 %);
//! a click opens what fills the window — a stacked bar, every category with
//! tokens and share, the largest tool owners, and whether the total is the
//! provider's count or an estimate. From 80 % a one-line banner over the
//! thread offers "Compact now" (the runtime's own compaction command, sent
//! as a turn) and "New thread from summary" (a fork seeded with a digest of
//! the conversation).

use std::collections::HashSet;

use zlo_client::orchestration::ForkOptions;
use zlo_client::usage::{
    ContextCategory, ContextLevel, ContextSource, ContextToolOwner, ThreadContextReport,
    ThreadTelemetry,
};
use rune::{Color, El, col, el, row};
use rune_kit::{ControlSize, Kit, Placement, Spinner};

use super::status::tokens;
use super::ui::{self, MEDIUM, XS, t, tw};
use super::{active_id, inbox};
use crate::design::Palette;
use crate::icons;
use crate::jobs;
use crate::state::State;

/// Width of the popover's stacked bar.
const BAR_WIDTH: f32 = 288.;
/// Tool owners listed under the categories.
const TOP_OWNERS: usize = 4;

#[derive(Default)]
pub struct ContextMeter {
    pub open: bool,
    report: Option<ThreadContextReport>,
    loading: bool,
    /// An action (compact, summary fork) is in flight.
    busy: bool,
    error: Option<String>,
    /// Threads whose banner was closed, until their level changes.
    dismissed: HashSet<(String, u8)>,
    /// Bumped per load: only the newest load's answer lands (two loads of
    /// one thread, A → B → A, must not land oldest last).
    load_gen: jobs::Generation,
}

impl ContextMeter {
    /// A scene's open popover on `report`.
    pub fn fixture(&mut self, report: ThreadContextReport) {
        self.report = Some(report);
        self.open = true;
    }

    /// Thread switch: the popover belongs to the thread it was opened on.
    pub(super) fn close(&mut self) {
        self.open = false;
        self.error = None;
    }
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

pub(super) fn level_tint(level: ContextLevel, p: &Palette) -> Color {
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
/// threads and themes (GPUI's HSL at 55 % saturation and lightness).
fn category_color(category: ContextCategory) -> Color {
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
    hsl(hue, saturation, 0.55)
}

/// HSL (0..1 each) to a colour.
fn hsl(h: f32, s: f32, l: f32) -> Color {
    let q = if l < 0.5 { l * (1. + s) } else { l + s - l * s };
    let p = 2. * l - q;
    let channel = |t: f32| {
        let t = t.rem_euclid(1.);
        if t < 1. / 6. {
            p + (q - p) * 6. * t
        } else if t < 0.5 {
            q
        } else if t < 2. / 3. {
            p + (q - p) * (2. / 3. - t) * 6.
        } else {
            p
        }
    };
    Color {
        r: channel(h + 1. / 3.),
        g: channel(h),
        b: channel(h - 1. / 3.),
        a: 1.,
    }
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

/// The report shown, when it belongs to the open thread.
fn report(s: &State) -> Option<&ThreadContextReport> {
    let active = active_id(s)?;
    s.workspace
        .context
        .report
        .as_ref()
        .filter(|r| r.thread_id == active)
}

/// Fetch the open thread's report (cheap: the backend estimates once per
/// turn).
pub(super) fn load(s: &mut State) {
    let Some(id) = active_id(s).map(str::to_string) else {
        return;
    };
    let asked = id.clone();
    s.workspace.context.loading = true;
    let generation = s.workspace.context.load_gen.bump();
    jobs::run(
        s,
        "context",
        move |c| async move { c.thread_context(&id).await },
        move |s, result, _| {
            if s.workspace.context.load_gen != generation {
                return;
            }
            s.workspace.context.loading = false;
            if active_id(s) != Some(asked.as_str()) {
                return;
            }
            match result {
                Ok(report) => {
                    s.workspace.context.report = report;
                    s.workspace.context.error = None;
                }
                Err(e) => s.workspace.context.error = Some(e.to_string()),
            }
        },
    );
}

/// After a telemetry change: keep an open popover current, and have the
/// banner's actions ready once the window is filling up.
pub(super) fn telemetry_changed(s: &mut State) {
    let stale = report(s).map(|r| r.updated_at)
        != Some(s.workspace.telemetry.as_ref().and_then(|t| t.updated_at));
    let filling = level_of(s.workspace.telemetry.as_ref()) != ContextLevel::Normal;
    if stale && (s.workspace.context.open || filling) {
        load(s);
    }
}

fn toggle(s: &mut State) {
    s.workspace.context.open = !s.workspace.context.open;
    if s.workspace.context.open {
        load(s);
    }
}

/// Send the runtime's compaction command as a turn of the open thread.
fn compact_now(s: &mut State) {
    let (Some(id), Some(channel)) = (
        active_id(s).map(str::to_string),
        s.session
            .as_ref()
            .and_then(|s| s.session_channel_extension_id.clone()),
    ) else {
        return;
    };
    let Some(prompt) = report(s).and_then(|r| r.compact_prompt.clone()) else {
        return;
    };
    s.workspace.context.busy = true;
    s.workspace.context.open = false;
    jobs::run(
        s,
        "compact",
        move |c| async move { c.send_message(&channel, &id, &prompt, Vec::new()).await },
        |s, result, _| {
            s.workspace.context.busy = false;
            if let Err(e) = result {
                s.workspace.context.error = Some(e.to_string());
            }
        },
    );
}

/// Fork the open thread into a fresh one seeded with a digest, and open it.
fn new_from_summary(s: &mut State) {
    let Some(id) = active_id(s).map(str::to_string) else {
        return;
    };
    s.workspace.context.busy = true;
    s.workspace.context.open = false;
    jobs::run(
        s,
        "summary fork",
        move |c| async move {
            let options = ForkOptions {
                summary: true,
                ..ForkOptions::default()
            };
            c.fork_thread(&id, &options).await
        },
        |s, result, _| {
            s.workspace.context.busy = false;
            match result {
                Ok(forked) => {
                    inbox::refresh(s);
                    super::open_thread(s, forked.fork_thread_id);
                }
                Err(e) => s.workspace.context.error = Some(e.to_string()),
            }
        },
    );
}

/// The status bar's "Context" metric, clickable, coloured by level. The
/// popover goes into `layer`.
pub(super) fn meter(s: &State, value: String, k: &Kit, layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let level = level_of(s.workspace.telemetry.as_ref());
    let tint = level_tint(level, p);
    let open = s.workspace.context.open;
    let ink = if open {
        p.text
    } else if level != ContextLevel::Normal {
        tint
    } else {
        p.text_faint
    };
    if open {
        layer.push(rune_kit::popover::anchored(
            k,
            el().modal(true, |s: &mut State| s.workspace.context.open = false)
                .child(popover(s)),
            "status-context",
            Placement::TopEnd,
            6.,
        ));
    }
    row()
        .key("status-context")
        .role("button")
        .label("What fills the context window")
        .gap(4.)
        .center_y()
        .cursor(rune::Cursor::Pointer)
        .hover_group()
        .child(ui::dot(tint, 6.))
        .child(t("Context", XS, ink).group_color(p.text))
        .child(t(value, XS, ink).group_color(p.text))
        .on_press(toggle)
}

fn popover(s: &State) -> El {
    let p = &s.palette;
    let head = el()
        .px(8.)
        .pt(4.)
        .pb(4.)
        .child(tw("Context window", XS, MEDIUM, p.text_faint));
    let Some(report) = report(s) else {
        let note = match (&s.workspace.context.error, s.workspace.context.loading) {
            (Some(e), _) => e.clone(),
            (None, true) => "Measuring…".into(),
            (None, false) => "Nothing measured for this thread yet.".into(),
        };
        return ui::floating(
            p,
            col()
                .key("context-pop")
                .w(BAR_WIDTH + 24.)
                .padding(8.)
                .child(head)
                .child(el().px(8.).child(t(note, XS, p.text_muted).wrap())),
            12.,
        );
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
    let bar = row()
        .mx(8.)
        .h(8.)
        .w(BAR_WIDTH)
        .radius(4.)
        .clip_rounded(4.)
        .fill(p.hover)
        .children(
            segments(report, BAR_WIDTH)
                .into_iter()
                .map(|(category, w)| el().h_full().w(w).fill(category_color(category))),
        );
    let rows = report.categories.iter().map(|c| {
        row()
            .px(8.)
            .gap(8.)
            .center_y()
            .child(
                el().size(8., 8.)
                    .radius(2.)
                    .fill(category_color(c.category)),
            )
            .child(
                el().grow(1.0)
                    .min_w(0.0)
                    .child(t(category_label(c.category), XS, p.text_muted).truncate()),
            )
            .child(t(tokens(c.tokens), XS, p.text))
            .child(row().w(40.).justify_end().child(t(
                format!("{:.0}%", percent(c.tokens, report.used)),
                XS,
                p.text_faint,
            )))
    });
    let owners = report.tool_owners.iter().take(TOP_OWNERS).map(|o| {
        row()
            .px(8.)
            .gap(8.)
            .child(
                el().grow(1.0)
                    .min_w(0.0)
                    .child(t(owner_label(&o.owner), XS, p.text_muted).truncate()),
            )
            .child(t(format!("{} tools", o.tools), XS, p.text_faint))
            .child(
                row()
                    .w(48.)
                    .justify_end()
                    .child(t(tokens(o.tokens), XS, p.text)),
            )
    });
    let mut body = col()
        .key("context-pop")
        .w(BAR_WIDTH + 24.)
        .py(4.)
        .gap(6.)
        .child(
            row()
                .justify_between()
                .child(head)
                .child(el().px(8.).child(t(total, XS, level_tint(report.level, p)))),
        )
        .child(bar)
        .children(rows);
    if !report.tool_owners.is_empty() {
        body = body
            .child(
                el().px(8.)
                    .pt(4.)
                    .child(tw("Largest tool owners", XS, MEDIUM, p.text_faint)),
            )
            .children(owners);
    }
    body = body.child(
        col()
            .mt(4.)
            .child(ui::rule(p))
            .child(
                el().px(8.)
                    .pt(6.)
                    .child(t(source_label(report.source), XS, p.text_faint)),
            ),
    );
    if report.compact_prompt.is_none() && report.auto_compacts {
        body = body.child(el().px(8.).child(t(
            "This thread compacts by itself near the limit.",
            XS,
            p.text_faint,
        )));
    }
    ui::floating(p, body, 12.)
}

/// One line over the thread from 80 %: why, and what to do about it.
pub(super) fn banner(s: &State, k: &Kit) -> Option<El> {
    let p = &s.palette;
    let id = active_id(s)?;
    let level = level_of(s.workspace.telemetry.as_ref());
    if level == ContextLevel::Normal
        || s.workspace
            .context
            .dismissed
            .contains(&(id.to_string(), level_key(level)))
    {
        return None;
    }
    let tel = s.workspace.telemetry.as_ref()?;
    let pct = match (tel.context_used, tel.context_max) {
        (Some(used), Some(max)) => percent(used, max),
        _ => return None,
    };
    let tint = level_tint(level, p);
    let text = match level {
        ContextLevel::Exceeded => format!("Context window full ({pct:.0}%)."),
        _ => format!("Context window {pct:.0}% full."),
    };
    let compact = report(s).and_then(|r| r.compact_prompt.as_ref()).is_some();
    let busy = s.workspace.context.busy;
    let dismiss_key = (id.to_string(), level_key(level));
    let mut line = row()
        .key("context-banner")
        .w_full()
        .px(12.)
        .py(4.)
        .gap(8.)
        .center_y()
        .fill(tint.alpha(0.10))
        .child(ui::dot(tint, 6.))
        .child(t(text, XS, p.text));
    if let Some(e) = s.workspace.context.error.clone() {
        line = line.child(el().min_w(0.0).child(t(e, XS, p.danger).truncate()));
    }
    line = line.child(el().grow(1.0));
    if busy {
        line = line.child(Spinner::new().size(12.).build(k));
    }
    if compact {
        line = line.child(ui::small_button(k, "Compact now", busy, compact_now));
    }
    line = line
        .child(ui::small_button(
            k,
            "New thread from summary",
            busy,
            new_from_summary,
        ))
        .child(ui::ghost_icon(
            k,
            &icons::CLOSE,
            "Hide until it fills further",
            ControlSize::Mini,
            move |s| {
                s.workspace.context.dismissed.insert(dismiss_key.clone());
            },
        ));
    Some(
        col()
            .w_full()
            .child(line)
            .child(el().h(1.).w_full().fill(tint.alpha(0.35))),
    )
}

#[cfg(test)]
mod tests {
    use zlo_client::usage::ContextCategory;

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

    #[test]
    fn hsl_matches_the_primaries() {
        let red = hsl(0., 1., 0.5);
        assert!((red.r - 1.).abs() < 1e-5 && red.g.abs() < 1e-5 && red.b.abs() < 1e-5);
    }
}
