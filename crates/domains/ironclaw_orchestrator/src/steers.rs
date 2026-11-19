//! Steers: text delivered into a running turn (t3code's steering model).
//!
//! Every steer the engine hands a runtime gets a [`SteerId`] and a row in
//! `orch_steers`, the ledger the transcript reads ("Steer · waiting to be
//! read" until the runtime reports it read). A steer still unread when its
//! turn ends is sent again as a queued turn that keeps its place ahead of
//! messages queued after it was sent; after a Stop it is not resent but
//! kept as a "Steer delivery not confirmed" notice the person decides on.
//! A steer that reaches a turn which cannot take input yet waits in the
//! queue as a deferred steer and is delivered once the runtime reports the
//! turn ready, or runs as the next turn if that turn ends first.

use std::time::Duration;

use ironclaw_host_api::ids::ThreadId;
use serde::Serialize;

use crate::engine::{Engine, Result};
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
"#;

/// Title of the notice a steer Stop left unconfirmed gets (t3code's).
pub const STEER_UNCONFIRMED_TITLE: &str = "Steer delivery not confirmed";

/// How long a send waits for a turn its runtime says has ended to be
/// recorded as ended, before it falls back to the queue head.
const TURN_END_WAIT: Duration = Duration::from_secs(3);
const TURN_END_POLL: Duration = Duration::from_millis(25);

/// Write attempts for one unread steer, and the pause before each retry.
const STEER_QUEUE_RETRIES: [Duration; 2] = [Duration::from_millis(50), Duration::from_millis(250)];

/// One steer in the ledger.
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
    /// A Stop reached its run while it was unread.
    pub stop_reached: bool,
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
            "The agent did not confirm reading your steer before Stop. It may not have been \
             delivered. Resend it if needed: \"{}\"",
            self.text
        )
    }

    fn unread(&self) -> bool {
        matches!(
            self.state,
            SteerState::Sending | SteerState::Pending | SteerState::Dropped
        )
    }
}

const STEER_COLS: &str =
    "id, thread_id, run_id, text, created_by, state, stop_reached, sent_at, updated_at";

