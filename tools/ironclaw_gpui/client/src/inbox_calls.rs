//! The inbox and thread goals (v2 stage 5). The backend joins and classifies
//! the rows, so the sidebar renders one response. A gateway without the
//! inbox route (older build) answers 404; [`Client::inbox`] then falls back
//! to the plain thread list, every row in the main list.

use ironclaw_product_contracts::inbox::{
    GoalCheckAction, GoalCheckSpec, GoalStatus, InboxAction, InboxEngine, InboxListResponse,
    InboxRow, InboxState, ThreadGoalResponse,
};
use ironclaw_product_contracts::profiles::DEFAULT_PROFILE_ID;
use ironclaw_product_contracts::session_import::{
    ImportTrigger, SessionImportRequest, SessionImportStatus,
};
use ironclaw_product_contracts::task_models::TaskModels;
use serde_json::json;

use crate::api::{Client, V2, seg};
use crate::error::ClientError;
use crate::profile_calls::document;

/// One page of the old list for the fallback.
const FALLBACK_THREADS: u32 = 100;

impl Client {
    pub async fn inbox(&self) -> Result<InboxListResponse, ClientError> {
        match self.get(&format!("{V2}/inbox")).await {
            Ok(value) => document(value),
            Err(e) if e.is_not_found() => self.inbox_from_thread_list().await,
            Err(e) => Err(e),
        }
    }

    async fn inbox_from_thread_list(&self) -> Result<InboxListResponse, ClientError> {
        let page = self.list_threads(FALLBACK_THREADS, None).await?;
        let rows = page
            .threads
            .into_iter()
            .map(|t| InboxRow {
                last_activity_at: t
                    .updated_at
                    .or(t.created_at)
                    .map_or(0, |at| at.timestamp_millis()),
                title: t.title,
                thread_id: t.thread_id,
                profile_id: DEFAULT_PROFILE_ID.into(),
                state: InboxState::Inbox,
                status: None,
                engines: vec![InboxEngine {
                    runtime_id: "ironclaw".into(),
                    provider: None,
                    model: None,
                }],
                snooze_until: None,
                settled_at: None,
                limit_reset_at: None,
                working_since: None,
                parent_thread_id: None,
                automation: false,
                title_pending: false,
                imported: None,
            })
            .collect();
        Ok(InboxListResponse {
            rows,
            ..InboxListResponse::default()
        })
    }

    /// Pin, snooze (`until` unix ms), settle, visit… Answers with the list.
    pub async fn inbox_mutate(
        &self,
        thread_id: &str,
        action: InboxAction,
        until: Option<i64>,
    ) -> Result<InboxListResponse, ClientError> {
        let body = json!({ "action": action, "until": until });
        let path = format!("{V2}/inbox/{}", seg(thread_id));
        document(self.post(&path, &body).await?)
    }

    /// Terminal session import: settings, last run, and whether one runs.
    pub async fn session_import_status(&self) -> Result<SessionImportStatus, ClientError> {
        document(self.get(&format!("{V2}/session-imports")).await?)
    }

    /// Change import settings and/or start a run (`run`). Answers the
    /// status the backend stored; `running` is set while a run goes on.
    pub async fn session_import(
        &self,
        request: &SessionImportRequest,
    ) -> Result<SessionImportStatus, ClientError> {
        let body = serde_json::to_value(request).map_err(|e| ClientError::Decode(e.to_string()))?;
        document(self.post(&format!("{V2}/session-imports"), &body).await?)
    }

    /// Start an import run for `trigger` (the backend skips it when not due).
    /// An older gateway without the route answers 404: nothing to do.
    pub async fn session_import_tick(&self, trigger: ImportTrigger) -> Result<(), ClientError> {
        let request = SessionImportRequest {
            run: Some(trigger),
            ..SessionImportRequest::default()
        };
        match self.session_import(&request).await {
            Ok(_) => Ok(()),
            Err(e) if e.is_not_found() => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub async fn task_models(&self) -> Result<TaskModels, ClientError> {
        document(self.get(&format!("{V2}/llm/task-models")).await?)
    }

    pub async fn set_task_models(&self, models: &TaskModels) -> Result<TaskModels, ClientError> {
        let body = serde_json::to_value(models).map_err(|e| ClientError::Decode(e.to_string()))?;
        document(self.post(&format!("{V2}/llm/task-models"), &body).await?)
    }

    pub async fn thread_goal(&self, thread_id: &str) -> Result<ThreadGoalResponse, ClientError> {
        document(
            self.get(&format!("{V2}/threads/{}/goal", seg(thread_id)))
                .await?,
        )
    }

    /// `objective: None` with `status: None` clears the goal.
    pub async fn set_thread_goal(
        &self,
        thread_id: &str,
        objective: Option<&str>,
        status: Option<GoalStatus>,
    ) -> Result<ThreadGoalResponse, ClientError> {
        let body = json!({ "objective": objective, "status": status });
        let path = format!("{V2}/threads/{}/goal", seg(thread_id));
        document(self.post(&path, &body).await?)
    }

    /// Set the goal and its check in one write (`/goal <text> --check`).
    /// `objective: None` keeps the current goal and only sets the check.
    pub async fn set_thread_goal_check(
        &self,
        thread_id: &str,
        objective: Option<&str>,
        check: Option<&GoalCheckSpec>,
    ) -> Result<ThreadGoalResponse, ClientError> {
        let body = match check {
            Some(check) => json!({ "objective": objective, "check": check }),
            None => json!({ "objective": objective, "clear_check": true }),
        };
        let path = format!("{V2}/threads/{}/goal", seg(thread_id));
        document(self.post(&path, &body).await?)
    }

    /// "Run check now" / "Stop retrying" on the goal strip. `revision` is
    /// the check the person saw; "Run check now" approves only that one.
    pub async fn goal_check_action(
        &self,
        thread_id: &str,
        action: GoalCheckAction,
        revision: u64,
    ) -> Result<ThreadGoalResponse, ClientError> {
        let body = json!({ "check_action": action, "check_revision": revision });
        let path = format!("{V2}/threads/{}/goal", seg(thread_id));
        document(self.post(&path, &body).await?)
    }
}
