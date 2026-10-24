//! Thread goals (v2 stage 5, t3code's goal strip): one persisted objective
//! per thread in `orch_goals`, shown above the composer. Clearing keeps the
//! row with `cleared_at` set, so a goal's history is never deleted.

use ironclaw_host_api::ids::ThreadId;
use ironclaw_product_contracts::inbox::{GoalRecord, GoalStatus};
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

/// Like a profile's text fields: short enough for one strip line.
const MAX_OBJECTIVE_CHARS: usize = 2000;

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

impl Store {
    /// The thread's goal, `None` when it has none or it was cleared.
    pub async fn goal(&self, thread: &ThreadId) -> Result<Option<GoalRecord>> {
        let row = self
            .row(
                "SELECT objective, status, updated_at FROM orch_goals \
                 WHERE thread_id = ?1 AND cleared_at IS NULL",
                vec![s(thread.as_str())],
                |r| {
                    let mut c = Cols::new(r);
                    Ok((c.text()?, c.text()?, c.int()?))
                },
            )
            .await?;
        row.map(|(objective, status, updated_at)| {
            Ok(GoalRecord {
                objective,
                status: parse_status(&status)?,
                updated_at,
            })
        })
        .transpose()
    }

    /// Set the objective (and status), change only the status of the
    /// current goal, or clear it (`objective` and `status` both `None`).
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
                     objective = ?2, status = ?3, updated_at = ?4, cleared_at = NULL",
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
}
