//! `ProductSurface` decorator that routes threads through the orchestrator.
//!
//! - `thread.create` registers the new thread with the engine;
//! - `turn.submit` (a typed message) lifts the Stop hold, stamps user
//!   activity (which supersedes pending auto-resumes) and carries any pending
//!   next-message context;
//! - `run.cancel` (the Stop button) holds the thread and stops every
//!   delegated task beneath it before the run is cancelled;
//! - the `orchestrator.*` views and commands are answered here.
//!
//! Everything else passes through untouched, so existing route shapes keep
//! working.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_host_api::ids::ThreadId;
use ironclaw_orchestrator::engine::{CallContext, Engine};
use ironclaw_orchestrator::error::OrchestratorError;
use ironclaw_orchestrator::model::{Author, QueueItemId, TaskId, ThreadRecord, ThreadScope};
use ironclaw_product_contracts::inbound_requests::{
    ProductCancelRunRequest, ProductSubmitTurnRequest,
};
use ironclaw_product_contracts::orchestration::*;
use ironclaw_product_contracts::surface::{
    ProductSurface, ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceInvokeRequest,
    ProductSurfaceInvokeResponse, ProductSurfaceQueryPage, ProductSurfaceQueryRequest,
    ProductSurfaceStreamRequest, ProductSurfaceStreamResponse, ProductSurfaceValidationCode,
};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::reborn_services::{
    CANCEL_RUN_COMMAND_ID, CREATE_THREAD_COMMAND_ID, SUBMIT_TURN_COMMAND_ID,
};

pub struct OrchestratedProductSurface {
    inner: Arc<dyn ProductSurface>,
    engine: Arc<Engine>,
}

impl OrchestratedProductSurface {
    pub fn new(inner: Arc<dyn ProductSurface>, engine: Arc<Engine>) -> Self {
        Self { inner, engine }
    }
}

pub(crate) fn scope_of(caller: &ProductSurfaceCaller) -> ThreadScope {
    ThreadScope {
        tenant_id: caller.tenant_id.clone(),
        user_id: caller.user_id.clone(),
        agent_id: caller.agent_id.clone(),
        project_id: caller.project_id.clone(),
    }
}

fn decode<T: DeserializeOwned>(input: Value) -> Result<T, ProductSurfaceError> {
    serde_json::from_value(input).map_err(|error| {
        tracing::debug!(%error, "orchestrator request rejected");
        ProductSurfaceError::validation("body", ProductSurfaceValidationCode::InvalidValue)
    })
}

/// Correctable failures are answered as the T3 error document; ownership
/// misses are 404; storage faults stay sanitized 5xx.
fn answer(result: Result<Value, OrchestratorError>) -> Result<Value, ProductSurfaceError> {
    match result {
        Ok(value) => Ok(value),
        Err(error) if error.code().ends_with("not_found") => Err(ProductSurfaceError::not_found()),
        Err(error) if error.is_correctable() => Ok(error.to_tool_json()),
        Err(error) => Err(ProductSurfaceError::internal_from(error)),
    }
}

fn page(value: Value) -> ProductSurfaceQueryPage {
    ProductSurfaceQueryPage {
        items: vec![value],
        next_cursor: None,
    }
}

/// Orchestrator bookkeeping must never break the user's turn: log and go on.
fn soft(step: &str, result: Result<impl Sized, OrchestratorError>) {
    if let Err(error) = result {
        tracing::warn!(step, %error, "orchestrator bookkeeping failed");
    }
}

impl OrchestratedProductSurface {
    async fn owned(
        &self,
        raw: &str,
        scope: &ThreadScope,
    ) -> Result<ThreadRecord, OrchestratorError> {
        let id = ThreadId::new(raw)?;
        self.engine.owned_thread(&id, scope).await
    }

    fn ctx(thread: &ThreadRecord, scope: &ThreadScope) -> CallContext {
        CallContext {
            thread_id: thread.thread_id.clone(),
            scope: scope.clone(),
            provider: thread.provider.clone(),
            model: thread.model.clone(),
        }
    }

