//! `ProductSurface` decorator that routes threads through the orchestrator.
//!
//! - `thread.create` registers the new thread with the engine;
//! - `turn.submit` (a typed message) lifts the Stop hold, stamps user
//!   activity (which supersedes pending auto-resumes) and carries any pending
//!   next-message context;
//! - `run.cancel` (the Stop button) holds the thread and stops every
//!   delegated task beneath it before the run is cancelled;
//! - the `orchestrator.*` views and commands are answered here;
//! - a thread bound to another runtime (a profile on a CLI driver) has its
//!   submit, cancel and timeline answered by the engine, and its live stream
//!   and approval gates by the CLI bridge (`external.rs`).
//!
//! Everything else passes through untouched, so existing route shapes keep
//! working.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_cli_providers::bridge::CliBridge;
use ironclaw_host_api::ids::ThreadId;
use ironclaw_orchestrator::engine::{CallContext, Engine};
use ironclaw_orchestrator::error::OrchestratorError;
use ironclaw_orchestrator::model::{Author, QueueItemId, TaskId, ThreadRecord, ThreadScope};
use ironclaw_product_contracts::inbound_requests::{
    ProductCancelRunRequest, ProductCreateThreadRequest, ProductResolveGateRequest,
    ProductSubmitTurnRequest,
};
use ironclaw_product_contracts::orchestration::*;
use ironclaw_product_contracts::profiles::*;
use ironclaw_product_contracts::surface::{
    ProductSurface, ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceInvokeRequest,
    ProductSurfaceInvokeResponse, ProductSurfaceQueryPage, ProductSurfaceQueryRequest,
    ProductSurfaceStreamRequest, ProductSurfaceStreamResponse, ProductSurfaceValidationCode,
};
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::external;
use super::profiles::{self, ProfileHome};
use crate::reborn_services::{
    CANCEL_RUN_COMMAND_ID, CREATE_THREAD_COMMAND_ID, RESOLVE_GATE_COMMAND_ID,
    SUBMIT_TURN_COMMAND_ID, TIMELINE_VIEW,
};

pub struct OrchestratedProductSurface {
    inner: Arc<dyn ProductSurface>,
    engine: Arc<Engine>,
    /// `None` when the process has no home folder: profile routes then fail.
    home: Option<ProfileHome>,
    /// Live turns and pending tool calls of CLI threads.
    // arch-exempt: optional_arc, deployments without CLI runtimes have no bridge, plan #v3-cli-bridge
    bridge: Option<Arc<CliBridge>>,
}

impl OrchestratedProductSurface {
    pub fn new(
        inner: Arc<dyn ProductSurface>,
        engine: Arc<Engine>,
        bridge: Option<Arc<CliBridge>>,
    ) -> Self {
        Self {
            inner,
            engine,
            home: ProfileHome::from_env(),
            bridge,
        }
    }
}

