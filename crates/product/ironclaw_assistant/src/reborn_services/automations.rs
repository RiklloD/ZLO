//! Automation listing and lifecycle, automation name validation, trigger-thread
//! recognition, and the notification-approval candidate scan (`automations`
//! charter row in `AGENTS.md`).

use super::*;

pub const AUTOMATION_PAUSE_CAPABILITY_ID: &str = "builtin.automation_pause";

pub const AUTOMATION_PAUSE_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(AUTOMATION_PAUSE_CAPABILITY_ID);

pub const AUTOMATION_RUN_CAPABILITY_ID: &str = "builtin.automation_run";

pub const AUTOMATION_RUN_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(AUTOMATION_RUN_CAPABILITY_ID);

pub const AUTOMATION_RESUME_CAPABILITY_ID: &str = "builtin.automation_resume";

pub const AUTOMATION_RESUME_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(AUTOMATION_RESUME_CAPABILITY_ID);

pub const AUTOMATION_RENAME_CAPABILITY_ID: &str = "builtin.automation_rename";

pub const AUTOMATION_RENAME_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(AUTOMATION_RENAME_CAPABILITY_ID);

pub const AUTOMATION_DELETE_CAPABILITY_ID: &str = "builtin.automation_delete";

pub const AUTOMATION_DELETE_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(AUTOMATION_DELETE_CAPABILITY_ID);

pub const AUTOMATION_PAUSE_COMMAND_ID: &str = "automation.pause";

pub const AUTOMATION_PAUSE_COMMAND: ProductSurfaceCommandDescriptor<
    RebornAutomationRequest,
    RebornAutomationMutationResponse,
> = ProductSurfaceCommandDescriptor::new(AUTOMATION_PAUSE_COMMAND_ID);

pub const AUTOMATION_RUN_COMMAND_ID: &str = "automation.run";

pub const AUTOMATION_RUN_COMMAND: ProductSurfaceCommandDescriptor<
    RebornAutomationRequest,
    RebornAutomationMutationResponse,
> = ProductSurfaceCommandDescriptor::new(AUTOMATION_RUN_COMMAND_ID);

pub const AUTOMATION_RESUME_COMMAND_ID: &str = "automation.resume";

pub const AUTOMATION_RESUME_COMMAND: ProductSurfaceCommandDescriptor<
    RebornAutomationRequest,
    RebornAutomationMutationResponse,
> = ProductSurfaceCommandDescriptor::new(AUTOMATION_RESUME_COMMAND_ID);

pub const AUTOMATION_RENAME_COMMAND_ID: &str = "automation.rename";

pub const AUTOMATION_RENAME_COMMAND: ProductSurfaceCommandDescriptor<
    RebornRenameAutomationProductRequest,
    RebornAutomationMutationResponse,
> = ProductSurfaceCommandDescriptor::new(AUTOMATION_RENAME_COMMAND_ID);

pub const AUTOMATION_DELETE_COMMAND_ID: &str = "automation.delete";

pub const AUTOMATION_DELETE_COMMAND: ProductSurfaceCommandDescriptor<
    RebornAutomationRequest,
    RebornAutomationMutationResponse,
> = ProductSurfaceCommandDescriptor::new(AUTOMATION_DELETE_COMMAND_ID);

pub const AUTOMATIONS_VIEW: ProductView<
    ProductListAutomationsRequest,
    RebornListAutomationsResponse,
> = ProductView::unpaginated("automations");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutomationListRequest {
    pub limit: usize,
    pub run_limit: usize,
    /// When `true`, include completed (fire-once) automations alongside the
    /// active ones. When `false` (the default), only active automations are
    /// returned. Services apply `limit` after this filter, so a full page of
    /// active automations is returned regardless of how many completed ones
    /// exist.
    pub include_completed: bool,
}

