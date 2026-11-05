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
//! Every change to a check is a new revision; approvals name the revision
//! the person saw, and each execution gets its own run id. A run is claimed
//! (`pending` → `running`) under the `dispatch` lock that Stop and automatic
//! turns take, and only the execution in flight may record a result, so an
//! edited, removed or re-run check never takes a stale one. A failure and its
//! follow-up commit in one transaction. Stop, a turn starting on the thread,
//! and shutdown cancel the run (the process group dies with it); Stop and a
//! turn put it back to `pending`, and runs a previous process left `running`
//! run again on boot. At most [`MAX_CONCURRENT_GOAL_CHECKS`] run at once.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ironclaw_host_api::ids::ThreadId;

use crate::engine::{Engine, Result};
use crate::goals::{CheckResult, CheckRun};
use crate::model::{ThreadRecord, now_ms};

/// Output kept on the goal and sent back to the agent: the last lines.
const TAIL_MAX_LINES: usize = 40;
const TAIL_MAX_CHARS: usize = 3_000;
/// Checks running at once across every thread; the rest wait in `pending`.
pub(crate) const MAX_CONCURRENT_GOAL_CHECKS: usize = 4;

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

/// The check executions in flight, one per thread: the run they belong to
/// and the handle that cancels them. Aborting drops the runner's future,
/// which kills the process group (`HostProcessPort`'s guard).
pub(crate) type GoalCheckTasks = HashMap<ThreadId, (CheckRun, tokio::task::AbortHandle)>;

impl Engine {
    /// Run goal checks through `runner` (first call wins). Without one,
    /// armed checks wait.
    pub fn set_goal_check_runner(&self, runner: Arc<dyn GoalCheckRunner>) {
        let _ = self.goal_check_runner.set(runner); // silent-ok: already set by an earlier wiring
    }

    /// Set, replace or remove (`None`) the goal's check, cancelling a run
    /// of the previous revision still in flight (its result would be stale).
    pub async fn set_goal_check(
        &self,
        thread: &ThreadId,
        spec: Option<&ironclaw_product_contracts::inbox::GoalCheckSpec>,
        now: crate::model::Millis,
    ) -> Result<Option<ironclaw_product_contracts::inbox::GoalRecord>> {
        let _guard = self.dispatch.lock().await;
        self.cancel_goal_check(thread).await?;
        self.store.set_goal_check(thread, spec, now).await
    }

    /// Cancel the goal check running on `thread` (the goal is being cleared
    /// or replaced): a public form of [`Self::cancel_goal_check`] that holds
    /// `dispatch` like every launch does.
    pub async fn stop_goal_check(&self, thread: &ThreadId) -> Result<()> {
        let _guard = self.dispatch.lock().await;
        self.cancel_goal_check(thread).await
    }

    /// A check execution is in flight on `thread` (automatic turns wait).
    pub(crate) fn goal_check_running(&self, thread: &ThreadId) -> bool {
        self.goal_check_tasks
            .lock()
            .is_ok_and(|tasks| tasks.contains_key(thread))
    }

    /// Cancel the check running on `thread`, if any, and put it back to
    /// `pending` (it runs again once the thread is idle and unheld). Callers
    /// that must not race a launch hold `dispatch`.
    pub(crate) async fn cancel_goal_check(&self, thread: &ThreadId) -> Result<()> {
        let taken = self
            .goal_check_tasks
            .lock()
            .map_err(crate::engine::lock_poisoned)?
            .remove(thread);
        if let Some((run, handle)) = taken {
            handle.abort();
            self.store.requeue_goal_check_run(&run).await?;
        }
        Ok(())
    }

    /// Shutdown: cancel every run in flight. Their rows stay `running`, so
    /// the next boot runs them again.
    pub(crate) fn abort_goal_checks(&self) {
        if let Ok(mut tasks) = self.goal_check_tasks.lock() {
            for (_, (_, handle)) in tasks.drain() {
                handle.abort();
            }
        }
    }

    /// The folder a check runs in. `Ok(None)` means the thread has no
    /// folder of its own (the host workspace); a profile that cannot be
    /// read, a source that fails, or a configured folder that is missing is
    /// an error, never a silent fallback to another directory.
    async fn goal_check_folder(
        &self,
        thread: &ThreadRecord,
    ) -> std::result::Result<Option<PathBuf>, String> {
        let Some(source) = self.profile_context.get() else {
            return Ok(None);
        };
        let profile = match self.store.thread_profile(&thread.thread_id).await {
            Ok(Some(profile)) => profile,
            Ok(None) if thread.profile_id.is_none() => return Ok(None),
            Ok(None) => return Err("the thread's profile no longer exists".into()),
            Err(error) => return Err(format!("the thread's profile could not be read: {error}")),
        };
        let configured = profile.settings.working_folder.is_some();
        match source.turn_profile(&profile).await {
            Ok(context) => match context.folder {
                Some(folder) => Ok(Some(folder)),
                None if configured => Err("the profile's working folder does not exist".into()),
                None => Ok(None),
            },
            Err(error) => Err(format!(
                "the profile's folder could not be resolved: {error}"
            )),
        }
    }

