//! Inbox lifecycle (v2 stage 5, t3code's inbox): pin, snooze, settle and
//! visit records per (tenant, user, thread) in `orch_inbox`, the thread
//! facts the inbox joins from the engine's own tables, and the pure state
//! derivation (T3's precedence: snoozed, settled, pinned, working, inbox;
//! a working thread that waits on the person comes back to the inbox).
//!
//! Records are keyed by the caller, not only the thread, so automation-run
//! threads (which the engine does not own) can be pinned or settled too. A
//! record carries no thread content; the product tier checks the thread is
//! the caller's before writing one.

use std::collections::HashMap;

use ironclaw_host_api::ids::ThreadId;
use ironclaw_product_contracts::inbox::{InboxAction, InboxEngine, InboxState, InboxStatus};
use libsql::Value;

use crate::error::OrchestratorError;
use crate::model::{HoldReason, Millis, ProfileId, TASK_ACTIVE_SQL, TurnState};
use crate::profiles::ProfileOwner;
use crate::store::{Cols, Store, s};

type Result<T> = std::result::Result<T, OrchestratorError>;

pub(crate) const INBOX_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS orch_inbox (
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  thread_id TEXT NOT NULL,
  pinned_at INTEGER,
  snooze_until INTEGER,
  snoozed_at INTEGER,
  settled_at INTEGER,
  visited_at INTEGER,
  title TEXT,
  titled_through INTEGER,
  updated_at INTEGER NOT NULL,
  PRIMARY KEY (tenant_id, user_id, thread_id)
);
CREATE TABLE IF NOT EXISTS orch_awaiting (
  thread_id TEXT PRIMARY KEY,
  run_id TEXT,
  kind TEXT NOT NULL,
  since INTEGER NOT NULL
);
INSERT OR IGNORE INTO orch_meta (key, value)
  VALUES ('inbox_epoch_ms', CAST((julianday('now') - 2440587.5) * 86400000 AS INTEGER));
"#;

/// Turns that ended before the inbox existed count as seen.
const EPOCH_KEY: &str = "inbox_epoch_ms";
/// Activity this close to a settle is the settle itself, not new work.
const SETTLE_GRACE_MS: Millis = 1000;

/// One caller's lifecycle marks on one thread.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InboxRecord {
    pub pinned_at: Option<Millis>,
    pub snooze_until: Option<Millis>,
    pub snoozed_at: Option<Millis>,
    pub settled_at: Option<Millis>,
    pub visited_at: Option<Millis>,
    /// Written by the title task model; wins over the derived title.
    pub title: Option<String>,
}

/// A generated title and the last reply (message sequence) it was made from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedTitle {
    pub title: String,
    pub through_sequence: Option<i64>,
}

/// What the inbox needs to know about a thread's live state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InboxFacts {
    pub running: bool,
    /// The running turn waits on the person (an approval or a sign-in).
    pub awaiting_input: bool,
    pub live_children: bool,
    pub limit_reset_at: Option<Millis>,
    pub interrupted: bool,
    pub failed: bool,
    pub stopped: bool,
    pub last_activity_at: Millis,
    pub last_ended_at: Option<Millis>,
    /// When the running turn started (the inbox's working timer).
    pub working_since: Option<Millis>,
}

/// An engine-owned thread, joined for the inbox.
#[derive(Debug, Clone)]
pub struct InboxThread {
    pub thread_id: ThreadId,
    pub profile_id: Option<ProfileId>,
    pub title: Option<String>,
    pub parent_thread_id: Option<ThreadId>,
    /// Own engine first, then every delegated descendant's.
    pub engines: Vec<InboxEngine>,
    pub facts: InboxFacts,
}