/// Stored scope of a trigger-fired thread, returned by
/// `AutomationProductService::resolve_run_thread_scope`.
///
/// Trigger threads are written by `record_trigger_prompt` with:
///  - `agent_id` = trigger record's `agent_id` (or default agent)
///  - `project_id` = trigger record's `project_id`
///  - `owner_user_id` = `Some(creator_user_id)` (the actor that fired it)
///
/// These three fields let the caller reconstruct the true `TurnScope` / `ThreadScope`
/// needed to locate the thread in storage without guessing.
#[derive(Debug, Clone)]
pub struct TriggerRunThreadScope {
    /// `agent_id` stored on the trigger record.
    pub agent_id: Option<AgentId>,
    /// `project_id` stored on the trigger record.
    pub project_id: Option<ProjectId>,
    /// `creator_user_id` stored on the trigger record, which equals
    /// `owner_user_id` in the stored thread scope.
    pub creator_user_id: UserId,
}

#[derive(Debug, Clone)]
pub(super) struct AutomationNotificationTitle(String);

impl AutomationNotificationTitle {
    const MAX_CHARS: usize = 120;

    fn from_name(name: &str) -> Option<Self> {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return None;
        }
        let sanitized = trimmed
            .chars()
            .filter(|character| !character.is_control())
            .take(Self::MAX_CHARS)
            .collect::<String>();
        let sanitized = sanitized.trim();
        if sanitized.is_empty() {
            None
        } else {
            Some(Self(sanitized.to_string()))
        }
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone)]
pub(super) struct AutomationApprovalThreadCandidate {
    thread_id: ThreadId,
    title: Option<AutomationNotificationTitle>,
}

pub(super) struct AutomationApprovalThread {
    record: SessionThreadRecord,
    approvals: Vec<PendingApprovalInteractionView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct LegacyApprovalBackfillKey {
    tenant_id: String,
    user_id: String,
    agent_id: Option<String>,
    project_id: Option<String>,
}

#[async_trait]
pub trait AutomationProductService: Send + Sync {
    /// Whether this service can enumerate caller-scoped historical runs for
    /// the one-release migration of approval gates created before durable
    /// Inbox publishing existed.
    fn supports_legacy_approval_notification_backfill(&self) -> bool {
        false
    }

    async fn list_automations(
        &self,
        caller: ProductAgentBoundCaller,
        request: AutomationListRequest,
    ) -> Result<Vec<RebornAutomationInfo>, ProductSurfaceError>;

    async fn pause_automation(
        &self,
        _caller: ProductAgentBoundCaller,
        _automation_id: String,
    ) -> Result<RebornAutomationMutationResponse, ProductSurfaceError> {
        Err(automation_unavailable())
    }

    async fn run_automation(
        &self,
        _caller: ProductAgentBoundCaller,
        _automation_id: String,
    ) -> Result<RebornAutomationMutationResponse, ProductSurfaceError> {
        Err(automation_unavailable())
    }

    async fn resume_automation(
        &self,
        _caller: ProductAgentBoundCaller,
        _automation_id: String,
    ) -> Result<RebornAutomationMutationResponse, ProductSurfaceError> {
        Err(automation_unavailable())
    }

    async fn rename_automation(
        &self,
        _caller: ProductAgentBoundCaller,
        _automation_id: String,
        _name: AutomationName,
    ) -> Result<RebornAutomationMutationResponse, ProductSurfaceError> {
        Err(automation_unavailable())
    }

    async fn delete_automation(
        &self,
        _caller: ProductAgentBoundCaller,
        _automation_id: String,
    ) -> Result<RebornAutomationMutationResponse, ProductSurfaceError> {
        Err(automation_unavailable())
    }

    /// Whether the background trigger poller (scheduler) is running.
    ///
    /// Surfaced to the browser so the panel can warn that listed automations
    /// will not fire while scheduling is off. Defaults to `true` so a service
    /// that does not know its scheduler state never produces a false "off"
    /// notice; the production service overrides this with the real value.
    fn scheduler_enabled(&self) -> bool {
        true
    }

