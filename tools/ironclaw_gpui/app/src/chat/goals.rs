//! The goal strip above the composer (t3code's goal callout): a target,
//! "Pursuing goal" and the objective on one muted line, with edit, done and
//! clear on hover. `/goal <objective>` sets it, `/goal clear` clears it and
//! a bare `/goal` opens the editor.
//!
//! A goal may carry a check: `/goal <objective> --check "<cmd>"` (optionally
//! `--attempts N`, `--timeout SECS`; `/goal --check "<cmd>"` changes only the
//! check, `/goal --check off` removes it). The backend runs it after every
//! completed turn; the strip shows where it stands (not run, running,
//! passed, failed n/N, needs you) with "Run check now", "Stop retrying" and
//! an editor for the command. The goal is read when the thread opens, after
//! every change and whenever the orchestration state refreshes while its
//! check can still move; a queued or running check keeps the thread polling.

use ironclaw_gpui_client::inbox::{
    GoalCheckAction, GoalCheckRecord, GoalCheckSpec, GoalCheckStatus, GoalRecord, GoalStatus,
    ThreadGoalResponse,
};

use super::ChatView;
use crate::assets::ExtraIcon;
use crate::jobs::run;
use crate::ui::*;

#[derive(Default)]
pub(super) struct Goal {
    goal: Option<GoalRecord>,
    loaded: bool,
    /// A read is in flight.
    reading: bool,
    pub(super) editor: Option<(Entity<InputState>, Subscription)>,
    /// The editor edits the check command, not the objective.
    editing_check: bool,
}

impl Goal {
    /// The check can still change on its own (a turn ends, a run finishes).
    fn check_can_move(&self) -> bool {
        self.goal.as_ref().is_some_and(|g| {
            g.status == GoalStatus::Active
                && g.check.as_ref().is_some_and(|c| {
                    matches!(
                        c.status,
                        GoalCheckStatus::NotRun
                            | GoalCheckStatus::Pending
                            | GoalCheckStatus::Running
                            | GoalCheckStatus::Failed
                    )
                })
        })
    }

    /// A check is queued or running: worth polling for.
    pub(super) fn check_in_flight(&self) -> bool {
        self.goal
            .as_ref()
            .and_then(|g| g.check.as_ref())
            .is_some_and(|c| {
                matches!(
                    c.status,
                    GoalCheckStatus::Pending | GoalCheckStatus::Running
                )
            })
    }
}

/// What a `/goal` line asks for.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum GoalCommand {
    Edit,
    Clear,
    Set(String),
    /// Set the check, and the objective when one is given.
    SetChecked {
        objective: Option<String>,
        check: GoalCheckSpec,
    },
    ClearCheck,
    /// A malformed `--check`; the message says why.
    Invalid(String),
}

/// `Some` when `text` is a `/goal` command.
pub(super) fn goal_command(text: &str) -> Option<GoalCommand> {
    let rest = text.trim().strip_prefix("/goal")?;
    if !(rest.is_empty() || rest.starts_with(char::is_whitespace)) {
        return None;
    }
    let rest = rest.trim();
    if let Some((objective, args)) = split_flag(rest, "--check") {
        let objective = objective.strip_prefix("set ").unwrap_or(objective).trim();
        let objective = (!objective.is_empty()).then(|| objective.to_string());
        if args.trim() == "off" && objective.is_none() {
            return Some(GoalCommand::ClearCheck);
        }
        return Some(match parse_check(args) {
            Ok(check) => GoalCommand::SetChecked { objective, check },
            Err(message) => GoalCommand::Invalid(message),
        });
    }
    Some(match rest {
        "" => GoalCommand::Edit,
        "clear" | "off" => GoalCommand::Clear,
        objective => GoalCommand::Set(objective.strip_prefix("set ").unwrap_or(objective).into()),
    })
}

/// Split `text` at a whitespace-delimited `flag`: (before, after).
fn split_flag<'a>(text: &'a str, flag: &str) -> Option<(&'a str, &'a str)> {
    let mut from = 0;
    while let Some(at) = text[from..].find(flag).map(|i| i + from) {
        let before_ok = at == 0 || text[..at].ends_with(char::is_whitespace);
        let end = at + flag.len();
        let after_ok = end == text.len() || text[end..].starts_with(char::is_whitespace);
        if before_ok && after_ok {
            return Some((&text[..at], &text[end..]));
        }
        from = end;
    }
    None
}

