//! Thread goals (v2 stage 5, t3code's goal strip): one persisted objective
//! per thread in `orch_goals`, shown above the composer. Clearing keeps the
//! row with `cleared_at` set, so a goal's history is never deleted.
//!
//! A goal may carry a check: a shell command that verifies it, run by the
//! engine after each completed turn (`crate::goal_checks`). Its spec and
//! where it stands live in the `check_*` columns of the same row; a goal
//! stored before checks existed reads with none.

use ironclaw_host_api::ids::ThreadId;
use ironclaw_product_contracts::inbox::{
    GoalCheckAction, GoalCheckRecord, GoalCheckSpec, GoalCheckStatus, GoalRecord, GoalStatus,
};
use libsql::Value;

use crate::error::OrchestratorError;
use crate::model::Millis;
use crate::store::{Cols, Store, s};

type Result<T> = std::result::Result<T, OrchestratorError>;

pub(crate) const GOALS_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS orch_goals (
  thread_id TEXT PRIMARY KEY,
  objective TEXT NOT NULL,
  status TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  cleared_at INTEGER
);
"#;

/// Check columns added to `orch_goals` in place (older files gain them on
/// open; their goals keep `check_command` NULL, i.e. no check).
const CHECK_COLUMNS: [(&str, &str); 9] = [
    ("check_command", "TEXT"),
    ("check_max_attempts", "INTEGER"),
    ("check_timeout_secs", "INTEGER"),
    ("check_status", "TEXT"),
    ("check_attempts", "INTEGER"),
    ("check_exit_code", "INTEGER"),
    ("check_output", "TEXT"),
    ("check_ran_at", "INTEGER"),
    // The command the person approved to run without asking (Ask first).
    ("check_approved", "TEXT"),
];

/// Like a profile's text fields: short enough for one strip line.
const MAX_OBJECTIVE_CHARS: usize = 2000;
const MAX_CHECK_COMMAND_CHARS: usize = 2000;
pub const DEFAULT_CHECK_MAX_ATTEMPTS: u32 = 5;
const MAX_CHECK_MAX_ATTEMPTS: u32 = 50;
pub const DEFAULT_CHECK_TIMEOUT_SECS: u64 = 120;
pub const MAX_CHECK_TIMEOUT_SECS: u64 = 600;

/// The columns `goal` reads, in [`read_goal`]'s order.
const GOAL_COLUMNS: &str = "objective, status, updated_at, check_command, check_max_attempts, \
     check_timeout_secs, check_status, check_attempts, check_exit_code, check_output, check_ran_at";

fn status_text(status: GoalStatus) -> &'static str {
    match status {
        GoalStatus::Active => "active",
        GoalStatus::Paused => "paused",
        GoalStatus::Done => "done",
    }
}

fn parse_status(raw: &str) -> Result<GoalStatus> {
    match raw {
        "active" => Ok(GoalStatus::Active),
        "paused" => Ok(GoalStatus::Paused),
        "done" => Ok(GoalStatus::Done),
        other => Err(OrchestratorError::Corrupt(format!(
            "unknown goal status {other:?}"
        ))),
    }
}

pub(crate) fn check_status_text(status: GoalCheckStatus) -> &'static str {
    match status {
        GoalCheckStatus::NotRun => "not_run",
        GoalCheckStatus::Pending => "pending",
        GoalCheckStatus::AwaitingApproval => "awaiting_approval",
        GoalCheckStatus::Running => "running",
        GoalCheckStatus::Passed => "passed",
        GoalCheckStatus::Failed => "failed",
        GoalCheckStatus::NeedsYou => "needs_you",
        GoalCheckStatus::Stopped => "stopped",
    }
}