    /// Looks up the stored trigger-thread scope for a given `thread_id`.
    ///
    /// Scans the caller-scoped triggers for one whose run history contains
    /// `thread_id`, then returns the scope fields from that trigger record.
    /// The lookup is caller-scoped via `list_scoped_triggers`, so authorization
    /// is embedded: if the trigger exists for this caller and contains the run,
    /// the caller is permitted to access it.
    ///
    /// Returns `Ok(None)` when no caller-scoped trigger has a run with this
    /// `thread_id`. Backend lookup failures should return a stable
    /// `ProductSurfaceError` so outages do not masquerade as authorization
    /// misses.
    ///
    /// Implementors that do not support trigger-thread access must provide an
    /// explicit `Ok(None)` body with a short comment noting the unsupported
    /// state. No default body is provided here so a future production service
    /// cannot silently forget to implement this method and degrade
    /// timeline/SSE/gate/cancel/run-state to 404.
    async fn resolve_run_thread_scope(
        &self,
        caller: ProductAgentBoundCaller,
        thread_id: &ThreadId,
    ) -> Result<Option<TriggerRunThreadScope>, ProductSurfaceError>;
}

#[derive(Debug)]
pub struct UnsupportedAutomationProductService;

impl UnsupportedAutomationProductService {
    pub fn new_static() -> Self {
        Self
    }
}

#[async_trait]
impl AutomationProductService for UnsupportedAutomationProductService {
    async fn list_automations(
        &self,
        _caller: ProductAgentBoundCaller,
        _request: AutomationListRequest,
    ) -> Result<Vec<RebornAutomationInfo>, ProductSurfaceError> {
        Err(automation_unavailable())
    }

    async fn pause_automation(
        &self,
        _caller: ProductAgentBoundCaller,
        _automation_id: String,
    ) -> Result<RebornAutomationMutationResponse, ProductSurfaceError> {
        Err(automation_unavailable())
    }

    async fn run_automation(
        &self,
        _caller: ProductAgentBoundCaller,
        _automation_id: String,
    ) -> Result<RebornAutomationMutationResponse, ProductSurfaceError> {
        Err(automation_unavailable())
    }

    async fn resume_automation(
        &self,
        _caller: ProductAgentBoundCaller,
        _automation_id: String,
    ) -> Result<RebornAutomationMutationResponse, ProductSurfaceError> {
        Err(automation_unavailable())
    }

    async fn rename_automation(
        &self,
        _caller: ProductAgentBoundCaller,
        _automation_id: String,
        _name: AutomationName,
    ) -> Result<RebornAutomationMutationResponse, ProductSurfaceError> {
        Err(automation_unavailable())
    }

    async fn delete_automation(
        &self,
        _caller: ProductAgentBoundCaller,
        _automation_id: String,
    ) -> Result<RebornAutomationMutationResponse, ProductSurfaceError> {
        Err(automation_unavailable())
    }

    async fn resolve_run_thread_scope(
        &self,
        _caller: ProductAgentBoundCaller,
        _thread_id: &ThreadId,
    ) -> Result<Option<TriggerRunThreadScope>, ProductSurfaceError> {
        // Trigger-thread access is unsupported when no automation service is wired.
        Ok(None)
    }
}

pub(super) fn automation_unavailable() -> ProductSurfaceError {
    ProductSurfaceError::service_unavailable(true)
}

pub(super) fn is_automation_trigger_thread(thread: &SessionThreadRecord) -> bool {
    let Some(metadata) = thread.metadata_json.as_deref() else {
        return false;
    };
    match thread_metadata_is_automation_trigger(metadata) {
        Ok(is_automation_trigger) => is_automation_trigger,
        Err(error) => {
            tracing::debug!(
                error = %error,
                thread_id = %thread.thread_id,
                "failed to parse thread metadata_json for automation filter"
            );
            false
        }
    }
}

/// Default number of automation rows returned when the browser does not
/// request a smaller page.
pub const AUTOMATION_LIST_DEFAULT_PAGE_SIZE: u32 = 50;

/// Hard ceiling for the beta automation management list response. This keeps
/// the user-facing endpoint bounded until the trigger capability exposes an
/// opaque cursor contract.
pub const AUTOMATION_LIST_MAX_PAGE_SIZE: u32 = 100;

/// Default number of recent runs returned per automation row.
pub const AUTOMATION_RUN_HISTORY_DEFAULT_PAGE_SIZE: u32 = 25;

/// Hard ceiling for recent runs embedded in each automation row.
pub const AUTOMATION_RUN_HISTORY_MAX_PAGE_SIZE: u32 = 100;

pub(super) const NOTIFICATION_APPROVAL_AUTOMATION_LIMIT: usize = 20;

pub(super) const NOTIFICATION_APPROVAL_RUN_LIMIT: usize = 20;

pub(super) const NOTIFICATION_APPROVAL_CANDIDATE_LIMIT: usize = 20;

pub(super) const NOTIFICATION_APPROVAL_QUERY_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) fn clamp_automation_list_limit(requested: Option<u32>) -> usize {
    let raw = requested.unwrap_or(AUTOMATION_LIST_DEFAULT_PAGE_SIZE);
    let clamped = raw.clamp(1, AUTOMATION_LIST_MAX_PAGE_SIZE);
    clamped as usize
}