/// `"<cmd>" [--attempts N] [--timeout SECS]`; an unquoted command runs to
/// the first flag.
fn parse_check(args: &str) -> Result<GoalCheckSpec, String> {
    let args = args.trim_start();
    let (command, mut rest) = match args.chars().next() {
        Some(quote @ ('"' | '\'')) => {
            let body = &args[1..];
            let close = body
                .find(quote)
                .ok_or_else(|| format!("--check: missing closing {quote}"))?;
            (body[..close].to_string(), &body[close + 1..])
        }
        _ => {
            let end = [" --attempts", " --timeout"]
                .iter()
                .filter_map(|f| args.find(f))
                .min()
                .unwrap_or(args.len());
            (args[..end].trim().to_string(), &args[end..])
        }
    };
    if command.trim().is_empty() {
        return Err("--check needs a command, e.g. /goal tests pass --check \"cargo test\"".into());
    }
    let mut spec = GoalCheckSpec {
        command: command.trim().to_string(),
        max_attempts: None,
        timeout_secs: None,
    };
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return Ok(spec);
        }
        let mut words = rest.splitn(3, char::is_whitespace);
        let (Some(flag), Some(value)) = (words.next(), words.next()) else {
            return Err(format!("{rest}: expected a value"));
        };
        let number = value
            .trim_end_matches('s')
            .parse::<u64>()
            .map_err(|_| format!("{flag}: {value} is not a number"))?;
        match flag {
            "--attempts" => {
                spec.max_attempts = Some(u32::try_from(number).map_err(|e| e.to_string())?)
            }
            "--timeout" => spec.timeout_secs = Some(number),
            other => return Err(format!("unknown option {other}")),
        }
        rest = words.next().unwrap_or("");
    }
}

/// The chip after the objective: label, icon and how loud it is.
enum Tone {
    Muted,
    Good,
    Warn,
}

fn check_chip(check: &GoalCheckRecord) -> (String, Option<IconName>, Tone) {
    let n = check.attempts;
    let max = check.max_attempts;
    match check.status {
        GoalCheckStatus::NotRun => ("Check not run".into(), None, Tone::Muted),
        GoalCheckStatus::Pending => ("Check queued".into(), None, Tone::Muted),
        GoalCheckStatus::AwaitingApproval => (
            "Check needs approval".into(),
            Some(IconName::CircleAlert),
            Tone::Warn,
        ),
        GoalCheckStatus::Running => ("Checking…".into(), None, Tone::Muted),
        GoalCheckStatus::Passed => (
            "Check passed".into(),
            Some(IconName::CircleCheck),
            Tone::Good,
        ),
        GoalCheckStatus::Failed => (
            format!("Check failed {n}/{max}"),
            Some(IconName::CircleX),
            Tone::Warn,
        ),
        GoalCheckStatus::NeedsYou => (
            if n >= max {
                format!("Needs you · failed {n}/{max}")
            } else {
                "Needs you".into()
            },
            Some(IconName::CircleAlert),
            Tone::Warn,
        ),
        GoalCheckStatus::Stopped => ("Retries stopped".into(), None, Tone::Muted),
    }
}

impl ChatView {
    /// Read the goal once per thread open, and again while its check can
    /// still move (each orchestration refresh asks).
    pub(super) fn load_goal(&mut self, cx: &mut Context<Self>) {
        let g = &mut self.orch.goal;
        if g.reading || (g.loaded && !g.check_can_move()) {
            return;
        }
        g.loaded = true;
        g.reading = true;
        let thread_id = self.thread_id.clone();
        run(
            &self.backend,
            cx,
            move |c| async move { c.thread_goal(&thread_id).await },
            |this, result, cx| {
                this.orch.goal.reading = false;
                match result {
                    Ok(doc) => this.orch.goal.goal = doc.goal,
                    // silent-ok: no goal route (older gateway) or a thread the
                    // orchestrator has not seen yet both mean "no goal".
                    Err(e) => tracing::debug!(error = %e, "thread goal read failed"),
                }
                this.ensure_poll(cx);
                cx.notify();
            },
        );
    }

