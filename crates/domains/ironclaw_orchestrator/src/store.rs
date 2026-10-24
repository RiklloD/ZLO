//! Durable orchestration state: `orch_*` tables in the deployment's libSQL
//! file, on the shared admission runtime (one writer lane per database).
//!
//! Every multi-statement invariant runs in one IMMEDIATE transaction;
//! status transitions are conditional `UPDATE … WHERE status …` so exactly
//! one writer wins (terminal event vs cancel fallback vs reconcile).

use std::sync::Arc;

use ironclaw_host_api::ids::{AgentId, ProjectId, TenantId, ThreadId, UserId};
use ironclaw_libsql_runtime::LibSqlRuntime;
use libsql::Value;

use crate::error::OrchestratorError;
use crate::model::*;
use crate::runtime::{TurnEndStatus, TurnUsage};
use crate::telemetry::{ThreadTelemetryRecord, TurnTelemetry};

type Result<T> = std::result::Result<T, OrchestratorError>;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS orch_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS orch_settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS orch_threads (
  thread_id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  agent_id TEXT,
  project_id TEXT,
  profile_id TEXT,
  runtime_id TEXT NOT NULL,
  provider TEXT,
  model TEXT,
  title TEXT,
  native_session_id TEXT,
  turn_state TEXT NOT NULL DEFAULT 'idle',
  active_run_id TEXT,
  turn_started_at INTEGER,
  turn_ended_at INTEGER,
  last_user_at INTEGER,
  last_error TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  imported INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS orch_threads_owner ON orch_threads(tenant_id, user_id);
CREATE TABLE IF NOT EXISTS orch_tasks (
  id TEXT PRIMARY KEY,
  parent_thread TEXT NOT NULL,
  child_thread TEXT,
  root_thread TEXT NOT NULL,
  depth INTEGER NOT NULL,
  profile_id TEXT,
  parent_profile_id TEXT,
  title TEXT NOT NULL,
  role TEXT NOT NULL,
  runtime_id TEXT NOT NULL,
  provider TEXT,
  model TEXT,
  prompt TEXT NOT NULL,
  status TEXT NOT NULL,
  result TEXT,
  error TEXT,
  progress TEXT,
  usage TEXT,
  created_at INTEGER NOT NULL,
  started_at INTEGER,
  finished_at INTEGER,
  client_request_id TEXT,
  delivery TEXT NOT NULL DEFAULT 'none',
  delivered_at INTEGER,
  cancel_requested INTEGER NOT NULL DEFAULT 0,
  limit_reset_at INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS orch_tasks_request ON orch_tasks(parent_thread, client_request_id)
  WHERE client_request_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS orch_tasks_parent ON orch_tasks(parent_thread);
CREATE INDEX IF NOT EXISTS orch_tasks_child ON orch_tasks(child_thread);
CREATE TABLE IF NOT EXISTS orch_queue (
  id TEXT PRIMARY KEY,
  thread_id TEXT NOT NULL,
  position REAL NOT NULL,
  text TEXT NOT NULL,
  status TEXT NOT NULL,
  created_by TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  sent_at INTEGER
);
CREATE INDEX IF NOT EXISTS orch_queue_thread ON orch_queue(thread_id, status, position);
CREATE TABLE IF NOT EXISTS orch_holds (
  thread_id TEXT PRIMARY KEY,
  reason TEXT NOT NULL,
  created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS orch_transfers (
  id TEXT PRIMARY KEY,
  kind TEXT NOT NULL,
  source_thread TEXT NOT NULL,
  target_thread TEXT,
  source_point TEXT,
  base_point TEXT,
  strategy TEXT NOT NULL,
  status TEXT NOT NULL,
  delivery TEXT NOT NULL,
  handoff TEXT,
  created_by TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  consumed_at INTEGER
);
CREATE INDEX IF NOT EXISTS orch_transfers_target ON orch_transfers(target_thread, status);
CREATE INDEX IF NOT EXISTS orch_transfers_source ON orch_transfers(source_thread);
CREATE TABLE IF NOT EXISTS orch_recoveries (
  id TEXT PRIMARY KEY,
  thread_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  reset_at INTEGER NOT NULL,
  status TEXT NOT NULL,
  detail TEXT NOT NULL DEFAULT '{}',
  attempts INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL,
  fired_at INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS orch_recoveries_identity ON orch_recoveries(thread_id, kind, reset_at);
CREATE TABLE IF NOT EXISTS orch_live_work (
  thread_id TEXT PRIMARY KEY,
  instance_id TEXT NOT NULL,
  detail TEXT NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS orch_turns (
  id TEXT PRIMARY KEY,
  thread_id TEXT NOT NULL,
  run_id TEXT,
  status TEXT NOT NULL,
  started_at INTEGER,
  ended_at INTEGER NOT NULL,
  usage TEXT,
  error TEXT
);
CREATE INDEX IF NOT EXISTS orch_turns_thread ON orch_turns(thread_id, ended_at);
CREATE TABLE IF NOT EXISTS orch_telemetry (
  thread_id TEXT PRIMARY KEY,
  doc TEXT NOT NULL,
  updated_at INTEGER NOT NULL
);
"#;

const LEGACY_IMPORT_MARKER: &str = "legacy_threads_imported_v1";

/// One-shot import of `ironclaw_threads` thread records (the
/// `session_thread` rows of the root-filesystem table in the same file).
/// Transcripts stay where they are; only the thread registry moves.
const LEGACY_IMPORT_SQL: &str = r#"
INSERT OR IGNORE INTO orch_threads
  (thread_id, tenant_id, user_id, agent_id, project_id, runtime_id, title, turn_state,
   created_at, updated_at, imported)
SELECT
  json_extract(c, '$.thread_id'),
  json_extract(c, '$.scope.tenant_id'),
  COALESCE(json_extract(c, '$.scope.owner_user_id'), json_extract(c, '$.created_by_actor_id')),
  json_extract(c, '$.scope.agent_id'),
  json_extract(c, '$.scope.project_id'),
  ?1,
  json_extract(c, '$.title'),
  'idle',
  CAST((julianday(json_extract(c, '$.created_at')) - 2440587.5) * 86400000 AS INTEGER),
  CAST((julianday(json_extract(c, '$.updated_at')) - 2440587.5) * 86400000 AS INTEGER),
  1
FROM (SELECT CAST(contents AS TEXT) AS c FROM root_filesystem_entries
      WHERE kind = 'session_thread' AND is_dir = 0)
WHERE json_valid(c)
  AND json_extract(c, '$.thread_id') IS NOT NULL
  AND json_extract(c, '$.scope.tenant_id') IS NOT NULL
  AND COALESCE(json_extract(c, '$.scope.owner_user_id'), json_extract(c, '$.created_by_actor_id')) IS NOT NULL
"#;

fn opt<T: Into<Value>>(value: Option<T>) -> Value {
    value.map_or(Value::Null, Into::into)
}

pub(crate) fn s(value: impl AsRef<str>) -> Value {
    Value::Text(value.as_ref().to_string())
}

fn json_opt<T: serde::Serialize>(value: Option<&T>) -> Result<Value> {
    Ok(match value {
        Some(v) => Value::Text(serde_json::to_string(v)?),
        None => Value::Null,
    })
}

/// Sequential column reader for one row.
pub(crate) struct Cols<'a> {
    row: &'a libsql::Row,
    i: i32,
}

impl<'a> Cols<'a> {
    pub(crate) fn new(row: &'a libsql::Row) -> Self {
        Self { row, i: 0 }
    }
    fn value(&mut self) -> Result<Value> {
        let v = self.row.get_value(self.i)?;
        self.i += 1;
        Ok(v)
    }
    pub(crate) fn otext(&mut self) -> Result<Option<String>> {
        match self.value()? {
            Value::Null => Ok(None),
            Value::Text(t) => Ok(Some(t)),
            Value::Blob(b) => Ok(Some(String::from_utf8_lossy(&b).into_owned())),
            Value::Integer(i) => Ok(Some(i.to_string())),
            Value::Real(r) => Ok(Some(r.to_string())),
        }
    }
    pub(crate) fn text(&mut self) -> Result<String> {
        let col = self.i;
        self.otext()?
            .ok_or_else(|| OrchestratorError::Corrupt(format!("column {col} is null")))
    }
    pub(crate) fn oint(&mut self) -> Result<Option<i64>> {
        match self.value()? {
            Value::Null => Ok(None),
            Value::Integer(i) => Ok(Some(i)),
            Value::Real(r) => Ok(Some(r as i64)),
            other => Err(OrchestratorError::Corrupt(format!(
                "expected integer, got {other:?}"
            ))),
        }
    }
    pub(crate) fn int(&mut self) -> Result<i64> {
        let col = self.i;
        self.oint()?
            .ok_or_else(|| OrchestratorError::Corrupt(format!("column {col} is null")))
    }
    fn real(&mut self) -> Result<f64> {
        match self.value()? {
            Value::Integer(i) => Ok(i as f64),
            Value::Real(r) => Ok(r),
            other => Err(OrchestratorError::Corrupt(format!(
                "expected real, got {other:?}"
            ))),
        }
    }
    fn ojson<T: serde::de::DeserializeOwned>(&mut self) -> Result<Option<T>> {
        match self.otext()? {
            Some(raw) if !raw.is_empty() => Ok(Some(serde_json::from_str(&raw)?)),
            _ => Ok(None),
        }
    }
    fn thread(&mut self) -> Result<ThreadId> {
        Ok(ThreadId::new(self.text()?)?)
    }
    fn othread(&mut self) -> Result<Option<ThreadId>> {
        self.otext()?
            .map(ThreadId::new)
            .transpose()
            .map_err(Into::into)
    }
    fn oprofile(&mut self) -> Result<Option<ProfileId>> {
        self.otext()?.map(ProfileId::new).transpose()
    }
}

const THREAD_COLS: &str = "thread_id, tenant_id, user_id, agent_id, project_id, profile_id, runtime_id, \
    provider, model, title, native_session_id, turn_state, active_run_id, turn_started_at, turn_ended_at, \
    last_user_at, last_error, created_at, updated_at";

fn thread_row(row: &libsql::Row) -> Result<ThreadRecord> {
    let mut c = Cols::new(row);
    Ok(ThreadRecord {
        thread_id: c.thread()?,
        scope: ThreadScope {
            tenant_id: TenantId::new(c.text()?)?,
            user_id: UserId::new(c.text()?)?,
            agent_id: c.otext()?.map(AgentId::new).transpose()?,
            project_id: c.otext()?.map(ProjectId::new).transpose()?,
        },
        profile_id: c.oprofile()?,
        runtime_id: RuntimeId::new(c.text()?)?,
        provider: c.otext()?,
        model: c.otext()?,
        title: c.otext()?,
        native_session_id: c.otext()?,
        turn_state: TurnState::parse(&c.text()?)?,
        active_run_id: c.otext()?,
        turn_started_at: c.oint()?,
        turn_ended_at: c.oint()?,
        last_user_at: c.oint()?,
        last_error: c.otext()?,
        created_at: c.int()?,
        updated_at: c.int()?,
    })
}

const TASK_COLS: &str = "id, parent_thread, child_thread, root_thread, depth, profile_id, parent_profile_id, \
    title, role, runtime_id, provider, model, prompt, status, result, error, progress, usage, created_at, \
    started_at, finished_at, client_request_id, delivery, delivered_at, cancel_requested, limit_reset_at";

fn task_row(row: &libsql::Row) -> Result<TaskRecord> {
    let mut c = Cols::new(row);
    Ok(TaskRecord {
        id: TaskId::new(c.text()?)?,
        parent_thread: c.thread()?,
        child_thread: c.othread()?,
        root_thread: c.thread()?,
        depth: c.int()? as u32,
        profile_id: c.oprofile()?,
        parent_profile_id: c.oprofile()?,
        title: c.text()?,
        role: TaskRole::parse(&c.text()?)?,
        runtime_id: RuntimeId::new(c.text()?)?,
        provider: c.otext()?,
        model: c.otext()?,
        prompt: c.text()?,
        status: TaskStatus::parse(&c.text()?)?,
        result: c.otext()?,
        error: c.otext()?,
        progress: c.ojson()?,
        usage: c.ojson()?,
        created_at: c.int()?,
        started_at: c.oint()?,
        finished_at: c.oint()?,
        client_request_id: c.otext()?,
        delivery: Delivery::parse(&c.text()?)?,
        delivered_at: c.oint()?,
        cancel_requested: c.int()? != 0,
        limit_reset_at: c.oint()?,
    })
}

const QUEUE_COLS: &str =
    "id, thread_id, position, text, status, created_by, created_at, updated_at, sent_at";

fn queue_row(row: &libsql::Row) -> Result<QueueItem> {
    let mut c = Cols::new(row);
    Ok(QueueItem {
        id: QueueItemId::new(c.text()?)?,
        thread_id: c.thread()?,
        position: c.real()?,
        text: c.text()?,
        status: QueueStatus::parse(&c.text()?)?,
        created_by: Author::parse(&c.text()?)?,
        created_at: c.int()?,
        updated_at: c.int()?,
        sent_at: c.oint()?,
    })
}

const TRANSFER_COLS: &str = "id, kind, source_thread, target_thread, source_point, base_point, strategy, \
    status, delivery, handoff, created_by, created_at, consumed_at";

fn transfer_row(row: &libsql::Row) -> Result<TransferRecord> {
    let mut c = Cols::new(row);
    Ok(TransferRecord {
        id: TransferId::new(c.text()?)?,
        kind: TransferKind::parse(&c.text()?)?,
        source_thread: c.thread()?,
        target_thread: c.othread()?,
        source_point: c.ojson()?,
        base_point: c.ojson()?,
        strategy: TransferStrategy::parse(&c.text()?)?,
        status: TransferStatus::parse(&c.text()?)?,
        delivery: TransferDelivery::parse(&c.text()?)?,
        handoff: c.otext()?,
        created_by: Author::parse(&c.text()?)?,
        created_at: c.int()?,
        consumed_at: c.oint()?,
    })
}

const RECOVERY_COLS: &str =
    "id, thread_id, kind, reset_at, status, detail, attempts, created_at, fired_at";

fn recovery_row(row: &libsql::Row) -> Result<RecoveryRecord> {
    let mut c = Cols::new(row);
    Ok(RecoveryRecord {
        id: RecoveryId::new(c.text()?)?,
        thread_id: c.thread()?,
        kind: RecoveryKind::parse(&c.text()?)?,
        reset_at: c.int()?,
        status: RecoveryStatus::parse(&c.text()?)?,
        detail: c.ojson()?.unwrap_or(serde_json::Value::Null),
        attempts: c.int()? as u32,
        created_at: c.int()?,
        fired_at: c.oint()?,
    })
}

/// A live-work row: a turn this engine instance is running right now.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveWork {
    pub thread_id: ThreadId,
    pub instance_id: String,
    pub detail: serde_json::Value,
    pub updated_at: Millis,
}

#[derive(Clone)]
pub struct Store {
    db: Arc<LibSqlRuntime>,
}

impl Store {
    pub fn new(db: Arc<LibSqlRuntime>) -> Self {
        Self { db }
    }

    // ── plumbing ────────────────────────────────────────────────────────

    pub(crate) async fn exec(&self, sql: &str, params: Vec<Value>) -> Result<u64> {
        let conn = self.db.write().await?;
        Ok(conn.execute(sql, params).await?)
    }

    pub(crate) async fn rows<T>(
        &self,
        sql: &str,
        params: Vec<Value>,
        map: fn(&libsql::Row) -> Result<T>,
    ) -> Result<Vec<T>> {
        let conn = self.db.read().await?;
        let mut rows = conn.query(sql, params).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(map(&row)?);
        }
        Ok(out)
    }

    pub(crate) async fn row<T>(
        &self,
        sql: &str,
        params: Vec<Value>,
        map: fn(&libsql::Row) -> Result<T>,
    ) -> Result<Option<T>> {
        Ok(self.rows(sql, params, map).await?.into_iter().next())
    }

    /// Run `body` statements in one IMMEDIATE transaction on the writer.
    async fn tx(&self, statements: Vec<(String, Vec<Value>)>) -> Result<()> {
        let conn = self.db.write().await?;
        let tx = conn
            .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
            .await?;
        for (sql, params) in statements {
            if let Err(error) = tx.execute(&sql, params).await {
                tx.rollback().await?;
                return Err(error.into());
            }
        }
        tx.commit().await?;
        Ok(())
    }

    // ── schema ──────────────────────────────────────────────────────────

    /// Create the tables (idempotent) and import legacy threads once.
    pub async fn migrate(&self, legacy_runtime: &RuntimeId) -> Result<usize> {
        {
            let conn = self.db.write().await?;
            conn.execute_batch(SCHEMA).await?;
            conn.execute_batch(crate::profiles::PROFILES_SCHEMA).await?;
            conn.execute_batch(crate::inbox::INBOX_SCHEMA).await?;
            conn.execute_batch(crate::goals::GOALS_SCHEMA).await?;
        }
        self.import_legacy_threads(legacy_runtime).await
    }

    async fn import_legacy_threads(&self, runtime: &RuntimeId) -> Result<usize> {
        if self.meta(LEGACY_IMPORT_MARKER).await?.is_some() {
            return Ok(0);
        }
        let has_source = self
            .row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'root_filesystem_entries'",
                vec![],
                |r| Cols::new(r).int(),
            )
            .await?
            .is_some();
        let conn = self.db.write().await?;
        let tx = conn
            .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
            .await?;
        let imported = if has_source {
            match tx
                .execute(LEGACY_IMPORT_SQL, vec![s(runtime.as_str())])
                .await
            {
                Ok(n) => n as usize,
                Err(error) => {
                    tx.rollback().await?;
                    return Err(error.into());
                }
            }
        } else {
            0
        };
        tx.execute(
            "INSERT OR REPLACE INTO orch_meta (key, value) VALUES (?1, ?2)",
            vec![s(LEGACY_IMPORT_MARKER), s(imported.to_string())],
        )
        .await?;
        tx.commit().await?;
        Ok(imported)
    }

    pub async fn meta(&self, key: &str) -> Result<Option<String>> {
        self.row(
            "SELECT value FROM orch_meta WHERE key = ?1",
            vec![s(key)],
            |r| Cols::new(r).text(),
        )
        .await
    }

    // ── settings ────────────────────────────────────────────────────────

    pub async fn settings(&self) -> Result<Settings> {
        let raw = self
            .row(
                "SELECT value FROM orch_settings WHERE key = 'settings'",
                vec![],
                |r| Cols::new(r).text(),
            )
            .await?;
        match raw {
            Some(raw) => Ok(serde_json::from_str(&raw)?),
            None => Ok(Settings::default()),
        }
    }

    /// Merge `patch` (a partial `Settings` object) into the stored settings.
    pub async fn set_settings(&self, patch: &serde_json::Value) -> Result<Settings> {
        let mut current = serde_json::to_value(self.settings().await?)?;
        let (Some(target), Some(fields)) = (current.as_object_mut(), patch.as_object()) else {
            return Err(OrchestratorError::invalid(
                "settings patch must be an object",
            ));
        };
        for (key, value) in fields {
            if !target.contains_key(key) {
                return Err(OrchestratorError::invalid(format!(
                    "unknown setting: {key}"
                )));
            }
            target.insert(key.clone(), value.clone());
        }
        let settings: Settings = serde_json::from_value(current)
            .map_err(|e| OrchestratorError::invalid(format!("invalid setting value: {e}")))?;
        self.exec(
            "INSERT OR REPLACE INTO orch_settings (key, value) VALUES ('settings', ?1)",
            vec![s(serde_json::to_string(&settings)?)],
        )
        .await?;
        Ok(settings)
    }

    // ── threads ─────────────────────────────────────────────────────────

    pub async fn thread(&self, id: &ThreadId) -> Result<Option<ThreadRecord>> {
        self.row(
            &format!("SELECT {THREAD_COLS} FROM orch_threads WHERE thread_id = ?1"),
            vec![s(id.as_str())],
            thread_row,
        )
        .await
    }

    /// Insert the thread if unknown; never overwrites an existing row.
    pub async fn ensure_thread(&self, record: &ThreadRecord) -> Result<bool> {
        let r = record;
        let n = self
            .exec(
                &format!(
                    "INSERT OR IGNORE INTO orch_threads ({THREAD_COLS}) VALUES \
                     (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)"
                ),
                vec![
                    s(r.thread_id.as_str()),
                    s(r.scope.tenant_id.as_str()),
                    s(r.scope.user_id.as_str()),
                    opt(r.scope.agent_id.as_ref().map(|v| v.as_str().to_string())),
                    opt(r.scope.project_id.as_ref().map(|v| v.as_str().to_string())),
                    opt(r.profile_id.as_ref().map(|v| v.as_str().to_string())),
                    s(r.runtime_id.as_str()),
                    opt(r.provider.clone()),
                    opt(r.model.clone()),
                    opt(r.title.clone()),
                    opt(r.native_session_id.clone()),
                    s(r.turn_state.as_str()),
                    opt(r.active_run_id.clone()),
                    opt(r.turn_started_at),
                    opt(r.turn_ended_at),
                    opt(r.last_user_at),
                    opt(r.last_error.clone()),
                    r.created_at.into(),
                    r.updated_at.into(),
                ],
            )
            .await?;
        Ok(n > 0)
    }

    pub async fn set_thread_route(
        &self,
        id: &ThreadId,
        provider: Option<&str>,
        model: Option<&str>,
        title: Option<&str>,
    ) -> Result<()> {
        self.exec(
            "UPDATE orch_threads SET provider = COALESCE(?2, provider), model = COALESCE(?3, model), \
             title = COALESCE(?4, title), updated_at = ?5 WHERE thread_id = ?1",
            vec![
                s(id.as_str()),
                opt(provider),
                opt(model),
                opt(title),
                now_ms().into(),
            ],
        )
        .await?;
        Ok(())
    }

    /// Move a thread to another runtime (a profile's engine, before its first turn).
    pub async fn set_thread_runtime(
        &self,
        id: &ThreadId,
        runtime: &RuntimeId,
        provider: Option<&str>,
        model: Option<&str>,
        native_session_id: Option<&str>,
    ) -> Result<()> {
        self.exec(
            "UPDATE orch_threads SET runtime_id = ?2, provider = ?3, model = ?4, \
             native_session_id = ?5, updated_at = ?6 WHERE thread_id = ?1",
            vec![
                s(id.as_str()),
                s(runtime.as_str()),
                opt(provider),
                opt(model),
                opt(native_session_id),
                now_ms().into(),
            ],
        )
        .await?;
        Ok(())
    }

    pub async fn set_turn_running(
        &self,
        id: &ThreadId,
        run_id: Option<&str>,
        at: Millis,
    ) -> Result<()> {
        self.exec(
            "UPDATE orch_threads SET turn_state = 'running', active_run_id = COALESCE(?2, active_run_id), \
             turn_started_at = ?3, updated_at = ?3 WHERE thread_id = ?1",
            vec![s(id.as_str()), opt(run_id), at.into()],
        )
        .await?;
        Ok(())
    }

    /// Mark the turn ended unless a newer turn already started (a late
    /// terminal event for an older run must not idle a running thread).
    pub async fn set_turn_ended(
        &self,
        id: &ThreadId,
        run_id: Option<&str>,
        state: TurnState,
        error: Option<&str>,
        at: Millis,
    ) -> Result<bool> {
        let n = self
            .exec(
                "UPDATE orch_threads SET turn_state = ?2, turn_ended_at = ?4, last_error = ?3, \
                 active_run_id = NULL, updated_at = ?4 WHERE thread_id = ?1 \
                 AND (?5 IS NULL OR active_run_id IS NULL OR active_run_id = ?5)",
                vec![
                    s(id.as_str()),
                    s(state.as_str()),
                    opt(error),
                    at.into(),
                    opt(run_id),
                ],
            )
            .await?;
        Ok(n > 0)
    }

    pub async fn set_last_user(&self, id: &ThreadId, at: Millis) -> Result<()> {
        self.exec(
            "UPDATE orch_threads SET last_user_at = ?2, updated_at = ?2 WHERE thread_id = ?1",
            vec![s(id.as_str()), at.into()],
        )
        .await?;
        Ok(())
    }

    // ── turns log ───────────────────────────────────────────────────────

    pub async fn insert_turn_log(&self, record: &TurnLogRecord) -> Result<()> {
        self.exec(
            "INSERT INTO orch_turns (id, thread_id, run_id, status, started_at, ended_at, usage, error) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            vec![
                s(record.id.as_str()),
                s(record.thread_id.as_str()),
                opt(record.run_id.clone()),
                s(record.status.as_str()),
                opt(record.started_at),
                record.ended_at.into(),
                json_opt(record.usage.as_ref())?,
                opt(record.error.clone()),
            ],
        )
        .await?;
        Ok(())
    }

    pub async fn turn_log(&self, thread: &ThreadId, limit: u32) -> Result<Vec<TurnLogRecord>> {
        self.rows(
            "SELECT id, thread_id, run_id, status, started_at, ended_at, usage, error FROM orch_turns \
             WHERE thread_id = ?1 ORDER BY ended_at DESC LIMIT ?2",
            vec![s(thread.as_str()), i64::from(limit).into()],
            |row| {
                let mut c = Cols::new(row);
                Ok(TurnLogRecord {
                    id: TurnLogId::new(c.text()?)?,
                    thread_id: c.thread()?,
                    run_id: c.otext()?,
                    status: {
                        let raw = c.text()?;
                        TurnEndStatus::parse(&raw)
                            .ok_or_else(|| OrchestratorError::Corrupt(format!("turn status {raw:?}")))?
                    },
                    started_at: c.oint()?,
                    ended_at: c.int()?,
                    usage: c.ojson::<TurnUsage>()?,
                    error: c.otext()?,
                })
            },
        )
        .await
    }

    // ── tasks ───────────────────────────────────────────────────────────

    pub async fn insert_task(&self, t: &TaskRecord) -> Result<()> {
        self.exec(
            &format!(
                "INSERT INTO orch_tasks ({TASK_COLS}) VALUES \
                 (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26)"
            ),
            vec![
                s(t.id.as_str()),
                s(t.parent_thread.as_str()),
                opt(t.child_thread.as_ref().map(|v| v.as_str().to_string())),
                s(t.root_thread.as_str()),
                i64::from(t.depth).into(),
                opt(t.profile_id.as_ref().map(|v| v.as_str().to_string())),
                opt(t.parent_profile_id.as_ref().map(|v| v.as_str().to_string())),
                s(&t.title),
                s(t.role.as_str()),
                s(t.runtime_id.as_str()),
                opt(t.provider.clone()),
                opt(t.model.clone()),
                s(&t.prompt),
                s(t.status.as_str()),
                opt(t.result.clone()),
                opt(t.error.clone()),
                json_opt(t.progress.as_ref())?,
                json_opt(t.usage.as_ref())?,
                t.created_at.into(),
                opt(t.started_at),
                opt(t.finished_at),
                opt(t.client_request_id.clone()),
                s(t.delivery.as_str()),
                opt(t.delivered_at),
                i64::from(t.cancel_requested).into(),
                opt(t.limit_reset_at),
            ],
        )
        .await?;
        Ok(())
    }

    pub async fn task(&self, id: &TaskId) -> Result<Option<TaskRecord>> {
        self.row(
            &format!("SELECT {TASK_COLS} FROM orch_tasks WHERE id = ?1"),
            vec![s(id.as_str())],
            task_row,
        )
        .await
    }

    pub async fn task_by_request(
        &self,
        parent: &ThreadId,
        request_id: &str,
    ) -> Result<Option<TaskRecord>> {
        self.row(
            &format!(
                "SELECT {TASK_COLS} FROM orch_tasks WHERE parent_thread = ?1 AND client_request_id = ?2"
            ),
            vec![s(parent.as_str()), s(request_id)],
            task_row,
        )
        .await
    }

    pub async fn task_for_child(&self, child: &ThreadId) -> Result<Option<TaskRecord>> {
        self.row(
            &format!(
                "SELECT {TASK_COLS} FROM orch_tasks WHERE child_thread = ?1 ORDER BY created_at DESC LIMIT 1"
            ),
            vec![s(child.as_str())],
            task_row,
        )
        .await
    }

    pub async fn tasks_for_parent(
        &self,
        parent: &ThreadId,
        active_only: bool,
    ) -> Result<Vec<TaskRecord>> {
        let filter = if active_only {
            format!(" AND {TASK_ACTIVE_SQL}")
        } else {
            String::new()
        };
        self.rows(
            &format!(
                "SELECT {TASK_COLS} FROM orch_tasks WHERE parent_thread = ?1{filter} ORDER BY created_at"
            ),
            vec![s(parent.as_str())],
            task_row,
        )
        .await
    }

    pub async fn active_tasks(&self) -> Result<Vec<TaskRecord>> {
        self.rows(
            &format!(
                "SELECT {TASK_COLS} FROM orch_tasks WHERE {TASK_ACTIVE_SQL} ORDER BY created_at"
            ),
            vec![],
            task_row,
        )
        .await
    }

    pub async fn pending_deliveries(&self) -> Result<Vec<TaskRecord>> {
        self.rows(
            &format!(
                "SELECT {TASK_COLS} FROM orch_tasks WHERE delivery = 'pending' ORDER BY finished_at"
            ),
            vec![],
            task_row,
        )
        .await
    }

    pub async fn set_task_child(
        &self,
        id: &TaskId,
        child: &ThreadId,
        provider: Option<&str>,
        model: Option<&str>,
    ) -> Result<()> {
        self.exec(
            "UPDATE orch_tasks SET child_thread = ?2, provider = COALESCE(?3, provider), \
             model = COALESCE(?4, model) WHERE id = ?1",
            vec![s(id.as_str()), s(child.as_str()), opt(provider), opt(model)],
        )
        .await?;
        Ok(())
    }

    /// Conditional status move. `from` empty = any non-terminal status.
    /// `require_live` additionally refuses cancel-requested rows.
    pub async fn move_task(
        &self,
        id: &TaskId,
        from: &[TaskStatus],
        require_live: bool,
        to: TaskStatus,
        patch: TaskPatch,
    ) -> Result<bool> {
        let guard = if from.is_empty() {
            TASK_ACTIVE_SQL.to_string()
        } else {
            let list: Vec<String> = from.iter().map(|st| format!("'{}'", st.as_str())).collect();
            format!("status IN ({})", list.join(","))
        };
        let live = if require_live {
            " AND cancel_requested = 0"
        } else {
            ""
        };
        let n = self
            .exec(
                &format!(
                    "UPDATE orch_tasks SET status = ?2, \
                     result = CASE WHEN ?3 THEN ?4 ELSE result END, \
                     error = CASE WHEN ?5 THEN ?6 ELSE error END, \
                     started_at = COALESCE(started_at, ?7), \
                     finished_at = COALESCE(?8, finished_at), \
                     delivery = COALESCE(?9, delivery), \
                     limit_reset_at = CASE WHEN ?10 THEN ?11 ELSE limit_reset_at END, \
                     usage = COALESCE(?12, usage) \
                     WHERE id = ?1 AND {guard}{live}"
                ),
                vec![
                    s(id.as_str()),
                    s(to.as_str()),
                    patch.result.is_some().into(),
                    opt(patch.result.flatten()),
                    patch.error.is_some().into(),
                    opt(patch.error.flatten()),
                    opt(patch.started_at),
                    opt(patch.finished_at),
                    opt(patch.delivery.map(Delivery::as_str)),
                    patch.limit_reset_at.is_some().into(),
                    opt(patch.limit_reset_at.flatten()),
                    json_opt(patch.usage.as_ref())?,
                ],
            )
            .await?;
        Ok(n > 0)
    }

    pub async fn set_task_progress(&self, id: &TaskId, progress: &serde_json::Value) -> Result<()> {
        self.exec(
            "UPDATE orch_tasks SET progress = ?2 WHERE id = ?1",
            vec![s(id.as_str()), s(serde_json::to_string(progress)?)],
        )
        .await?;
        Ok(())
    }

    /// Durable cancel intent on a live task. Returns whether it changed.
    pub async fn mark_task_cancel(&self, id: &TaskId, reason: &str) -> Result<bool> {
        let n = self
            .exec(
                &format!(
                    "UPDATE orch_tasks SET cancel_requested = 1, error = ?2 WHERE id = ?1 AND {TASK_ACTIVE_SQL}"
                ),
                vec![s(id.as_str()), s(clip(reason, 500))],
            )
            .await?;
        Ok(n > 0)
    }

    pub async fn set_delivery(
        &self,
        id: &TaskId,
        from: Delivery,
        to: Delivery,
        at: Millis,
    ) -> Result<bool> {
        let n = self
            .exec(
                "UPDATE orch_tasks SET delivery = ?3, delivered_at = ?4 WHERE id = ?1 AND delivery = ?2",
                vec![s(id.as_str()), s(from.as_str()), s(to.as_str()), at.into()],
            )
            .await?;
        Ok(n > 0)
    }

    // ── queue ───────────────────────────────────────────────────────────

    pub async fn queue(&self, thread: &ThreadId, include_done: bool) -> Result<Vec<QueueItem>> {
        let filter = if include_done {
            ""
        } else {
            " AND status IN ('pending','sending')"
        };
        self.rows(
            &format!(
                "SELECT {QUEUE_COLS} FROM orch_queue WHERE thread_id = ?1{filter} ORDER BY position, created_at"
            ),
            vec![s(thread.as_str())],
            queue_row,
        )
        .await
    }

    pub async fn queue_item(
        &self,
        thread: &ThreadId,
        id: &QueueItemId,
    ) -> Result<Option<QueueItem>> {
        self.row(
            &format!("SELECT {QUEUE_COLS} FROM orch_queue WHERE id = ?1 AND thread_id = ?2"),
            vec![s(id.as_str()), s(thread.as_str())],
            queue_row,
        )
        .await
    }

    pub async fn queued_threads(&self) -> Result<Vec<ThreadId>> {
        self.rows(
            "SELECT DISTINCT thread_id FROM orch_queue WHERE status = 'pending'",
            vec![],
            |r| Cols::new(r).thread(),
        )
        .await
    }

    pub async fn queue_add(
        &self,
        thread: &ThreadId,
        text: &str,
        author: Author,
    ) -> Result<QueueItemId> {
        let id = QueueItemId::generate();
        let now = now_ms();
        self.exec(
            "INSERT INTO orch_queue (id, thread_id, position, text, status, created_by, created_at, updated_at) \
             VALUES (?1, ?2, (SELECT COALESCE(MAX(position), 0) + 1 FROM orch_queue WHERE thread_id = ?2), \
             ?3, 'pending', ?4, ?5, ?5)",
            vec![
                s(id.as_str()),
                s(thread.as_str()),
                s(text),
                s(author.as_str()),
                now.into(),
            ],
        )
        .await?;
        Ok(id)
    }

    /// Edit a pending item's text; false when it is no longer pending.
    pub async fn queue_edit(&self, id: &QueueItemId, text: &str) -> Result<bool> {
        let n = self
            .exec(
                "UPDATE orch_queue SET text = ?2, updated_at = ?3 WHERE id = ?1 AND status = 'pending'",
                vec![s(id.as_str()), s(text), now_ms().into()],
            )
            .await?;
        Ok(n > 0)
    }

    pub async fn set_queue_status(
        &self,
        id: &QueueItemId,
        from: QueueStatus,
        to: QueueStatus,
    ) -> Result<bool> {
        let now = now_ms();
        let sent = if to == QueueStatus::Sent {
            Some(now)
        } else {
            None
        };
        let n = self
            .exec(
                "UPDATE orch_queue SET status = ?3, updated_at = ?4, sent_at = COALESCE(?5, sent_at) \
                 WHERE id = ?1 AND status = ?2",
                vec![
                    s(id.as_str()),
                    s(from.as_str()),
                    s(to.as_str()),
                    now.into(),
                    opt(sent),
                ],
            )
            .await?;
        Ok(n > 0)
    }

    pub async fn queue_clear(&self, thread: &ThreadId) -> Result<u64> {
        self.exec(
            "UPDATE orch_queue SET status = 'cancelled', updated_at = ?2 WHERE thread_id = ?1 AND status = 'pending'",
            vec![s(thread.as_str()), now_ms().into()],
        )
        .await
    }

    /// Move `id` before `before` (or to the end) by renumbering positions.
    pub async fn queue_reorder(
        &self,
        thread: &ThreadId,
        id: &QueueItemId,
        before: Option<&QueueItemId>,
    ) -> Result<()> {
        let mut ids: Vec<QueueItemId> = self
            .queue(thread, false)
            .await?
            .into_iter()
            .filter(|q| q.status == QueueStatus::Pending && &q.id != id)
            .map(|q| q.id)
            .collect();
        let at = before
            .and_then(|b| ids.iter().position(|x| x == b))
            .unwrap_or(ids.len());
        ids.insert(at, id.clone());
        let now = now_ms();
        let statements = ids
            .iter()
            .enumerate()
            .map(|(i, qid)| {
                (
                    "UPDATE orch_queue SET position = ?2, updated_at = ?3 WHERE id = ?1 AND status = 'pending'"
                        .to_string(),
                    vec![s(qid.as_str()), ((i + 1) as f64).into(), now.into()],
                )
            })
            .collect();
        self.tx(statements).await
    }

    /// Claim the current head (pending → sending) atomically.
    pub async fn claim_queue_head(&self, thread: &ThreadId) -> Result<Option<QueueItem>> {
        let conn = self.db.write().await?;
        let tx = conn
            .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
            .await?;
        let mut rows = tx
            .query(
                &format!(
                    "SELECT {QUEUE_COLS} FROM orch_queue WHERE thread_id = ?1 AND status = 'pending' \
                     ORDER BY position, created_at LIMIT 1"
                ),
                vec![s(thread.as_str())],
            )
            .await?;
        let head = match rows.next().await? {
            Some(row) => Some(queue_row(&row)?),
            None => None,
        };
        drop(rows);
        if let Some(item) = &head {
            tx.execute(
                "UPDATE orch_queue SET status = 'sending', updated_at = ?2 WHERE id = ?1",
                vec![s(item.id.as_str()), now_ms().into()],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(head)
    }

    /// Boot: sends a crash cut mid-flight go back to pending.
    pub async fn requeue_sending(&self) -> Result<u64> {
        self.exec(
            "UPDATE orch_queue SET status = 'pending', updated_at = ?1 WHERE status = 'sending'",
            vec![now_ms().into()],
        )
        .await
    }

    // ── holds ───────────────────────────────────────────────────────────

    pub async fn hold(&self, thread: &ThreadId) -> Result<Option<HoldReason>> {
        self.row(
            "SELECT reason FROM orch_holds WHERE thread_id = ?1",
            vec![s(thread.as_str())],
            |r| HoldReason::parse(&Cols::new(r).text()?),
        )
        .await
    }

    pub async fn set_hold(&self, thread: &ThreadId, reason: Option<HoldReason>) -> Result<()> {
        match reason {
            Some(reason) => {
                self.exec(
                    "INSERT OR REPLACE INTO orch_holds (thread_id, reason, created_at) VALUES (?1, ?2, ?3)",
                    vec![s(thread.as_str()), s(reason.as_str()), now_ms().into()],
                )
                .await?
            }
            None => {
                self.exec(
                    "DELETE FROM orch_holds WHERE thread_id = ?1",
                    vec![s(thread.as_str())],
                )
                .await?
            }
        };
        Ok(())
    }

    // ── transfers ───────────────────────────────────────────────────────

    pub async fn insert_transfer(&self, x: &TransferRecord) -> Result<()> {
        self.exec(
            &format!(
                "INSERT INTO orch_transfers ({TRANSFER_COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)"
            ),
            vec![
                s(x.id.as_str()),
                s(x.kind.as_str()),
                s(x.source_thread.as_str()),
                opt(x.target_thread.as_ref().map(|v| v.as_str().to_string())),
                json_opt(x.source_point.as_ref())?,
                json_opt(x.base_point.as_ref())?,
                s(x.strategy.as_str()),
                s(x.status.as_str()),
                s(x.delivery.as_str()),
                opt(x.handoff.clone()),
                s(x.created_by.as_str()),
                x.created_at.into(),
                opt(x.consumed_at),
            ],
        )
        .await?;
        Ok(())
    }

    pub async fn transfer(&self, id: &TransferId) -> Result<Option<TransferRecord>> {
        self.row(
            &format!("SELECT {TRANSFER_COLS} FROM orch_transfers WHERE id = ?1"),
            vec![s(id.as_str())],
            transfer_row,
        )
        .await
    }

    pub async fn pending_transfers_now(&self) -> Result<Vec<TransferRecord>> {
        self.rows(
            &format!(
                "SELECT {TRANSFER_COLS} FROM orch_transfers WHERE status = 'pending' AND delivery = 'now' \
                 AND kind IN ('fork','merge_back') ORDER BY created_at"
            ),
            vec![],
            transfer_row,
        )
        .await
    }

    pub async fn pending_next_message(&self, target: &ThreadId) -> Result<Vec<TransferRecord>> {
        self.rows(
            &format!(
                "SELECT {TRANSFER_COLS} FROM orch_transfers WHERE target_thread = ?1 AND status = 'pending' \
                 AND delivery = 'next_message' ORDER BY created_at"
            ),
            vec![s(target.as_str())],
            transfer_row,
        )
        .await
    }

    pub async fn consume_transfer(&self, id: &TransferId) -> Result<bool> {
        let n = self
            .exec(
                "UPDATE orch_transfers SET status = 'consumed', consumed_at = ?2 WHERE id = ?1 AND status = 'pending'",
                vec![s(id.as_str()), now_ms().into()],
            )
            .await?;
        Ok(n > 0)
    }

    pub async fn consume_result_transfers(&self, child: &ThreadId) -> Result<()> {
        self.exec(
            "UPDATE orch_transfers SET status = 'consumed', consumed_at = ?2 WHERE kind = 'subagent_result' \
             AND source_thread = ?1 AND status = 'pending'",
            vec![s(child.as_str()), now_ms().into()],
        )
        .await?;
        Ok(())
    }

    pub async fn fork_origin(&self, fork: &ThreadId) -> Result<Option<TransferRecord>> {
        self.row(
            &format!(
                "SELECT {TRANSFER_COLS} FROM orch_transfers WHERE kind = 'fork' AND target_thread = ?1 \
                 ORDER BY created_at DESC LIMIT 1"
            ),
            vec![s(fork.as_str())],
            transfer_row,
        )
        .await
    }

    /// Forks out of and merges into/out of `thread`.
    pub async fn lineage_rows(&self, thread: &ThreadId) -> Result<Vec<TransferRecord>> {
        self.rows(
            &format!(
                "SELECT {TRANSFER_COLS} FROM orch_transfers WHERE kind IN ('fork','merge_back') \
                 AND (source_thread = ?1 OR target_thread = ?1) ORDER BY created_at"
            ),
            vec![s(thread.as_str())],
            transfer_row,
        )
        .await
    }

    // ── recoveries ──────────────────────────────────────────────────────

    pub async fn recovery_at(
        &self,
        thread: &ThreadId,
        kind: RecoveryKind,
        reset_at: Millis,
    ) -> Result<Option<RecoveryRecord>> {
        self.row(
            &format!(
                "SELECT {RECOVERY_COLS} FROM orch_recoveries WHERE thread_id = ?1 AND kind = ?2 AND reset_at = ?3"
            ),
            vec![s(thread.as_str()), s(kind.as_str()), reset_at.into()],
            recovery_row,
        )
        .await
    }

    pub async fn armed_recovery(
        &self,
        thread: &ThreadId,
        kind: RecoveryKind,
    ) -> Result<Option<RecoveryRecord>> {
        self.row(
            &format!(
                "SELECT {RECOVERY_COLS} FROM orch_recoveries WHERE thread_id = ?1 AND kind = ?2 \
                 AND status = 'armed' ORDER BY created_at LIMIT 1"
            ),
            vec![s(thread.as_str()), s(kind.as_str())],
            recovery_row,
        )
        .await
    }

    pub async fn armed_recoveries(&self, kind: RecoveryKind) -> Result<Vec<RecoveryRecord>> {
        self.rows(
            &format!(
                "SELECT {RECOVERY_COLS} FROM orch_recoveries WHERE kind = ?1 AND status = 'armed' ORDER BY created_at"
            ),
            vec![s(kind.as_str())],
            recovery_row,
        )
        .await
    }

    pub async fn recoveries_for(&self, thread: &ThreadId) -> Result<Vec<RecoveryRecord>> {
        self.rows(
            &format!(
                "SELECT {RECOVERY_COLS} FROM orch_recoveries WHERE thread_id = ?1 ORDER BY created_at DESC LIMIT 20"
            ),
            vec![s(thread.as_str())],
            recovery_row,
        )
        .await
    }

    pub async fn insert_recovery(&self, r: &RecoveryRecord) -> Result<bool> {
        let n = self
            .exec(
                &format!(
                    "INSERT OR IGNORE INTO orch_recoveries ({RECOVERY_COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)"
                ),
                vec![
                    s(r.id.as_str()),
                    s(r.thread_id.as_str()),
                    s(r.kind.as_str()),
                    r.reset_at.into(),
                    s(r.status.as_str()),
                    s(serde_json::to_string(&r.detail)?),
                    i64::from(r.attempts).into(),
                    r.created_at.into(),
                    opt(r.fired_at),
                ],
            )
            .await?;
        Ok(n > 0)
    }

    pub async fn rearm_recovery(&self, id: &RecoveryId, detail: &serde_json::Value) -> Result<()> {
        self.exec(
            "UPDATE orch_recoveries SET status = 'armed', created_at = ?2, fired_at = NULL, detail = ?3 WHERE id = ?1",
            vec![s(id.as_str()), now_ms().into(), s(serde_json::to_string(detail)?)],
        )
        .await?;
        Ok(())
    }

    /// Settle an armed recovery. `fired` bumps attempts and stamps `fired_at`.
    pub async fn settle_recovery(
        &self,
        id: &RecoveryId,
        status: RecoveryStatus,
        note: Option<&str>,
    ) -> Result<bool> {
        let fired = status == RecoveryStatus::Fired;
        let n = self
            .exec(
                "UPDATE orch_recoveries SET status = ?2, \
                 attempts = attempts + CASE WHEN ?3 THEN 1 ELSE 0 END, \
                 fired_at = CASE WHEN ?3 THEN ?4 ELSE fired_at END, \
                 detail = CASE WHEN ?5 IS NULL THEN detail ELSE json_set(detail, '$.outcome', ?5) END \
                 WHERE id = ?1 AND status = 'armed'",
                vec![
                    s(id.as_str()),
                    s(status.as_str()),
                    fired.into(),
                    now_ms().into(),
                    opt(note),
                ],
            )
            .await?;
        Ok(n > 0)
    }

    pub async fn cancel_armed_recoveries(&self, thread: &ThreadId) -> Result<u64> {
        self.exec(
            "UPDATE orch_recoveries SET status = 'cancelled' WHERE thread_id = ?1 AND status = 'armed'",
            vec![s(thread.as_str())],
        )
        .await
    }

    /// Restart continuations fired since `since` (the last user turn).
    pub async fn restart_chain(&self, thread: &ThreadId, since: Millis) -> Result<u32> {
        let n = self
            .row(
                "SELECT COUNT(*) FROM orch_recoveries WHERE thread_id = ?1 AND kind = 'restart' \
                 AND status = 'fired' AND fired_at > ?2",
                vec![s(thread.as_str()), since.into()],
                |r| Cols::new(r).int(),
            )
            .await?
            .unwrap_or(0);
        Ok(n as u32)
    }

    // ── live work ───────────────────────────────────────────────────────

    pub async fn put_live_work(
        &self,
        thread: &ThreadId,
        instance: &str,
        detail: &serde_json::Value,
    ) -> Result<()> {
        self.exec(
            "INSERT OR REPLACE INTO orch_live_work (thread_id, instance_id, detail, updated_at) VALUES (?1,?2,?3,?4)",
            vec![
                s(thread.as_str()),
                s(instance),
                s(serde_json::to_string(detail)?),
                now_ms().into(),
            ],
        )
        .await?;
        Ok(())
    }

    pub async fn delete_live_work(&self, thread: &ThreadId) -> Result<()> {
        self.exec(
            "DELETE FROM orch_live_work WHERE thread_id = ?1",
            vec![s(thread.as_str())],
        )
        .await?;
        Ok(())
    }

    // ── telemetry ───────────────────────────────────────────────────────

    /// Replace the thread's latest telemetry.
    pub async fn put_telemetry(
        &self,
        thread: &ThreadId,
        telemetry: &TurnTelemetry,
        at: Millis,
    ) -> Result<()> {
        self.exec(
            "INSERT OR REPLACE INTO orch_telemetry (thread_id, doc, updated_at) VALUES (?1, ?2, ?3)",
            vec![
                s(thread.as_str()),
                s(serde_json::to_string(telemetry)?),
                Value::Integer(at),
            ],
        )
        .await?;
        Ok(())
    }

    pub async fn telemetry(&self, thread: &ThreadId) -> Result<Option<ThreadTelemetryRecord>> {
        let row = self
            .row(
                "SELECT doc, updated_at FROM orch_telemetry WHERE thread_id = ?1",
                vec![s(thread.as_str())],
                |r| {
                    let mut c = Cols::new(r);
                    Ok((c.text()?, c.int()?))
                },
            )
            .await?;
        row.map(|(doc, updated_at)| {
            Ok(ThreadTelemetryRecord {
                telemetry: serde_json::from_str(&doc)?,
                updated_at,
            })
        })
        .transpose()
    }

    pub async fn live_work(&self) -> Result<Vec<LiveWork>> {
        self.rows(
            "SELECT thread_id, instance_id, detail, updated_at FROM orch_live_work",
            vec![],
            |row| {
                let mut c = Cols::new(row);
                Ok(LiveWork {
                    thread_id: c.thread()?,
                    instance_id: c.text()?,
                    detail: c.ojson()?.unwrap_or(serde_json::Value::Null),
                    updated_at: c.int()?,
                })
            },
        )
        .await
    }
}

/// Optional field updates for [`Store::move_task`]. `Some(None)` clears.
#[derive(Debug, Default, Clone)]
pub struct TaskPatch {
    pub result: Option<Option<String>>,
    pub error: Option<Option<String>>,
    pub started_at: Option<Millis>,
    pub finished_at: Option<Millis>,
    pub delivery: Option<Delivery>,
    pub limit_reset_at: Option<Option<Millis>>,
    pub usage: Option<TurnUsage>,
}
