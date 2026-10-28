//! The inbox and thread goals (v2 stage 5). The backend joins and classifies
//! the rows, so the sidebar renders one response. A gateway without the
//! inbox route (older build) answers 404; [`Client::inbox`] then falls back
//! to the plain thread list, every row in the main list.

use ironclaw_product_contracts::inbox::{
    GoalStatus, InboxAction, InboxEngine, InboxListResponse, InboxRow, InboxState,
    ThreadGoalResponse,
};
use ironclaw_product_contracts::profiles::DEFAULT_PROFILE_ID;
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
}
