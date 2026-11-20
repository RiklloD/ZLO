//! Steers: text delivered into a running turn (t3code's steering model) —
//! the ledger, run-scoped Stop and readiness, and unread-steer recovery.
//!
//! Every message an admission path handles is one durable delivery row in
//! `orch_queue` (its id is the delivery id across every attempt and
//! fallback; `crate::admission`). Each attempt to steer it into a run gets a
//! [`SteerId`] and a row in `orch_steers` ("Steer · waiting to be read" until
//! the runtime reports it read). A steer still unread once its run is no
//! longer the thread's live run goes back to the queue ahead of messages
//! enqueued after it (by the monotonic `seq`), in one transaction with its
//! ledger transition; when a Stop reached that run it becomes a "Steer
//! delivery not confirmed" notice instead.

use std::time::Duration;

use ironclaw_host_api::ids::ThreadId;
use serde::Serialize;

use crate::engine::{Engine, Result};
use crate::error::OrchestratorError;
use crate::model::*;
use crate::runtime::*;
use crate::store::{Cols, Store, opt, s};

pub(crate) const STEERS_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS orch_steers (
  id TEXT PRIMARY KEY,
  thread_id TEXT NOT NULL,
  run_id TEXT,
  text TEXT NOT NULL,
  created_by TEXT NOT NULL,
  state TEXT NOT NULL,
  stop_reached INTEGER NOT NULL DEFAULT 0,
  sent_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS orch_steers_thread ON orch_steers(thread_id, state);
CREATE TABLE IF NOT EXISTS orch_run_stops (
  thread_id TEXT NOT NULL,
  run_id TEXT NOT NULL,
  at INTEGER NOT NULL,
  PRIMARY KEY (thread_id, run_id)
);
CREATE TABLE IF NOT EXISTS orch_run_ready (
  thread_id TEXT NOT NULL,
  run_id TEXT NOT NULL,
  at INTEGER NOT NULL,
  PRIMARY KEY (thread_id, run_id)
);
CREATE TABLE IF NOT EXISTS orch_submissions (
  thread_id TEXT NOT NULL,
  client_action_id TEXT NOT NULL,
  outcome TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (thread_id, client_action_id)
);
"#;

/// Title of the notice a steer Stop left unread gets (t3code's).
pub const STEER_UNCONFIRMED_TITLE: &str = "Steer delivery not confirmed";

/// Write attempts for one unread steer, and the pause before each retry.
const STEER_QUEUE_RETRIES: [Duration; 2] = [Duration::from_millis(50), Duration::from_millis(250)];

/// The run key Stop and readiness are recorded under (`""`: a run the
/// runtime gave no id).
fn run_key(run: Option<&str>) -> String {
    run.unwrap_or_default().to_string()
}

/// One steer attempt in the ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SteerRecord {
    pub id: SteerId,
    pub thread_id: ThreadId,
    /// The run it was sent into.
    pub run_id: Option<String>,
    pub text: String,
    pub created_by: Author,
    pub state: SteerState,
    /// The delivery (queue row) this attempt belongs to.
    pub queue_id: Option<QueueItemId>,
    pub sent_at: Millis,
    pub updated_at: Millis,
}

impl SteerRecord {
    /// The transcript marker: waiting to be read, or read.
    pub fn waiting_to_be_read(&self) -> bool {
        matches!(
            self.state,
            SteerState::Sending | SteerState::Pending | SteerState::Dropped
        )
    }

    /// The notice body of an unconfirmed steer (t3code's wording).
    pub fn unconfirmed_notice(&self) -> String {
        format!(
            "The agent did not confirm reading your steer before its turn ended. It may not \
             have been delivered. Resend it if needed: \"{}\"",
            self.text
        )
    }
}

const STEER_COLS: &str =
    "id, thread_id, run_id, text, created_by, state, queue_id, sent_at, updated_at";

