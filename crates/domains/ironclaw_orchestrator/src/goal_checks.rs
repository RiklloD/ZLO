//! Goal checks: a goal that is verified, not just noted.
//!
//! A goal may carry a shell command (`/goal <text> --check "<cmd>"`). When a
//! turn on the thread completes, on any runtime, the check is armed
//! (`pending`); the tick runs it once the thread is idle and unheld:
//!
//! - The host's [`GoalCheckRunner`] first answers what the thread's approval
//!   policy says about a shell command. Refused → the check stops and needs
//!   the person. Ask first → the first run of a given command waits
//!   (`awaiting_approval`) until the person approves it ("Run check now");
//!   an approval holds for that exact command only.
//! - The runner runs the command in the thread's folder through the host's
//!   process runtime (never a raw spawn here). Exit 0 meets the goal (`done`).
//!   Non-zero queues a follow-up to the same thread through the durable
//!   queue, with a bounded output tail, so the agent keeps going; the turn
//!   it starts re-arms the check. The failure that reaches `max_attempts`
//!   stops and needs the person instead.
//!
//! A run is claimed (`pending` → `running`) and recorded with conditional
//! updates on the command, so a changed or removed check never takes a stale
//! result; runs a previous process left `running` run again on boot.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use ironclaw_product_contracts::inbox::GoalCheckStatus;

use crate::engine::{Engine, Result};
use crate::goals::{CheckResult, DueCheck};
use crate::model::{Author, ThreadRecord, now_ms};

/// Output kept on the goal and sent back to the agent: the last lines.
const TAIL_MAX_LINES: usize = 40;
const TAIL_MAX_CHARS: usize = 3_000;

/// What the thread's approval policy says about running a shell command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GoalCheckGate {
    /// Full access: run it.
    Allowed,
    /// Ask first: run only a command the person approved.
    AskFirst,
    /// The shell tool asks each time: every run needs its own approval.
    AskEachRun,
    /// The policy refuses shell commands here (a tool override, a deployment
    /// without a process runtime); the reason is shown to the person.
    Refused(String),
}

/// One check run as the host executes it.
#[derive(Debug, Clone, PartialEq)]
pub struct GoalCheckRequest {
    pub thread: ThreadRecord,
    pub command: String,
    /// The thread's working folder (its profile's), when it has one; the
    /// host's workspace otherwise.
    pub cwd: Option<PathBuf>,
    pub timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalCheckOutput {
    pub exit_code: i64,
    /// Combined stdout/stderr as the process runtime captured (and bounded) it.
    pub output: String,
}

/// Runs goal checks for the engine; plugged in by the host, which owns the
/// process runtime and the approval settings.
#[async_trait]
pub trait GoalCheckRunner: Send + Sync {
    async fn gate(&self, thread: &ThreadRecord) -> GoalCheckGate;
    /// `Err` is a run that could not happen (no runtime, spawn failure,
    /// timeout); it is shown to the person, not counted as an attempt.
    async fn run(&self, request: GoalCheckRequest) -> std::result::Result<GoalCheckOutput, String>;
}

/// The last [`TAIL_MAX_LINES`] lines of `output`, at most [`TAIL_MAX_CHARS`]
/// characters, cut on character boundaries.
pub fn output_tail(output: &str) -> String {
    let trimmed = output.trim_end();
    let lines: Vec<&str> = trimmed.lines().collect();
    let start = lines.len().saturating_sub(TAIL_MAX_LINES);
    let tail = lines[start..].join("\n");
    let chars = tail.chars().count();
    if chars <= TAIL_MAX_CHARS {
        return tail;
    }
    let skip = chars - TAIL_MAX_CHARS;
    let cut: String = tail.chars().skip(skip).collect();
    format!("…{cut}")
}

/// The follow-up the agent reads after a failed check.
pub fn failure_follow_up(
    command: &str,
    exit_code: i64,
    attempt: u32,
    max_attempts: u32,
    tail: &str,
) -> String {
    let tail = if tail.is_empty() { "(no output)" } else { tail };
    format!(
        "Goal check failed (attempt {attempt}/{max_attempts}): `{command}` exited {exit_code}. \
         Output tail:\n```\n{tail}\n```\nKeep working toward the goal; the check runs again \
         when this turn ends."
    )
}

impl Engine {
    /// Run goal checks through `runner` (first call wins). Without one,
    /// armed checks wait.
    pub fn set_goal_check_runner(&self, runner: std::sync::Arc<dyn GoalCheckRunner>) {
        let _ = self.goal_check_runner.set(runner); // silent-ok: already set by an earlier wiring
    }

