//! One-time move of `ironclaw_threads` data into the orchestrator (PLAN_V2
//! stage 2: "Current ironclaw_threads data migrates into it once").
//!
//! The thread registry moves: every `session_thread` record of the
//! root-filesystem table in the same libSQL file becomes an `orch_threads`
//! row on the default runtime (`imported = 1`), with no `profile_id`, which
//! resolves to the owner's default profile. Transcripts are never copied;
//! they stay in `ironclaw_threads`.
//!
//! Each step runs once in one IMMEDIATE transaction and records an
//! `orch_meta` marker in it, so a rerun is a no-op and a crash rolls the
//! whole step back. Nothing is updated or deleted: rows already known to
//! the engine are kept (`INSERT OR IGNORE`).

use ironclaw_product_contracts::profiles::{DEFAULT_PROFILE_ID, ProfileSettings};
use libsql::Value;

use crate::engine::Result;
use crate::model::{RuntimeId, now_ms};
use crate::store::{Cols, Store, s};

/// Kept from the first release of this import: databases that already ran
/// it are not imported again.
const THREADS_MARKER: &str = "legacy_threads_imported_v1";
const PROFILES_MARKER: &str = "legacy_threads_default_profiles_v1";

const IMPORT_THREADS_SQL: &str = r#"
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

/// The default profile row for every owner of an imported thread, so the
/// threads land on a real profile (the profiles list creates it lazily too).
const DEFAULT_PROFILES_SQL: &str = r#"
INSERT OR IGNORE INTO orch_profiles (tenant_id, user_id, profile_id, settings, created_at, updated_at)
SELECT DISTINCT tenant_id, user_id, ?1, ?2, ?3, ?3 FROM orch_threads WHERE imported = 1
"#;

impl Store {
    /// Run the pending one-time steps; returns the number of threads imported.
    pub(crate) async fn import_legacy_threads(&self, runtime: &RuntimeId) -> Result<usize> {
        let has_source = self
            .row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'root_filesystem_entries'",
                vec![],
                |r| Cols::new(r).int(),
            )
            .await?
            .is_some();
        let imported = if has_source {
            self.once(
                THREADS_MARKER,
                Some((IMPORT_THREADS_SQL, vec![s(runtime.as_str())])),
            )
            .await?
        } else {
            // A fresh database has nothing to import; never look again.
            self.once(THREADS_MARKER, None).await?
        };
        let settings = ProfileSettings {
            display_name: "Default".to_string(),
            ..ProfileSettings::default()
        };
        let params = vec![
            s(DEFAULT_PROFILE_ID),
            s(serde_json::to_string(&settings)?),
            now_ms().into(),
        ];
        self.once(PROFILES_MARKER, Some((DEFAULT_PROFILES_SQL, params)))
            .await?;
        Ok(imported)
    }

    /// Run `step` and record `marker` in one transaction, unless the marker
    /// is already there. Returns the rows `step` changed (0 when skipped).
    async fn once(&self, marker: &str, step: Option<(&str, Vec<Value>)>) -> Result<usize> {
        let conn = self.db.write().await?;
        let tx = conn
            .transaction_with_behavior(libsql::TransactionBehavior::Immediate)
            .await?;
        let done = async {
            let mut seen = tx
                .query("SELECT 1 FROM orch_meta WHERE key = ?1", vec![s(marker)])
                .await?;
            if seen.next().await?.is_some() {
                return Ok::<_, libsql::Error>(None);
            }
            drop(seen);
            let changed = match step {
                Some((sql, params)) => tx.execute(sql, params).await? as usize,
                None => 0,
            };
            tx.execute(
                "INSERT INTO orch_meta (key, value) VALUES (?1, ?2)",
                vec![s(marker), s(changed.to_string())],
            )
            .await?;
            Ok(Some(changed))
        }
        .await;
        match done {
            Ok(Some(changed)) => {
                tx.commit().await?;
                Ok(changed)
            }
            Ok(None) => {
                tx.rollback().await?;
                Ok(0)
            }
            Err(error) => {
                tx.rollback().await?;
                Err(error.into())
            }
        }
    }
}