pub(super) fn clamp_automation_run_limit(requested: Option<u32>) -> usize {
    let raw = requested.unwrap_or(AUTOMATION_RUN_HISTORY_DEFAULT_PAGE_SIZE);
    // 0 is intentional: callers suppress embedded run history by passing run_limit=0.
    let clamped = raw.min(AUTOMATION_RUN_HISTORY_MAX_PAGE_SIZE);
    clamped as usize
}

pub(super) fn parse_automation_name(
    request: ProductRenameAutomationRequest,
) -> Result<AutomationName, ProductSurfaceError> {
    let Some(raw_name) = request.name else {
        return Err(ProductSurfaceError::validation(
            "name",
            ProductSurfaceValidationCode::MissingField,
        ));
    };
    AutomationName::new(raw_name).map_err(automation_name_validation_error)
}

pub(super) fn automation_name_validation_code(
    error: AutomationNameError,
) -> ProductSurfaceValidationCode {
    match error {
        AutomationNameError::Empty => ProductSurfaceValidationCode::Blank,
        AutomationNameError::TooLong => ProductSurfaceValidationCode::TooLong,
    }
}

pub(super) fn automation_name_validation_error(error: AutomationNameError) -> ProductSurfaceError {
    ProductSurfaceError::validation("name", automation_name_validation_code(error))
}

pub(super) fn notification_approval_timeout_error() -> ProductSurfaceError {
    ProductSurfaceError::service_unavailable(true)
}