    /// The task, when its parent thread belongs to the caller.
    async fn owned_task(
        &self,
        raw: &str,
        scope: &ThreadScope,
    ) -> Result<(TaskId, ThreadRecord), OrchestratorError> {
        let id = TaskId::new(raw)?;
        let task = self.engine.store().task(&id).await?.ok_or_else(|| {
            OrchestratorError::not_found("task_not_found", format!("No task {id}."))
        })?;
        let parent = self.engine.owned_thread(&task.parent_thread, scope).await?;
        Ok((id, parent))
    }

    async fn orchestrator_view(
        &self,
        view: &str,
        input: Value,
        scope: &ThreadScope,
    ) -> Result<Option<Value>, ProductSurfaceError> {
        let engine = &self.engine;
        let result = match view {
            v if v == ORCHESTRATOR_TASK_VIEW.id => {
                let req: OrchestratorTaskRequest = decode(input)?;
                answer(
                    async {
                        let (id, parent) = self.owned_task(&req.task_id, scope).await?;
                        engine.task_status(&Self::ctx(&parent, scope), &id).await
                    }
                    .await,
                )
            }
            v if v == ORCHESTRATOR_THREAD_TASKS_VIEW.id
                || v == ORCHESTRATOR_THREAD_QUEUE_VIEW.id
                || v == ORCHESTRATOR_THREAD_LINEAGE_VIEW.id
                || v == ORCHESTRATOR_THREAD_RUNS_VIEW.id =>
            {
                let req: OrchestratorThreadRequest = decode(input)?;
                answer(
                    async {
                        let thread = self.owned(&req.thread_id, scope).await?;
                        let id = &thread.thread_id;
                        match view {
                            v if v == ORCHESTRATOR_THREAD_TASKS_VIEW.id => {
                                engine.task_list(id).await
                            }
                            v if v == ORCHESTRATOR_THREAD_QUEUE_VIEW.id => {
                                engine.queue_list(id).await
                            }
                            v if v == ORCHESTRATOR_THREAD_LINEAGE_VIEW.id => {
                                engine.lineage(id).await
                            }
                            _ => engine.runs(&thread).await,
                        }
                    }
                    .await,
                )
            }
            _ => return Ok(None),
        };
        result.map(Some)
    }

    async fn orchestrator_command(
        &self,
        operation: &str,
        input: Value,
        scope: &ThreadScope,
    ) -> Result<Option<Value>, ProductSurfaceError> {
        let engine = &self.engine;
        let result = match operation {
            ORCHESTRATOR_TASK_CANCEL_COMMAND_ID => {
                let req: OrchestratorTaskCancelRequest = decode(input)?;
                answer(
                    async {
                        let (id, parent) = self.owned_task(&req.task_id, scope).await?;
                        let reason = req.reason.as_deref().unwrap_or("cancelled by user");
                        engine
                            .cancel_task(&Self::ctx(&parent, scope), &id, reason)
                            .await
                    }
                    .await,
                )
            }
            ORCHESTRATOR_QUEUE_MUTATE_COMMAND_ID => {
                let req: OrchestratorQueueMutationRequest = decode(input)?;
                answer(
                    async {
                        let thread = self.owned(&req.thread_id, scope).await?;
                        let id = &thread.thread_id;
                        let item = || -> Result<QueueItemId, OrchestratorError> {
                            QueueItemId::new(req.item_id.clone().unwrap_or_default())
                        };
                        let text = req.text.as_deref().unwrap_or("");
                        match req.action.as_str() {
                            "add" => engine.queue_add(id, text, Author::User).await,
                            "edit" => engine.queue_edit(id, &item()?, text).await,
                            "remove" => engine.queue_remove(id, &item()?).await,
                            "reorder" => {
                                let before =
                                    req.before_id.clone().map(QueueItemId::new).transpose()?;
                                engine.queue_reorder(id, &item()?, before.as_ref()).await
                            }
                            "clear" => engine.queue_clear(id).await,
                            "hold" => engine.queue_hold(id, true).await,
                            "release" => engine.queue_hold(id, false).await,
                            other => Err(OrchestratorError::invalid(format!(
                                "unknown queue action {other}"
                            ))),
                        }
                    }
                    .await,
                )
            }
            ORCHESTRATOR_FORK_COMMAND_ID => {
                let req: OrchestratorForkRequest = decode(input)?;
                answer(
                    async {
                        let source = self.owned(&req.thread_id, scope).await?;
                        let target = ironclaw_orchestrator::delegation::Target {
                            provider: req.provider.clone(),
                            model: req.model.clone(),
                            ..Default::default()
                        };
                        engine
                            .fork(
                                &Self::ctx(&source, scope),
                                &source,
                                req.run.map(|r| r as usize),
                                req.title.as_deref(),
                                Some(&target),
                                Author::User,
                            )
                            .await
                    }
                    .await,
                )
            }
            ORCHESTRATOR_MERGE_BACK_COMMAND_ID => {
                let req: OrchestratorMergeBackRequest = decode(input)?;
                answer(
                    async {
                        let fork = self.owned(&req.thread_id, scope).await?;
                        let target = req
                            .target_thread_id
                            .as_deref()
                            .map(ThreadId::new)
                            .transpose()?;
                        engine
                            .merge_back(
                                &Self::ctx(&fork, scope),
                                &fork,
                                target.as_ref(),
                                req.run.map(|r| r as usize),
                                None,
                                Author::User,
                            )
                            .await
                    }
                    .await,
                )
            }
            _ => return Ok(None),
        };
        result.map(Some)
    }
}

