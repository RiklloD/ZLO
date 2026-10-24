//! The goal strip above the composer (t3code's goal callout): a target,
//! "Pursuing goal" and the objective on one muted line, with edit, done and
//! clear on hover. `/goal <objective>` sets it, `/goal clear` clears it and
//! a bare `/goal` opens the editor. The goal is stored by the backend per
//! thread; it is read when the thread opens and after every change, never
//! polled.

use ironclaw_gpui_client::inbox::{GoalRecord, GoalStatus, ThreadGoalResponse};

use super::ChatView;
use crate::assets::ExtraIcon;
use crate::jobs::run;
use crate::ui::*;

#[derive(Default)]
pub(super) struct Goal {
    goal: Option<GoalRecord>,
    loaded: bool,
    editor: Option<(Entity<InputState>, Subscription)>,
}

/// What a `/goal` line asks for.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum GoalCommand {
    Edit,
    Clear,
    Set(String),
}

/// `Some` when `text` is a `/goal` command.
pub(super) fn goal_command(text: &str) -> Option<GoalCommand> {
    let rest = text.trim().strip_prefix("/goal")?;
    if !(rest.is_empty() || rest.starts_with(char::is_whitespace)) {
        return None;
    }
    Some(match rest.trim() {
        "" => GoalCommand::Edit,
        "clear" | "off" => GoalCommand::Clear,
        objective => GoalCommand::Set(objective.strip_prefix("set ").unwrap_or(objective).into()),
    })
}

impl ChatView {
    pub(super) fn load_goal(&mut self, cx: &mut Context<Self>) {
        if self.orch.goal.loaded {
            return;
        }
        self.orch.goal.loaded = true;
        let thread_id = self.thread_id.clone();
        run(
            &self.backend,
            cx,
            move |c| async move { c.thread_goal(&thread_id).await },
            |this, result, cx| {
                match result {
                    Ok(doc) => this.orch.goal.goal = doc.goal,
                    // silent-ok: no goal route (older gateway) or a thread the
                    // orchestrator has not seen yet both mean "no goal".
                    Err(e) => tracing::debug!(error = %e, "thread goal read failed"),
                }
                cx.notify();
            },
        );
    }

    fn write_goal(
        &mut self,
        objective: Option<String>,
        status: Option<GoalStatus>,
        cx: &mut Context<Self>,
    ) {
        let thread_id = self.thread_id.clone();
        run(
            &self.backend,
            cx,
            move |c| async move {
                c.set_thread_goal(&thread_id, objective.as_deref(), status)
                    .await
            },
            |this, result: Result<ThreadGoalResponse, _>, cx| {
                match result {
                    Ok(doc) => {
                        this.orch.goal.goal = doc.goal;
                        this.orch.goal.editor = None;
                    }
                    Err(e) => this.set_error(e.to_string(), cx),
                }
                cx.notify();
            },
        );
    }

    pub(super) fn goal_command(
        &mut self,
        command: GoalCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.composer
            .update(cx, |state, cx| state.clean(window, cx));
        match command {
            GoalCommand::Edit => self.edit_goal(window, cx),
            GoalCommand::Clear => self.write_goal(None, None, cx),
            GoalCommand::Set(objective) => self.write_goal(Some(objective), None, cx),
        }
    }

    fn edit_goal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let current = self
            .orch
            .goal
            .goal
            .as_ref()
            .map(|g| g.objective.clone())
            .unwrap_or_default();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("What should this thread achieve?")
                .default_value(current)
        });
        let sub = cx.subscribe_in(&input, window, |this, input, event, _, cx| {
            if let InputEvent::PressEnter { .. } = event {
                let text = input.read(cx).value().trim().to_string();
                if !text.is_empty() {
                    this.write_goal(Some(text), None, cx);
                }
            }
        });
        input.update(cx, |state, cx| state.focus(window, cx));
        self.orch.goal.editor = Some((input, sub));
        cx.notify();
    }

    pub(super) fn render_goal(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        if let Some((input, _)) = &self.orch.goal.editor {
            return Some(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(Icon::new(ExtraIcon::Target).xsmall().text_color(muted))
                    .child(div().flex_1().child(Input::new(input).small()))
                    .child(
                        Button::new("goal-cancel")
                            .label("Cancel")
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.orch.goal.editor = None;
                                cx.notify();
                            })),
                    )
                    .into_any_element(),
            );
        }
        let goal = self.orch.goal.goal.as_ref()?;
        let title = match goal.status {
            GoalStatus::Active => "Pursuing goal",
            GoalStatus::Paused => "Goal paused",
            GoalStatus::Done => "Goal complete",
        };
        let done = goal.status == GoalStatus::Done;
        Some(
            h_flex()
                .id("goal-strip")
                .group("goal-strip")
                .role(Role::Status)
                .aria_label(format!("{title}: {}", goal.objective))
                .h(px(28.))
                .gap_1()
                .items_center()
                .text_xs()
                .child(
                    div()
                        .w(px(24.))
                        .flex_none()
                        .flex()
                        .justify_center()
                        .text_color(muted)
                        .child(Icon::new(ExtraIcon::Target).xsmall()),
                )
                .child(div().flex_none().font_medium().child(title))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(muted)
                        .child(goal.objective.clone()),
                )
                .child(
                    h_flex()
                        .gap_px()
                        .invisible()
                        .group_hover("goal-strip", |s| s.visible())
                        .child(
                            Button::new("goal-edit")
                                .icon(ExtraIcon::Pencil)
                                .ghost()
                                .xsmall()
                                .tooltip("Edit goal")
                                .accessibility_label("Edit goal")
                                .on_click(
                                    cx.listener(|this, _, window, cx| this.edit_goal(window, cx)),
                                ),
                        )
                        .child(
                            Button::new("goal-done")
                                .icon(if done {
                                    IconName::Undo2
                                } else {
                                    IconName::Check
                                })
                                .ghost()
                                .xsmall()
                                .tooltip(if done { "Pursue again" } else { "Mark done" })
                                .accessibility_label(if done {
                                    "Pursue again"
                                } else {
                                    "Mark done"
                                })
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let status = if done {
                                        GoalStatus::Active
                                    } else {
                                        GoalStatus::Done
                                    };
                                    this.write_goal(None, Some(status), cx)
                                })),
                        )
                        .child(
                            Button::new("goal-clear")
                                .icon(IconName::Close)
                                .ghost()
                                .xsmall()
                                .tooltip("Clear goal")
                                .accessibility_label("Clear goal")
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.write_goal(None, None, cx)),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_commands_parse_like_t3() {
        assert_eq!(goal_command("/goal"), Some(GoalCommand::Edit));
        assert_eq!(goal_command(" /goal clear "), Some(GoalCommand::Clear));
        assert_eq!(
            goal_command("/goal set ship the inbox"),
            Some(GoalCommand::Set("ship the inbox".into()))
        );
        assert_eq!(
            goal_command("/goal ship it"),
            Some(GoalCommand::Set("ship it".into()))
        );
        assert_eq!(goal_command("/goals"), None);
        assert_eq!(goal_command("set a /goal"), None);
    }
}