fn steer_row(row: &libsql::Row) -> Result<SteerRecord> {
    let mut c = Cols::new(row);
    Ok(SteerRecord {
        id: SteerId::new(c.text()?).map_err(|e| OrchestratorError::Corrupt(e.to_string()))?,
        thread_id: c.thread()?,
        run_id: c.otext()?,
        text: c.text()?,
        created_by: Author::parse(&c.text()?)?,
        state: SteerState::parse(&c.text()?)?,
        queue_id: c.otext()?.map(QueueItemId::new).transpose()?,
        sent_at: c.int()?,
        updated_at: c.int()?,
    })
}

fn states_sql(states: &[SteerState]) -> String {
    states
        .iter()
        .map(|st| format!("'{}'", st.as_str()))
        .collect::<Vec<_>>()
        .join(",")
}

/// What the recovery of one steer did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Recovered {
    /// Back in the queue ahead of later messages.
    Requeued,
    /// A Stop reached its run: kept as a notice, not resent.
    Unconfirmed,
    /// Not unread any more (read, or recovered by another path).
    Skipped,
}

/// Where a settled delivery goes.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Settle {
    pub(crate) status: QueueStatus,
    pub(crate) kind: QueueKind,
    pub(crate) steer_run: Option<String>,
    pub(crate) claim_run: Option<String>,
    /// Move it ahead of every pending item.
    pub(crate) front: bool,
}

impl Settle {
    pub(crate) fn to(status: QueueStatus) -> Self {
        Self {
            status,
            kind: QueueKind::Message,
            steer_run: None,
            claim_run: None,
            front: false,
        }
    }
}

