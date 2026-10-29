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
    ProductCancelRunRequest, ProductCreateThreadRequest, ProductInboundAttachment,
    ProductSubmitTurnRequest,
};
use ironclaw_product_contracts::operator_llm::{
    NativeModelRoute, NativeModelRoutes, USER_MODEL_CATALOG_VIEW, UserModelCatalog,
};
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
    /// Other configured providers a thread can run on (`None`: no LLM admin wired).
    routes: Option<Arc<dyn NativeModelRoutes>>,
}

impl AgentLoopRuntime {
    pub fn new(
        surface: Arc<dyn ProductSurface>,
        session_extension_id: Option<String>,
        routes: Option<Arc<dyn NativeModelRoutes>>,
    ) -> Result<Self, ironclaw_orchestrator::error::OrchestratorError> {
        Ok(Self {
            id: RuntimeId::new(IRONCLAW_RUNTIME_ID)?,
            surface,
            session_extension_id,
            routes,
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
        attachments: Vec<ProductInboundAttachment>,
    ) -> Result<RebornSubmitTurnResponse, RuntimeError> {
        SUBMIT_TURN_COMMAND
            .invoke_on(
                &self.bound(&thread.scope),
                ProductSubmitTurnRequest {
                    extension_id: self.session_extension_id.clone(),
                    client_action_id: Some(uuid::Uuid::new_v4().to_string()),
                    thread_id: Some(thread.thread_id.as_str().to_string()),
                    content: Some(text.to_string()),
                    attachments,
                    model,
                },
                ActivityId::new(),
            )
            .await
            .map_err(surface_error)
    }
}

/// The turn model for a thread on IronClaw's loop: a pick on another
/// configured provider (`/route`) runs there, and an effort pick rides along,
/// both encoded for the model router. CLI runtimes get the plain model.
pub(crate) fn native_turn_model(
    thread: &ThreadRecord,
    model: Option<String>,
    effort: Option<&str>,
) -> Option<String> {
    native_turn_model_on(thread, thread.provider.as_deref(), model, effort)
}

/// [`native_turn_model`] for a model that comes from somewhere other than
/// the thread's pick, such as its profile's default, with that provider.
pub(crate) fn native_turn_model_on(
    thread: &ThreadRecord,
    provider: Option<&str>,
    model: Option<String>,
    effort: Option<&str>,
) -> Option<String> {
    let model = model.filter(|m| !m.trim().is_empty() && m != "default");
    if thread.runtime_id.as_str() != IRONCLAW_RUNTIME_ID {
        return model;
    }
    let provider = provider.filter(|p| *p != IRONCLAW_RUNTIME_ID && model.is_some());
    NativeModelRoute::turn_model(provider, model.as_deref(), effort)
}

/// The brief's host files a sandboxed child may receive, as inline uploads
/// under the same budgets and format registry the submit decoder enforces
/// (a file it would reject must not sink the whole brief). Only regular
/// files under the home folder, outside hidden folders and `~/Library` (app
/// data and credentials, not documents). The file read is the one that was
/// checked: it is opened, its inode compared with the vetted path's, and
/// read no further than the remaining budget. A skipped file is logged and
/// left out; the child sees the brief either way.
async fn host_attachments(paths: &[std::path::PathBuf]) -> Vec<ProductInboundAttachment> {
    use base64::Engine as _;
    use std::os::unix::fs::MetadataExt as _;
    use tokio::io::AsyncReadExt as _;
    let budgets = ironclaw_attachments::DEFAULT_ATTACHMENT_BUDGETS;
    let Some(home) = std::env::var_os("HOME").map(std::path::PathBuf::from) else {
        tracing::debug!("brief attachments skipped: no HOME");
        return Vec::new();
    };
    let home = match tokio::fs::canonicalize(&home).await {
        Ok(home) => home,
        Err(error) => {
            tracing::debug!(%error, "brief attachments skipped: home unresolved");
            return Vec::new();
        }
    };
    let mut total = 0usize;
    let mut out: Vec<ProductInboundAttachment> = Vec::new();
    let mut seen = Vec::new();
    for raw in paths {
        if out.len() == budgets.max_count {
            tracing::debug!("brief attachments: count budget reached");
            break;
        }
        let path = match raw.strip_prefix("~") {
            Ok(rest) => home.join(rest),
            Err(_) => raw.clone(),
        };
        let real = match tokio::fs::canonicalize(&path).await {
            Ok(real) => real,
            // Most candidates are prose that only looks like a path.
            Err(error) => {
                tracing::debug!(%error, "brief path skipped: not found");
                continue;
            }
        };
        let Ok(rel) = real.strip_prefix(&home) else {
            tracing::debug!("brief path skipped: outside home");
            continue;
        };
        let hidden = rel
            .components()
            .any(|c| c.as_os_str().to_string_lossy().starts_with('.'));
        if hidden || rel.starts_with("Library") || seen.contains(&real) {
            tracing::debug!("brief path skipped: hidden, Library or repeated");
            continue;
        }
        let name = real.to_string_lossy().to_string();
        let mime = crate::scoped_fs::project_filesystem_reader::mime_for_path(&name);
        if !ironclaw_common::is_supported_mime(&mime) {
            tracing::debug!(%mime, "brief path skipped: unsupported format");
            continue;
        }
        let vetted = match tokio::fs::metadata(&real).await {
            Ok(meta) => meta,
            Err(error) => {
                tracing::debug!(%error, "brief path skipped: metadata");
                continue;
            }
        };
        let room = budgets
            .max_file_bytes
            .min(budgets.max_total_bytes.saturating_sub(total));
        if !vetted.is_file() || vetted.len() > room as u64 {
            tracing::debug!("brief path skipped: not a file or over budget");
            continue;
        }
        let file = match tokio::fs::File::open(&real).await {
            Ok(file) => file,
            Err(error) => {
                tracing::debug!(%error, "brief path skipped: open");
                continue;
            }
        };
        // The opened file must be the vetted one, not a swapped-in link.
        let opened = match file.metadata().await {
            Ok(meta) => meta,
            Err(error) => {
                tracing::debug!(%error, "brief path skipped: opened metadata");
                continue;
            }
        };
        if !opened.is_file() || (opened.dev(), opened.ino()) != (vetted.dev(), vetted.ino()) {
            tracing::debug!("brief path skipped: changed while opening");
            continue;
        }
        let mut bytes = Vec::new();
        if let Err(error) = file.take(room as u64 + 1).read_to_end(&mut bytes).await {
            tracing::debug!(%error, "brief path skipped: read");
            continue;
        }
        if bytes.len() > room {
            tracing::debug!("brief path skipped: grew past the budget");
            continue;
        }
        total += bytes.len();
        out.push(ProductInboundAttachment {
            mime_type: mime,
            filename: crate::scoped_fs::project_filesystem_reader::file_name_of(&name),
            data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
            path: None,
        });
        seen.push(real);
    }
    out
}

/// `name(hint)` for a tool-preview row, so child-run summaries and task
/// progress count real tool calls. The preview is our own JSON record.
fn preview_tool_call(content: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(content).ok()?;
    let name = value.get("capability_id")?.as_str()?;
    let hint = value
        .get("input_summary")
        .and_then(|v| v.as_str())
        .and_then(|s| s.lines().next())
        .unwrap_or("");
    let hint: String = hint.chars().take(80).collect();
    Some(format!("{name}({hint})"))
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
            host_files: false,
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
        let model = native_turn_model(thread, input.model, input.effort.as_deref());
        let attachments = host_attachments(&input.files).await;
        Ok(
            match self.submit(thread, &input.text, model, attachments).await? {
                RebornSubmitTurnResponse::Submitted { run_id, .. }
                | RebornSubmitTurnResponse::AlreadySubmitted { run_id, .. } => {
                    TurnAdmission::Started {
                        run_id: Some(run_id.to_string()),
                    }
                }
                RebornSubmitTurnResponse::DeferredBusy { active_run_id, .. } => {
                    TurnAdmission::Steered {
                        run_id: Some(active_run_id.to_string()),
                    }
                }
                RebornSubmitTurnResponse::RejectedBusy { .. } => TurnAdmission::Busy,
            },
        )
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
        Ok(match self.submit(thread, text, None, Vec::new()).await? {
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
                let tool_calls = if m.kind == MessageKind::CapabilityDisplayPreview {
                    preview_tool_call(&text).into_iter().collect()
                } else {
                    Vec::new()
                };
                Some(HistoryMessage {
                    role,
                    text,
                    tool_calls,
                    run_id: m.turn_run_id.clone(),
                    hidden,
                    at: m.created_at.or(m.updated_at).map(|t| t.timestamp_millis()),
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
        let primary = match &self.routes {
            Some(routes) => routes.primary().await,
            None => None,
        };
        // IronClaw's loop is a runtime, not a vendor: the picker names the
        // group after the provider it runs on (ChatGPT, Z.AI, ...).
        let (name, brand) = primary.as_ref().map_or_else(
            || ("IronClaw".to_string(), None),
            |p| (p.name.clone(), Some(p.provider_id.clone())),
        );
        let (efforts, mut model_efforts, primary_models) = primary
            .map(|p| (p.efforts, p.model_efforts, p.models))
            .unwrap_or_default();
        // Without an operator model policy the active provider's own list
        // is the choice (the ChatGPT plan's models, for one).
        let mut models = if catalog.selection_enabled {
            catalog.models
        } else {
            primary_models
        };
        // The profile's model leads, so a client can show it by name.
        if let Some(ix) = catalog
            .workspace_default
            .as_ref()
            .filter(|_| catalog.selection_enabled)
            .and_then(|d| models.iter().position(|m| m == d))
        {
            models[..=ix].rotate_right(1);
        }
        model_efforts.retain(|model, _| models.contains(model));
        let mut providers = vec![CatalogProvider {
            provider: IRONCLAW_RUNTIME_ID.to_string(),
            name,
            authenticated: true,
            current: true,
            models,
            efforts,
            model_efforts,
            brand,
        }];
        if let Some(routes) = &self.routes {
            providers.extend(routes.routes().await.into_iter().map(|r| CatalogProvider {
                provider: r.provider_id,
                name: r.name,
                authenticated: true,
                current: false,
                models: r.models,
                efforts: r.efforts,
                model_efforts: r.model_efforts,
                brand: None,
            }));
        }
        Ok(providers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_host_api::ids::ThreadId;

    fn native(provider: Option<&str>, runtime: &str) -> ThreadRecord {
        let mut thread = ThreadRecord::new(
            ThreadId::new("t").unwrap(),
            ThreadScope {
                tenant_id: ironclaw_host_api::ids::TenantId::new("t1").unwrap(),
                user_id: ironclaw_host_api::ids::UserId::new("u1").unwrap(),
                agent_id: None,
                project_id: None,
            },
            RuntimeId::new(runtime).unwrap(),
        );
        thread.provider = provider.map(str::to_string);
        thread
    }

    #[test]
    fn a_pick_on_another_provider_is_encoded_for_the_model_router() {
        let model = || Some("glm-5.3".to_string());
        assert_eq!(
            native_turn_model(&native(Some("glm"), "ironclaw"), model(), None).as_deref(),
            Some("glm::glm-5.3")
        );
        for plain in [
            native(Some("ironclaw"), "ironclaw"),
            native(None, "ironclaw"),
        ] {
            assert_eq!(
                native_turn_model(&plain, model(), None).as_deref(),
                Some("glm-5.3")
            );
        }
        assert_eq!(
            native_turn_model(&native(Some("claude"), "claude-cli"), model(), Some("high"))
                .as_deref(),
            Some("glm-5.3"),
            "CLI runtimes get the plain model; their effort travels in TurnInput"
        );
        assert_eq!(
            native_turn_model(
                &native(Some("glm"), "ironclaw"),
                Some("default".into()),
                None
            ),
            None
        );
        assert_eq!(
            native_turn_model(&native(Some("glm"), "ironclaw"), model(), Some("low")).as_deref(),
            Some("glm::glm-5.3::low")
        );
        // A profile default on another provider routes there even when the
        // thread has no pick of its own (a bare GLM model reached ChatGPT).
        assert_eq!(
            native_turn_model_on(&native(None, "ironclaw"), Some("glm"), model(), None).as_deref(),
            Some("glm::glm-5.3")
        );
        assert_eq!(
            native_turn_model(&native(None, "ironclaw"), None, Some("xhigh")).as_deref(),
            Some("::::xhigh"),
            "an effort alone keeps the active provider and its default model"
        );
    }
}