impl<I, V> RebornServices<I, V>
where
    I: ProductCapabilityInvoker + Clone + 'static,
    V: RebornViewProvider + Clone + 'static,
{
    pub(super) async fn build_automations_view(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductListAutomationsRequest,
    ) -> Result<RebornListAutomationsResponse, ProductSurfaceError> {
        let Some(caller) = product_agent_bound_caller_from_webui(caller) else {
            return Err(ProductSurfaceError::from_status(
                ProductSurfaceErrorCode::InvalidRequest,
                400,
                false,
            ));
        };
        let limit = clamp_automation_list_limit(request.limit);
        let run_limit = clamp_automation_run_limit(request.run_limit);
        let scheduler_enabled = self.automation_service.scheduler_enabled();
        let automations = self
            .automation_service
            .list_automations(
                caller,
                AutomationListRequest {
                    limit,
                    run_limit,
                    include_completed: request.include_completed,
                },
            )
            .await?;
        Ok(RebornListAutomationsResponse {
            automations,
            scheduler_enabled,
        })
    }

    pub(super) async fn pause_automation(
        &self,
        caller: ProductSurfaceCaller,
        automation_id: String,
    ) -> Result<RebornAutomationMutationResponse, ProductSurfaceError> {
        let Some(caller) = product_agent_bound_caller_from_webui(caller) else {
            return Err(ProductSurfaceError::from_status(
                ProductSurfaceErrorCode::InvalidRequest,
                400,
                false,
            ));
        };
        self.automation_service
            .pause_automation(caller, automation_id)
            .await
    }

    pub(super) async fn run_automation(
        &self,
        caller: ProductSurfaceCaller,
        automation_id: String,
    ) -> Result<RebornAutomationMutationResponse, ProductSurfaceError> {
        let Some(caller) = product_agent_bound_caller_from_webui(caller) else {
            return Err(ProductSurfaceError::from_status(
                ProductSurfaceErrorCode::InvalidRequest,
                400,
                false,
            ));
        };
        self.automation_service
            .run_automation(caller, automation_id)
            .await
    }

    pub(super) async fn resume_automation(
        &self,
        caller: ProductSurfaceCaller,
        automation_id: String,
    ) -> Result<RebornAutomationMutationResponse, ProductSurfaceError> {
        let Some(caller) = product_agent_bound_caller_from_webui(caller) else {
            return Err(ProductSurfaceError::from_status(
                ProductSurfaceErrorCode::InvalidRequest,
                400,
                false,
            ));
        };
        self.automation_service
            .resume_automation(caller, automation_id)
            .await
    }

    pub(super) async fn rename_automation(
        &self,
        caller: ProductSurfaceCaller,
        automation_id: String,
        request: ProductRenameAutomationRequest,
    ) -> Result<RebornAutomationMutationResponse, ProductSurfaceError> {
        let Some(caller) = product_agent_bound_caller_from_webui(caller) else {
            return Err(ProductSurfaceError::from_status(
                ProductSurfaceErrorCode::InvalidRequest,
                400,
                false,
            ));
        };
        let name = parse_automation_name(request)?;
        self.automation_service
            .rename_automation(caller, automation_id, name)
            .await
    }

    pub(super) async fn delete_automation(
        &self,
        caller: ProductSurfaceCaller,
        automation_id: String,
    ) -> Result<RebornAutomationMutationResponse, ProductSurfaceError> {
        let Some(caller) = product_agent_bound_caller_from_webui(caller) else {
            return Err(ProductSurfaceError::from_status(
                ProductSurfaceErrorCode::InvalidRequest,
                400,
                false,
            ));
        };
        self.automation_service
            .delete_automation(caller, automation_id)
            .await
    }
}