/// Section and badge of one row.
pub fn classify(
    facts: &InboxFacts,
    record: Option<&InboxRecord>,
    epoch: Millis,
    now: Millis,
) -> (InboxState, Option<InboxStatus>) {
    let empty = InboxRecord::default();
    let rec = record.unwrap_or(&empty);
    let live = facts.running || facts.live_children;
    let needs_input = facts.running && facts.awaiting_input;
    let seen = rec.visited_at.unwrap_or(epoch);

    // A snooze wakes early when a turn ends after it was set (a result or
    // a failure arrived), otherwise at its time.
    let wake_at = rec.snooze_until.and_then(|until| {
        let ended_since = facts
            .last_ended_at
            .filter(|ended| !live && *ended > rec.snoozed_at.unwrap_or(0));
        ended_since.or((until <= now).then_some(until))
    });
    let snoozed = rec.snooze_until.is_some() && wake_at.is_none();
    // New work since the settle brings the thread back.
    let settled = rec
        .settled_at
        .is_some_and(|at| !live && facts.last_activity_at <= at + SETTLE_GRACE_MS);

    let state = if snoozed {
        InboxState::Snoozed
    } else if settled {
        InboxState::Settled
    } else if rec.pinned_at.is_some() {
        InboxState::Pinned
    } else if live && !needs_input {
        InboxState::Working
    } else {
        InboxState::Inbox
    };

    let status = if needs_input {
        Some(InboxStatus::NeedsInput)
    } else if facts.running {
        Some(InboxStatus::Working)
    } else if facts.live_children {
        Some(InboxStatus::Background)
    } else if facts.limit_reset_at.is_some() {
        Some(InboxStatus::Limited)
    } else if facts.interrupted {
        Some(InboxStatus::Interrupted)
    } else if facts.failed {
        Some(InboxStatus::Failed)
    } else if facts.stopped {
        Some(InboxStatus::Stopped)
    } else if wake_at.is_some_and(|at| at > seen) {
        Some(InboxStatus::Woke)
    } else if facts.last_ended_at.is_some_and(|at| at > seen) {
        Some(InboxStatus::Done)
    } else {
        None
    };
    (state, status)
}

/// Columns one action writes (`?4` = now, `?5` = until); the rest keep
/// their value (or stay NULL on a new record).
fn action_columns(action: InboxAction) -> &'static [(&'static str, &'static str)] {
    match action {
        // A pin is an explicit promotion: it leaves Snoozed and Settled.
        InboxAction::Pin => &[
            ("pinned_at", "?4"),
            ("snooze_until", "NULL"),
            ("snoozed_at", "NULL"),
            ("settled_at", "NULL"),
        ],
        InboxAction::Unpin => &[("pinned_at", "NULL")],
        InboxAction::Snooze => &[
            ("snooze_until", "?5"),
            ("snoozed_at", "?4"),
            ("settled_at", "NULL"),
        ],
        InboxAction::Wake => &[
            ("snooze_until", "NULL"),
            ("snoozed_at", "NULL"),
            ("visited_at", "?4"),
        ],
        InboxAction::Settle => &[
            ("settled_at", "?4"),
            ("pinned_at", "NULL"),
            ("snooze_until", "NULL"),
            ("snoozed_at", "NULL"),
        ],
        InboxAction::Unsettle => &[("settled_at", "NULL")],
        InboxAction::Visit => &[("visited_at", "?4")],
    }
}

/// Why a running turn waits on the person.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AwaitingKind {
    Approval,
    Auth,
}

impl AwaitingKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Approval => "approval",
            Self::Auth => "auth",
        }
    }
}

impl Store {
    /// The thread's running turn parked on a gate the person answers.
    /// CLI runtimes record theirs in `orch_cli_approvals` instead.
    pub async fn set_awaiting(
        &self,
        thread: &ThreadId,
        run_id: Option<&str>,
        kind: AwaitingKind,
        at: Millis,
    ) -> Result<()> {
        self.exec(
            "INSERT INTO orch_awaiting (thread_id, run_id, kind, since) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(thread_id) DO UPDATE SET run_id = ?2, kind = ?3, since = ?4",
            vec![
                s(thread.as_str()),
                run_id.map_or(Value::Null, s),
                s(kind.as_str()),
                Value::Integer(at),
            ],
        )
        .await?;
        Ok(())
    }

    /// The turn resumed or ended: nothing waits on the person any more.
    pub async fn clear_awaiting(&self, thread: &ThreadId) -> Result<()> {
        self.exec(
            "DELETE FROM orch_awaiting WHERE thread_id = ?1",
            vec![s(thread.as_str())],
        )
        .await?;
        Ok(())
    }

