//! Automatic recovery: usage-limit resume and restart continuation.

use ironclaw_host_api::ids::ThreadId;
use serde_json::{Value, json};

use crate::engine::{Engine, Result, continue_prompt, skip_note};
use crate::model::*;
use crate::restart::{self, SkipReason};
use crate::runtime::TurnOrigin;
use crate::store::TaskPatch;

/// Accept reset times from one minute ago up to eight days ahead.
const USAGE_RESET_WINDOW_MS: (i64, i64) = (60_000, 8 * 86_400_000);

impl Engine {
    /// Arm (or re-arm, bounded) an automatic resume for a usage-limit
    /// failure. Returns whether a resume is armed.
    pub(crate) async fn arm_usage_limit(
        &self,
        thread: &ThreadId,
        reset_at: Millis,
        error: Option<&str>,
    ) -> Result<bool> {
        let now = now_ms();
        if reset_at <= now - USAGE_RESET_WINDOW_MS.0 || reset_at > now + USAGE_RESET_WINDOW_MS.1 {
            return Ok(false);
        }
        let detail = json!({"error": error.map(|e| clip(e, 500))});
        match self
            .store
            .recovery_at(thread, RecoveryKind::UsageLimit, reset_at)
            .await?
        {
            None => {
                self.store
                    .insert_recovery(&RecoveryRecord {
                        id: RecoveryId::generate(),
                        thread_id: thread.clone(),
                        kind: RecoveryKind::UsageLimit,
                        reset_at,
                        status: RecoveryStatus::Armed,
                        detail,
                        attempts: 0,
                        created_at: now,
                        fired_at: None,
                    })
                    .await?;
            }
            Some(existing) if existing.status == RecoveryStatus::Armed => {}
            Some(existing) => {
                // The same window failed again after a resume fired.
                if existing.attempts >= restart::MAX_USAGE_LIMIT_ATTEMPTS {
                    return Ok(false);
                }
                self.store.rearm_recovery(&existing.id, &detail).await?;
            }
        }
        Ok(true)
    }

    pub(crate) async fn fire_usage_limits(&self) -> Result<()> {
        let settings = self.store.settings().await?;
        if !settings.resume_on_usage_limit {
            return Ok(());
        }
        let due = now_ms() - settings.usage_limit_grace_ms;
        for r in self
            .store
            .armed_recoveries(RecoveryKind::UsageLimit)
            .await?
        {
            if r.reset_at > due {
                continue;
            }
            let Some(thread) = self.store.thread(&r.thread_id).await? else {
                self.store
                    .settle_recovery(&r.id, RecoveryStatus::Cancelled, Some("thread gone"))
                    .await?;
                continue;
            };
            if thread.last_user_at.unwrap_or(0) > r.created_at {
                // The user already moved on.
                self.store
                    .settle_recovery(&r.id, RecoveryStatus::Superseded, None)
                    .await?;
                continue;
            }
            if self
                .deliver(
                    &r.thread_id,
                    continue_prompt(),
                    TurnOrigin::AutoContinue,
                    true,
                )
                .await?
            {
                self.store
                    .settle_recovery(&r.id, RecoveryStatus::Fired, None)
                    .await?;
                if let Some(task) = self.store.task_for_child(&r.thread_id).await? {
                    self.store
                        .move_task(
                            &task.id,
                            &[TaskStatus::Waiting],
                            true,
                            TaskStatus::Running,
                            TaskPatch {
                                error: Some(None),
                                ..TaskPatch::default()
                            },
                        )
                        .await?;
                }
            }
        }
        Ok(())
    }

    /// Record (or merge into) the armed restart intent for `thread`.
    pub(crate) async fn record_restart_intent(
        &self,
        thread: &ThreadId,
        detail: Value,
        cut_at: Millis,
    ) -> Result<()> {
        if now_ms() - cut_at > restart::MAX_INTENT_AGE_MS
            || self.store.thread(thread).await?.is_none()
        {
            return Ok(());
        }
        if self
            .store
            .armed_recovery(thread, RecoveryKind::Restart)
            .await?
            .is_some()
        {
            return Ok(());
        }
        self.store
            .insert_recovery(&RecoveryRecord {
                id: RecoveryId::generate(),
                thread_id: thread.clone(),
                kind: RecoveryKind::Restart,
                reset_at: cut_at,
                status: RecoveryStatus::Armed,
                detail,
                attempts: 0,
                created_at: now_ms(),
                fired_at: None,
            })
            .await?;
        Ok(())
    }