fn parse_check_status(raw: &str) -> Result<GoalCheckStatus> {
    Ok(match raw {
        "not_run" => GoalCheckStatus::NotRun,
        "pending" => GoalCheckStatus::Pending,
        "awaiting_approval" => GoalCheckStatus::AwaitingApproval,
        "running" => GoalCheckStatus::Running,
        "passed" => GoalCheckStatus::Passed,
        "failed" => GoalCheckStatus::Failed,
        "needs_you" => GoalCheckStatus::NeedsYou,
        "stopped" => GoalCheckStatus::Stopped,
        other => {
            return Err(OrchestratorError::Corrupt(format!(
                "unknown goal check status {other:?}"
            )));
        }
    })
}

fn count(raw: Option<i64>, default: u64) -> u64 {
    raw.and_then(|n| u64::try_from(n).ok()).unwrap_or(default)
}

fn read_goal(r: &libsql::Row) -> Result<GoalRecord> {
    let mut c = Cols::new(r);
    let objective = c.text()?;
    let status = parse_status(&c.text()?)?;
    let updated_at = c.int()?;
    let command = c.otext()?;
    let max_attempts = c.oint()?;
    let timeout_secs = c.oint()?;
    let check_status = c.otext()?;
    let attempts = c.oint()?;
    let exit_code = c.oint()?;
    let output_tail = c.otext()?;
    let ran_at = c.oint()?;
    let check = match command {
        None => None,
        Some(command) => Some(GoalCheckRecord {
            command,
            max_attempts: u32::try_from(count(max_attempts, u64::from(DEFAULT_CHECK_MAX_ATTEMPTS)))
                .unwrap_or(DEFAULT_CHECK_MAX_ATTEMPTS),
            timeout_secs: count(timeout_secs, DEFAULT_CHECK_TIMEOUT_SECS),
            status: match check_status {
                Some(raw) => parse_check_status(&raw)?,
                None => GoalCheckStatus::NotRun,
            },
            attempts: u32::try_from(count(attempts, 0)).unwrap_or(u32::MAX),
            exit_code,
            output_tail,
            ran_at,
        }),
    };
    Ok(GoalRecord {
        objective,
        status,
        updated_at,
        check,
    })
}

/// A check validated for storage.
struct ValidCheck {
    command: String,
    max_attempts: u32,
    timeout_secs: u64,
}

fn validate_check(spec: &GoalCheckSpec) -> Result<ValidCheck> {
    let command = spec.command.trim();
    if command.is_empty() || command.chars().count() > MAX_CHECK_COMMAND_CHARS {
        return Err(OrchestratorError::invalid(
            "check command must be 1-2000 characters",
        ));
    }
    if command.chars().any(|c| c.is_control() && c != '\t') {
        return Err(OrchestratorError::invalid(
            "check command must be one line without control characters",
        ));
    }
    let max_attempts = spec.max_attempts.unwrap_or(DEFAULT_CHECK_MAX_ATTEMPTS);
    if !(1..=MAX_CHECK_MAX_ATTEMPTS).contains(&max_attempts) {
        return Err(OrchestratorError::invalid(
            "check max_attempts must be 1-50",
        ));
    }
    let timeout_secs = spec.timeout_secs.unwrap_or(DEFAULT_CHECK_TIMEOUT_SECS);
    if !(1..=MAX_CHECK_TIMEOUT_SECS).contains(&timeout_secs) {
        return Err(OrchestratorError::invalid(
            "check timeout must be 1-600 seconds",
        ));
    }
    Ok(ValidCheck {
        command: command.to_string(),
        max_attempts,
        timeout_secs,
    })
}

/// A check the engine is about to gate or run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DueCheck {
    pub thread_id: ThreadId,
    pub command: String,
    pub max_attempts: u32,
    pub timeout_secs: u64,
    pub attempts: u32,
    /// The person approved this exact command.
    pub approved: bool,
}

/// How one check run ended, as the store records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CheckResult {
    Passed {
        output: String,
    },
    Failed {
        exit_code: i64,
        output: String,
    },
    /// It could not run, or the policy refused it: the person decides.
    Blocked {
        reason: String,
    },
}