    /// When the inbox table was created (turns before it count as seen).
    pub async fn inbox_epoch(&self) -> Result<Millis> {
        let raw = self.meta(EPOCH_KEY).await?;
        raw.as_deref()
            .map(str::parse::<Millis>)
            .transpose()
            .map_err(|e| OrchestratorError::Corrupt(format!("{EPOCH_KEY}: {e}")))
            .map(Option::unwrap_or_default)
    }

    pub async fn inbox_records(
        &self,
        owner: &ProfileOwner,
    ) -> Result<HashMap<String, InboxRecord>> {
        let rows = self
            .rows(
                "SELECT thread_id, pinned_at, snooze_until, snoozed_at, settled_at, visited_at, title \
                 FROM orch_inbox WHERE tenant_id = ?1 AND user_id = ?2",
                vec![s(owner.tenant_id.as_str()), s(owner.user_id.as_str())],
                |row| {
                    let mut c = Cols::new(row);
                    Ok((
                        c.text()?,
                        InboxRecord {
                            pinned_at: c.oint()?,
                            snooze_until: c.oint()?,
                            snoozed_at: c.oint()?,
                            settled_at: c.oint()?,
                            visited_at: c.oint()?,
                            title: c.otext()?,
                        },
                    ))
                },
            )
            .await?;
        Ok(rows.into_iter().collect())
    }

    /// Bring an `orch_inbox` created before the title column up to date.
    pub(crate) async fn upgrade_inbox_schema(&self) -> Result<()> {
        let present = self
            .rows("PRAGMA table_info(orch_inbox)", vec![], |r| {
                let mut c = Cols::new(r);
                c.int()?;
                c.text()
            })
            .await?;
        for (name, definition) in [("title", "TEXT"), ("titled_through", "INTEGER")] {
            if !present.iter().any(|p| p == name) {
                self.exec(
                    &format!("ALTER TABLE orch_inbox ADD COLUMN {name} {definition}"),
                    vec![],
                )
                .await?;
            }
        }
        Ok(())
    }

    /// The caller's saved title for a thread, if one was generated.
    pub async fn inbox_title(
        &self,
        owner: &ProfileOwner,
        thread: &ThreadId,
    ) -> Result<Option<SavedTitle>> {
        let row = self
            .row(
                "SELECT title, titled_through FROM orch_inbox \
                 WHERE tenant_id = ?1 AND user_id = ?2 AND thread_id = ?3",
                vec![
                    s(owner.tenant_id.as_str()),
                    s(owner.user_id.as_str()),
                    s(thread.as_str()),
                ],
                |r| {
                    let mut c = Cols::new(r);
                    Ok(c.otext()?.map(|title| (title, c.oint())))
                },
            )
            .await?;
        match row.flatten() {
            Some((title, through)) => Ok(Some(SavedTitle {
                title,
                through_sequence: through?,
            })),
            None => Ok(None),
        }
    }

    /// Save a generated title (upsert; lifecycle marks are kept).
    pub async fn inbox_set_title(
        &self,
        owner: &ProfileOwner,
        thread: &ThreadId,
        saved: &SavedTitle,
        now: Millis,
    ) -> Result<()> {
        self.exec(
            "INSERT INTO orch_inbox (tenant_id, user_id, thread_id, updated_at, title, titled_through) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
             ON CONFLICT (tenant_id, user_id, thread_id) \
             DO UPDATE SET title = ?5, titled_through = ?6, updated_at = ?4",
            vec![
                s(owner.tenant_id.as_str()),
                s(owner.user_id.as_str()),
                s(thread.as_str()),
                Value::Integer(now),
                s(&saved.title),
                saved.through_sequence.map_or(Value::Null, Value::Integer),
            ],
        )
        .await?;
        Ok(())
    }