fn to_json<T: serde::Serialize>(value: T) -> Result<Value, OrchestratorError> {
    Ok(serde_json::to_value(value)?)
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

/// The thread's telemetry document; measurements stay `null` until reported.
async fn telemetry_view(
    engine: &Engine,
    thread: &ThreadRecord,
) -> Result<Value, OrchestratorError> {
    let latest = engine.thread_telemetry(&thread.thread_id).await?;
    let (t, updated_at) = match latest {
        Some(r) => (r.telemetry, Some(r.updated_at)),
        None => (Default::default(), None),
    };
    let response = ThreadTelemetryResponse {
        thread_id: thread.thread_id.as_str().to_string(),
        runtime_id: thread.runtime_id.as_str().to_string(),
        provider: t.provider.clone().or_else(|| thread.provider.clone()),
        model: t.model.clone().or_else(|| thread.model.clone()),
        context_used: t.context_used,
        context_max: t.context_max,
        context_pct: t.context_pct(),
        tps: t.tps,
        cache_hit_rate: t.cache_hit_rate,
        updated_at,
    };
    Ok(serde_json::to_value(response)?)
}

impl OrchestratedProductSurface {
    /// The caller's thread, when it runs off IronClaw's loop.
    async fn external(&self, id: &ThreadId, scope: &ThreadScope) -> Option<ThreadRecord> {
        let thread = self.engine.external_thread(id).await.ok().flatten()?;
        self.engine.owned_thread(id, scope).await.ok()?;
        Some(thread)
    }

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
            v if v == ORCHESTRATOR_THREAD_TELEMETRY_VIEW.id => {
                let req: OrchestratorThreadRequest = decode(input)?;
                answer(
                    async {
                        let thread = self.owned(&req.thread_id, scope).await?;
                        telemetry_view(engine, &thread).await
                    }
                    .await,
                )
            }
            v if v == PROFILES_VIEW.id || v == PROFILE_VIEW.id => answer(
                async {
                    let home = self.home()?;
                    if v == PROFILES_VIEW.id {
                        to_json(profiles::list(engine, home, scope).await?)
                    } else {
                        let req: ProfileRequest = serde_json::from_value(input)?;
                        to_json(profiles::get(engine, home, scope, &req.profile_id).await?)
                    }
                }
                .await,
            ),
            _ => return Ok(None),
        };
        result.map(Some)
    }

    fn home(&self) -> Result<&ProfileHome, OrchestratorError> {
        self.home.as_ref().ok_or_else(|| {
            OrchestratorError::request("profiles_unavailable", "The backend has no home folder.")
        })
    }

    /// `profiles.thread_create`: a new thread that belongs to one profile.
    async fn create_profile_thread(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSurfaceInvokeRequest,
        scope: &ThreadScope,
    ) -> Result<Value, ProductSurfaceError> {
        let body: ProfileThreadCreateRequest = decode(request.input.clone())?;
        let profile = answer(
            profiles::require_active(&self.engine, scope, &body.profile_id)
                .await
                .and_then(|id| to_json(id.as_str())),
        )?;
        let Some(profile_id) = profile.as_str().map(str::to_string) else {
            return Ok(profile); // the correctable error document
        };
        let create = ProductCreateThreadRequest {
            client_action_id: body.client_action_id,
            requested_thread_id: None,
            project_id: caller.project_id.as_ref().map(|p| p.as_str().to_string()),
        };
        let input = serde_json::to_value(create).map_err(ProductSurfaceError::internal_from)?;
        let operation_id = ironclaw_host_api::ids::CapabilityId::new(CREATE_THREAD_COMMAND_ID)
            .map_err(ProductSurfaceError::internal_from)?;
        let response = self
            .inner
            .invoke(
                caller,
                ProductSurfaceInvokeRequest {
                    operation_id,
                    input,
                    activity_id: request.activity_id,
                },
            )
            .await?;
        let thread = response.output["thread"]["thread_id"]
            .as_str()
            .and_then(|raw| ThreadId::new(raw).ok())
            .ok_or_else(|| ProductSurfaceError::internal_from("create thread returned no id"))?;
        answer(
            async {
                let id = ironclaw_orchestrator::profiles::parse_profile_slug(&profile_id)?;
                self.engine.ensure_thread(&thread, scope, None).await?;
                self.engine.store().set_thread_profile(&thread, &id).await?;
                // A profile on a CLI provider runs its threads there.
                if let Some(profile) = self.engine.store().thread_profile(&thread).await? {
                    self.engine.bind_profile_runtime(&thread, &profile).await?;
                }
                to_json(ProfileThreadCreateResponse {
                    thread_id: thread.to_string(),
                    profile_id,
                })
            }
            .await,
        )
    }

    async fn orchestrator_command(
        &self,
        operation: &str,
        input: Value,
        scope: &ThreadScope,
    ) -> Result<Option<Value>, ProductSurfaceError> {
        let engine = &self.engine;
        let result = match operation {
            PROFILE_CREATE_COMMAND_ID | PROFILE_UPDATE_COMMAND_ID => {
                let req: ProfileWriteRequest = decode(input)?;
                let create = operation == PROFILE_CREATE_COMMAND_ID;
                answer(
                    async {
                        to_json(profiles::write(engine, self.home()?, scope, req, create).await?)
                    }
                    .await,
                )
            }
            PROFILE_DELETE_COMMAND_ID => {
                let req: ProfileRequest = decode(input)?;
                answer(
                    profiles::delete(engine, scope, &req.profile_id)
                        .await
                        .map(|()| serde_json::json!({"ok": true})),
                )
            }
            PROFILE_IMPORT_HERMES_COMMAND_ID => {
                let _: HermesImportRequest = decode(input)?;
                answer(
                    async { to_json(profiles::import_hermes(engine, self.home()?, scope).await?) }
                        .await,
                )
            }
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
            PROFILE_THREAD_CREATE_COMMAND_ID => {
                let output = self.create_profile_thread(caller, request, &scope).await?;
                Ok(ProductSurfaceInvokeResponse { output })
            }
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
                    let mut changed = false;
                    match self.engine.take_next_message_context(&id).await {
                        Ok(Some(context)) => {
                            let typed = body.content.take().unwrap_or_default();
                            body.content = Some(format!("{context}\n\n---\n\n{typed}"));
                            changed = true;
                        }
                        Ok(None) => {}
                        Err(error) => soft("next-message context", Err::<(), _>(error)),
                    }
                    // A thread without a picked model runs on its profile's default.
                    let unpicked = body
                        .model
                        .as_deref()
                        .is_none_or(|m| m.trim().is_empty() || m == "default");
                    if unpicked {
                        match profiles::default_model(&self.engine, &id).await {
                            Ok(Some(model)) => {
                                body.model = Some(model);
                                changed = true;
                            }
                            Ok(None) => {}
                            Err(error) => soft("profile default model", Err::<(), _>(error)),
                        }
                    }
                    if let Some(thread) = self.external(&id, &scope).await {
                        let text = body.content.unwrap_or_default();
                        let turn = self
                            .engine
                            .user_turn(&thread, &text)
                            .await
                            .map_err(ProductSurfaceError::internal_from)?;
                        let output = external::submit_response(&id, turn)
                            .ok_or_else(|| ProductSurfaceError::internal_from("submit ref"))?;
                        return Ok(ProductSurfaceInvokeResponse {
                            output: serde_json::to_value(output)
                                .map_err(ProductSurfaceError::internal_from)?,
                        });
                    }
                    if changed && let Ok(input) = serde_json::to_value(&body) {
                        request.input = input;
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
                    if let Some(thread) = self.external(&id, &scope).await {
                        self.engine
                            .user_stop_external(&thread)
                            .await
                            .map_err(ProductSurfaceError::internal_from)?;
                        let run = body.run_id.as_deref().unwrap_or_default();
                        return Ok(ProductSurfaceInvokeResponse {
                            output: serde_json::to_value(external::cancel_response(run))
                                .map_err(ProductSurfaceError::internal_from)?,
                        });
                    }
                    soft("user stop", self.engine.on_user_stop(&id).await);
                }
                self.inner.invoke(caller, request).await
            }
            RESOLVE_GATE_COMMAND_ID => {
                if let Some(bridge) = &self.bridge
                    && let Ok(body) =
                        serde_json::from_value::<ProductResolveGateRequest>(request.input.clone())
                    && let Some(id) = body
                        .thread_id
                        .as_deref()
                        .and_then(|raw| ThreadId::new(raw).ok())
                    && self.external(&id, &scope).await.is_some()
                {
                    let approved = body.resolution.as_deref() == Some("approved");
                    let response = external::resolve_gate(
                        bridge,
                        &id,
                        body.run_id.as_deref().unwrap_or_default(),
                        body.gate_ref.as_deref().unwrap_or_default(),
                        approved,
                    )?;
                    return Ok(ProductSurfaceInvokeResponse {
                        output: serde_json::to_value(response)
                            .map_err(ProductSurfaceError::internal_from)?,
                    });
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
        if request.view_id == TIMELINE_VIEW.id
            && let Some(id) = request.input["thread_id"]
                .as_str()
                .and_then(|raw| ThreadId::new(raw).ok())
            && let Some(thread) = self.external(&id, &scope).await
        {
            let limit = request.input["limit"].as_u64().unwrap_or(200).clamp(1, 500) as usize;
            let history = self
                .engine
                .external_history(&thread)
                .await
                .map_err(ProductSurfaceError::internal_from)?;
            let mut result = self.inner.query(caller, request).await?;
            if let Some(item) = result.items.first_mut() {
                item["messages"] = Value::Array(external::timeline_messages(&id, &history, limit));
                item["next_cursor"] = Value::Null;
            }
            result.next_cursor = None;
            return Ok(result);
        }
        self.inner.query(caller, request).await
    }

    async fn stream_events(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSurfaceStreamRequest,
    ) -> Result<ProductSurfaceStreamResponse, ProductSurfaceError> {
        if let Some(bridge) = &self.bridge
            && let Some(id) = request
                .stream_id
                .as_deref()
                .and_then(|raw| ThreadId::new(raw).ok())
            && self.external(&id, &scope_of(&caller)).await.is_some()
        {
            return external::live_stream(Arc::clone(bridge), id);
        }
        self.inner.stream_events(caller, request).await
    }
}
