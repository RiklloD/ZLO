//! Imported terminal sessions: the ledger mapping one CLI session (a source
//! id plus the CLI's own session id) to the engine thread it became, with the
//! file watermark that makes a rescan incremental, and the per-user import
//! settings and last-run record.
//!
//! The ledger is keyed by caller and session, so a rerun never creates a
//! second thread, and a thread the person deleted is never brought back (its
//! ledger row stays). An imported thread lands in the inbox's Settled shelf
//! with nothing unseen: its record is written with `settled_at` and
//! `visited_at` at the session's last activity, in the same transaction as
//! the thread. Nothing here reads session files; the CLI drivers do.

use std::collections::{BTreeMap, HashMap, HashSet};

use ironclaw_host_api::ids::ThreadId;
use ironclaw_product_contracts::session_import::{
    ImportTrigger, SessionImportCounts, SessionImportSettings,
};
use libsql::Value;
use serde::{Deserialize, Serialize};

use crate::error::OrchestratorError;
use crate::model::{Millis, RuntimeId, ThreadRecord, now_ms};
use crate::profiles::ProfileOwner;
use crate::store::{Cols, Store, opt, s};

type Result<T> = std::result::Result<T, OrchestratorError>;

pub(crate) const SESSION_IMPORTS_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS orch_session_imports (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  source TEXT NOT NULL,
  native_id TEXT NOT NULL,
  thread_id TEXT NOT NULL,
  path TEXT NOT NULL,
  size INTEGER NOT NULL,
  mtime_ms INTEGER NOT NULL,
  read_offset INTEGER NOT NULL,
  messages INTEGER NOT NULL,
  imported_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (tenant_id, user_id, source, native_id)
);
CREATE INDEX IF NOT EXISTS orch_session_imports_thread ON orch_session_imports(thread_id);
CREATE TABLE IF NOT EXISTS orch_session_import_state (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  doc TEXT NOT NULL,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (tenant_id, user_id)
);
"#;

/// Activity this close to a settle is the settle itself (as in the inbox).
const SETTLE_GRACE_MS: Millis = 1000;

/// How far a session file was read: its size and mtime when last looked
/// at, and the byte offset after the last complete line taken in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportWatermark {
    pub size: u64,
    pub mtime_ms: Millis,
    pub offset: u64,
}

/// One ledger row as the importer needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportLedgerEntry {
    pub thread_id: ThreadId,
    pub watermark: ImportWatermark,
    pub messages: u64,
    /// The thread still exists (the person did not delete it).
    pub thread_live: bool,
    /// A turn ran in the app since the import: the app's own record of the
    /// thread is authoritative from then on.
    pub adopted: bool,
}

/// A new thread for one session.
#[derive(Debug, Clone)]
pub struct ImportedThread {
    /// Scope, runtime, native session id, title and the session's own
    /// `created_at`/`updated_at`/`last_user_at`.
    pub record: ThreadRecord,
    pub source: String,
    pub native_id: String,
    pub path: String,
    pub watermark: ImportWatermark,
    pub messages: u64,
}

/// What a grown session added since its watermark.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportGrowth {
    pub watermark: ImportWatermark,
    pub messages_added: u64,
    pub last_user_at: Option<Millis>,
    pub last_at: Option<Millis>,
}

/// A caller's settings and last run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionImportState {
    pub settings: SessionImportSettings,
    pub last_run_at: Option<Millis>,
    pub last_trigger: Option<ImportTrigger>,
    pub last_counts: BTreeMap<String, SessionImportCounts>,
    pub last_error: Option<String>,
}

fn int(v: u64) -> Value {
    Value::Integer(i64::try_from(v).unwrap_or(i64::MAX))
}