impl Store {
    /// Add the check columns to an `orch_goals` table that predates them.
    pub(crate) async fn upgrade_goals_schema(&self) -> Result<()> {
        let present = self
            .rows("PRAGMA table_info(orch_goals)", vec![], |r| {
                let mut c = Cols::new(r);
                c.int()?;
                c.text()
            })
            .await?;
        for (name, definition) in CHECK_COLUMNS {
            if !present.iter().any(|p| p == name) {
                self.exec(
                    &format!("ALTER TABLE orch_goals ADD COLUMN {name} {definition}"),
                    vec![],
                )
                .await?;
            }
        }
        Ok(())
    }

    /// The thread's goal, `None` when it has none or it was cleared.
    pub async fn goal(&self, thread: &ThreadId) -> Result<Option<GoalRecord>> {
        self.row(
            &format!(
                "SELECT {GOAL_COLUMNS} FROM orch_goals WHERE thread_id = ?1 AND cleared_at IS NULL"
            ),
            vec![s(thread.as_str())],
            read_goal,
        )
        .await
    }

    /// Set the objective (and status), change only the status of the
    /// current goal, or clear it (`objective` and `status` both `None`).
    /// Replacing the objective keeps the check; a goal set again after it
    /// was cleared starts without one.
    pub async fn set_goal(
        &self,
        thread: &ThreadId,
        objective: Option<&str>,
        status: Option<GoalStatus>,
        now: Millis,
    ) -> Result<Option<GoalRecord>> {
        let objective = objective.map(str::trim);
        if let Some(text) = objective
            && (text.is_empty() || text.chars().count() > MAX_OBJECTIVE_CHARS)
        {
            return Err(OrchestratorError::invalid(
                "objective must be 1-2000 characters",
            ));
        }
        let id = s(thread.as_str());
        match (objective, status) {
            (Some(text), status) => {
                let status = status_text(status.unwrap_or(GoalStatus::Active));
                self.exec(
                    "INSERT INTO orch_goals (thread_id, objective, status, created_at, updated_at) \
                     VALUES (?1, ?2, ?3, ?4, ?4) ON CONFLICT (thread_id) DO UPDATE SET \
                     objective = ?2, status = ?3, updated_at = ?4, \
                     check_command = CASE WHEN orch_goals.cleared_at IS NULL \
                       THEN orch_goals.check_command ELSE NULL END, \
                     cleared_at = NULL",
                    vec![id, s(text), s(status), Value::Integer(now)],
                )
                .await?;
            }
            (None, Some(status)) => {
                let changed = self
                    .exec(
                        "UPDATE orch_goals SET status = ?2, updated_at = ?3 \
                         WHERE thread_id = ?1 AND cleared_at IS NULL",
                        vec![id, s(status_text(status)), Value::Integer(now)],
                    )
                    .await?;
                if changed == 0 {
                    return Err(OrchestratorError::invalid("this thread has no goal"));
                }
            }
            (None, None) => {
                self.exec(
                    "UPDATE orch_goals SET cleared_at = ?2, updated_at = ?2 \
                     WHERE thread_id = ?1 AND cleared_at IS NULL",
                    vec![id, Value::Integer(now)],
                )
                .await?;
            }
        }
        self.goal(thread).await
    }

    /// Set (or replace) the goal's check, or remove it (`None`). A new or
    /// changed spec starts over: not run, no attempts. An approval carries
    /// over only while the command stays the same.
    pub async fn set_goal_check(
        &self,
        thread: &ThreadId,
        spec: Option<&GoalCheckSpec>,
        now: Millis,
    ) -> Result<Option<GoalRecord>> {
        let changed = match spec {
            Some(spec) => {
                let check = validate_check(spec)?;
                self.exec(
                    "UPDATE orch_goals SET check_command = ?2, check_max_attempts = ?3, \
                     check_timeout_secs = ?4, check_status = 'not_run', check_attempts = 0, \
                     check_exit_code = NULL, check_output = NULL, check_ran_at = NULL, \
                     check_approved = CASE WHEN check_approved = ?2 THEN check_approved ELSE NULL END, \
                     updated_at = ?5 WHERE thread_id = ?1 AND cleared_at IS NULL",
                    vec![
                        s(thread.as_str()),
                        s(&check.command),
                        Value::Integer(i64::from(check.max_attempts)),
                        Value::Integer(i64::try_from(check.timeout_secs).unwrap_or(i64::MAX)),
                        Value::Integer(now),
                    ],
                )
                .await?
            }
            None => {
                self.exec(
                    "UPDATE orch_goals SET check_command = NULL, check_status = NULL, \
                     updated_at = ?2 WHERE thread_id = ?1 AND cleared_at IS NULL",
                    vec![s(thread.as_str()), Value::Integer(now)],
                )
                .await?
            }
        };
        if changed == 0 {
            return Err(OrchestratorError::invalid("this thread has no goal"));
        }
        self.goal(thread).await
    }

