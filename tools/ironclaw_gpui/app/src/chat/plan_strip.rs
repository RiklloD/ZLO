//! The plan strip above the composer: the agent's live checklist, from
//! whichever runtime reported it (Claude's `TodoWrite`, Codex plan updates,
//! ACP plans, the native `builtin.plan_update`). Collapsed it is one line —
//! progress ("3/7") and the step in progress; open it lists every step, the
//! current one highlighted. It hides once every step is done and the run is
//! over, so a finished plan does not linger on the next task.

use ironclaw_host_api::agent_plan::{AgentPlan, PlanEntry, PlanEntryStatus};

use super::{ChatView, flow};
use crate::assets::ExtraIcon;
use crate::design::Palette;
use crate::ui::*;

#[derive(Default)]
pub(super) struct PlanStrip {
    /// The reader opened the full list.
    open: bool,
}

/// The one-line summary: "3/7 · Writing the tests".
pub(super) fn plan_summary(plan: &AgentPlan) -> String {
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

fn entry_row(ix: usize, entry: &PlanEntry, p: &Palette) -> AnyElement {
    let (icon, color, text) = match entry.status {
        PlanEntryStatus::Completed => (ExtraIcon::CircleCheck, p.success, p.text_muted),
        PlanEntryStatus::InProgress => (ExtraIcon::CircleDashed, p.accent, p.text),
        PlanEntryStatus::Pending => (ExtraIcon::Circle, p.text_faint, p.text_muted),
    };
    let current = entry.status == PlanEntryStatus::InProgress;
    h_flex()
        .id(SharedString::from(format!("plan-entry-{ix}")))
        .gap_2()
        .items_center()
        .px_1()
        .rounded(px(6.))
        .when(current, |d| d.bg(p.accent_wash))
        .child(Icon::new(icon).xsmall().text_color(color))
        .child(
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .text_color(text)
                .when(current, |d| d.font_weight(gpui_kit::FontWeight::MEDIUM))
                .when(entry.status == PlanEntryStatus::Completed, |d| {
                    d.line_through()
                })
                .child(entry.content.clone()),
        )
        .into_any_element()
}

impl ChatView {
    pub(super) fn render_plan_strip(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let plan = self.transcript.plan()?;
        if !shown(plan, self.transcript.active_run().is_some()) {
            return None;
        }
        let p = *Palette::of(cx);
        let open = self.plan_strip.open;
        let summary = plan_summary(plan);
        let head = h_flex()
            .id("plan-strip-head")
            .role(Role::Button)
            .aria_label(format!("Plan {summary}"))
            .h(px(28.))
            .gap_1()
            .items_center()
            .text_xs()
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| {
                this.plan_strip.open = !this.plan_strip.open;
                cx.notify();
            }))
            .child(
                div()
                    .w(px(24.))
                    .flex_none()
                    .flex()
                    .justify_center()
                    .text_color(p.text_muted)
                    .child(Icon::new(ExtraIcon::ListChecks).xsmall()),
            )
            .child(
                div()
                    .flex_none()
                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                    .child("Plan"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(p.text_muted)
                    .child(summary),
            )
            .child(
                Icon::new(if open {
                    IconName::ChevronUp
                } else {
                    IconName::ChevronDown
                })
                .xsmall()
                .text_color(p.text_muted),
            );
        let mut strip = v_flex()
            .id("plan-strip")
            .role(Role::Status)
            .w_full()
            .child(head);
        if open {
            let mut list = v_flex()
                .gap_0p5()
                .pl(px(24.))
                .pb_1()
                .text_size(flow::ui(12.));
            if let Some(why) = &plan.explanation {
                list = list.child(div().text_color(p.text_faint).pb_1().child(why.clone()));
            }
            for (ix, entry) in plan.entries.iter().enumerate() {
                list = list.child(entry_row(ix, entry, &p));
            }
            strip = strip.child(list);
        }
        Some(strip.into_any_element())
    }
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
