//! IronClaw's own agent loop as an orchestrator runtime.
//!
//! Every operation goes through the *inner* product surface (the one the
//! orchestrator decorator wraps), acting as the thread's owner: automatic
//! turns use the same admission, idempotency and steering path a typed
//! message does, but are not mistaken for user activity.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_host_api::ids::ActivityId;
use ironclaw_orchestrator::model::{RuntimeId, ThreadRecord, ThreadScope};
use ironclaw_orchestrator::runtime::*;
use ironclaw_product_contracts::inbound_requests::{
    ProductCancelRunRequest, ProductCreateThreadRequest, ProductSubmitTurnRequest,
};
use ironclaw_product_contracts::operator_llm::{USER_MODEL_CATALOG_VIEW, UserModelCatalog};
use ironclaw_product_contracts::product_wire::{RebornSubmitTurnResponse, RebornTimelineRequest};
use ironclaw_product_contracts::surface::{
    BoundProductSurface, ProductSurface, ProductSurfaceCaller, ProductSurfaceError,
    ProductSurfaceQueryRequest,
};
use ironclaw_threads::MessageKind;

use crate::{CANCEL_RUN_COMMAND, CREATE_THREAD_COMMAND, SUBMIT_TURN_COMMAND, TIMELINE_VIEW};

/// Runtime id of IronClaw's agent loop in orchestrator rows.
pub const IRONCLAW_RUNTIME_ID: &str = "ironclaw";
const TIMELINE_PAGE: u32 = 200;

/// Scaffolding the engine sends on its own; runs and handoffs skip it.
const ENGINE_PREFIXES: [&str; 4] = [
    "Context handoff (",
    "Merged context from fork",
    "Delegated work finished.",
    ironclaw_orchestrator::restart::CONTINUE_PROMPT,
];

pub struct AgentLoopRuntime {
    id: RuntimeId,
    surface: Arc<dyn ProductSurface>,
    /// Session channel the turns enter through (`None` = legacy session surface).
    session_extension_id: Option<String>,
}

impl AgentLoopRuntime {
    pub fn new(
        surface: Arc<dyn ProductSurface>,
        session_extension_id: Option<String>,
    ) -> Result<Self, ironclaw_orchestrator::error::OrchestratorError> {
        Ok(Self {
            id: RuntimeId::new(IRONCLAW_RUNTIME_ID)?,
            surface,
            session_extension_id,
        })
    }

    fn bound(&self, scope: &ThreadScope) -> BoundProductSurface {
        BoundProductSurface::new(
            Arc::clone(&self.surface),
            ProductSurfaceCaller::new(
                scope.tenant_id.clone(),
                scope.user_id.clone(),
                scope.agent_id.clone(),
                scope.project_id.clone(),
            ),
        )
    }

    async fn submit(
        &self,
        thread: &ThreadRecord,
        text: &str,
        model: Option<String>,
    ) -> Result<RebornSubmitTurnResponse, RuntimeError> {
        SUBMIT_TURN_COMMAND
            .invoke_on(
                &self.bound(&thread.scope),
                ProductSubmitTurnRequest {
                    extension_id: self.session_extension_id.clone(),
                    client_action_id: Some(uuid::Uuid::new_v4().to_string()),
                    thread_id: Some(thread.thread_id.as_str().to_string()),
                    content: Some(text.to_string()),
                    attachments: Vec::new(),
                    model,
                },
                ActivityId::new(),
            )
            .await
            .map_err(surface_error)
    }
}

fn surface_error(error: ProductSurfaceError) -> RuntimeError {
    match error.status_code {
        404 => RuntimeError::ThreadNotFound,
        400..=499 => RuntimeError::Rejected(format!("{:?}", error.kind).to_lowercase()),
        _ => RuntimeError::Unavailable(format!("{:?}", error.code).to_lowercase()),
    }
}

