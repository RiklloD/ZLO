//! The durable record of coding-agent CLI tool approvals (`orch_cli_approvals`).
//!
//! The live gate (the waiting CLI call) lives in `ironclaw_cli_providers`'
//! bridge and dies with the backend, like the CLI process it belongs to.
//! This table keeps what was asked and what was answered, so a decision
//! outlives the process and a restart can say which calls it cut: rows still
//! open when the engine opens are closed as `interrupted`. Rows are never
//! deleted.

use ironclaw_host_api::ids::ThreadId;
use libsql::Value;

use crate::error::OrchestratorError;
use crate::model::Millis;
use crate::store::{Cols, Store, s};

type Result<T> = std::result::Result<T, OrchestratorError>;

pub(crate) const CLI_APPROVALS_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS orch_cli_approvals (
  gate_ref TEXT PRIMARY KEY,
  thread_id TEXT NOT NULL,
  run_id TEXT NOT NULL,
  tool_name TEXT NOT NULL,
  body TEXT,
  requested_at INTEGER NOT NULL,
  decision TEXT,
  reason TEXT,
  decided_at INTEGER
);
CREATE INDEX IF NOT EXISTS orch_cli_approvals_thread ON orch_cli_approvals(thread_id, requested_at);
"#;

/// How a CLI approval ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliApprovalDecision {
    Allowed,
    Declined,
    TimedOut,
    TurnEnded,
    /// The backend stopped while the call waited.
    Interrupted,
}

impl CliApprovalDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allowed => "allowed",
            Self::Declined => "declined",
            Self::TimedOut => "timed_out",
            Self::TurnEnded => "turn_ended",
            Self::Interrupted => "interrupted",
        }
    }

    fn parse(raw: &str) -> Result<Self> {
        Ok(match raw {
            "allowed" => Self::Allowed,
            "declined" => Self::Declined,
            "timed_out" => Self::TimedOut,
            "turn_ended" => Self::TurnEnded,
            "interrupted" => Self::Interrupted,
            other => {
                return Err(OrchestratorError::Corrupt(format!(
                    "unknown cli approval decision {other:?}"
                )));
            }
        })
    }
}

/// One asked tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliApprovalRecord {
    pub gate_ref: String,
    pub thread_id: ThreadId,
    pub run_id: String,
    pub tool_name: String,
    pub body: Option<String>,
    pub requested_at: Millis,
    pub decision: Option<CliApprovalDecision>,
    pub reason: Option<String>,
    pub decided_at: Option<Millis>,
}

impl Store {
    /// Record a call that now waits for its owner.
    pub async fn cli_approval_opened(&self, record: &CliApprovalRecord) -> Result<()> {
        self.exec(
            "INSERT INTO orch_cli_approvals (gate_ref, thread_id, run_id, tool_name, body, requested_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            vec![
                s(&record.gate_ref),
                s(record.thread_id.as_str()),
                s(&record.run_id),
                s(&record.tool_name),
                record.body.as_deref().map_or(Value::Null, s),
                Value::Integer(record.requested_at),
            ],
        )
        .await?;
        Ok(())
    }

    /// Close an open call with its decision; a closed one keeps its first.
    pub async fn cli_approval_closed(
        &self,
        gate_ref: &str,
        decision: CliApprovalDecision,
        reason: Option<&str>,
        now: Millis,
    ) -> Result<()> {
        self.exec(
            "UPDATE orch_cli_approvals SET decision = ?2, reason = ?3, decided_at = ?4 \
             WHERE gate_ref = ?1 AND decided_at IS NULL",
            vec![
                s(gate_ref),
                s(decision.as_str()),
                reason.map_or(Value::Null, s),
                Value::Integer(now),
            ],
        )
        .await?;
        Ok(())
    }

    /// A thread's CLI approvals, oldest first.
    pub async fn cli_approvals(&self, thread: &ThreadId) -> Result<Vec<CliApprovalRecord>> {
        self.rows(
            "SELECT gate_ref, thread_id, run_id, tool_name, body, requested_at, decision, reason, \
             decided_at FROM orch_cli_approvals WHERE thread_id = ?1 ORDER BY requested_at, rowid",
            vec![s(thread.as_str())],
            |r| {
                let mut c = Cols::new(r);
                Ok(CliApprovalRecord {
                    gate_ref: c.text()?,
                    thread_id: ThreadId::new(c.text()?)?,
                    run_id: c.text()?,
                    tool_name: c.text()?,
                    body: c.otext()?,
                    requested_at: c.int()?,
                    decision: c
                        .otext()?
                        .as_deref()
                        .map(CliApprovalDecision::parse)
                        .transpose()?,
                    reason: c.otext()?,
                    decided_at: c.oint()?,
                })
            },
        )
        .await
    }

    /// Engine open: every call still open belonged to a process that is gone.
    pub(crate) async fn close_interrupted_cli_approvals(&self, now: Millis) -> Result<u64> {
        self.exec(
            "UPDATE orch_cli_approvals SET decision = ?1, reason = 'backend restarted', \
             decided_at = ?2 WHERE decided_at IS NULL",
            vec![
                s(CliApprovalDecision::Interrupted.as_str()),
                Value::Integer(now),
            ],
        )
        .await
    }
}