impl Store {
    /// Add a column when it is missing. Two backends opening the same file
    /// can both see it missing: the loser's "duplicate column" is success.
    pub(crate) async fn add_column_once(
        &self,
        table: &str,
        name: &str,
        definition: &str,
    ) -> Result<()> {
        let present = self
            .rows(&format!("PRAGMA table_info({table})"), vec![], |r| {
                let mut c = Cols::new(r);
                c.int()?;
                c.text()
            })
            .await?;
        if present.iter().any(|p| p == name) {
            return Ok(());
        }
        match self
            .exec(
                &format!("ALTER TABLE {table} ADD COLUMN {name} {definition}"),
                vec![],
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(error) if error.to_string().contains("duplicate column") => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub(crate) async fn upgrade_queue_schema(&self) -> Result<()> {
        for (name, definition) in [
            ("kind", "TEXT NOT NULL DEFAULT 'message'"),
            ("steer_run", "TEXT"),
            ("seq", "INTEGER"),
            ("claim", "TEXT"),
            ("claimed_at", "INTEGER"),
            ("claim_run", "TEXT"),
        ] {
            self.add_column_once("orch_queue", name, definition).await?;
        }
        self.add_column_once("orch_steers", "queue_id", "TEXT")
            .await?;
        // Rows from before the column keep their insertion order.
        self.exec(
            "UPDATE orch_queue SET seq = rowid WHERE seq IS NULL",
            vec![],
        )
        .await?;
        Ok(())
    }

    // ── deliveries ──────────────────────────────────────────────────────

    /// Insert one delivery: `claim` set means claimed at birth (`sending`),
    /// otherwise pending. Its `seq` is the next in the database.
    pub(crate) async fn queue_insert(
        &self,
        thread: &ThreadId,
        text: &str,
        author: Author,
        kind: QueueKind,
        steer_run: Option<&str>,
        claim: Option<&str>,
    ) -> Result<QueueItemId> {
        let id = QueueItemId::generate();
        let now = now_ms();
        let status = if claim.is_some() {
            QueueStatus::Sending
        } else {
            QueueStatus::Pending
        };
        self.exec(
            "INSERT INTO orch_queue (id, thread_id, position, text, status, created_by, created_at, \
             updated_at, kind, steer_run, seq, claim, claimed_at) \
             VALUES (?1, ?2, (SELECT COALESCE(MAX(position), 0) + 1 FROM orch_queue WHERE thread_id = ?2), \
             ?3, ?4, ?5, ?6, ?6, ?7, ?8, (SELECT COALESCE(MAX(seq), 0) + 1 FROM orch_queue), ?9, ?10)",
            vec![
                s(id.as_str()),
                s(thread.as_str()),
                s(text),
                s(status.as_str()),
                s(author.as_str()),
                now.into(),
                s(kind.as_str()),
                opt(steer_run.map(str::to_string)),
                opt(claim.map(str::to_string)),
                opt(claim.map(|_| now)),
            ],
        )
        .await?;
        Ok(id)
    }

    /// Claim a pending delivery for one admission attempt (pending →
    /// sending); `false` when someone else holds or settled it.
    pub(crate) async fn queue_claim(&self, id: &QueueItemId, claim: &str) -> Result<bool> {
        let now = now_ms();
        let n = self
            .exec(
                "UPDATE orch_queue SET status = 'sending', claim = ?2, claimed_at = ?3, updated_at = ?3 \
                 WHERE id = ?1 AND status = 'pending'",
                vec![s(id.as_str()), s(claim), now.into()],
            )
            .await?;
        Ok(n > 0)
    }

    /// Settle the delivery `claim` holds; `false` when the claim is gone
    /// (recovery took it back meanwhile).
    pub(crate) async fn queue_settle(
        &self,
        id: &QueueItemId,
        claim: &str,
        settle: &Settle,
    ) -> Result<bool> {
        let now = now_ms();
        let position = if settle.front {
            "(SELECT COALESCE(MIN(q.position), 1) - 1 FROM orch_queue q WHERE q.thread_id = orch_queue.thread_id \
             AND q.status IN ('pending','sending'))"
        } else {
            "position"
        };
        let n = self
            .exec(
                &format!(
                    "UPDATE orch_queue SET status = ?3, kind = ?4, steer_run = ?5, \
                     claim_run = COALESCE(?6, claim_run), position = {position}, updated_at = ?7, \
                     sent_at = CASE WHEN ?3 = 'sent' THEN ?7 ELSE sent_at END, \
                     claim = CASE WHEN ?3 = 'sending' THEN claim ELSE NULL END \
                     WHERE id = ?1 AND status = 'sending' AND claim = ?2"
                ),
                vec![
                    s(id.as_str()),
                    s(claim),
                    s(settle.status.as_str()),
                    s(settle.kind.as_str()),
                    opt(settle.steer_run.clone()),
                    opt(settle.claim_run.clone()),
                    now.into(),
                ],
            )
            .await?;
        Ok(n > 0)
    }

    /// Change a pending item's kind (a deferred steer whose run is gone
    /// runs as a message, keeping its place).
    pub(crate) async fn set_pending_kind(
        &self,
        id: &QueueItemId,
        kind: QueueKind,
        steer_run: Option<&str>,
    ) -> Result<bool> {
        let n = self
            .exec(
                "UPDATE orch_queue SET kind = ?2, steer_run = ?3, updated_at = ?4 \
                 WHERE id = ?1 AND status = 'pending'",
                vec![
                    s(id.as_str()),
                    s(kind.as_str()),
                    opt(steer_run.map(str::to_string)),
                    now_ms().into(),
                ],
            )
            .await?;
        Ok(n > 0)
    }

    pub(crate) async fn deferred_steer_threads(&self) -> Result<Vec<ThreadId>> {
        self.rows(
            "SELECT DISTINCT thread_id FROM orch_queue WHERE status = 'pending' AND kind = 'deferred_steer'",
            vec![],
            |r| Cols::new(r).thread(),
        )
        .await
    }

    // ── ledger ──────────────────────────────────────────────────────────

    pub(crate) async fn insert_steer(&self, steer: &SteerRecord) -> Result<bool> {
        let n = self
            .exec(
                &format!(
                    "INSERT INTO orch_steers ({STEER_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
                     ON CONFLICT(id) DO NOTHING"
                ),
                vec![
                    s(steer.id.as_str()),
                    s(steer.thread_id.as_str()),
                    opt(steer.run_id.clone()),
                    s(&steer.text),
                    s(steer.created_by.as_str()),
                    s(steer.state.as_str()),
                    opt(steer.queue_id.as_ref().map(|q| q.as_str().to_string())),
                    steer.sent_at.into(),
                    steer.updated_at.into(),
                ],
            )
            .await?;
        Ok(n > 0)
    }

    /// Move a steer to `to` when it is in one of `from`; `true` when moved.
    pub(crate) async fn set_steer_state(
        &self,
        thread: &ThreadId,
        id: &SteerId,
        from: &[SteerState],
        to: SteerState,
    ) -> Result<bool> {
        let n = self
            .exec(
                &format!(
                    "UPDATE orch_steers SET state = ?3, updated_at = ?4 \
                     WHERE thread_id = ?1 AND id = ?2 AND state IN ({})",
                    states_sql(from)
                ),
                vec![
                    s(thread.as_str()),
                    s(id.as_str()),
                    s(to.as_str()),
                    now_ms().into(),
                ],
            )
            .await?;
        Ok(n > 0)
    }

    pub async fn steer_record(
        &self,
        thread: &ThreadId,
        id: &SteerId,
    ) -> Result<Option<SteerRecord>> {
        self.row(
            &format!("SELECT {STEER_COLS} FROM orch_steers WHERE thread_id = ?1 AND id = ?2"),
            vec![s(thread.as_str()), s(id.as_str())],
            steer_row,
        )
        .await
    }

    /// Every steer sent into the thread, oldest first (not-sent ones left out).
    pub async fn steers(&self, thread: &ThreadId) -> Result<Vec<SteerRecord>> {
        self.rows(
            &format!(
                "SELECT {STEER_COLS} FROM orch_steers WHERE thread_id = ?1 AND state != 'not_sent' \
                 ORDER BY sent_at, rowid"
            ),
            vec![s(thread.as_str())],
            steer_row,
        )
        .await
    }

    /// Accepted-but-unread steers of the thread, oldest first.
    async fn open_steers(&self, thread: &ThreadId) -> Result<Vec<SteerRecord>> {
        self.rows(
            &format!(
                "SELECT {STEER_COLS} FROM orch_steers WHERE thread_id = ?1 \
                 AND state IN ('pending','dropped') ORDER BY sent_at, rowid"
            ),
            vec![s(thread.as_str())],
            steer_row,
        )
        .await
    }

    async fn threads_with_open_steers(&self) -> Result<Vec<ThreadId>> {
        self.rows(
            "SELECT DISTINCT thread_id FROM orch_steers WHERE state IN ('pending','dropped')",
            vec![],
            |r| Cols::new(r).thread(),
        )
        .await
    }

    /// A steered delivery: its ledger row leaves `sending` and its queue row
    /// is sent into `run`, in one transaction. `false` when the queue claim
    /// was taken back meanwhile.
    pub(crate) async fn settle_steered(
        &self,
        steer: &SteerRecord,
        state: SteerState,
        claim: &str,
    ) -> Result<bool> {
        let now = now_ms();
        let Some(queue_id) = &steer.queue_id else {
            return Ok(false);
        };
        self.tx(vec![
            (
                "UPDATE orch_steers SET state = ?3, updated_at = ?4 \
                 WHERE thread_id = ?1 AND id = ?2 AND state = 'sending'"
                    .to_string(),
                vec![
                    s(steer.thread_id.as_str()),
                    s(steer.id.as_str()),
                    s(state.as_str()),
                    now.into(),
                ],
            ),
            (
                "UPDATE orch_queue SET status = 'sent', sent_at = ?3, updated_at = ?3, \
                 claim_run = ?4, claim = NULL WHERE id = ?1 AND status = 'sending' AND claim = ?2"
                    .to_string(),
                vec![
                    s(queue_id.as_str()),
                    s(claim),
                    now.into(),
                    opt(steer.run_id.clone()),
                ],
            ),
        ])
        .await?;
        Ok(true)
    }

    /// Recover one unread steer in one transaction: re-check that it is
    /// still unread, check whether a Stop reached its run, then either note
    /// it unconfirmed or put its delivery back in the queue ahead of every
    /// pending item enqueued after it. A delivery the ledger has no row for
    /// (a runtime's own id) is created at the tail.
    pub(crate) async fn recover_steer(&self, thread: &ThreadId, id: &SteerId) -> Result<Recovered> {
        let conn = self.db.write().await?;
        let tx = conn
            .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
            .await?;
        let outcome = recover_in(&tx, thread, id).await;
        match outcome {
            Ok(done) => {
                tx.commit().await?;
                Ok(done)
            }
            Err(error) => {
                tx.rollback().await?;
                Err(error)
            }
        }
    }

    // ── run-scoped Stop and readiness ───────────────────────────────────

    pub(crate) async fn record_run_stop(&self, thread: &ThreadId, run: Option<&str>) -> Result<()> {
        self.exec(
            "INSERT OR IGNORE INTO orch_run_stops (thread_id, run_id, at) VALUES (?1, ?2, ?3)",
            vec![s(thread.as_str()), s(run_key(run)), now_ms().into()],
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn run_stopped(&self, thread: &ThreadId, run: Option<&str>) -> Result<bool> {
        Ok(self
            .row(
                "SELECT 1 FROM orch_run_stops WHERE thread_id = ?1 AND run_id = ?2",
                vec![s(thread.as_str()), s(run_key(run))],
                |r| Cols::new(r).int(),
            )
            .await?
            .is_some())
    }

    pub(crate) async fn record_run_ready(
        &self,
        thread: &ThreadId,
        run: Option<&str>,
    ) -> Result<()> {
        self.exec(
            "INSERT OR IGNORE INTO orch_run_ready (thread_id, run_id, at) VALUES (?1, ?2, ?3)",
            vec![s(thread.as_str()), s(run_key(run)), now_ms().into()],
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn run_ready(&self, thread: &ThreadId, run: Option<&str>) -> Result<bool> {
        Ok(self
            .row(
                "SELECT 1 FROM orch_run_ready WHERE thread_id = ?1 AND run_id = ?2",
                vec![s(thread.as_str()), s(run_key(run))],
                |r| Cols::new(r).int(),
            )
            .await?
            .is_some())
    }

    // ── submissions (client action idempotency) ─────────────────────────

    pub(crate) async fn submission(
        &self,
        thread: &ThreadId,
        client_action_id: &str,
    ) -> Result<Option<String>> {
        self.row(
            "SELECT outcome FROM orch_submissions WHERE thread_id = ?1 AND client_action_id = ?2",
            vec![s(thread.as_str()), s(client_action_id)],
            |r| Cols::new(r).text(),
        )
        .await
    }

    pub(crate) async fn put_submission(
        &self,
        thread: &ThreadId,
        client_action_id: &str,
        outcome: &str,
    ) -> Result<()> {
        self.exec(
            "INSERT OR IGNORE INTO orch_submissions (thread_id, client_action_id, outcome, created_at) \
             VALUES (?1, ?2, ?3, ?4)",
            vec![
                s(thread.as_str()),
                s(client_action_id),
                s(outcome),
                now_ms().into(),
            ],
        )
        .await?;
        Ok(())
    }

    // ── boot ────────────────────────────────────────────────────────────

    /// Settle deliveries a crash left claimed, before anything drains. A
    /// steer attempt in flight may or may not have reached the agent: it is
    /// kept as an unconfirmed notice, never replayed. A start claim counts
    /// as sent when the thread recorded a turn start after it, otherwise it
    /// goes back to pending.
    pub(crate) async fn reconcile_claims(&self) -> Result<()> {
        let claimed = self
            .rows(
                &format!(
                    "SELECT {} FROM orch_queue WHERE status = 'sending'",
                    crate::store::QUEUE_COLS
                ),
                vec![],
                crate::store::queue_row,
            )
            .await?;
        let now = now_ms();
        for item in claimed {
            let in_flight = self
                .rows(
                    &format!(
                        "SELECT {STEER_COLS} FROM orch_steers WHERE queue_id = ?1 AND state = 'sending'"
                    ),
                    vec![s(item.id.as_str())],
                    steer_row,
                )
                .await?;
            if !in_flight.is_empty() {
                let mut statements = vec![(
                    "UPDATE orch_queue SET status = 'sent', claim = NULL, updated_at = ?2, sent_at = ?2 \
                     WHERE id = ?1 AND status = 'sending'"
                        .to_string(),
                    vec![s(item.id.as_str()), now.into()],
                )];
                for steer in in_flight {
                    statements.push((
                        "UPDATE orch_steers SET state = 'unconfirmed', updated_at = ?2 \
                         WHERE id = ?1 AND state = 'sending'"
                            .to_string(),
                        vec![s(steer.id.as_str()), now.into()],
                    ));
                }
                self.tx(statements).await?;
                continue;
            }
            let started = match (self.thread(&item.thread_id).await?, item.claimed_at) {
                (Some(t), Some(claimed)) => t.turn_started_at.is_some_and(|at| at >= claimed),
                _ => false,
            };
            let (status, run) = if started {
                let run = self
                    .thread(&item.thread_id)
                    .await?
                    .and_then(|t| t.active_run_id);
                (QueueStatus::Sent, run)
            } else {
                (QueueStatus::Pending, None)
            };
            self.exec(
                "UPDATE orch_queue SET status = ?2, claim = NULL, claim_run = COALESCE(?3, claim_run), \
                 updated_at = ?4 WHERE id = ?1 AND status = 'sending'",
                vec![
                    s(item.id.as_str()),
                    s(status.as_str()),
                    opt(run),
                    now.into(),
                ],
            )
            .await?;
        }
        // Ledger rows whose delivery row is gone or never existed.
        self.exec(
            "UPDATE orch_steers SET state = 'unconfirmed', updated_at = ?1 WHERE state = 'sending'",
            vec![now.into()],
        )
        .await?;
        Ok(())
    }
}

async fn recover_in(
    tx: &libsql::Transaction,
    thread: &ThreadId,
    id: &SteerId,
) -> Result<Recovered> {
    let mut rows = tx
        .query(
            &format!("SELECT {STEER_COLS} FROM orch_steers WHERE thread_id = ?1 AND id = ?2"),
            vec![s(thread.as_str()), s(id.as_str())],
        )
        .await?;
    let steer = match rows.next().await? {
        Some(row) => steer_row(&row)?,
        None => return Ok(Recovered::Skipped),
    };
    drop(rows);
    if !matches!(steer.state, SteerState::Pending | SteerState::Dropped) {
        return Ok(Recovered::Skipped);
    }
    let now = now_ms();
    let mut stop = tx
        .query(
            "SELECT 1 FROM orch_run_stops WHERE thread_id = ?1 AND run_id = ?2",
            vec![s(thread.as_str()), s(run_key(steer.run_id.as_deref()))],
        )
        .await?;
    let stopped = stop.next().await?.is_some();
    drop(stop);
    if stopped {
        tx.execute(
            "UPDATE orch_steers SET state = 'unconfirmed', updated_at = ?2 WHERE id = ?1",
            vec![s(id.as_str()), now.into()],
        )
        .await?;
        return Ok(Recovered::Unconfirmed);
    }
    // The delivery's enqueue order, when it has a row.
    let mut own_seq = None;
    if let Some(queue_id) = &steer.queue_id {
        let mut rows = tx
            .query(
                "SELECT COALESCE(seq, 0) FROM orch_queue WHERE id = ?1",
                vec![s(queue_id.as_str())],
            )
            .await?;
        if let Some(row) = rows.next().await? {
            own_seq = Some(Cols::new(&row).int()?);
        }
        drop(rows);
    }
    let mut rows = tx
        .query(
            "SELECT position, COALESCE(seq, 0) FROM orch_queue WHERE thread_id = ?1 \
             AND status = 'pending' ORDER BY position, seq",
            vec![s(thread.as_str())],
        )
        .await?;
    let mut pending: Vec<(f64, i64)> = Vec::new();
    while let Some(row) = rows.next().await? {
        let mut c = Cols::new(&row);
        pending.push((c.real()?, c.int()?));
    }
    drop(rows);
    let later = own_seq.and_then(|mine| pending.iter().position(|(_, seq)| *seq > mine));
    let position = match later {
        Some(0) => pending.first().map_or(1.0, |p| p.0 - 1.0),
        Some(i) => match (pending.get(i - 1), pending.get(i)) {
            (Some(a), Some(b)) => (a.0 + b.0) / 2.0,
            _ => 1.0,
        },
        None => pending.last().map_or(1.0, |p| p.0 + 1.0),
    };
    let queue_id = match (&steer.queue_id, own_seq) {
        (Some(queue_id), Some(_)) => {
            tx.execute(
                "UPDATE orch_queue SET status = 'pending', kind = 'recovered_steer', steer_run = ?2, \
                 position = ?3, claim = NULL, updated_at = ?4 WHERE id = ?1",
                vec![
                    s(queue_id.as_str()),
                    opt(steer.run_id.clone()),
                    position.into(),
                    now.into(),
                ],
            )
            .await?;
            queue_id.clone()
        }
        _ => {
            let queue_id = unread_steer_queue_id(
                thread,
                steer.run_id.as_deref().unwrap_or_default(),
                steer.id.as_str(),
            )?;
            tx.execute(
                "INSERT INTO orch_queue (id, thread_id, position, text, status, created_by, created_at, \
                 updated_at, kind, steer_run, seq) VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, ?6, \
                 'recovered_steer', ?7, (SELECT COALESCE(MAX(seq), 0) + 1 FROM orch_queue)) \
                 ON CONFLICT(id) DO NOTHING",
                vec![
                    s(queue_id.as_str()),
                    s(thread.as_str()),
                    position.into(),
                    s(&steer.text),
                    s(steer.created_by.as_str()),
                    now.into(),
                    opt(steer.run_id.clone()),
                ],
            )
            .await?;
            queue_id
        }
    };
    tx.execute(
        "UPDATE orch_steers SET state = 'requeued', queue_id = ?2, updated_at = ?3 WHERE id = ?1",
        vec![s(id.as_str()), s(queue_id.as_str()), now.into()],
    )
    .await?;
    Ok(Recovered::Requeued)
}

impl Engine {
    /// The thread's steer ledger, oldest first: transcript markers ("Steer ·
    /// waiting to be read") and "Steer delivery not confirmed" notices.
    pub async fn thread_steers(&self, thread: &ThreadId) -> Result<Vec<SteerRecord>> {
        self.store.steers(thread).await
    }

    /// Recover one steer with bounded retries; `false` when it could not
    /// be written (it stays unread, and a later pass retries it).
    async fn recover_with_retries(&self, thread: &ThreadId, id: &SteerId) -> bool {
        let mut pauses = STEER_QUEUE_RETRIES.iter();
        loop {
            match self.store.recover_steer(thread, id).await {
                Ok(_) => return true,
                Err(error) => match pauses.next() {
                    Some(pause) => {
                        tracing::debug!(%error, "unread steer not recovered yet; retrying");
                        tokio::time::sleep(*pause).await;
                    }
                    None => {
                        tracing::debug!(%error, "unread steer could not be recovered");
                        return false;
                    }
                },
            }
        }
    }

    /// Recover the thread's unread steers whose run is no longer its live
    /// run. Idempotent and safe beside any admission: each steer moves in
    /// one transaction guarded by its ledger state. Returns the ones that
    /// could not be written.
    pub(crate) async fn recover_stranded(&self, thread_id: &ThreadId) -> Result<Vec<UnreadSteer>> {
        let thread = self.store.thread(thread_id).await?;
        let mut lost = Vec::new();
        for steer in self.store.open_steers(thread_id).await? {
            let live = thread
                .as_ref()
                .is_some_and(|t| t.is_running() && t.active_run_id == steer.run_id);
            if live {
                continue;
            }
            if !self.recover_with_retries(thread_id, &steer.id).await {
                lost.push(UnreadSteer {
                    id: steer.id.as_str().to_string(),
                    text: steer.text.clone(),
                });
            }
        }
        Ok(lost)
    }

    /// Tick step: steers left unread by runs that ended (a recovery that
    /// failed at the turn's end is retried here).
    pub(crate) async fn recover_all_stranded(&self) -> Result<()> {
        for thread in self.store.threads_with_open_steers().await? {
            self.recover_stranded(&thread).await?;
        }
        Ok(())
    }

    /// A runtime's report of unread steers (`TurnEventSink::steers_unread`):
    /// the run will not read them, so each is recovered now; one keyed by
    /// the runtime's own id is entered in the ledger first.
    pub(crate) async fn report_unread_steers(
        &self,
        thread: &ThreadId,
        run: &str,
        steers: Vec<UnreadSteer>,
    ) -> Vec<UnreadSteer> {
        let mut undelivered = Vec::new();
        for steer in steers {
            let recorded = self.mark_unread(thread, run, &steer).await;
            let kept = match recorded {
                Ok(id) => self.recover_with_retries(thread, &id).await,
                Err(error) => {
                    tracing::debug!(%error, "unread steer report not recorded");
                    false
                }
            };
            if !kept {
                undelivered.push(steer);
            }
        }
        undelivered
    }

    async fn mark_unread(
        &self,
        thread: &ThreadId,
        run: &str,
        steer: &UnreadSteer,
    ) -> Result<SteerId> {
        let id = SteerId::new(steer.id.clone())
            .map_err(|e| OrchestratorError::invalid(e.to_string()))?;
        if self.store.steer_record(thread, &id).await?.is_some() {
            self.store
                .set_steer_state(
                    thread,
                    &id,
                    &[SteerState::Sending, SteerState::Pending, SteerState::Read],
                    SteerState::Dropped,
                )
                .await?;
        } else {
            let now = now_ms();
            self.store
                .insert_steer(&SteerRecord {
                    id: id.clone(),
                    thread_id: thread.clone(),
                    run_id: Some(run.to_string()),
                    text: steer.text.clone(),
                    created_by: Author::User,
                    state: SteerState::Dropped,
                    queue_id: None,
                    sent_at: now,
                    updated_at: now,
                })
                .await?;
        }
        Ok(id)
    }

    /// The runtime's read report for one steer.
    pub(crate) async fn record_steer_read(
        &self,
        thread: &ThreadId,
        id: &SteerId,
        state: SteerReadState,
    ) -> Result<()> {
        let (from, to): (&[SteerState], SteerState) = match state {
            SteerReadState::Pending => (&[SteerState::Sending], SteerState::Pending),
            SteerReadState::Read => (
                &[
                    SteerState::Sending,
                    SteerState::Pending,
                    SteerState::Dropped,
                ],
                SteerState::Read,
            ),
            SteerReadState::Dropped => (
                &[SteerState::Sending, SteerState::Pending],
                SteerState::Dropped,
            ),
            // From any open state, and from Read for a runtime that does not
            // report reads (its delivered steers start Read).
            SteerReadState::Unconfirmed => (
                &[
                    SteerState::Sending,
                    SteerState::Pending,
                    SteerState::Read,
                    SteerState::Dropped,
                ],
                SteerState::Unconfirmed,
            ),
        };
        if !self.store.set_steer_state(thread, id, from, to).await? {
            tracing::debug!(steer = %id, "steer read report matched no open steer");
        }
        Ok(())
    }

    /// Stop on `thread`, recorded against its live run: steers into that
    /// run are never resent, and deferred steers aimed at it wait in the
    /// (held) queue as ordinary messages.
    pub(crate) async fn steers_on_stop(&self, thread: &ThreadId) -> Result<()> {
        let run = self
            .store
            .thread(thread)
            .await?
            .filter(ThreadRecord::is_running)
            .map(|t| t.active_run_id);
        if let Some(run) = &run {
            self.store.record_run_stop(thread, run.as_deref()).await?;
        }
        for item in self.store.queue(thread, false).await? {
            if item.kind == QueueKind::DeferredSteer {
                self.store
                    .set_pending_kind(&item.id, QueueKind::Message, None)
                    .await?;
            }
        }
        Ok(())
    }
}

/// The queue id of a recovered steer without a delivery row: one per
/// (thread, run, steer).
pub(crate) fn unread_steer_queue_id(
    thread: &ThreadId,
    run_id: &str,
    steer_id: &str,
) -> Result<QueueItemId> {
    use sha2::Digest;
    let mut hash = sha2::Sha256::new();
    for part in [thread.as_str(), run_id, steer_id] {
        hash.update(part.as_bytes());
        hash.update([0u8]);
    }
    let digest = hash.finalize();
    let hex: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    QueueItemId::new(format!("q_steer_{hex}"))
}