#[async_trait]
impl ProductSurface for OrchestratedProductSurface {
    async fn invoke(
        &self,
        caller: ProductSurfaceCaller,
        mut request: ProductSurfaceInvokeRequest,
    ) -> Result<ProductSurfaceInvokeResponse, ProductSurfaceError> {
        let scope = scope_of(&caller);
        let operation = request.operation_id.as_str().to_string();
        if let Some(output) = self
            .orchestrator_command(&operation, request.input.clone(), &scope)
            .await?
        {
            return Ok(ProductSurfaceInvokeResponse { output });
        }
        match operation.as_str() {
            SUBMIT_TURN_COMMAND_ID => {
                if let Ok(mut body) =
                    serde_json::from_value::<ProductSubmitTurnRequest>(request.input.clone())
                    && let Some(id) = body
                        .thread_id
                        .as_deref()
                        .and_then(|raw| ThreadId::new(raw).ok())
                {
                    soft(
                        "register thread",
                        self.engine.ensure_thread(&id, &scope, None).await,
                    );
                    soft("user prompt", self.engine.on_user_prompt(&id).await);
                    match self.engine.take_next_message_context(&id).await {
                        Ok(Some(context)) => {
                            let typed = body.content.take().unwrap_or_default();
                            body.content = Some(format!("{context}\n\n---\n\n{typed}"));
                            if let Ok(input) = serde_json::to_value(&body) {
                                request.input = input;
                            }
                        }
                        Ok(None) => {}
                        Err(error) => soft("next-message context", Err::<(), _>(error)),
                    }
                }
                self.inner.invoke(caller, request).await
            }
            CANCEL_RUN_COMMAND_ID => {
                if let Ok(body) =
                    serde_json::from_value::<ProductCancelRunRequest>(request.input.clone())
                    && let Some(id) = body
                        .thread_id
                        .as_deref()
                        .and_then(|raw| ThreadId::new(raw).ok())
                    && self.engine.owned_thread(&id, &scope).await.is_ok()
                {
                    soft("user stop", self.engine.on_user_stop(&id).await);
                }
                self.inner.invoke(caller, request).await
            }
            CREATE_THREAD_COMMAND_ID => {
                let response = self.inner.invoke(caller, request).await?;
                if let Some(id) = response.output["thread"]["thread_id"]
                    .as_str()
                    .and_then(|raw| ThreadId::new(raw).ok())
                {
                    soft(
                        "register thread",
                        self.engine.ensure_thread(&id, &scope, None).await,
                    );
                }
                Ok(response)
            }
            _ => self.inner.invoke(caller, request).await,
        }
    }

    async fn query(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSurfaceQueryRequest,
    ) -> Result<ProductSurfaceQueryPage, ProductSurfaceError> {
        let scope = scope_of(&caller);
        if let Some(value) = self
            .orchestrator_view(&request.view_id, request.input.clone(), &scope)
            .await?
        {
            return Ok(page(value));
        }
        self.inner.query(caller, request).await
    }

    async fn stream_events(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSurfaceStreamRequest,
    ) -> Result<ProductSurfaceStreamResponse, ProductSurfaceError> {
        self.inner.stream_events(caller, request).await
    }
}