fn uint(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

fn owner_params(owner: &ProfileOwner) -> Vec<Value> {
    vec![s(owner.tenant_id.as_str()), s(owner.user_id.as_str())]
}

/// The settle mark of an imported thread: its last activity, the value the
/// inbox compares new activity against.
fn settle_at(record: &ThreadRecord) -> Millis {
    [
        Some(record.created_at),
        Some(record.updated_at),
        record.last_user_at,
    ]
    .into_iter()
    .flatten()
    .max()
    .unwrap_or(record.created_at)
}

impl Store {
    /// The caller's imported sessions of one source, by native session id.
    pub async fn import_ledger(
        &self,
        owner: &ProfileOwner,
        source: &str,
    ) -> Result<HashMap<String, ImportLedgerEntry>> {
        let mut params = owner_params(owner);
        params.push(s(source));
        let rows = self
            .rows(
                "SELECT i.native_id, i.thread_id, i.size, i.mtime_ms, i.read_offset, i.messages, \
                   t.thread_id IS NOT NULL, \
                   t.turn_started_at IS NOT NULL OR COALESCE(t.turn_state, 'idle') = 'running' \
                 FROM orch_session_imports i LEFT JOIN orch_threads t ON t.thread_id = i.thread_id \
                 WHERE i.tenant_id = ?1 AND i.user_id = ?2 AND i.source = ?3",
                params,
                |r| {
                    let mut c = Cols::new(r);
                    let native = c.text()?;
                    Ok((
                        native,
                        ImportLedgerEntry {
                            thread_id: ThreadId::new(c.text()?)?,
                            watermark: ImportWatermark {
                                size: uint(c.int()?),
                                mtime_ms: c.int()?,
                                offset: uint(c.int()?),
                            },
                            messages: uint(c.int()?),
                            thread_live: c.int()? != 0,
                            adopted: c.int()? != 0,
                        },
                    ))
                },
            )
            .await?;
        Ok(rows.into_iter().collect())
    }

    /// Native session ids of every thread on these runtimes (all callers):
    /// sessions this app started, which an import must not duplicate.
    pub async fn native_sessions(&self, runtimes: &[RuntimeId]) -> Result<HashSet<String>> {
        let mut out = HashSet::new();
        for runtime in runtimes {
            let ids = self
                .rows(
                    "SELECT native_session_id FROM orch_threads \
                     WHERE runtime_id = ?1 AND native_session_id IS NOT NULL",
                    vec![s(runtime.as_str())],
                    |r| Cols::new(r).text(),
                )
                .await?;
            out.extend(ids);
        }
        Ok(out)
    }

    /// Create the thread, its ledger row and its settled, seen inbox record
    /// in one transaction. A rerun for the same session changes nothing
    /// (`INSERT OR IGNORE` throughout; existing inbox marks are kept).
    pub async fn record_imported_thread(
        &self,
        owner: &ProfileOwner,
        imported: &ImportedThread,
    ) -> Result<()> {
        let r = &imported.record;
        let settle = settle_at(r);
        let now = now_ms();
        let mut ledger = owner_params(owner);
        ledger.extend([
            s(&imported.source),
            s(&imported.native_id),
            s(r.thread_id.as_str()),
            s(&imported.path),
            int(imported.watermark.size),
            Value::Integer(imported.watermark.mtime_ms),
            int(imported.watermark.offset),
            int(imported.messages),
            Value::Integer(now),
        ]);
        let mut inbox = owner_params(owner);
        inbox.extend([
            s(r.thread_id.as_str()),
            Value::Integer(now),
            Value::Integer(settle),
        ]);
        let statements = vec![
            crate::store::insert_thread_sql(r),
            (
                "INSERT OR IGNORE INTO orch_session_imports (tenant_id, user_id, source, native_id, \
                   thread_id, path, size, mtime_ms, read_offset, messages, imported_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11)"
                    .to_string(),
                ledger,
            ),
            (
                "INSERT OR IGNORE INTO orch_inbox (tenant_id, user_id, thread_id, updated_at, \
                   settled_at, visited_at) VALUES (?1, ?2, ?3, ?4, ?5, ?5)"
                    .to_string(),
                inbox,
            ),
        ];
        self.tx(statements).await
    }

    /// Record how far a session was read, and what it added. New activity
    /// moves the thread's recency, and keeps it settled when it still sits
    /// on the Settled shelf as imported (not pinned, snoozed, brought back
    /// or continued in the app). A thread that ran a turn here is left
    /// alone apart from the watermark.
    pub async fn advance_import(
        &self,
        owner: &ProfileOwner,
        source: &str,
        native_id: &str,
        thread: &ThreadId,
        growth: &ImportGrowth,
    ) -> Result<()> {
        let now = now_ms();
        let activity = growth.last_user_at.max(growth.last_at);
        let mut statements = Vec::new();
        if let Some(at) = activity {
            let mut params = owner_params(owner);
            params.extend([
                s(thread.as_str()),
                Value::Integer(at),
                Value::Integer(now),
                Value::Integer(SETTLE_GRACE_MS),
            ]);
            // Before the thread row moves: its activity must still be the
            // settle's own for the shelf to keep it.
            statements.push((
                "UPDATE orch_inbox SET settled_at = ?4, \
                   visited_at = MAX(COALESCE(visited_at, 0), ?4), updated_at = ?5 \
                 WHERE tenant_id = ?1 AND user_id = ?2 AND thread_id = ?3 \
                   AND settled_at IS NOT NULL AND ?4 > settled_at \
                   AND pinned_at IS NULL AND snooze_until IS NULL \
                   AND EXISTS (SELECT 1 FROM orch_threads t WHERE t.thread_id = ?3 \
                     AND t.turn_started_at IS NULL AND t.turn_state <> 'running' \
                     AND MAX(t.created_at, COALESCE(t.turn_ended_at, 0), \
                             COALESCE(t.last_user_at, 0)) <= orch_inbox.settled_at + ?6)"
                    .to_string(),
                params,
            ));
            statements.push((
                "UPDATE orch_threads SET updated_at = MAX(updated_at, ?2), \
                   last_user_at = CASE WHEN ?3 IS NULL THEN last_user_at \
                     ELSE MAX(COALESCE(last_user_at, 0), ?3) END \
                 WHERE thread_id = ?1 AND turn_started_at IS NULL"
                    .to_string(),
                vec![
                    s(thread.as_str()),
                    Value::Integer(at),
                    opt(growth.last_user_at),
                ],
            ));
        }
        let mut ledger = owner_params(owner);
        ledger.extend([
            s(source),
            s(native_id),
            int(growth.watermark.size),
            Value::Integer(growth.watermark.mtime_ms),
            int(growth.watermark.offset),
            int(growth.messages_added),
            Value::Integer(now),
        ]);
        statements.push((
            "UPDATE orch_session_imports SET size = ?5, mtime_ms = ?6, read_offset = ?7, \
               messages = messages + ?8, updated_at = ?9 \
             WHERE tenant_id = ?1 AND user_id = ?2 AND source = ?3 AND native_id = ?4"
                .to_string(),
            ledger,
        ));
        self.tx(statements).await
    }

    /// The caller's imported threads and the source each came from.
    pub async fn imported_thread_sources(
        &self,
        owner: &ProfileOwner,
    ) -> Result<HashMap<String, String>> {
        let rows = self
            .rows(
                "SELECT thread_id, source FROM orch_session_imports \
                 WHERE tenant_id = ?1 AND user_id = ?2",
                owner_params(owner),
                |r| {
                    let mut c = Cols::new(r);
                    Ok((c.text()?, c.text()?))
                },
            )
            .await?;
        Ok(rows.into_iter().collect())
    }

    pub async fn session_import_state(&self, owner: &ProfileOwner) -> Result<SessionImportState> {
        let raw = self
            .row(
                "SELECT doc FROM orch_session_import_state WHERE tenant_id = ?1 AND user_id = ?2",
                owner_params(owner),
                |r| Cols::new(r).text(),
            )
            .await?;
        match raw {
            Some(raw) => Ok(serde_json::from_str(&raw)?),
            None => Ok(SessionImportState::default()),
        }
    }

    pub async fn put_session_import_state(
        &self,
        owner: &ProfileOwner,
        state: &SessionImportState,
    ) -> Result<()> {
        let mut params = owner_params(owner);
        params.extend([s(serde_json::to_string(state)?), Value::Integer(now_ms())]);
        self.exec(
            "INSERT INTO orch_session_import_state (tenant_id, user_id, doc, updated_at) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (tenant_id, user_id) DO UPDATE SET doc = ?3, updated_at = ?4",
            params,
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ironclaw_host_api::ids::{TenantId, UserId};
    use ironclaw_product_contracts::inbox::InboxState;

    use super::*;
    use crate::inbox::classify;
    use crate::model::ThreadScope;

    async fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            libsql::Builder::new_local(dir.path().join("o.db"))
                .build()
                .await
                .unwrap(),
        );
        let runtime = ironclaw_libsql_runtime::LibSqlRuntime::new(db).unwrap();
        let store = Store::new(Arc::new(runtime));
        store
            .migrate(&RuntimeId::new("ironclaw").unwrap())
            .await
            .unwrap();
        (dir, store)
    }

    fn scope() -> ThreadScope {
        ThreadScope {
            tenant_id: TenantId::new("t").unwrap(),
            user_id: UserId::new("u").unwrap(),
            agent_id: None,
            project_id: None,
        }
    }

    fn imported(native: &str, created: Millis, last: Millis) -> ImportedThread {
        let mut record = ThreadRecord::new(
            ThreadId::new(format!("thread-{native}")).unwrap(),
            scope(),
            RuntimeId::new("cli").unwrap(),
        );
        record.native_session_id = Some(native.into());
        record.created_at = created;
        record.updated_at = last;
        record.last_user_at = Some(last);
        ImportedThread {
            record,
            source: "src".into(),
            native_id: native.into(),
            path: "/x.jsonl".into(),
            watermark: ImportWatermark {
                size: 10,
                mtime_ms: 5,
                offset: 10,
            },
            messages: 2,
        }
    }

    async fn state_of(store: &Store, thread: &str) -> InboxState {
        let owner = ProfileOwner::from(&scope());
        let threads = store.inbox_threads(&owner).await.unwrap();
        let t = threads
            .iter()
            .find(|t| t.thread_id.as_str() == thread)
            .unwrap();
        let records = store.inbox_records(&owner).await.unwrap();
        let epoch = store.inbox_epoch().await.unwrap();
        let (state, status) = classify(&t.facts, records.get(thread), epoch, now_ms());
        assert_eq!(
            status, None,
            "an imported thread asks nothing of the person"
        );
        state
    }

    #[tokio::test]
    async fn an_import_is_one_settled_seen_thread_however_often_it_runs() {
        let (_dir, store) = store().await;
        let owner = ProfileOwner::from(&scope());
        let now = now_ms();
        let session = imported("s1", now - 50_000, now - 20_000);
        store
            .record_imported_thread(&owner, &session)
            .await
            .unwrap();
        store
            .record_imported_thread(&owner, &session)
            .await
            .unwrap();

        let ledger = store.import_ledger(&owner, "src").await.unwrap();
        assert_eq!(ledger.len(), 1);
        let entry = &ledger["s1"];
        assert!(entry.thread_live && !entry.adopted);
        assert_eq!(entry.watermark.offset, 10);
        assert_eq!(state_of(&store, "thread-s1").await, InboxState::Settled);
        let record = store
            .thread(&ThreadId::new("thread-s1").unwrap())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.created_at, now - 50_000, "original time kept");
        assert_eq!(
            store.imported_thread_sources(&owner).await.unwrap()["thread-s1"],
            "src"
        );
        assert!(
            store
                .native_sessions(&[RuntimeId::new("cli").unwrap()])
                .await
                .unwrap()
                .contains("s1")
        );
    }

    #[tokio::test]
    async fn growth_stays_settled_until_the_person_uses_the_thread() {
        let (_dir, store) = store().await;
        let owner = ProfileOwner::from(&scope());
        let now = now_ms();
        let thread = ThreadId::new("thread-s2").unwrap();
        store
            .record_imported_thread(&owner, &imported("s2", now - 90_000, now - 60_000))
            .await
            .unwrap();
        let grow = |at: Millis, offset: u64| ImportGrowth {
            watermark: ImportWatermark {
                size: offset,
                mtime_ms: at,
                offset,
            },
            messages_added: 1,
            last_user_at: Some(at),
            last_at: Some(at),
        };
        // The session grew in the terminal: newer, still settled.
        store
            .advance_import(&owner, "src", "s2", &thread, &grow(now - 30_000, 20))
            .await
            .unwrap();
        assert_eq!(state_of(&store, "thread-s2").await, InboxState::Settled);
        let entry = &store.import_ledger(&owner, "src").await.unwrap()["s2"];
        assert_eq!((entry.watermark.offset, entry.messages), (20, 3));
        let rec = store.thread(&thread).await.unwrap().unwrap();
        assert_eq!(rec.last_user_at, Some(now - 30_000));

        // The person sends a message here: it leaves the shelf like any
        // settled thread, and later terminal growth does not put it back.
        store.set_last_user(&thread, now - 1_000).await.unwrap();
        assert_ne!(state_of(&store, "thread-s2").await, InboxState::Settled);
        store
            .advance_import(&owner, "src", "s2", &thread, &grow(now, 30))
            .await
            .unwrap();
        assert_ne!(state_of(&store, "thread-s2").await, InboxState::Settled);
    }

    #[tokio::test]
    async fn state_round_trips_per_caller() {
        let (_dir, store) = store().await;
        let owner = ProfileOwner::from(&scope());
        assert_eq!(
            store.session_import_state(&owner).await.unwrap(),
            SessionImportState::default()
        );
        let mut state = SessionImportState {
            last_run_at: Some(7),
            last_trigger: Some(ImportTrigger::Launch),
            ..SessionImportState::default()
        };
        state.settings.sources.insert("src".into(), false);
        store
            .put_session_import_state(&owner, &state)
            .await
            .unwrap();
        assert_eq!(store.session_import_state(&owner).await.unwrap(), state);
    }
}