#[async_trait]
impl TurnRuntime for AgentLoopRuntime {
    fn id(&self) -> RuntimeId {
        self.id.clone()
    }

    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities {
            native_fork: false,
            // A submit to a busy thread is folded into the running turn.
            steer_in_turn: true,
            reports_usage: false,
            // An expired turn lease surfaces as `RecoveryRequired` in the journal.
            reports_restart_cuts: true,
        }
    }

    async fn create_thread(&self, request: CreateThread) -> Result<CreatedThread, RuntimeError> {
        let response = CREATE_THREAD_COMMAND
            .invoke_on(
                &self.bound(&request.scope),
                ProductCreateThreadRequest {
                    client_action_id: Some(request.idempotency_key),
                    requested_thread_id: None,
                    project_id: request
                        .scope
                        .project_id
                        .as_ref()
                        .map(|p| p.as_str().to_string()),
                },
                ActivityId::new(),
            )
            .await
            .map_err(surface_error)?;
        Ok(CreatedThread {
            thread_id: response.thread.thread_id,
            native_session_id: None,
            provider: request.provider,
            model: request.model,
        })
    }

    async fn start_turn(
        &self,
        thread: &ThreadRecord,
        input: TurnInput,
    ) -> Result<TurnAdmission, RuntimeError> {
        Ok(match self.submit(thread, &input.text, input.model).await? {
            RebornSubmitTurnResponse::Submitted { run_id, .. }
            | RebornSubmitTurnResponse::AlreadySubmitted { run_id, .. } => TurnAdmission::Started {
                run_id: Some(run_id.to_string()),
            },
            RebornSubmitTurnResponse::DeferredBusy { active_run_id, .. } => {
                TurnAdmission::Steered {
                    run_id: Some(active_run_id.to_string()),
                }
            }
            RebornSubmitTurnResponse::RejectedBusy { .. } => TurnAdmission::Busy,
        })
    }

    async fn steer(
        &self,
        thread: &ThreadRecord,
        text: &str,
        how: SteerHow,
    ) -> Result<SteerOutcome, RuntimeError> {
        if how == SteerHow::Restart {
            self.cancel(thread, "redirected").await?;
        }
        Ok(match self.submit(thread, text, None).await? {
            RebornSubmitTurnResponse::DeferredBusy { .. } => SteerOutcome::Steered,
            RebornSubmitTurnResponse::RejectedBusy { .. } => SteerOutcome::Rejected,
            _ if how == SteerHow::Restart => SteerOutcome::Restarted,
            // The turn ended meanwhile; the text started a new one.
            _ => SteerOutcome::Restarted,
        })
    }

    async fn cancel(&self, thread: &ThreadRecord, reason: &str) -> Result<(), RuntimeError> {
        let Some(run_id) = thread.active_run_id.clone() else {
            return Ok(());
        };
        CANCEL_RUN_COMMAND
            .invoke_on(
                &self.bound(&thread.scope),
                ProductCancelRunRequest {
                    client_action_id: Some(uuid::Uuid::new_v4().to_string()),
                    thread_id: Some(thread.thread_id.as_str().to_string()),
                    run_id: Some(run_id),
                    reason: Some(reason.to_string()),
                },
                ActivityId::new(),
            )
            .await
            .map(|_| ())
            .map_err(surface_error)
    }

    async fn history(
        &self,
        thread: &ThreadRecord,
        limit: usize,
    ) -> Result<Vec<HistoryMessage>, RuntimeError> {
        let bound = self.bound(&thread.scope);
        let mut pages = Vec::new();
        let mut cursor = None;
        let mut total = 0usize;
        loop {
            let page = TIMELINE_VIEW
                .query_on(
                    &bound,
                    RebornTimelineRequest {
                        thread_id: thread.thread_id.as_str().to_string(),
                        limit: Some(TIMELINE_PAGE),
                        cursor: None,
                    },
                    cursor.take(),
                )
                .await
                .map_err(surface_error)?;
            total += page.messages.len();
            let next = page.next_cursor.clone();
            pages.push(page.messages);
            match next {
                Some(c) if total < limit => cursor = Some(c),
                _ => break,
            }
        }
        // Pages arrive newest first; each page is oldest-first inside.
        Ok(pages
            .into_iter()
            .rev()
            .flatten()
            .filter_map(|m| {
                let text = m.content.clone().unwrap_or_default();
                let role = match m.kind {
                    MessageKind::User => HistoryRole::User,
                    MessageKind::Assistant => HistoryRole::Assistant,
                    MessageKind::System | MessageKind::Summary => HistoryRole::System,
                    MessageKind::ToolResultReference | MessageKind::CapabilityDisplayPreview => {
                        HistoryRole::Tool
                    }
                    MessageKind::CheckpointReference => return None,
                };
                let hidden = role == HistoryRole::User
                    && ENGINE_PREFIXES.iter().any(|p| text.starts_with(p));
                Some(HistoryMessage {
                    role,
                    text,
                    tool_calls: Vec::new(),
                    run_id: m.turn_run_id.clone(),
                    hidden,
                })
            })
            .collect())
    }

    async fn catalog(&self, scope: &ThreadScope) -> Result<Vec<CatalogProvider>, RuntimeError> {
        let page = self
            .bound(scope)
            .query(ProductSurfaceQueryRequest {
                view_id: USER_MODEL_CATALOG_VIEW.id.to_string(),
                input: serde_json::json!({}),
                cursor: None,
                limit: None,
            })
            .await
            .map_err(surface_error)?;
        let catalog: UserModelCatalog = page
            .items
            .into_iter()
            .next()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|e| RuntimeError::Unavailable(format!("model catalog: {e}")))?
            .unwrap_or_else(UserModelCatalog::disabled);
        Ok(vec![CatalogProvider {
            provider: IRONCLAW_RUNTIME_ID.to_string(),
            name: "IronClaw".to_string(),
            authenticated: true,
            current: true,
            models: catalog.models,
        }])
    }
}
