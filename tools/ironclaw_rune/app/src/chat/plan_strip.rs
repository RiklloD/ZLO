//! The plan strip above the composer: the agent's live checklist, from
//! whichever runtime reported it (Claude's `TodoWrite`, Codex plan updates,
//! ACP plans, the native `builtin.plan_update`). Collapsed it is one line —
//! progress ("3/7") and the step in progress; open it lists every step, the
//! current one highlighted. It hides once every step is done and the run is
//! over, so a finished plan does not linger on the next task.
//!
//! GPUI `chat/plan_strip.rs`. The strip is drawn inside the orchestration
//! panel above the composer (area H's `orchestration::panel` puts
//! [`view`] first, as GPUI's `render_orchestration` did).

use ironclaw_host_api::agent_plan::{AgentPlan, PlanEntry, PlanEntryStatus};
use rune::{El, col, icon, row, text};

use super::{ChatState, flow, glyphs};
use crate::icons;
use crate::state::State;

#[derive(Default)]
pub struct PlanStrip {
    /// The reader opened the full list.
    pub open: bool,
}

/// The one-line summary: "3/7 · Writing the tests".
pub fn plan_summary(plan: &AgentPlan) -> String {
    let (done, total) = plan.progress();
    match plan.current() {
        Some(entry) => format!(
            "{done}/{total} · {}",
            entry.active_form.as_deref().unwrap_or(&entry.content)
        ),
        None if plan.is_complete() => format!("{done}/{total} · All done"),
        None => format!("{done}/{total}"),
    }
}

/// Whether the strip shows: a plan with steps, unless it is all done and
/// nothing runs.
fn shown(plan: &AgentPlan, running: bool) -> bool {
    !plan.entries.is_empty() && (running || !plan.is_complete())
}

fn entry_row(s: &State, ix: usize, entry: &PlanEntry) -> El {
    let p = &s.palette;
    let (glyph, color, ink) = match entry.status {
        PlanEntryStatus::Completed => (&icons::CIRCLE_CHECK, p.success, p.text_muted),
        PlanEntryStatus::InProgress => (&glyphs::CIRCLE_DASHED, p.accent, p.text),
        PlanEntryStatus::Pending => (&glyphs::CIRCLE, p.text_faint, p.text_muted),
    };
    let current = entry.status == PlanEntryStatus::InProgress;
    let weight = if current { 500. } else { 400. };
    let mut label = text(entry.content.clone())
        .font(flow::type_at(s, 12., 18., weight))
        .color(ink)
        .truncate()
        .min_w(0.)
        .grow(1.);
    if entry.status == PlanEntryStatus::Completed {
        label = label.strike(0, entry.content.len(), None);
    }
    let mut line = row()
        .key(format!("plan-entry-{ix}"))
        .gap(8.)
        .center_y()
        .px(4.)
        .radius(6.)
        .child(icon(glyph, 12.).color(color))
        .child(label);
    if current {
        line = line.fill(p.accent_wash);
    }
    line
}

/// The strip for `chat`, when its plan shows.
pub fn view(s: &State, chat: &ChatState) -> Option<El> {
    let plan = chat.transcript.plan()?;
    if !shown(plan, chat.transcript.active_run().is_some()) {
        return None;
    }
    let p = &s.palette;
    let open = chat.plan_strip.open;
    let summary = plan_summary(plan);
    let thread = chat.thread_id.clone();
    let small = flow::type_at(s, 12., 16., 400.);
    let head = row()
        .key("plan-strip-head")
        .role("button")
        .label(format!("Plan {summary}"))
        .h(28.)
        .gap(4.)
        .center_y()
        .cursor(rune::Cursor::Pointer)
        .on_press(move |s: &mut State| {
            if let Some(c) = super::chat_for(s, &thread) {
                c.plan_strip.open = !c.plan_strip.open;
            }
        })
        .child(
            row()
                .w(24.)
                .justify_center()
                .child(icon(&glyphs::LIST_CHECKS, 12.).color(p.text_muted)),
        )
        .child(text("Plan").font(flow::type_at(s, 12., 16., 500.)).color(p.text))
        .child(
            text(summary)
                .font(small)
                .color(p.text_muted)
                .truncate()
                .min_w(0.)
                .grow(1.),
        )
        .child(
            icon(
                if open {
                    &icons::CHEVRON_UP
                } else {
                    &icons::CHEVRON_DOWN
                },
                12.,
            )
            .color(p.text_muted),
        );
    let mut strip = col()
        .key("plan-strip")
        .role("status")
        .w_full()
        .child(head);
    if open {
        let mut list = col()
            .key("plan-list")
            .gap(2.)
            .pl(24.)
            .pb(4.)
            .transition(flow::rise(4.));
        if let Some(why) = &plan.explanation {
            list = list.child(
                text(why.clone())
                    .font(small)
                    .color(p.text_faint)
                    .wrap()
                    .pb(4.),
            );
        }
        for (ix, entry) in plan.entries.iter().enumerate() {
            list = list.child(entry_row(s, ix, entry));
        }
        strip = strip.child(list);
    }
    Some(strip)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(statuses: &[PlanEntryStatus]) -> AgentPlan {
        AgentPlan::new(
            statuses.iter().enumerate().map(|(i, s)| {
                PlanEntry::new(format!("step {i}"), *s)
                    .with_active_form(Some(format!("doing step {i}")))
            }),
            None,
            chrono::Utc::now(),
        )
    }

    #[test]
    fn the_summary_counts_and_names_the_current_step() {
        use PlanEntryStatus::*;
        assert_eq!(
            plan_summary(&plan(&[Completed, InProgress, Pending])),
            "1/3 · doing step 1"
        );
        assert_eq!(
            plan_summary(&plan(&[Completed, Completed])),
            "2/2 · All done"
        );
        assert_eq!(plan_summary(&plan(&[Pending, Pending])), "0/2");
    }

    #[test]
    fn a_finished_plan_hides_once_the_run_is_over() {
        use PlanEntryStatus::*;
        assert!(shown(&plan(&[Completed]), true));
        assert!(!shown(&plan(&[Completed]), false));
        assert!(shown(&plan(&[Pending]), false));
    }
}