    /// Tick step: gate and start armed checks whose thread is idle and
    /// unheld, at most [`MAX_CONCURRENT_GOAL_CHECKS`] at once (the rest stay
    /// `pending`). The policy and the folder are read first; the hold and
    /// idle re-check, the claim and the launch then happen under `dispatch`,
    /// the lock Stop and automatic turns take, so neither can slip between.
    pub(crate) async fn run_goal_checks(&self) -> Result<()> {
        let Some(runner) = self.goal_check_runner.get().cloned() else {
            return Ok(());
        };
        for due in self.store.due_goal_checks().await? {
            let in_flight = self
                .goal_check_tasks
                .lock()
                .map_err(crate::engine::lock_poisoned)?
                .len();
            if in_flight >= MAX_CONCURRENT_GOAL_CHECKS {
                break;
            }
            if !self.goal_check_may_start(&due.thread_id).await? {
                continue;
            }
            let Some(thread) = self.store.thread(&due.thread_id).await? else {
                continue;
            };
            let gate = runner.gate(&thread).await;
            match &gate {
                GoalCheckGate::Allowed => {}
                GoalCheckGate::AskFirst | GoalCheckGate::AskEachRun if due.approved => {}
                GoalCheckGate::AskFirst | GoalCheckGate::AskEachRun => {
                    self.store.await_goal_check_approval(&due, now_ms()).await?;
                    continue;
                }
                GoalCheckGate::Refused(reason) => {
                    self.store.block_goal_check(&due, reason, now_ms()).await?;
                    continue;
                }
            }
            let cwd = match self.goal_check_folder(&thread).await {
                Ok(cwd) => cwd,
                Err(cause) => {
                    tracing::debug!(%cause, "goal check folder unresolved; not running it");
                    let reason = format!("The check could not run: {cause}.");
                    self.store.block_goal_check(&due, &reason, now_ms()).await?;
                    continue;
                }
            };
            let _guard = self.dispatch.lock().await;
            if !self.goal_check_may_start(&due.thread_id).await? {
                continue;
            }
            let spend = gate == GoalCheckGate::AskEachRun;
            let Some(run) = self.store.claim_goal_check(&due, spend, now_ms()).await? else {
                continue;
            };
            self.launch_goal_check(Arc::clone(&runner), run, thread, cwd)?;
        }
        Ok(())
    }

    /// Idle, unheld, and not already checking.
    async fn goal_check_may_start(&self, thread: &ThreadId) -> Result<bool> {
        if self.goal_check_running(thread) || self.store.hold(thread).await?.is_some() {
            return Ok(false);
        }
        Ok(self
            .store
            .thread(thread)
            .await?
            .is_some_and(|t| !t.is_running()))
    }

    /// Spawn the execution and register its handle before it can finish
    /// (the task removes itself under the same lock).
    fn launch_goal_check(
        &self,
        runner: Arc<dyn GoalCheckRunner>,
        run: CheckRun,
        thread: ThreadRecord,
        cwd: Option<PathBuf>,
    ) -> Result<()> {
        let Some(engine) = self.me.upgrade() else {
            return Ok(());
        };
        let request = GoalCheckRequest {
            thread,
            command: run.due.command.clone(),
            cwd,
            timeout: Duration::from_secs(run.due.timeout_secs),
        };
        let mut tasks = self
            .goal_check_tasks
            .lock()
            .map_err(crate::engine::lock_poisoned)?;
        let task_run = run.clone();
        let handle = tokio::spawn(async move {
            let outcome = runner.run(request).await;
            if let Err(error) = engine.finish_goal_check(&task_run, outcome).await {
                tracing::debug!(%error, "goal check result was not recorded");
            }
            if let Ok(mut tasks) = engine.goal_check_tasks.lock()
                && tasks
                    .get(&task_run.due.thread_id)
                    .is_some_and(|(r, _)| r.run_id == task_run.run_id)
            {
                tasks.remove(&task_run.due.thread_id);
            }
        });
        tasks.insert(run.due.thread_id.clone(), (run, handle.abort_handle()));
        Ok(())
    }

    /// Record a finished execution; a failure with attempts left writes the
    /// follow-up that sends the agent back to work in the same transaction.
    /// A run that is no longer the one in flight (the check changed, or it
    /// was cancelled) records nothing.
    pub(crate) async fn finish_goal_check(
        &self,
        run: &CheckRun,
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
        // Attempts cannot move while a run is in flight (the actions refuse
        // `running`, an edit replaces the run id), so this is the count the
        // store writes.
        let attempt = run.due.attempts.saturating_add(1);
        let follow_up = match &result {
            CheckResult::Failed { exit_code, output } if attempt < run.due.max_attempts => {
                Some(failure_follow_up(
                    &run.due.command,
                    *exit_code,
                    attempt,
                    run.due.max_attempts,
                    output,
                ))
            }
            _ => None,
        };
        self.store
            .record_goal_check_run(run, &result, follow_up.as_deref(), now_ms())
            .await
    }
}

#[cfg(test)]
mod review_tests;
#[cfg(test)]
mod tests;
