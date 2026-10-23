//! `t3_schedule`: scheduled prompts that post into an orchestrated thread,
//! even when no turn is active. A fire is a durable queue item authored by
//! `schedule`, so it waits while the thread is busy or held by a user Stop
//! and drains on the same tick when the thread is idle.
//!
//! The engine keeps the schedule rows (`orch_schedules`) and fires due rows
//! on its existing tick. *When* a schedule fires is not decided here: the
//! host plugs a [`SchedulePlanner`] in (the product tier backs it with the
//! routines engine's cron/timezone grammar in `ironclaw_triggers`), so there
//! is exactly one schedule grammar in the deployment.
//!
//! Fires are at-most-once: the row's `next_run_at` advances with a
//! conditional update before the queue insert, so two engines (or a retry
//! after a crash between the two writes) never post the same slot twice.

use ironclaw_host_api::ids::{TenantId, ThreadId, UserId};
use libsql::Value as Sql;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::engine::{CallContext, Engine, Result};
use crate::error::OrchestratorError;
use crate::model::*;
use crate::store::{Cols, s};

pub(crate) const SCHEDULES_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS orch_schedules (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  thread_id TEXT NOT NULL,
  title TEXT NOT NULL,
  prompt TEXT NOT NULL,
  spec TEXT NOT NULL,
  enabled INTEGER NOT NULL DEFAULT 1,
  next_run_at INTEGER,
  last_run_at INTEGER,
  run_count INTEGER NOT NULL DEFAULT 0,
  last_error TEXT,
  client_request_id TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS orch_schedules_due ON orch_schedules(enabled, next_run_at);
CREATE INDEX IF NOT EXISTS orch_schedules_owner ON orch_schedules(tenant_id, user_id);
CREATE UNIQUE INDEX IF NOT EXISTS orch_schedules_request ON orch_schedules(thread_id, client_request_id)
  WHERE client_request_id IS NOT NULL;
"#;

const MAX_PROMPT_BYTES: usize = 32 * 1024;
const MAX_TITLE_CHARS: usize = 120;
/// Due rows fired per tick; the rest wait for the next tick.
const FIRES_PER_TICK: u32 = 32;

/// Schedule grammar the host provides. Specs are the T3 trigger objects
/// (`{type:'interval', everyMs}`, `{type:'fixed_time', timeOfDay, weekdays}`).
pub trait SchedulePlanner: Send + Sync {
    /// Validate a schedule object and return its canonical form.
    fn normalize(&self, raw: &Value) -> Result<Value>;
    /// First fire strictly after `now`. `last_slot` is the slot that just
    /// fired (interval schedules stay on their grid); `None` = never again.
    fn next_after(
        &self,
        spec: &Value,
        last_slot: Option<Millis>,
        now: Millis,
    ) -> Result<Option<Millis>>;
    /// Human label, e.g. "every 2 hours" or "weekdays at 09:00".
    fn describe(&self, spec: &Value) -> String;
    /// Fires run with nobody watching: reject prompts the unattended-prompt
    /// safety scan refuses, at create/update time.
    fn check_prompt(&self, prompt: &str) -> Result<()>;
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScheduleRecord {
    pub id: ScheduleId,
    pub tenant_id: TenantId,
    pub user_id: UserId,
    pub thread_id: ThreadId,
    pub title: String,
    pub prompt: String,
    pub spec: Value,
    pub enabled: bool,
    pub next_run_at: Option<Millis>,
    pub last_run_at: Option<Millis>,
    pub run_count: i64,
    pub last_error: Option<String>,
    pub created_at: Millis,
}

const COLS: &str = "id, tenant_id, user_id, thread_id, title, prompt, spec, enabled, next_run_at, \
                    last_run_at, run_count, last_error, created_at";

fn schedule_row(row: &libsql::Row) -> Result<ScheduleRecord> {
    let mut c = Cols::new(row);
    Ok(ScheduleRecord {
        id: ScheduleId::new(c.text()?)?,
        tenant_id: TenantId::new(c.text()?)?,
        user_id: UserId::new(c.text()?)?,
        thread_id: ThreadId::new(c.text()?)?,
        title: c.text()?,
        prompt: c.text()?,
        spec: serde_json::from_str(&c.text()?)?,
        enabled: c.int()? != 0,
        next_run_at: c.oint()?,
        last_run_at: c.oint()?,
        run_count: c.int()?,
        last_error: c.otext()?,
        created_at: c.int()?,
    })
}

fn opt_int(v: Option<Millis>) -> Sql {
    v.map_or(Sql::Null, Sql::Integer)
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct ScheduleArgs {
    action: String,
    id: Option<String>,
    prompt: Option<String>,
    title: Option<String>,
    schedule: Option<Value>,
    bind_to_current_thread: Option<bool>,
    enabled: Option<bool>,
    signature: Option<Value>,
    client_request_id: Option<String>,
}

impl Engine {
    fn planner(&self) -> Result<std::sync::Arc<dyn SchedulePlanner>> {
        self.schedule_planner.get().cloned().ok_or_else(|| {
            OrchestratorError::request(
                "schedules_unavailable",
                "Schedules are not available in this backend.",
            )
        })
    }

    pub(crate) async fn schedule_tool(&self, ctx: &CallContext, a: ScheduleArgs) -> Result<Value> {
        match a.action.as_str() {
            "list" => self.schedule_list(ctx).await,
            "create" => self.schedule_create(ctx, a).await,
            "update" | "delete" | "run_now" => {
                let id =
                    a.id.as_deref()
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                        .ok_or_else(|| OrchestratorError::invalid("id is required"))?;
                let record = self.owned_schedule(ctx, &ScheduleId::new(id)?).await?;
                match a.action.as_str() {
                    "update" => self.schedule_update(record, a).await,
                    "delete" => self.schedule_delete(record).await,
                    _ => self.schedule_run_now(record).await,
                }
            }
            _ => Err(OrchestratorError::invalid(
                "action must be create, list, update, delete or run_now",
            )),
        }
    }

    async fn owned_schedule(&self, ctx: &CallContext, id: &ScheduleId) -> Result<ScheduleRecord> {
        match self.schedule(id).await? {
            Some(r) if r.tenant_id == ctx.scope.tenant_id && r.user_id == ctx.scope.user_id => {
                Ok(r)
            }
            _ => Err(OrchestratorError::not_found(
                "schedule_not_found",
                format!("No schedule {id} for this user."),
            )),
        }
    }

    pub async fn schedule(&self, id: &ScheduleId) -> Result<Option<ScheduleRecord>> {
        self.store
            .row(
                &format!("SELECT {COLS} FROM orch_schedules WHERE id = ?1"),
                vec![s(id.as_str())],
                schedule_row,
            )
            .await
    }

    fn schedule_view(&self, r: &ScheduleRecord) -> Value {
        let describe = self
            .schedule_planner
            .get()
            .map(|p| p.describe(&r.spec))
            .unwrap_or_default();
        json!({
            "id": r.id.as_str(), "title": r.title, "prompt": r.prompt, "schedule": r.spec,
            "describe": describe, "enabled": r.enabled, "threadId": r.thread_id.as_str(),
            "nextRunAt": r.next_run_at, "lastRunAt": r.last_run_at, "runCount": r.run_count,
            "lastError": r.last_error, "createdAt": r.created_at,
        })
    }

    /// Read the row back so the tool reports the stored definition.
    async fn schedule_readback(&self, id: &ScheduleId) -> Result<Value> {
        let record = self
            .schedule(id)
            .await?
            .ok_or_else(|| OrchestratorError::Store("schedule row vanished after write".into()))?;
        Ok(json!({"ok": true, "schedule": self.schedule_view(&record)}))
    }

    async fn schedule_list(&self, ctx: &CallContext) -> Result<Value> {
        let rows = self
            .store
            .rows(
                &format!(
                    "SELECT {COLS} FROM orch_schedules WHERE tenant_id = ?1 AND user_id = ?2 \
                     ORDER BY created_at"
                ),
                vec![
                    s(ctx.scope.tenant_id.as_str()),
                    s(ctx.scope.user_id.as_str()),
                ],
                schedule_row,
            )
            .await?;
        let views: Vec<Value> = rows.iter().map(|r| self.schedule_view(r)).collect();
        Ok(json!({"ok": true, "schedules": views}))
    }

    fn check_schedule_text(planner: &dyn SchedulePlanner, prompt: &str) -> Result<String> {
        let prompt = prompt.trim();
        if prompt.is_empty() || prompt.len() > MAX_PROMPT_BYTES {
            return Err(OrchestratorError::invalid(
                "prompt is required (at most 32 KiB)",
            ));
        }
        planner.check_prompt(prompt)?;
        Ok(prompt.to_string())
    }

    async fn schedule_create(&self, ctx: &CallContext, a: ScheduleArgs) -> Result<Value> {
        let planner = self.planner()?;
        if a.bind_to_current_thread == Some(false) {
            return Err(OrchestratorError::request(
                "unsupported",
                "Schedules post into this thread; bind_to_current_thread=false is not supported yet.",
            ));
        }
        if a.signature.as_ref().is_some_and(|v| !v.is_null()) {
            return Err(OrchestratorError::request(
                "unsupported",
                "Webhook signatures are not supported: webhook schedules are not available yet.",
            ));
        }
        let prompt = Self::check_schedule_text(&*planner, a.prompt.as_deref().unwrap_or(""))?;
        let raw = a
            .schedule
            .as_ref()
            .ok_or_else(|| OrchestratorError::invalid("schedule is required"))?;
        let spec = planner.normalize(raw)?;
        let request = a
            .client_request_id
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty());
        if let Some(request) = request
            && let Some(existing) = self
                .store
                .row(
                    "SELECT id FROM orch_schedules WHERE thread_id = ?1 AND client_request_id = ?2",
                    vec![s(ctx.thread_id.as_str()), s(request)],
                    |r| Cols::new(r).text(),
                )
                .await?
        {
            return self.schedule_readback(&ScheduleId::new(existing)?).await;
        }
        let now = now_ms();
        let next = planner.next_after(&spec, None, now)?;
        let title = a
            .title
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(|v| v.chars().take(MAX_TITLE_CHARS).collect::<String>())
            .unwrap_or_else(|| prompt.chars().take(60).collect());
        let id = ScheduleId::generate();
        self.store
            .exec(
                "INSERT INTO orch_schedules (id, tenant_id, user_id, thread_id, title, prompt, spec, \
                 enabled, next_run_at, client_request_id, created_at, updated_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8, ?9, ?10, ?10)",
                vec![
                    s(id.as_str()),
                    s(ctx.scope.tenant_id.as_str()),
                    s(ctx.scope.user_id.as_str()),
                    s(ctx.thread_id.as_str()),
                    s(&title),
                    s(&prompt),
                    s(serde_json::to_string(&spec)?),
                    opt_int(next),
                    request.map_or(Sql::Null, s),
                    now.into(),
                ],
            )
            .await?;
        self.schedule_readback(&id).await
    }

    async fn schedule_update(&self, r: ScheduleRecord, a: ScheduleArgs) -> Result<Value> {
        let planner = self.planner()?;
        let prompt = match a.prompt.as_deref() {
            Some(p) => Self::check_schedule_text(&*planner, p)?,
            None => r.prompt.clone(),
        };
        let spec = match &a.schedule {
            Some(raw) => planner.normalize(raw)?,
            None => r.spec.clone(),
        };
        let enabled = a.enabled.unwrap_or(r.enabled);
        let title = a
            .title
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(|v| v.chars().take(MAX_TITLE_CHARS).collect::<String>())
            .unwrap_or_else(|| r.title.clone());
        // A new grammar or a re-enable starts from now; otherwise keep the slot.
        let next = if a.schedule.is_some() || (enabled && !r.enabled) {
            planner.next_after(&spec, None, now_ms())?
        } else {
            r.next_run_at
        };
        self.store
            .exec(
                "UPDATE orch_schedules SET title = ?2, prompt = ?3, spec = ?4, enabled = ?5, \
                 next_run_at = ?6, updated_at = ?7 WHERE id = ?1",
                vec![
                    s(r.id.as_str()),
                    s(&title),
                    s(&prompt),
                    s(serde_json::to_string(&spec)?),
                    i64::from(enabled).into(),
                    opt_int(next),
                    now_ms().into(),
                ],
            )
            .await?;
        self.schedule_readback(&r.id).await
    }

    async fn schedule_delete(&self, r: ScheduleRecord) -> Result<Value> {
        self.store
            .exec(
                "DELETE FROM orch_schedules WHERE id = ?1",
                vec![s(r.id.as_str())],
            )
            .await?;
        Ok(json!({"ok": true, "deleted": r.id.as_str()}))
    }

    async fn schedule_run_now(&self, r: ScheduleRecord) -> Result<Value> {
        let queued = self.post_schedule(&r, now_ms()).await?;
        let mut view = self.schedule_readback(&r.id).await?;
        view["queuedId"] = json!(queued.as_str());
        Ok(view)
    }

    async fn post_schedule(&self, r: &ScheduleRecord, now: Millis) -> Result<QueueItemId> {
        let text = format!("[Scheduled: {}]\n\n{}", r.title, r.prompt);
        let id = self
            .store
            .queue_add(&r.thread_id, &text, Author::Schedule)
            .await?;
        self.store
            .exec(
                "UPDATE orch_schedules SET last_run_at = ?2, run_count = run_count + 1, \
                 last_error = NULL WHERE id = ?1",
                vec![s(r.id.as_str()), now.into()],
            )
            .await?;
        Ok(id)
    }

    /// Tick step: post every due schedule into its thread's queue. Missed
    /// slots (backend down) collapse into one fire.
    pub(crate) async fn fire_schedules(&self) -> Result<()> {
        let Some(planner) = self.schedule_planner.get().cloned() else {
            return Ok(());
        };
        let now = now_ms();
        let due = self
            .store
            .rows(
                &format!(
                    "SELECT {COLS} FROM orch_schedules WHERE enabled = 1 AND next_run_at IS NOT NULL \
                     AND next_run_at <= ?1 ORDER BY next_run_at LIMIT {FIRES_PER_TICK}"
                ),
                vec![now.into()],
                schedule_row,
            )
            .await?;
        for r in due {
            let (next, error) = match planner.next_after(&r.spec, r.next_run_at, now) {
                Ok(next) => (next, None),
                Err(e) => (None, Some(e.to_string())),
            };
            // Claim the slot; a lost race means another writer fired it.
            let claimed = self
                .store
                .exec(
                    "UPDATE orch_schedules SET next_run_at = ?3, last_error = ?4, updated_at = ?5 \
                     WHERE id = ?1 AND next_run_at = ?2",
                    vec![
                        s(r.id.as_str()),
                        opt_int(r.next_run_at),
                        opt_int(next),
                        error.clone().map_or(Sql::Null, s),
                        now.into(),
                    ],
                )
                .await?;
            if claimed == 0 || error.is_some() {
                continue;
            }
            if self.store.thread(&r.thread_id).await?.is_none() {
                self.store
                    .exec(
                        "UPDATE orch_schedules SET enabled = 0, last_error = 'thread_not_found' \
                         WHERE id = ?1",
                        vec![s(r.id.as_str())],
                    )
                    .await?;
                continue;
            }
            self.post_schedule(&r, now).await?;
        }
        Ok(())
    }
}