    /// Apply one lifecycle action (upsert, one statement).
    pub async fn inbox_mark(
        &self,
        owner: &ProfileOwner,
        thread: &ThreadId,
        action: InboxAction,
        until: Option<Millis>,
        now: Millis,
    ) -> Result<()> {
        if action == InboxAction::Snooze && !until.is_some_and(|u| u > now) {
            return Err(OrchestratorError::invalid(
                "snooze needs `until` in the future (unix ms)",
            ));
        }
        let columns = action_columns(action);
        let names: Vec<&str> = columns.iter().map(|(c, _)| *c).collect();
        let values: Vec<&str> = columns.iter().map(|(_, v)| *v).collect();
        let sets: Vec<String> = columns.iter().map(|(c, v)| format!("{c} = {v}")).collect();
        let sql = format!(
            "INSERT INTO orch_inbox (tenant_id, user_id, thread_id, updated_at, {names}) \
             VALUES (?1, ?2, ?3, ?4, {values}) \
             ON CONFLICT (tenant_id, user_id, thread_id) DO UPDATE SET {sets}, updated_at = ?4",
            names = names.join(", "),
            values = values.join(", "),
            sets = sets.join(", "),
        );
        let mut params = vec![
            s(owner.tenant_id.as_str()),
            s(owner.user_id.as_str()),
            s(thread.as_str()),
            Value::Integer(now),
        ];
        if let (InboxAction::Snooze, Some(until)) = (action, until) {
            params.push(Value::Integer(until));
        }
        self.exec(&sql, params).await?;
        Ok(())
    }