fn steer_row(row: &libsql::Row) -> Result<SteerRecord> {
    let mut c = Cols::new(row);
    Ok(SteerRecord {
        id: SteerId::new(c.text()?)
            .map_err(|e| crate::error::OrchestratorError::Corrupt(e.to_string()))?,
        thread_id: c.thread()?,
        run_id: c.otext()?,
        text: c.text()?,
        created_by: Author::parse(&c.text()?)?,
        state: SteerState::parse(&c.text()?)?,
        stop_reached: c.int()? != 0,
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

impl Store {
    pub(crate) async fn upgrade_queue_schema(&self) -> Result<()> {
        let present = self
            .rows("PRAGMA table_info(orch_queue)", vec![], |r| {
                let mut c = Cols::new(r);
                c.int()?;
                c.text()
            })
            .await?;
        for (name, definition) in [
            ("kind", "TEXT NOT NULL DEFAULT 'message'"),
            ("steer_run", "TEXT"),
        ] {
            if !present.iter().any(|p| p == name) {
                self.exec(
                    &format!("ALTER TABLE orch_queue ADD COLUMN {name} {definition}"),
                    vec![],
                )
                .await?;
            }
        }
        Ok(())
    }

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
                    i64::from(steer.stop_reached).into(),
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

    /// Unread steers of `run` (every run when `None`), oldest first.
    async fn unread_steers(
        &self,
        thread: &ThreadId,
        run: Option<&str>,
    ) -> Result<Vec<SteerRecord>> {
        self.rows(
            &format!(
                "SELECT {STEER_COLS} FROM orch_steers WHERE thread_id = ?1 \
                 AND state IN ('sending','pending','dropped') \
                 AND (?2 IS NULL OR run_id IS NULL OR run_id = ?2) ORDER BY sent_at, rowid"
            ),
            vec![s(thread.as_str()), opt(run.map(str::to_string))],
            steer_row,
        )
        .await
    }

    /// A Stop reached every unread steer of the thread.
    async fn mark_steers_stop_reached(&self, thread: &ThreadId) -> Result<()> {
        self.exec(
            "UPDATE orch_steers SET stop_reached = 1, updated_at = ?2 \
             WHERE thread_id = ?1 AND state IN ('sending','pending','dropped')",
            vec![s(thread.as_str()), now_ms().into()],
        )
        .await?;
        Ok(())
    }

    /// Queue `text` with a kind; at the head of the pending items when
    /// `front`, otherwise at the tail.
    pub(crate) async fn queue_add_kind(
        &self,
        thread: &ThreadId,
        text: &str,
        author: Author,
        kind: QueueKind,
        steer_run: Option<&str>,
        front: bool,
    ) -> Result<QueueItemId> {
        let id = QueueItemId::generate();
        let now = now_ms();
        let position = if front {
            "(SELECT COALESCE(MIN(position), 1) - 1 FROM orch_queue WHERE thread_id = ?2 AND status IN ('pending','sending'))"
        } else {
            "(SELECT COALESCE(MAX(position), 0) + 1 FROM orch_queue WHERE thread_id = ?2)"
        };
        self.exec(
            &format!(
                "INSERT INTO orch_queue (id, thread_id, position, text, status, created_by, created_at, updated_at, kind, steer_run) \
                 VALUES (?1, ?2, {position}, ?3, 'pending', ?4, ?5, ?5, ?6, ?7)"
            ),
            vec![
                s(id.as_str()),
                s(thread.as_str()),
                s(text),
                s(author.as_str()),
                now.into(),
                s(kind.as_str()),
                opt(steer_run.map(str::to_string)),
            ],
        )
        .await?;
        Ok(id)
    }

    /// Queue a recovered steer sent at `sent_at` ahead of every pending item
    /// queued after that moment (several keep their send order). A second
    /// call with the same id is a no-op; `true` when this call inserted it.
    pub(crate) async fn queue_insert_steer(
        &self,
        id: &QueueItemId,
        thread: &ThreadId,
        text: &str,
        author: Author,
        sent_at: Millis,
        steer_run: Option<&str>,
    ) -> Result<bool> {
        let conn = self.db.write().await?;
        let tx = conn
            .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
            .await?;
        let mut exists = tx
            .query(
                "SELECT 1 FROM orch_queue WHERE id = ?1",
                vec![s(id.as_str())],
            )
            .await?;
        if exists.next().await?.is_some() {
            drop(exists);
            tx.commit().await?;
            return Ok(false);
        }
        drop(exists);
        let mut rows = tx
            .query(
                "SELECT position, created_at, COALESCE(kind, 'message') FROM orch_queue \
                 WHERE thread_id = ?1 AND status = 'pending' ORDER BY position, created_at",
                vec![s(thread.as_str())],
            )
            .await?;
        let mut pending: Vec<(f64, Millis, QueueKind)> = Vec::new();
        while let Some(row) = rows.next().await? {
            let mut c = Cols::new(&row);
            pending.push((c.real()?, c.int()?, QueueKind::parse(&c.text()?)?));
        }
        drop(rows);
        // Same millisecond: a message counts as later; a steer recovered
        // before this one stays ahead.
        let later = pending.iter().position(|(_, at, kind)| {
            *at > sent_at || (*at == sent_at && *kind != QueueKind::RecoveredSteer)
        });
        let position = match later {
            Some(0) => pending[0].0 - 1.0,
            Some(i) => (pending[i - 1].0 + pending[i].0) / 2.0,
            None => {
                let mut max = tx
                    .query(
                        "SELECT COALESCE(MAX(position), 0) + 1 FROM orch_queue WHERE thread_id = ?1",
                        vec![s(thread.as_str())],
                    )
                    .await?;
                let value = match max.next().await? {
                    Some(row) => Cols::new(&row).real()?,
                    None => 1.0,
                };
                drop(max);
                value
            }
        };
        let inserted = tx
            .execute(
                "INSERT INTO orch_queue (id, thread_id, position, text, status, created_by, created_at, updated_at, kind, steer_run) \
                 VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, ?7, 'recovered_steer', ?8)",
                vec![
                    s(id.as_str()),
                    s(thread.as_str()),
                    position.into(),
                    s(text),
                    s(author.as_str()),
                    sent_at.into(),
                    now_ms().into(),
                    opt(steer_run.map(str::to_string)),
                ],
            )
            .await;
        match inserted {
            Ok(_) => {
                tx.commit().await?;
                Ok(true)
            }
            Err(error) => {
                tx.rollback().await?;
                Err(error.into())
            }
        }
    }

    pub(crate) async fn set_queue_kind(
        &self,
        id: &QueueItemId,
        kind: QueueKind,
        steer_run: Option<&str>,
    ) -> Result<()> {
        self.exec(
            "UPDATE orch_queue SET kind = ?2, steer_run = ?3, updated_at = ?4 WHERE id = ?1",
            vec![
                s(id.as_str()),
                s(kind.as_str()),
                opt(steer_run.map(str::to_string)),
                now_ms().into(),
            ],
        )
        .await?;
        Ok(())
    }

    async fn deferred_steer_threads(&self) -> Result<Vec<ThreadId>> {
        self.rows(
            "SELECT DISTINCT thread_id FROM orch_queue WHERE status = 'pending' AND kind = 'deferred_steer'",
            vec![],
            |r| Cols::new(r).thread(),
        )
        .await
    }
}

/// What a steer attempt did, and the ledger id of a delivered one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SteerAttempt {
    pub(crate) outcome: SteerOutcome,
    pub(crate) id: SteerId,
}

impl Engine {
    /// The thread's steer ledger, oldest first: transcript markers ("Steer ·
    /// waiting to be read") and "Steer delivery not confirmed" notices.
    pub async fn thread_steers(&self, thread: &ThreadId) -> Result<Vec<SteerRecord>> {
        self.store.steers(thread).await
    }

    /// Hand `text` to the running turn of `thread` as one steer, recorded in
    /// the ledger. A steer never kills the turn: on a runtime without
    /// in-turn steering it is `Rejected` without a call.
    pub(crate) async fn steer_text(
        &self,
        thread: &ThreadRecord,
        text: &str,
        how: SteerHow,
        author: Author,
    ) -> Result<SteerAttempt> {
        let runtime = self.runtime(&thread.runtime_id)?;
        let id = SteerId::generate();
        if how == SteerHow::Steer && !runtime.capabilities().steer_in_turn {
            return Ok(SteerAttempt {
                outcome: SteerOutcome::Rejected,
                id,
            });
        }
        let now = now_ms();
        // Written before the call: the runtime may report it read before
        // its answer returns.
        self.store
            .insert_steer(&SteerRecord {
                id: id.clone(),
                thread_id: thread.thread_id.clone(),
                run_id: thread.active_run_id.clone(),
                text: text.to_string(),
                created_by: author,
                state: SteerState::Sending,
                stop_reached: false,
                sent_at: now,
                updated_at: now,
            })
            .await?;
        let request = SteerRequest {
            id: id.clone(),
            text: text.to_string(),
            how,
        };
        let result = runtime.steer_message(thread, &request).await;
        let next = match &result {
            Ok(SteerOutcome::Steered) if runtime.reports_steer_reads() => SteerState::Pending,
            Ok(SteerOutcome::Steered) => SteerState::Read,
            // The restarted turn's prompt is this text.
            Ok(SteerOutcome::Restarted) => SteerState::Read,
            _ => SteerState::NotSent,
        };
        self.store
            .set_steer_state(&thread.thread_id, &id, &[SteerState::Sending], next)
            .await?;
        let outcome = result?;
        Ok(SteerAttempt { outcome, id })
    }

    /// Wait (bounded) for a turn its runtime no longer has to be recorded
    /// as ended. `true` once the thread is idle.
    pub(crate) async fn wait_turn_end(&self, thread: &ThreadId) -> Result<bool> {
        let deadline = tokio::time::Instant::now() + TURN_END_WAIT;
        loop {
            match self.store.thread(thread).await? {
                Some(t) if t.is_running() => {}
                _ => return Ok(true),
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(TURN_END_POLL).await;
        }
    }

    /// Recorded unread steers of `run` (every run when `None`): queued ahead
    /// of later messages, or, after a Stop, kept as an unconfirmed notice.
    /// Returns the ones that could not be queued.
    pub(crate) async fn recover_unread_steers(
        &self,
        thread: &ThreadId,
        run: Option<&str>,
    ) -> Result<Vec<UnreadSteer>> {
        let mut lost = Vec::new();
        for steer in self.store.unread_steers(thread, run).await? {
            if !self.recover_steer(&steer).await? {
                lost.push(UnreadSteer {
                    id: steer.id.as_str().to_string(),
                    text: steer.text.clone(),
                });
            }
        }
        Ok(lost)
    }

    /// `false` when the steer could not be queued (after retries).
    async fn recover_steer(&self, steer: &SteerRecord) -> Result<bool> {
        let unread = [
            SteerState::Sending,
            SteerState::Pending,
            SteerState::Dropped,
        ];
        if steer.stop_reached {
            self.store
                .set_steer_state(
                    &steer.thread_id,
                    &steer.id,
                    &unread,
                    SteerState::Unconfirmed,
                )
                .await?;
            return Ok(true);
        }
        let run = steer.run_id.as_deref().unwrap_or("");
        let qid = unread_steer_queue_id(&steer.thread_id, run, steer.id.as_str())?;
        let mut pauses = STEER_QUEUE_RETRIES.iter();
        loop {
            match self
                .store
                .queue_insert_steer(
                    &qid,
                    &steer.thread_id,
                    &steer.text,
                    steer.created_by,
                    steer.sent_at,
                    steer.run_id.as_deref(),
                )
                .await
            {
                Ok(_) => break,
                Err(error) => match pauses.next() {
                    Some(pause) => {
                        tracing::debug!(%error, "unread steer not queued yet; retrying");
                        tokio::time::sleep(*pause).await;
                    }
                    None => {
                        tracing::debug!(%error, "unread steer could not be queued");
                        return Ok(false);
                    }
                },
            }
        }
        self.store
            .set_steer_state(&steer.thread_id, &steer.id, &unread, SteerState::Requeued)
            .await?;
        Ok(true)
    }

    /// A runtime's report of unread steers (`TurnEventSink::steers_unread`):
    /// each goes through the ledger, recorded first when the runtime keyed
    /// it by an id of its own.
    pub(crate) async fn report_unread_steers(
        &self,
        thread: &ThreadId,
        run: &str,
        steers: Vec<UnreadSteer>,
    ) -> Vec<UnreadSteer> {
        let mut undelivered = Vec::new();
        for steer in steers {
            match self.report_unread_steer(thread, run, &steer).await {
                Ok(true) => {}
                Ok(false) => undelivered.push(steer),
                Err(error) => {
                    tracing::debug!(%error, "unread steer could not be recovered; reported back");
                    undelivered.push(steer);
                }
            }
        }
        undelivered
    }

    async fn report_unread_steer(
        &self,
        thread: &ThreadId,
        run: &str,
        steer: &UnreadSteer,
    ) -> Result<bool> {
        let id = SteerId::new(steer.id.clone())
            .map_err(|e| crate::error::OrchestratorError::invalid(e.to_string()))?;
        let record = match self.store.steer_record(thread, &id).await? {
            Some(record) => record,
            None => {
                let now = now_ms();
                let record = SteerRecord {
                    id: id.clone(),
                    thread_id: thread.clone(),
                    run_id: Some(run.to_string()),
                    text: steer.text.clone(),
                    created_by: Author::User,
                    state: SteerState::Pending,
                    stop_reached: self.store.hold(thread).await? == Some(HoldReason::Stopped),
                    sent_at: now,
                    updated_at: now,
                };
                self.store.insert_steer(&record).await?;
                record
            }
        };
        match record.state {
            // Already queued or noted (a repeated report).
            SteerState::Requeued | SteerState::Unconfirmed => Ok(true),
            _ => {
                let mut record = record;
                // The runtime's report wins over an earlier "read".
                if record.state == SteerState::Read {
                    self.store
                        .set_steer_state(thread, &id, &[SteerState::Read], SteerState::Pending)
                        .await?;
                    record.state = SteerState::Pending;
                }
                if !record.unread() {
                    return Ok(true);
                }
                self.recover_steer(&record).await
            }
        }
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
                    SteerState::NotSent,
                ],
                SteerState::Read,
            ),
            SteerReadState::Dropped => (
                &[SteerState::Sending, SteerState::Pending],
                SteerState::Dropped,
            ),
        };
        if !self.store.set_steer_state(thread, id, from, to).await? {
            tracing::debug!(steer = %id, "steer read report matched no open steer");
        }
        Ok(())
    }

    /// A Stop on `thread`: unread steers are not resent, deferred ones wait
    /// in the (held) queue as ordinary messages.
    pub(crate) async fn steers_on_stop(&self, thread: &ThreadId) -> Result<()> {
        self.store.mark_steers_stop_reached(thread).await?;
        for item in self.store.queue(thread, false).await? {
            if item.kind == QueueKind::DeferredSteer {
                self.store
                    .set_queue_kind(&item.id, QueueKind::Message, None)
                    .await?;
            }
        }
        Ok(())
    }

    /// Queue `text` to steer run `run` once it can take input.
    pub(crate) async fn defer_steer(
        &self,
        thread: &ThreadId,
        text: &str,
        author: Author,
        run: Option<&str>,
    ) -> Result<QueueItemId> {
        self.store
            .queue_add_kind(thread, text, author, QueueKind::DeferredSteer, run, false)
            .await
    }

    /// Deliver the thread's deferred steers into its running turn, in queue
    /// order. One whose run ended (or never matched) becomes an ordinary
    /// queued message, keeping its place.
    pub(crate) async fn deliver_deferred_steers(&self, thread_id: &ThreadId) -> Result<()> {
        for item in self.store.queue(thread_id, false).await? {
            if item.kind != QueueKind::DeferredSteer || item.status != QueueStatus::Pending {
                continue;
            }
            let thread = self.store.thread(thread_id).await?;
            let Some(thread) = thread.filter(|t| {
                t.is_running() && (item.steer_run.is_none() || t.active_run_id == item.steer_run)
            }) else {
                self.store
                    .set_queue_kind(&item.id, QueueKind::Message, None)
                    .await?;
                continue;
            };
            if !self
                .store
                .set_queue_status(&item.id, QueueStatus::Pending, QueueStatus::Sending)
                .await?
            {
                continue;
            }
            let attempt = self
                .steer_text(&thread, &item.text, SteerHow::Steer, item.created_by)
                .await;
            match attempt.as_ref().map(|a| &a.outcome) {
                Ok(SteerOutcome::Steered | SteerOutcome::Restarted) => {
                    self.store
                        .set_queue_status(&item.id, QueueStatus::Sending, QueueStatus::Sent)
                        .await?;
                }
                Ok(SteerOutcome::NotReady) => {
                    self.store
                        .set_queue_status(&item.id, QueueStatus::Sending, QueueStatus::Pending)
                        .await?;
                    // Later ones would overtake it.
                    return Ok(());
                }
                other => {
                    if let Err(error) = other {
                        tracing::debug!(%error, "deferred steer failed; it runs from the queue");
                    }
                    self.store
                        .set_queue_kind(&item.id, QueueKind::Message, None)
                        .await?;
                    self.store
                        .set_queue_status(&item.id, QueueStatus::Sending, QueueStatus::Pending)
                        .await?;
                }
            }
        }
        Ok(())
    }

    /// Tick step: retry deferred steers (runtimes that never report ready).
    pub(crate) async fn deliver_all_deferred_steers(&self) -> Result<()> {
        for thread in self.store.deferred_steer_threads().await? {
            self.deliver_deferred_steers(&thread).await?;
        }
        Ok(())
    }

    /// Automatic input (a delegated result, a context handoff) for a busy
    /// `thread`: steered into its running turn only when the runtime steers
    /// without cancelling tool calls (t3code's rule for wakes, which never
    /// restart). `false`: not delivered, the caller waits for the turn end.
    pub(crate) async fn steer_automatic(&self, thread_id: &ThreadId, text: &str) -> Result<bool> {
        let Some(thread) = self.store.thread(thread_id).await? else {
            return Ok(false);
        };
        if !thread.is_running() || self.store.hold(thread_id).await?.is_some() {
            return Ok(false);
        }
        let runtime = self.runtime(&thread.runtime_id)?;
        if !runtime.capabilities().steer_in_turn || runtime.steer_interrupts_tools() {
            return Ok(false);
        }
        match self
            .steer_text(&thread, text, SteerHow::Steer, Author::System)
            .await
        {
            Ok(attempt) => Ok(attempt.outcome == SteerOutcome::Steered),
            Err(error) => {
                tracing::debug!(%error, "automatic steer failed; it waits for the turn end");
                Ok(false)
            }
        }
    }
}

/// The queue id of a recovered steer: one per (thread, run, steer).
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