    /// Tick step: gate and start every armed check whose thread is idle and
    /// unheld. Runs are spawned; their results land through
    /// [`Self::finish_goal_check`].
    pub(crate) async fn run_goal_checks(&self) -> Result<()> {
        let Some(runner) = self.goal_check_runner.get().cloned() else {
            return Ok(());
        };
        for due in self.store.due_goal_checks().await? {
            let Some(thread) = self.store.thread(&due.thread_id).await? else {
                continue;
            };
            if thread.is_running() || self.store.hold(&due.thread_id).await?.is_some() {
                continue;
            }
            let gate = runner.gate(&thread).await;
            match &gate {
                GoalCheckGate::Allowed => {}
                GoalCheckGate::AskFirst | GoalCheckGate::AskEachRun if due.approved => {}
                GoalCheckGate::AskFirst | GoalCheckGate::AskEachRun => {
                    self.store
                        .move_goal_check(
                            &due.thread_id,
                            &due.command,
                            GoalCheckStatus::Pending,
                            GoalCheckStatus::AwaitingApproval,
                            now_ms(),
                        )
                        .await?;
                    continue;
                }
                GoalCheckGate::Refused(reason) => {
                    self.store
                        .record_goal_check(
                            &due.thread_id,
                            &due.command,
                            GoalCheckStatus::Pending,
                            &CheckResult::Blocked {
                                reason: reason.clone(),
                            },
                            now_ms(),
                        )
                        .await?;
                    continue;
                }
            }
            if !self
                .store
                .move_goal_check(
                    &due.thread_id,
                    &due.command,
                    GoalCheckStatus::Pending,
                    GoalCheckStatus::Running,
                    now_ms(),
                )
                .await?
            {
                continue;
            }
            if gate == GoalCheckGate::AskEachRun {
                // This run spends the approval; the next one asks again.
                self.store
                    .consume_goal_check_approval(&due.thread_id)
                    .await?;
            }
            let Some(engine) = self.me.upgrade() else {
                return Ok(());
            };
            let request = GoalCheckRequest {
                cwd: self
                    .turn_profile(&due.thread_id)
                    .await
                    .and_then(|p| p.folder),
                thread,
                command: due.command.clone(),
                timeout: Duration::from_secs(due.timeout_secs),
            };
            let runner = std::sync::Arc::clone(&runner);
            tokio::spawn(async move {
                let outcome = runner.run(request).await;
                if let Err(error) = engine.finish_goal_check(&due, outcome).await {
                    tracing::debug!(%error, "goal check result was not recorded");
                }
            });
        }
        Ok(())
    }

    /// Record a finished run and, on a failure with attempts left, queue
    /// the follow-up that sends the agent back to work.
    pub(crate) async fn finish_goal_check(
        &self,
        due: &DueCheck,
        outcome: std::result::Result<GoalCheckOutput, String>,
    ) -> Result<()> {
        let result = match outcome {
            Ok(out) if out.exit_code == 0 => CheckResult::Passed {
                output: output_tail(&out.output),
            },
            Ok(out) => CheckResult::Failed {
                exit_code: out.exit_code,
                output: output_tail(&out.output),
            },
            Err(reason) => CheckResult::Blocked {
                reason: format!("The check could not run: {reason}"),
            },
        };
        let Some(goal) = self
            .store
            .record_goal_check(
                &due.thread_id,
                &due.command,
                GoalCheckStatus::Running,
                &result,
                now_ms(),
            )
            .await?
        else {
            return Ok(()); // the check changed while it ran; its result is stale
        };
        if let (CheckResult::Failed { exit_code, output }, Some(check)) = (&result, &goal.check)
            && check.status == GoalCheckStatus::Failed
        {
            let text = failure_follow_up(
                &check.command,
                *exit_code,
                check.attempts,
                check.max_attempts,
                output,
            );
            self.store
                .queue_add(&due.thread_id, &text, Author::System)
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