    /// The owner's engine threads with their inbox facts and engines.
    pub async fn inbox_threads(&self, owner: &ProfileOwner) -> Result<Vec<InboxThread>> {
        let owner_params = || vec![s(owner.tenant_id.as_str()), s(owner.user_id.as_str())];
        let sql = format!(
            "SELECT t.thread_id, t.profile_id, t.title, t.runtime_id, \
               COALESCE(t.provider, json_extract(tel.doc, '$.provider')), \
               COALESCE(t.model, json_extract(tel.doc, '$.model')), \
               t.turn_state, t.turn_started_at, t.turn_ended_at, t.last_user_at, t.created_at, t.updated_at, \
               (SELECT h.reason FROM orch_holds h WHERE h.thread_id = t.thread_id), \
               (SELECT MIN(r.reset_at) FROM orch_recoveries r WHERE r.thread_id = t.thread_id \
                  AND r.kind = 'usage_limit' AND r.status = 'armed'), \
               (SELECT COUNT(*) FROM orch_recoveries r WHERE r.thread_id = t.thread_id \
                  AND r.kind = 'restart' AND r.status = 'armed'), \
               (SELECT COUNT(*) FROM orch_tasks k WHERE k.parent_thread = t.thread_id \
                  AND k.{TASK_ACTIVE_SQL}), \
               (SELECT k.parent_thread FROM orch_tasks k WHERE k.child_thread = t.thread_id \
                  ORDER BY k.created_at LIMIT 1), \
               (SELECT COUNT(*) FROM orch_awaiting a WHERE a.thread_id = t.thread_id) \
                 + (SELECT COUNT(*) FROM orch_cli_approvals c WHERE c.thread_id = t.thread_id \
                  AND c.decision IS NULL) \
             FROM orch_threads t LEFT JOIN orch_telemetry tel ON tel.thread_id = t.thread_id \
             WHERE t.tenant_id = ?1 AND t.user_id = ?2"
        );
        let mut threads = self
            .rows(&sql, owner_params(), |row| {
                let mut c = Cols::new(row);
                let thread_id = ThreadId::new(c.text()?)?;
                let profile_id = c.otext()?.map(ProfileId::new).transpose()?;
                let title = c.otext()?;
                let own = InboxEngine {
                    runtime_id: c.text()?,
                    provider: c.otext()?,
                    model: c.otext()?,
                };
                let turn_state = TurnState::parse(&c.text()?)?;
                let started = c.oint()?;
                let ended = c.oint()?;
                let last_user = c.oint()?;
                let created = c.int()?;
                let updated = c.int()?;
                let hold = c.otext()?.map(|h| HoldReason::parse(&h)).transpose()?;
                let limit_reset_at = c.oint()?;
                let restart_armed = c.int()? > 0;
                let live_children = c.int()? > 0;
                let parent = c.otext()?.map(ThreadId::new).transpose()?;
                let awaiting_input = c.int()? > 0;
                let last_activity_at = [Some(created), ended, last_user]
                    .into_iter()
                    .flatten()
                    .max()
                    .unwrap_or(updated);
                Ok(InboxThread {
                    thread_id,
                    profile_id,
                    title,
                    parent_thread_id: parent,
                    engines: vec![own],
                    facts: InboxFacts {
                        running: turn_state == TurnState::Running,
                        awaiting_input,
                        live_children,
                        limit_reset_at,
                        interrupted: restart_armed || hold == Some(HoldReason::Interrupted),
                        failed: turn_state == TurnState::Error,
                        stopped: matches!(hold, Some(HoldReason::Stopped | HoldReason::Cancelled)),
                        last_activity_at,
                        last_ended_at: ended,
                        working_since: started.filter(|_| turn_state == TurnState::Running),
                    },
                })
            })
            .await?;

        // Delegated children's engines, by parent thread, in delegation order.
        let tasks = self
            .rows(
                "SELECT k.parent_thread, k.child_thread, k.runtime_id, k.provider, k.model \
                 FROM orch_tasks k JOIN orch_threads t ON t.thread_id = k.root_thread \
                 WHERE t.tenant_id = ?1 AND t.user_id = ?2 ORDER BY k.created_at",
                owner_params(),
                |row| {
                    let mut c = Cols::new(row);
                    Ok((
                        c.text()?,
                        c.otext()?,
                        InboxEngine {
                            runtime_id: c.text()?,
                            provider: c.otext()?,
                            model: c.otext()?,
                        },
                    ))
                },
            )
            .await?;
        let mut children: HashMap<String, Vec<(Option<String>, InboxEngine)>> = HashMap::new();
        for (parent, child, engine) in tasks {
            children.entry(parent).or_default().push((child, engine));
        }
        for thread in &mut threads {
            // Walk the whole delegation tree under this thread (depth <= 3,
            // but guard against a cycle all the same).
            let mut stack = vec![thread.thread_id.as_str().to_string()];
            let mut visited = std::collections::HashSet::new();
            while let Some(parent) = stack.pop() {
                if !visited.insert(parent.clone()) {
                    continue;
                }
                for (child, engine) in children.get(&parent).into_iter().flatten() {
                    if !thread.engines.contains(engine) {
                        thread.engines.push(engine.clone());
                    }
                    if let Some(child) = child {
                        stack.push(child.clone());
                    }
                }
            }
        }
        Ok(threads)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ironclaw_host_api::ids::{TenantId, UserId};

    use super::*;
    use crate::model::{RuntimeId, ThreadRecord, ThreadScope, now_ms};

    fn idle(activity: Millis, ended: Option<Millis>) -> InboxFacts {
        InboxFacts {
            last_activity_at: activity,
            last_ended_at: ended,
            ..InboxFacts::default()
        }
    }

    #[test]
    fn precedence_and_wakeups_follow_t3() {
        let now = 10_000;
        // Unseen completion after the epoch is Done; before it, nothing.
        assert_eq!(
            classify(&idle(500, Some(500)), None, 100, now),
            (InboxState::Inbox, Some(InboxStatus::Done))
        );
        assert_eq!(classify(&idle(50, Some(50)), None, 100, now).1, None);
        // Working beats nothing but pins; a pin beats working.
        let running = InboxFacts {
            running: true,
            ..idle(900, None)
        };
        assert_eq!(
            classify(&running, None, 0, now),
            (InboxState::Working, Some(InboxStatus::Working))
        );
        let pinned = InboxRecord {
            pinned_at: Some(1),
            ..InboxRecord::default()
        };
        assert_eq!(
            classify(&running, Some(&pinned), 0, now).0,
            InboxState::Pinned
        );
        // Waiting on the person brings a working thread back to the inbox;
        // a stale gate on an idle thread counts for nothing.
        let asking = InboxFacts {
            awaiting_input: true,
            ..running.clone()
        };
        assert_eq!(
            classify(&asking, None, 0, now),
            (InboxState::Inbox, Some(InboxStatus::NeedsInput))
        );
        let stale = InboxFacts {
            awaiting_input: true,
            ..idle(900, None)
        };
        assert_eq!(classify(&stale, None, 0, now).1, None);
        // A snooze holds until its time, then wakes (Woke until visited).
        let snoozed = InboxRecord {
            snooze_until: Some(20_000),
            snoozed_at: Some(1_000),
            visited_at: Some(1_000),
            ..InboxRecord::default()
        };
        assert_eq!(
            classify(&idle(900, Some(900)), Some(&snoozed), 0, now).0,
            InboxState::Snoozed
        );
        assert_eq!(
            classify(&idle(900, Some(900)), Some(&snoozed), 0, 30_000),
            (InboxState::Inbox, Some(InboxStatus::Woke))
        );
        // A turn that ends after the snooze wakes it early.
        assert_eq!(
            classify(&idle(5_000, Some(5_000)), Some(&snoozed), 0, now),
            (InboxState::Inbox, Some(InboxStatus::Woke))
        );
        // Settled stays settled until new activity arrives.
        let settled = InboxRecord {
            settled_at: Some(2_000),
            visited_at: Some(2_000),
            ..InboxRecord::default()
        };
        assert_eq!(
            classify(&idle(1_500, Some(1_500)), Some(&settled), 0, now).0,
            InboxState::Settled
        );
        assert_eq!(
            classify(&idle(6_000, Some(6_000)), Some(&settled), 0, now),
            (InboxState::Inbox, Some(InboxStatus::Done))
        );
        // Recovery badges outrank Done.
        let limited = InboxFacts {
            limit_reset_at: Some(50_000),
            failed: true,
            ..idle(900, Some(900))
        };
        assert_eq!(
            classify(&limited, None, 0, now).1,
            Some(InboxStatus::Limited)
        );
        let crashed = InboxFacts {
            interrupted: true,
            ..idle(900, Some(900))
        };
        assert_eq!(
            classify(&crashed, None, 0, now).1,
            Some(InboxStatus::Interrupted)
        );
    }

    #[tokio::test]
    async fn marks_persist_per_owner_and_engines_cover_the_tree() {
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            libsql::Builder::new_local(dir.path().join("o.db"))
                .build()
                .await
                .unwrap(),
        );
        let runtime =
            Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(Arc::clone(&db)).unwrap());
        let store = Store::new(runtime);
        store
            .migrate(&RuntimeId::new("ironclaw").unwrap())
            .await
            .unwrap();
        assert!(
            store.inbox_epoch().await.unwrap() > 0,
            "epoch stamped at migrate"
        );