    pub(crate) async fn fire_restarts(&self) -> Result<()> {
        let settings = self.store.settings().await?;
        for r in self.store.armed_recoveries(RecoveryKind::Restart).await? {
            let outcome = self.continue_restarted(&r, &settings).await?;
            let Some(outcome) = outcome else {
                continue; // busy or not reachable yet; the next tick retries
            };
            // A quit-interrupted child stays live until the user continues it.
            let keep_task = outcome == Err(SkipReason::Interrupted);
            let (status, note) = match outcome {
                Ok(()) => (RecoveryStatus::Fired, None),
                Err(reason) => (
                    match reason {
                        SkipReason::Superseded => RecoveryStatus::Superseded,
                        SkipReason::Exhausted => RecoveryStatus::Exhausted,
                        SkipReason::Expired => RecoveryStatus::Expired,
                        _ => RecoveryStatus::Skipped,
                    },
                    Some(skip_note(reason)),
                ),
            };
            if !self.store.settle_recovery(&r.id, status, note).await?
                || status == RecoveryStatus::Fired
                || keep_task
            {
                continue;
            }
            // A delegated child that will not continue still owes its parent a result.
            if let Some(task) = self.store.task_for_child(&r.thread_id).await?
                && matches!(
                    task.status,
                    TaskStatus::Starting | TaskStatus::Running | TaskStatus::WaitingChildren
                )
            {
                let why = note.unwrap_or("not continued").replace('_', " ");
                self.finalize(
                    &task,
                    TaskStatus::Interrupted,
                    task.result.clone(),
                    Some(format!(
                        "The backend restarted and the task was not continued ({why})."
                    )),
                )
                .await?;
            }
        }
        Ok(())
    }

    /// T3 `continueRestartedRun`. `None` = retry later; `Some(Err)` = settle
    /// without continuing.
    async fn continue_restarted(
        &self,
        r: &RecoveryRecord,
        settings: &Settings,
    ) -> Result<Option<std::result::Result<(), SkipReason>>> {
        if !settings.continue_after_restart {
            return Ok(Some(Err(SkipReason::Disabled)));
        }
        let Some(thread) = self.store.thread(&r.thread_id).await? else {
            return Ok(Some(Err(SkipReason::Expired)));
        };
        let cut_at = r.reset_at;
        if thread.last_user_at.unwrap_or(0) > cut_at {
            return Ok(Some(Err(SkipReason::Superseded)));
        }
        match self.store.hold(&r.thread_id).await? {
            Some(HoldReason::Interrupted) => return Ok(Some(Err(SkipReason::Interrupted))),
            Some(_) => return Ok(Some(Err(SkipReason::Stopped))),
            None => {}
        }
        // A later turn already concluded on this thread: nothing is cut any more.
        if !thread.is_running() && thread.turn_ended_at.unwrap_or(0) > cut_at {
            let later_completed = self
                .store
                .turn_log(&r.thread_id, 1)
                .await?
                .first()
                .is_some_and(|t| {
                    t.ended_at > cut_at && t.status == crate::runtime::TurnEndStatus::Completed
                });
            if later_completed {
                return Ok(Some(Err(SkipReason::Superseded)));
            }
        }
        if restart::is_maintenance_command(r.detail["prompt"].as_str()) {
            return Ok(Some(Err(SkipReason::Maintenance)));
        }
        // A turn that keeps killing the backend would otherwise restart forever.
        let since = thread.last_user_at.unwrap_or(0);
        if self.store.restart_chain(&r.thread_id, since).await? >= restart::MAX_CONTINUATION_CHAIN {
            return Ok(Some(Err(SkipReason::Exhausted)));
        }
        if now_ms() - r.created_at > restart::MAX_INTENT_AGE_MS {
            return Ok(Some(Err(SkipReason::Expired)));
        }
        if self
            .deliver(
                &r.thread_id,
                continue_prompt(),
                TurnOrigin::AutoContinue,
                true,
            )
            .await?
        {
            return Ok(Some(Ok(())));
        }
        Ok(None)
    }
}