impl<I, V> RebornServices<I, V>
where
    I: ProductCapabilityInvoker,
    V: RebornViewProvider,
{
    pub(super) async fn list_automation_threads_needing_approval(
        &self,
        caller: ProductSurfaceCaller,
        visible_limit: usize,
        candidate_thread_id: Option<String>,
    ) -> Result<RebornListThreadsResponse, ProductSurfaceError> {
        let Some(bound_caller) = product_agent_bound_caller_from_webui(caller.clone()) else {
            return Err(ProductSurfaceError::from_status(
                ProductSurfaceErrorCode::InvalidRequest,
                400,
                false,
            ));
        };
        let candidates = self
            .automation_approval_thread_candidates(&bound_caller)
            .await?;

        let mut seen = HashSet::new();
        let mut threads = Vec::with_capacity(visible_limit);
        if let Some(candidate_thread_id) = candidate_thread_id {
            let thread_id = parse_thread_id_field("candidate_thread_id", candidate_thread_id)?;
            if seen.insert(thread_id.clone()) {
                let listed_candidate = candidates
                    .iter()
                    .find(|candidate| candidate.thread_id == thread_id)
                    .cloned();
                let approval_thread = if let Some(candidate) = listed_candidate {
                    self.automation_run_thread_record(
                        &caller,
                        &bound_caller,
                        candidate.thread_id,
                        candidate.title,
                    )
                    .await?
                } else {
                    self.automation_run_thread_record(&caller, &bound_caller, thread_id, None)
                        .await?
                };
                if let Some(approval_thread) = approval_thread {
                    threads.push(approval_thread.record);
                }
            }
        }
        for candidate in candidates {
            if threads.len() >= visible_limit {
                break;
            }
            if !seen.insert(candidate.thread_id.clone()) {
                continue;
            }
            let Some(approval_thread) = self
                .automation_run_thread_record(
                    &caller,
                    &bound_caller,
                    candidate.thread_id,
                    candidate.title,
                )
                .await?
            else {
                continue;
            };
            threads.push(approval_thread.record);
        }

        Ok(RebornListThreadsResponse {
            threads,
            next_cursor: None,
        })
    }

    pub(super) async fn backfill_legacy_approval_notifications(
        &self,
        caller: &ProductSurfaceCaller,
    ) -> Result<(), ProductSurfaceError> {
        if !self
            .automation_service
            .supports_legacy_approval_notification_backfill()
        {
            return Ok(());
        }
        let Some(bound_caller) = product_agent_bound_caller_from_webui(caller.clone()) else {
            return Ok(());
        };
        let migration_key = LegacyApprovalBackfillKey {
            tenant_id: caller.tenant_id.as_str().to_string(),
            user_id: caller.user_id.as_str().to_string(),
            agent_id: caller
                .agent_id
                .as_ref()
                .map(|agent_id| agent_id.as_str().to_string()),
            project_id: caller
                .project_id
                .as_ref()
                .map(|project_id| project_id.as_str().to_string()),
        };
        if !self
            .legacy_approval_backfill_attempted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(migration_key)
        {
            return Ok(());
        }

        let migrate = async {
            let candidates = self
                .automation_approval_thread_candidates(&bound_caller)
                .await?;
            for candidate in candidates {
                let Some(approval_thread) = self
                    .automation_run_thread_record(
                        caller,
                        &bound_caller,
                        candidate.thread_id,
                        candidate.title,
                    )
                    .await?
                else {
                    continue;
                };
                let occurred_at = approval_thread
                    .record
                    .updated_at
                    .or(approval_thread.record.created_at)
                    .unwrap_or_else(Utc::now);
                for approval in approval_thread.approvals {
                    let lifecycle_ref = LifecycleRef::new(approval.gate_ref.as_str())
                        .map_err(map_notification_inbox_error)?;
                    let id = crate::run_delivery::run_notification_inbox_id(
                        approval.run_id,
                        NotificationKind::ApprovalRequired,
                        Some(lifecycle_ref.as_str()),
                    )
                    .map_err(map_notification_inbox_error)?;
                    self.notification_inbox
                        .publish(PublishNotificationRequest {
                            id,
                            recipient: notification_recipient(caller),
                            kind: NotificationKind::ApprovalRequired,
                            severity: NotificationSeverity::Warning,
                            source: NotificationSource {
                                thread_id: Some(approval.scope.thread_id.clone()),
                                turn_run_id: Some(approval.run_id),
                                lifecycle_ref: Some(lifecycle_ref),
                                credential_providers: Vec::new(),
                            },
                            action: NotificationAction::OpenThread {
                                thread_id: approval.scope.thread_id,
                            },
                            initial_state: NotificationInitialState::Open,
                            occurred_at,
                        })
                        .await
                        .map_err(map_notification_inbox_error)?;
                }
            }
            Ok::<(), ProductSurfaceError>(())
        };

        tokio::time::timeout(NOTIFICATION_APPROVAL_QUERY_TIMEOUT, migrate)
            .await
            .map_err(|error| {
                tracing::warn!(%error, "legacy approval notification backfill timed out");
                ProductSurfaceError::service_unavailable(true)
            })??;
        Ok(())
    }

    pub(super) async fn automation_approval_thread_candidates(
        &self,
        bound_caller: &ProductAgentBoundCaller,
    ) -> Result<Vec<AutomationApprovalThreadCandidate>, ProductSurfaceError> {
        let automations = self
            .automation_service
            .list_automations(
                bound_caller.clone(),
                AutomationListRequest {
                    limit: NOTIFICATION_APPROVAL_AUTOMATION_LIMIT,
                    run_limit: NOTIFICATION_APPROVAL_RUN_LIMIT,
                    include_completed: true,
                },
            )
            .await?;

        let mut candidate_seen = HashSet::new();
        let mut candidates = Vec::with_capacity(NOTIFICATION_APPROVAL_CANDIDATE_LIMIT);
        for automation in &automations {
            let title = AutomationNotificationTitle::from_name(&automation.name);
            for run in &automation.recent_runs {
                if let Some(thread_id) = &run.thread_id {
                    if candidate_seen.insert(thread_id.clone()) {
                        candidates.push(AutomationApprovalThreadCandidate {
                            thread_id: thread_id.clone(),
                            title: title.clone(),
                        });
                    }
                    if candidates.len() >= NOTIFICATION_APPROVAL_CANDIDATE_LIMIT {
                        break;
                    }
                }
            }
            if candidates.len() >= NOTIFICATION_APPROVAL_CANDIDATE_LIMIT {
                break;
            }
        }
        Ok(candidates)
    }

    pub(super) async fn automation_run_thread_record(
        &self,
        caller: &ProductSurfaceCaller,
        bound_caller: &ProductAgentBoundCaller,
        thread_id: ThreadId,
        automation_title: Option<AutomationNotificationTitle>,
    ) -> Result<Option<AutomationApprovalThread>, ProductSurfaceError> {
        let Some(trigger_scope) = self
            .automation_service
            .resolve_run_thread_scope(bound_caller.clone(), &thread_id)
            .await?
        else {
            return Ok(None);
        };
        self.automation_run_thread_record_for_scope(
            caller,
            bound_caller,
            thread_id,
            trigger_scope,
            automation_title,
        )
        .await
    }

    pub(super) async fn automation_run_thread_record_for_scope(
        &self,
        caller: &ProductSurfaceCaller,
        bound_caller: &ProductAgentBoundCaller,
        thread_id: ThreadId,
        trigger_scope: TriggerRunThreadScope,
        title: Option<AutomationNotificationTitle>,
    ) -> Result<Option<AutomationApprovalThread>, ProductSurfaceError> {
        let true_agent_id = trigger_scope
            .agent_id
            .clone()
            .or_else(|| Some(bound_caller.agent_id.clone()));
        let creator_user_id = trigger_scope.creator_user_id.clone();

        let approval_turn_scope = TurnScope::new(
            caller.tenant_id.clone(),
            true_agent_id.clone(),
            trigger_scope.project_id.clone(),
            thread_id.clone(),
        );
        let run_actor = TurnActor::new(creator_user_id.clone());
        let approvals = self
            .pending_approvals_for_thread_scope(&approval_turn_scope, &run_actor)
            .await?;
        if approvals.is_empty() {
            return Ok(None);
        }

        let mut record = None;
        for owner_user_id in [Some(creator_user_id.clone()), None] {
            let thread_turn_scope = TurnScope::new_with_owner(
                caller.tenant_id.clone(),
                true_agent_id.clone(),
                trigger_scope.project_id.clone(),
                thread_id.clone(),
                owner_user_id,
            );
            let thread_scope = thread_scope_from_turn_scope(
                &thread_turn_scope,
                thread_turn_scope.explicit_owner_user_id().cloned(),
            )?;
            match self
                .thread_service
                .read_thread(ThreadHistoryRequest {
                    scope: thread_scope,
                    thread_id: thread_turn_scope.thread_id.clone(),
                })
                .await
            {
                Ok(found) => {
                    record = Some(found);
                    break;
                }
                Err(
                    SessionThreadError::UnknownThread { .. }
                    | SessionThreadError::ThreadScopeMismatch { .. },
                ) => {}
                Err(error) => return Err(map_ownership_probe_error(error)),
            }
        }
        let Some(mut record) = record else {
            return Ok(None);
        };
        if record
            .title
            .as_ref()
            .is_none_or(|title| title.trim().is_empty())
            && let Some(title) = title.as_ref()
        {
            record.title = Some(title.as_str().to_string());
        }
        Ok(Some(AutomationApprovalThread { record, approvals }))
    }
}