        let scope = ThreadScope {
            tenant_id: TenantId::new("t1").unwrap(),
            user_id: UserId::new("u1").unwrap(),
            agent_id: None,
            project_id: None,
        };
        let owner = ProfileOwner::from(&scope);
        let other = ProfileOwner {
            tenant_id: TenantId::new("t1").unwrap(),
            user_id: UserId::new("u2").unwrap(),
        };
        let parent = ThreadId::new("parent").unwrap();
        let child = ThreadId::new("child").unwrap();
        for id in [&parent, &child] {
            let record = ThreadRecord::new(
                id.clone(),
                scope.clone(),
                RuntimeId::new("ironclaw").unwrap(),
            );
            store.ensure_thread(&record).await.unwrap();
        }
        // A delegated child on a CLI runtime, and its own grandchild.
        for (id, parent_id, child_id, runtime) in [
            ("task_a", "parent", Some("child"), "claude_cli"),
            ("task_b", "child", None, "muse"),
        ] {
            store
                .exec(
                    "INSERT INTO orch_tasks (id, parent_thread, child_thread, root_thread, depth, \
                     title, role, runtime_id, provider, prompt, status, created_at) \
                     VALUES (?1, ?2, ?3, 'parent', 1, 't', 'general', ?4, 'p', 'x', 'running', 1)",
                    vec![
                        s(id),
                        s(parent_id),
                        child_id.map_or(Value::Null, s),
                        s(runtime),
                    ],
                )
                .await
                .unwrap();
        }
        let threads = store.inbox_threads(&owner).await.unwrap();
        let row = threads.iter().find(|t| t.thread_id == parent).unwrap();
        let runtimes: Vec<&str> = row.engines.iter().map(|e| e.runtime_id.as_str()).collect();
        assert_eq!(runtimes, ["ironclaw", "claude_cli", "muse"]);
        assert!(row.facts.live_children);
        let row = threads.iter().find(|t| t.thread_id == child).unwrap();
        assert_eq!(row.parent_thread_id.as_ref(), Some(&parent));
        assert!(store.inbox_threads(&other).await.unwrap().is_empty());