    /// The person's action on the check. "Run check now" approves the
    /// command, re-arms the goal and, once retries ended, restarts the
    /// attempt count; it is a no-op while a run is in flight.
    pub async fn goal_check_action(
        &self,
        thread: &ThreadId,
        action: GoalCheckAction,
        now: Millis,
    ) -> Result<Option<GoalRecord>> {
        let Some(goal) = self.goal(thread).await? else {
            return Err(OrchestratorError::invalid("this thread has no goal"));
        };
        if goal.check.is_none() {
            return Err(OrchestratorError::invalid("this goal has no check"));
        }
        let sql = match action {
            GoalCheckAction::RunNow => {
                "UPDATE orch_goals SET check_status = 'pending', status = 'active', \
                 check_approved = check_command, \
                 check_attempts = CASE WHEN check_status IN ('needs_you', 'stopped') \
                   THEN 0 ELSE check_attempts END, \
                 updated_at = ?2 \
                 WHERE thread_id = ?1 AND cleared_at IS NULL AND check_command IS NOT NULL \
                   AND check_status NOT IN ('running', 'pending')"
            }
            GoalCheckAction::StopRetrying => {
                "UPDATE orch_goals SET check_status = 'stopped', updated_at = ?2 \
                 WHERE thread_id = ?1 AND cleared_at IS NULL AND check_command IS NOT NULL \
                   AND check_status NOT IN ('passed', 'running')"
            }
        };
        self.exec(sql, vec![s(thread.as_str()), Value::Integer(now)])
            .await?;
        self.goal(thread).await
    }

    /// A turn on `thread` completed: an active goal's check runs next,
    /// unless it waits on the person, already runs, or retries ended.
    pub(crate) async fn arm_goal_check(&self, thread: &ThreadId, now: Millis) -> Result<bool> {
        let changed = self
            .exec(
                "UPDATE orch_goals SET check_status = 'pending', updated_at = ?2 \
                 WHERE thread_id = ?1 AND cleared_at IS NULL AND status = 'active' \
                   AND check_command IS NOT NULL \
                   AND COALESCE(check_status, 'not_run') IN ('not_run', 'failed')",
                vec![s(thread.as_str()), Value::Integer(now)],
            )
            .await?;
        Ok(changed > 0)
    }

    /// Checks waiting to run, oldest first.
    pub(crate) async fn due_goal_checks(&self) -> Result<Vec<DueCheck>> {
        self.rows(
            "SELECT thread_id, check_command, check_max_attempts, check_timeout_secs, \
             check_attempts, check_approved = check_command FROM orch_goals \
             WHERE cleared_at IS NULL AND status = 'active' AND check_command IS NOT NULL \
               AND check_status = 'pending' ORDER BY updated_at",
            vec![],
            |r| {
                let mut c = Cols::new(r);
                Ok(DueCheck {
                    thread_id: c.thread()?,
                    command: c.text()?,
                    max_attempts: u32::try_from(count(
                        c.oint()?,
                        u64::from(DEFAULT_CHECK_MAX_ATTEMPTS),
                    ))
                    .unwrap_or(DEFAULT_CHECK_MAX_ATTEMPTS),
                    timeout_secs: count(c.oint()?, DEFAULT_CHECK_TIMEOUT_SECS),
                    attempts: u32::try_from(count(c.oint()?, 0)).unwrap_or(u32::MAX),
                    approved: c.oint()? == Some(1),
                })
            },
        )
        .await
    }