    fn apply_goal(
        &mut self,
        result: Result<ThreadGoalResponse, ironclaw_gpui_client::ClientError>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(doc) => {
                self.orch.goal.goal = doc.goal;
                self.orch.goal.editor = None;
                self.orch.goal.editing_check = false;
            }
            Err(e) => self.set_error(e.to_string(), cx),
        }
        self.ensure_poll(cx);
        cx.notify();
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
            |this, result, cx| this.apply_goal(result, cx),
        );
    }

    /// Set (or with `None` remove) the check, and the objective if given.
    fn write_goal_check(
        &mut self,
        objective: Option<String>,
        check: Option<GoalCheckSpec>,
        cx: &mut Context<Self>,
    ) {
        let thread_id = self.thread_id.clone();
        run(
            &self.backend,
            cx,
            move |c| async move {
                c.set_thread_goal_check(&thread_id, objective.as_deref(), check.as_ref())
                    .await
            },
            |this, result, cx| this.apply_goal(result, cx),
        );
    }

    fn goal_check_action(&mut self, action: GoalCheckAction, cx: &mut Context<Self>) {
        let thread_id = self.thread_id.clone();
        run(
            &self.backend,
            cx,
            move |c| async move { c.goal_check_action(&thread_id, action).await },
            |this, result, cx| this.apply_goal(result, cx),
        );
    }

    pub(super) fn goal_command(
        &mut self,
        command: GoalCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let GoalCommand::Invalid(message) = &command {
            // Keep the line so it can be fixed.
            self.set_error(message.clone(), cx);
            return;
        }
        self.composer
            .update(cx, |state, cx| state.clean(window, cx));
        match command {
            GoalCommand::Edit => self.edit_goal(false, window, cx),
            GoalCommand::Clear => self.write_goal(None, None, cx),
            GoalCommand::Set(objective) => self.write_goal(Some(objective), None, cx),
            GoalCommand::SetChecked { objective, check } => {
                self.write_goal_check(objective, Some(check), cx)
            }
            GoalCommand::ClearCheck => self.write_goal_check(None, None, cx),
            GoalCommand::Invalid(_) => {}
        }
    }

    /// Open the editor on the objective, or on the check command.
    fn edit_goal(&mut self, check: bool, window: &mut Window, cx: &mut Context<Self>) {
        let goal = self.orch.goal.goal.as_ref();
        let current = if check {
            goal.and_then(|g| g.check.as_ref())
                .map(|c| c.command.clone())
        } else {
            goal.map(|g| g.objective.clone())
        }
        .unwrap_or_default();
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(if check {
                    "Command that verifies the goal, e.g. cargo test (empty removes the check)"
                } else {
                    "What should this thread achieve?"
                })
                .default_value(current)
        });
        let sub = cx.subscribe_in(&input, window, move |this, input, event, _, cx| {
            if let InputEvent::PressEnter { .. } = event {
                let text = input.read(cx).value().trim().to_string();
                if check {
                    let keep = this.orch.goal.goal.as_ref().and_then(|g| g.check.as_ref());
                    let spec = (!text.is_empty()).then(|| GoalCheckSpec {
                        command: text,
                        max_attempts: keep.map(|c| c.max_attempts),
                        timeout_secs: keep.map(|c| c.timeout_secs),
                    });
                    this.write_goal_check(None, spec, cx);
                } else if !text.is_empty() {
                    this.write_goal(Some(text), None, cx);
                }
            }
        });
        input.update(cx, |state, cx| state.focus(window, cx));
        self.orch.goal.editor = Some((input, sub));
        self.orch.goal.editing_check = check;
        cx.notify();
    }

    pub(super) fn render_goal(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let (success, warning) = (theme.success, theme.warning);
        if let Some((input, _)) = &self.orch.goal.editor {
            let icon = if self.orch.goal.editing_check {
                Icon::new(IconName::SquareTerminal)
            } else {
                Icon::new(ExtraIcon::Target)
            };
            return Some(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(icon.xsmall().text_color(muted))
                    .child(div().flex_1().child(Input::new(input).small()))
                    .child(
                        Button::new("goal-cancel")
                            .label("Cancel")
                            .ghost()
                            .xsmall()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.orch.goal.editor = None;
                                this.orch.goal.editing_check = false;
                                cx.notify();
                            })),
                    )
                    .into_any_element(),
            );
        }
        let goal = self.orch.goal.goal.as_ref()?;
        let check = goal.check.as_ref();
        let passed = check.is_some_and(|c| c.status == GoalCheckStatus::Passed);
        let title = match goal.status {
            GoalStatus::Active => "Pursuing goal",
            GoalStatus::Paused => "Goal paused",
            GoalStatus::Done if passed => "Goal met",
            GoalStatus::Done => "Goal complete",
        };
        let done = goal.status == GoalStatus::Done;
        let accent = if passed { success } else { muted };
        let chip = check.map(|c| {
            let (label, icon, tone) = check_chip(c);
            let color = match tone {
                Tone::Muted => muted,
                Tone::Good => success,
                Tone::Warn => warning,
            };
            h_flex()
                .id("goal-check-chip")
                .flex_none()
                .gap_1()
                .items_center()
                .text_color(color)
                .when(c.status == GoalCheckStatus::Running, |d| {
                    d.child(Spinner::new().xsmall())
                })
                .children(icon.map(|i| Icon::new(i).xsmall()))
                .child(label)
                .into_any_element()
        });
        let check_label = check
            .map(|c| format!(", check `{}`: {}", c.command, check_chip(c).0))
            .unwrap_or_default();
        // The last line of a failed run, under the strip.
        let tail = check
            .filter(|c| {
                matches!(
                    c.status,
                    GoalCheckStatus::Failed | GoalCheckStatus::NeedsYou
                )
            })
            .and_then(|c| c.output_tail.as_deref())
            .and_then(|t| t.lines().rev().find(|l| !l.trim().is_empty()))
            .map(|line| {
                format!(
                    "`{}` → {}",
                    check.map(|c| c.command.as_str()).unwrap_or(""),
                    line.trim()
                )
            });
        let can_run = check.is_some_and(|c| {
            !matches!(
                c.status,
                GoalCheckStatus::Pending | GoalCheckStatus::Running
            )
        });
        let can_stop = check.is_some_and(|c| {
            !matches!(
                c.status,
                GoalCheckStatus::Passed | GoalCheckStatus::Running | GoalCheckStatus::Stopped
            )
        }) && !done;
        let has_check = check.is_some();
        let strip = h_flex()
            .id("goal-strip")
            .group("goal-strip")
            .role(Role::Status)
            .aria_label(format!("{title}: {}{check_label}", goal.objective))
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
                    .text_color(accent)
                    .child(Icon::new(ExtraIcon::Target).xsmall()),
            )
            .child(
                div()
                    .flex_none()
                    .font_medium()
                    .when(passed, |d| d.text_color(success))
                    .child(title),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(muted)
                    .child(goal.objective.clone()),
            )
            .children(chip)
            .child(
                h_flex()
                    .gap_px()
                    .invisible()
                    .group_hover("goal-strip", |s| s.visible())
                    .when(can_run, |d| {
                        d.child(
                            Button::new("goal-check-run")
                                .icon(IconName::Play)
                                .ghost()
                                .xsmall()
                                .tooltip("Run check now")
                                .accessibility_label("Run check now")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.goal_check_action(GoalCheckAction::RunNow, cx)
                                })),
                        )
                    })
                    .when(can_stop, |d| {
                        d.child(
                            Button::new("goal-check-stop")
                                .icon(IconName::Square)
                                .ghost()
                                .xsmall()
                                .tooltip("Stop retrying")
                                .accessibility_label("Stop retrying")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.goal_check_action(GoalCheckAction::StopRetrying, cx)
                                })),
                        )
                    })
                    .child(
                        Button::new("goal-check-edit")
                            .icon(IconName::SquareTerminal)
                            .ghost()
                            .xsmall()
                            .tooltip(if has_check {
                                "Edit check"
                            } else {
                                "Add a check"
                            })
                            .accessibility_label(if has_check {
                                "Edit check"
                            } else {
                                "Add a check"
                            })
                            .on_click(
                                cx.listener(|this, _, window, cx| this.edit_goal(true, window, cx)),
                            ),
                    )
                    .child(
                        Button::new("goal-edit")
                            .icon(ExtraIcon::Pencil)
                            .ghost()
                            .xsmall()
                            .tooltip("Edit goal")
                            .accessibility_label("Edit goal")
                            .on_click(
                                cx.listener(|this, _, window, cx| {
                                    this.edit_goal(false, window, cx)
                                }),
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
                            .accessibility_label(if done { "Pursue again" } else { "Mark done" })
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
            );
        Some(
            v_flex()
                .children([strip.into_any_element()])
                .children(tail.map(|line| {
                    div()
                        .pl(px(28.))
                        .text_xs()
                        .font_family("monospace")
                        .text_color(muted)
                        .truncate()
                        .child(line)
                        .into_any_element()
                }))
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

    fn checked(objective: Option<&str>, command: &str) -> GoalCommand {
        GoalCommand::SetChecked {
            objective: objective.map(str::to_string),
            check: GoalCheckSpec {
                command: command.into(),
                max_attempts: None,
                timeout_secs: None,
            },
        }
    }

    #[test]
    fn check_flags_parse_quoted_bare_and_with_options() {
        assert_eq!(
            goal_command(r#"/goal tests pass --check "cargo test -p foo""#),
            Some(checked(Some("tests pass"), "cargo test -p foo"))
        );
        assert_eq!(
            goal_command("/goal lint clean --check 'npm run lint'"),
            Some(checked(Some("lint clean"), "npm run lint"))
        );
        assert_eq!(
            goal_command("/goal green --check make check"),
            Some(checked(Some("green"), "make check"))
        );
        assert_eq!(
            goal_command(r#"/goal --check "cargo test""#),
            Some(checked(None, "cargo test"))
        );
        assert_eq!(
            goal_command("/goal --check off"),
            Some(GoalCommand::ClearCheck)
        );
        assert_eq!(
            goal_command(r#"/goal ok --check "cargo test" --attempts 3 --timeout 300s"#),
            Some(GoalCommand::SetChecked {
                objective: Some("ok".into()),
                check: GoalCheckSpec {
                    command: "cargo test".into(),
                    max_attempts: Some(3),
                    timeout_secs: Some(300),
                },
            })
        );
        assert_eq!(
            goal_command("/goal x --check cargo test --attempts 2"),
            Some(GoalCommand::SetChecked {
                objective: Some("x".into()),
                check: GoalCheckSpec {
                    command: "cargo test".into(),
                    max_attempts: Some(2),
                    timeout_secs: None,
                },
            })
        );
        // `--checks` inside the objective is not the flag.
        assert_eq!(
            goal_command("/goal add --checks to CI"),
            Some(GoalCommand::Set("add --checks to CI".into()))
        );
        for bad in [
            r#"/goal x --check "cargo test"#,
            "/goal x --check",
            r#"/goal x --check "a" --attempts many"#,
            r#"/goal x --check "a" --verbose 1"#,
        ] {
            assert!(
                matches!(goal_command(bad), Some(GoalCommand::Invalid(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn chips_say_where_the_check_stands() {
        let mut c = GoalCheckRecord {
            command: "cargo test".into(),
            max_attempts: 5,
            timeout_secs: 120,
            status: GoalCheckStatus::Failed,
            attempts: 2,
            exit_code: Some(1),
            output_tail: None,
            ran_at: None,
        };
        assert_eq!(check_chip(&c).0, "Check failed 2/5");
        c.status = GoalCheckStatus::NeedsYou;
        c.attempts = 5;
        assert_eq!(check_chip(&c).0, "Needs you · failed 5/5");
        c.status = GoalCheckStatus::Passed;
        assert!(matches!(check_chip(&c).2, Tone::Good));
    }
}