        // Waiting on the person: an open CLI approval, then a native gate.
        let awaiting = |threads: &[InboxThread]| {
            threads
                .iter()
                .find(|t| t.thread_id == child)
                .unwrap()
                .facts
                .awaiting_input
        };
        assert!(!awaiting(&store.inbox_threads(&owner).await.unwrap()));
        store
            .exec(
                "INSERT INTO orch_cli_approvals (gate_ref, thread_id, run_id, tool_name, requested_at) \
                 VALUES ('g1', ?1, 'r1', 'Bash', 1)",
                vec![s(child.as_str())],
            )
            .await
            .unwrap();
        assert!(awaiting(&store.inbox_threads(&owner).await.unwrap()));
        store
            .exec(
                "UPDATE orch_cli_approvals SET decision = 'allowed' WHERE gate_ref = 'g1'",
                vec![],
            )
            .await
            .unwrap();
        assert!(!awaiting(&store.inbox_threads(&owner).await.unwrap()));
        store
            .set_awaiting(&child, Some("r2"), AwaitingKind::Auth, 2)
            .await
            .unwrap();
        assert!(awaiting(&store.inbox_threads(&owner).await.unwrap()));
        store.clear_awaiting(&child).await.unwrap();
        assert!(!awaiting(&store.inbox_threads(&owner).await.unwrap()));

        let now = now_ms();
        store
            .inbox_mark(&owner, &parent, InboxAction::Pin, None, now)
            .await
            .unwrap();
        store
            .inbox_mark(
                &owner,
                &parent,
                InboxAction::Snooze,
                Some(now + 60_000),
                now,
            )
            .await
            .unwrap();
        assert!(
            store
                .inbox_mark(&owner, &parent, InboxAction::Snooze, Some(now - 1), now)
                .await
                .is_err(),
            "a snooze in the past is refused"
        );
        let records = store.inbox_records(&owner).await.unwrap();
        let rec = &records["parent"];
        assert_eq!(rec.pinned_at, Some(now), "snoozing keeps the pin");
        assert_eq!(rec.snooze_until, Some(now + 60_000));
        store
            .inbox_mark(&owner, &parent, InboxAction::Settle, None, now + 1)
            .await
            .unwrap();
        let rec = &store.inbox_records(&owner).await.unwrap()["parent"];
        assert_eq!(
            (rec.pinned_at, rec.snooze_until, rec.settled_at),
            (None, None, Some(now + 1))
        );
        assert!(store.inbox_records(&other).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn goals_set_change_status_and_clear() {
        use ironclaw_product_contracts::inbox::GoalStatus;
        let dir = tempfile::tempdir().unwrap();
        let db = Arc::new(
            libsql::Builder::new_local(dir.path().join("o.db"))
                .build()
                .await
                .unwrap(),
        );
        let runtime =
            Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(Arc::clone(&db)).unwrap());
        let store = Store::new(runtime);
        store
            .migrate(&RuntimeId::new("ironclaw").unwrap())
            .await
            .unwrap();
        let t = ThreadId::new("t").unwrap();
        assert_eq!(store.goal(&t).await.unwrap(), None);
        let goal = store
            .set_goal(&t, Some("  ship the inbox "), None, 5)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            (goal.objective.as_str(), goal.status),
            ("ship the inbox", GoalStatus::Active)
        );
        let goal = store
            .set_goal(&t, None, Some(GoalStatus::Done), 6)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(goal.status, GoalStatus::Done);
        assert!(store.set_goal(&t, Some(" "), None, 7).await.is_err());
        assert_eq!(store.set_goal(&t, None, None, 8).await.unwrap(), None);
        assert!(
            store
                .set_goal(&t, None, Some(GoalStatus::Paused), 9)
                .await
                .is_err(),
            "a cleared goal has no status to change"
        );
    }
}