    /// Move a check from `from` to `to` when it is still `from` and its
    /// command is still `command`; false when someone changed it first.
    pub(crate) async fn move_goal_check(
        &self,
        thread: &ThreadId,
        command: &str,
        from: GoalCheckStatus,
        to: GoalCheckStatus,
        now: Millis,
    ) -> Result<bool> {
        let changed = self
            .exec(
                "UPDATE orch_goals SET check_status = ?4, updated_at = ?5 \
                 WHERE thread_id = ?1 AND cleared_at IS NULL AND check_command = ?2 \
                   AND check_status = ?3",
                vec![
                    s(thread.as_str()),
                    s(command),
                    s(check_status_text(from)),
                    s(check_status_text(to)),
                    Value::Integer(now),
                ],
            )
            .await?;
        Ok(changed > 0)
    }

    /// Record how a run of `command` ended, from `from` (`running`, or
    /// `pending` for a check refused before it ran). `max_attempts` bounds
    /// the retries: the failure that reaches it hands over to the person.
    /// `None` when the check changed meanwhile (nothing written).
    pub(crate) async fn record_goal_check(
        &self,
        thread: &ThreadId,
        command: &str,
        from: GoalCheckStatus,
        result: &CheckResult,
        now: Millis,
    ) -> Result<Option<GoalRecord>> {
        let (sql, params): (&str, Vec<Value>) = match result {
            CheckResult::Passed { output } => (
                "UPDATE orch_goals SET check_status = 'passed', status = 'done', \
                 check_exit_code = 0, check_output = ?4, check_ran_at = ?5, updated_at = ?5 \
                 WHERE thread_id = ?1 AND cleared_at IS NULL AND check_command = ?2 \
                   AND check_status = ?3",
                vec![s(output), Value::Integer(now)],
            ),
            CheckResult::Failed { exit_code, output } => (
                "UPDATE orch_goals SET check_attempts = COALESCE(check_attempts, 0) + 1, \
                 check_status = CASE WHEN COALESCE(check_attempts, 0) + 1 >= \
                   COALESCE(check_max_attempts, 5) THEN 'needs_you' ELSE 'failed' END, \
                 check_exit_code = ?4, check_output = ?5, check_ran_at = ?6, updated_at = ?6 \
                 WHERE thread_id = ?1 AND cleared_at IS NULL AND check_command = ?2 \
                   AND check_status = ?3",
                vec![Value::Integer(*exit_code), s(output), Value::Integer(now)],
            ),
            CheckResult::Blocked { reason } => (
                "UPDATE orch_goals SET check_status = 'needs_you', check_exit_code = NULL, \
                 check_output = ?4, check_ran_at = ?5, updated_at = ?5 \
                 WHERE thread_id = ?1 AND cleared_at IS NULL AND check_command = ?2 \
                   AND check_status = ?3",
                vec![s(reason), Value::Integer(now)],
            ),
        };
        let mut all = vec![s(thread.as_str()), s(command), s(check_status_text(from))];
        all.extend(params);
        if self.exec(sql, all).await? == 0 {
            return Ok(None);
        }
        self.goal(thread).await
    }

    /// Spend a one-run approval (the shell tool asks each time).
    pub(crate) async fn consume_goal_check_approval(&self, thread: &ThreadId) -> Result<()> {
        self.exec(
            "UPDATE orch_goals SET check_approved = NULL WHERE thread_id = ?1",
            vec![s(thread.as_str())],
        )
        .await?;
        Ok(())
    }

    /// Runs a previous process left `running` were cut by its exit: they
    /// run again.
    pub(crate) async fn requeue_running_goal_checks(&self) -> Result<u64> {
        self.exec(
            "UPDATE orch_goals SET check_status = 'pending' WHERE check_status = 'running'",
            vec![],
        )
        .await
    }
}
