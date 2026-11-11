//! Operator status, logs, service lifecycle, first-run setup validation, and the
//! doctor/diagnostics projections (`operator` charter row).

use super::*;

pub const OPERATOR_SETUP_RUN_CAPABILITY_ID: &str = "builtin.operator_setup_run";

pub const OPERATOR_SETUP_RUN_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(OPERATOR_SETUP_RUN_CAPABILITY_ID);

pub const OPERATOR_SERVICE_LIFECYCLE_COMMAND_ID: &str = "operator.service.lifecycle";

pub const OPERATOR_SERVICE_LIFECYCLE_COMMAND: ProductSurfaceCommandDescriptor<
    RebornOperatorServiceLifecycleRequest,
    RebornOperatorCommandPlaneResponse,
> = ProductSurfaceCommandDescriptor::new(OPERATOR_SERVICE_LIFECYCLE_COMMAND_ID);

pub(super) const OPERATOR_LOGS_DEFAULT_LIMIT: u32 = 100;

pub(super) const OPERATOR_LOGS_MAX_LIMIT: u32 = 500;

pub(super) const OPERATOR_LOGS_CURSOR_MAX_BYTES: usize = 512;

pub(super) const OPERATOR_LOGS_TARGET_MAX_BYTES: usize = 256;

#[derive(Debug, Clone)]
pub struct StaticOperatorStatusService {
    response: RebornOperatorStatusResponse,
}

impl StaticOperatorStatusService {
    pub fn new(response: RebornOperatorStatusResponse) -> Self {
        Self { response }
    }
}

#[async_trait]
impl OperatorStatusService for StaticOperatorStatusService {
    async fn status(
        &self,
        _caller: ProductSurfaceCaller,
    ) -> Result<RebornOperatorStatusResponse, ProductSurfaceError> {
        Ok(self.response.clone())
    }
}

#[derive(Debug, Default)]
pub struct UnsupportedOperatorStatusService;

#[async_trait]
impl OperatorStatusService for UnsupportedOperatorStatusService {
    async fn status(
        &self,
        _caller: ProductSurfaceCaller,
    ) -> Result<RebornOperatorStatusResponse, ProductSurfaceError> {
        Err(operator_surface_unavailable())
    }
}

#[derive(Debug, Default)]
pub struct UnsupportedOperatorLogsService;

#[async_trait]
impl OperatorLogsService for UnsupportedOperatorLogsService {
    async fn query_logs(
        &self,
        _caller: ProductSurfaceCaller,
        _request: RebornLogQueryRequest,
    ) -> Result<RebornLogQueryResponse, ProductSurfaceError> {
        Err(operator_surface_unavailable())
    }
}

#[derive(Debug, Default)]
pub struct UnsupportedOperatorServiceLifecycleService;

#[async_trait]
impl OperatorServiceLifecycleService for UnsupportedOperatorServiceLifecycleService {
    async fn control_service(
        &self,
        _caller: ProductSurfaceCaller,
        request: RebornServiceLifecycleRequest,
    ) -> Result<RebornServiceLifecycleResponse, ProductSurfaceError> {
        Ok(RebornServiceLifecycleResponse {
            action: request.action,
            state: RebornServiceLifecycleState::Unsupported,
            message: "local service lifecycle management is not wired for this runtime".to_string(),
            remediation: Some(
                "use the host process manager directly until a platform lifecycle backend is configured"
                    .to_string(),
            ),
        })
    }
}

pub(super) fn operator_setup_validation_error(field: &str) -> ProductSurfaceError {
    ProductSurfaceError::validation(field, ProductSurfaceValidationCode::InvalidValue)
}

/// Stable WebUI-facing service surface for beta Reborn routes.
pub(super) fn operator_setup_diagnostic(
    key: &str,
    severity: RebornOperatorConfigDiagnosticSeverity,
    reason_code: &str,
    message: &str,
    remediation: &str,
) -> RebornOperatorConfigDiagnostic {
    RebornOperatorConfigDiagnostic {
        key: key.to_string(),
        severity,
        reason_code: reason_code.to_string(),
        message: message.to_string(),
        owning_area: RebornOperatorArea::Setup,
        remediation: remediation.to_string(),
    }
}

pub(super) const OPERATOR_SETUP_PROFILE_ID_MAX_BYTES: usize = 128;

pub(super) const OPERATOR_SETUP_WEBUI_TOKEN_MIN_BYTES: usize = 32;

pub(super) const OPERATOR_SETUP_WEBUI_TOKEN_MAX_BYTES: usize = 4096;

pub(super) const OPERATOR_SETUP_REDACTED_SECRET_SENTINEL: &str = "••••••••";

pub(super) fn validate_operator_setup_profile_id(
    profile_id: Option<&str>,
) -> Result<Option<String>, ProductSurfaceError> {
    let Some(profile_id) = profile_id else {
        return Ok(None);
    };
    let trimmed = profile_id.trim();
    if trimmed.is_empty() || trimmed.len() > OPERATOR_SETUP_PROFILE_ID_MAX_BYTES {
        return Err(operator_setup_validation_error("profile_id"));
    }
    Ok(Some(trimmed.to_string()))
}

pub(super) fn validate_operator_setup_webui_access_token(
    webui_access_token: Option<&SecretString>,
) -> Result<bool, ProductSurfaceError> {
    let Some(token) = webui_access_token else {
        return Ok(false);
    };
    let token = token.expose_secret().trim();
    if token == OPERATOR_SETUP_REDACTED_SECRET_SENTINEL {
        return Ok(false);
    }
    if token.len() < OPERATOR_SETUP_WEBUI_TOKEN_MIN_BYTES
        || token.len() > OPERATOR_SETUP_WEBUI_TOKEN_MAX_BYTES
    {
        return Err(operator_setup_validation_error("webui_access_token"));
    }
    Ok(true)
}

pub(super) fn reject_unwired_operator_setup_host_mutation(
    profile_id: Option<String>,
    webui_access_token_updated: bool,
) -> Result<(), ProductSurfaceError> {
    if profile_id.is_some() || webui_access_token_updated {
        return Err(ProductSurfaceError::service_unavailable(false));
    }
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub(super) struct OperatorSetupHostState {
    profile_id: Option<String>,
    webui_access_token_updated: bool,
}

pub(super) fn setup_response_from_llm_snapshot(
    snapshot: LlmConfigSnapshot,
    diagnostics: Vec<RebornOperatorConfigDiagnostic>,
    host_state: OperatorSetupHostState,
) -> RebornOperatorSetupResponse {
    let active_provider_id = snapshot
        .active
        .as_ref()
        .map(|active| active.provider_id.clone());
    let active_model = snapshot
        .active
        .as_ref()
        .and_then(|active| active.model.clone());
    let provider_complete = active_provider_id.is_some();
    let model_complete = active_model.is_some();
    let profile_message = host_state.profile_id.as_deref().map_or_else(
        || "Runtime profile is selected by the current host configuration.".to_string(),
        |profile_id| format!("Runtime profile `{profile_id}` was accepted by the setup API."),
    );
    let webui_access_message = if host_state.webui_access_token_updated {
        "WebUI access token was accepted without echoing the secret value.".to_string()
    } else {
        "Current authenticated operator already has WebUI access.".to_string()
    };

    let status = if provider_complete && model_complete {
        RebornOperatorSetupStatus::Complete
    } else {
        RebornOperatorSetupStatus::Incomplete
    };

    RebornOperatorSetupResponse {
        area: RebornOperatorArea::Setup,
        status,
        message: if provider_complete {
            "Provider setup is available through the operator setup API.".to_string()
        } else {
            "Provider setup is incomplete.".to_string()
        },
        active_provider_id,
        active_model,
        steps: vec![
            RebornOperatorSetupStep {
                name: "provider".to_string(),
                status: if provider_complete {
                    RebornOperatorSetupStepStatus::Complete
                } else {
                    RebornOperatorSetupStepStatus::Required
                },
                message: if provider_complete {
                    "An active provider is configured.".to_string()
                } else {
                    "Select a provider before first use.".to_string()
                },
            },
            RebornOperatorSetupStep {
                name: "model".to_string(),
                status: if model_complete {
                    RebornOperatorSetupStepStatus::Complete
                } else {
                    RebornOperatorSetupStepStatus::Required
                },
                message: if model_complete {
                    "An active model is configured.".to_string()
                } else {
                    "Select a model for the active provider.".to_string()
                },
            },
            RebornOperatorSetupStep {
                name: "profile".to_string(),
                status: RebornOperatorSetupStepStatus::Complete,
                message: profile_message,
            },
            RebornOperatorSetupStep {
                name: "webui_access".to_string(),
                status: RebornOperatorSetupStepStatus::Complete,
                message: webui_access_message,
            },
        ],
        diagnostics,
    }
}

pub(super) fn operator_doctor_status_diagnostic(
    check: &RebornOperatorStatusCheck,
) -> Option<RebornOperatorConfigDiagnostic> {
    if check.status == RebornOperatorStatusState::Ready {
        return None;
    }

    let severity = match check.severity {
        RebornOperatorStatusSeverity::Info => RebornOperatorConfigDiagnosticSeverity::Info,
        RebornOperatorStatusSeverity::Warning => RebornOperatorConfigDiagnosticSeverity::Warning,
        RebornOperatorStatusSeverity::Critical => RebornOperatorConfigDiagnosticSeverity::Error,
    };
    let state = match check.status {
        RebornOperatorStatusState::Ready => "ready",
        RebornOperatorStatusState::Degraded => "degraded",
        RebornOperatorStatusState::Blocked => "blocked",
        RebornOperatorStatusState::Unsupported => "unsupported",
        RebornOperatorStatusState::NotConfigured => "not_configured",
    };
    let reason_code = operator_doctor_status_reason_code(&check.id, state);
    let remediation = check
        .remediation
        .as_deref()
        .unwrap_or("inspect the corresponding operator status check");
    Some(RebornOperatorConfigDiagnostic {
        key: operator_doctor_status_text(&check.id),
        severity,
        reason_code,
        message: operator_doctor_status_text(&check.summary),
        owning_area: RebornOperatorArea::Status,
        remediation: operator_doctor_status_text(remediation),
    })
}

pub(super) fn operator_doctor_status_response(
    mut status: RebornOperatorStatusResponse,
) -> RebornOperatorStatusResponse {
    status.checks = status
        .checks
        .into_iter()
        .map(operator_doctor_status_check)
        .collect();
    status
}

pub(super) fn operator_doctor_status_check(
    mut check: RebornOperatorStatusCheck,
) -> RebornOperatorStatusCheck {
    check.id = operator_doctor_status_text(&check.id);
    check.summary = operator_doctor_status_text(&check.summary);
    check.remediation = check
        .remediation
        .as_deref()
        .map(operator_doctor_status_text);
    check
}

pub(super) fn operator_doctor_status_reason_code(check_id: &str, state: &str) -> String {
    if is_operator_doctor_reason_code_component(check_id)
        && !operator_doctor_status_text_needs_redaction(check_id)
    {
        format!("operator_doctor_{check_id}_{state}")
    } else {
        format!("operator_doctor_status_{state}")
    }
}

pub(super) fn is_operator_doctor_reason_code_component(value: &str) -> bool {
    let mut chars = value.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_lowercase())
        && value.len() <= 64
        && chars.all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_'
        })
}

pub(super) fn operator_doctor_status_text(value: &str) -> String {
    if operator_doctor_status_text_needs_redaction(value) {
        "[redacted operator status detail]".to_string()
    } else {
        value.to_string()
    }
}

pub(super) fn operator_doctor_status_text_needs_redaction(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("sk-")
        || lower.contains("/home/")
        || lower.contains("/workspace/")
        || lower.contains("\\users\\")
        || lower.contains("/users/")
        || lower.contains(".ssh")
        || lower.contains(".env")
        || lower.contains("api_key")
        || lower.contains("password")
        || lower.contains("credential")
}

pub(super) fn operator_doctor_setup_unavailable_diagnostic(
    reason_code: &str,
    message: &str,
) -> RebornOperatorConfigDiagnostic {
    operator_setup_diagnostic(
        "setup",
        RebornOperatorConfigDiagnosticSeverity::Error,
        reason_code,
        message,
        "Complete provider/model setup through the operator setup API or bootstrap configuration.",
    )
}

pub(super) fn operator_doctor_status_unavailable_diagnostic() -> RebornOperatorConfigDiagnostic {
    RebornOperatorConfigDiagnostic {
        key: "status".to_string(),
        severity: RebornOperatorConfigDiagnosticSeverity::Error,
        reason_code: "operator_doctor_status_unavailable".to_string(),
        message: "Operator status checks are unavailable.".to_string(),
        owning_area: RebornOperatorArea::Status,
        remediation: "wire the operator status service before relying on doctor diagnostics"
            .to_string(),
    }
}

pub(super) fn operator_diagnostics_surface_status(
    diagnostics: &[RebornOperatorConfigDiagnostic],
) -> RebornOperatorSurfaceStatus {
    if diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == RebornOperatorConfigDiagnosticSeverity::Error)
    {
        RebornOperatorSurfaceStatus::Unavailable
    } else {
        RebornOperatorSurfaceStatus::Available
    }
}

pub(super) fn operator_surface_unavailable() -> ProductSurfaceError {
    ProductSurfaceError::service_unavailable(false)
}

pub(super) fn validate_log_query_modes(
    tail: bool,
    follow: bool,
) -> Result<(), ProductSurfaceError> {
    if tail && follow {
        return Err(ProductSurfaceError::validation(
            "follow",
            ProductSurfaceValidationCode::InvalidValue,
        ));
    }
    Ok(())
}

pub(super) fn bounded_operator_logs_query(query: RebornOperatorLogsQuery) -> RebornLogQueryRequest {
    bounded_log_query(RebornLogQueryRequest {
        limit: query.limit,
        cursor: query.cursor,
        level: query.level,
        target: query.target,
        thread_id: query.thread_id,
        run_id: query.run_id,
        turn_id: query.turn_id,
        tool_call_id: query.tool_call_id,
        tool_name: query.tool_name,
        source: query.source,
        tail: query.tail,
        follow: query.follow,
    })
}

pub(super) fn bounded_log_query(query: RebornLogQueryRequest) -> RebornLogQueryRequest {
    RebornLogQueryRequest {
        limit: query
            .limit
            .map(|limit| limit.clamp(1, OPERATOR_LOGS_MAX_LIMIT))
            .or(Some(OPERATOR_LOGS_DEFAULT_LIMIT)),
        cursor: bounded_operator_logs_string(query.cursor, OPERATOR_LOGS_CURSOR_MAX_BYTES),
        level: query.level,
        target: bounded_operator_logs_string(query.target, OPERATOR_LOGS_TARGET_MAX_BYTES),
        thread_id: bounded_operator_logs_context_string(query.thread_id),
        run_id: bounded_operator_logs_context_string(query.run_id),
        turn_id: bounded_operator_logs_context_string(query.turn_id),
        tool_call_id: bounded_operator_logs_context_string(query.tool_call_id),
        tool_name: bounded_operator_logs_context_string(query.tool_name),
        source: bounded_operator_logs_context_string(query.source),
        tail: query.tail,
        follow: query.follow,
    }
}

pub(super) fn bounded_operator_logs_string(
    value: Option<String>,
    max_bytes: usize,
) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else if trimmed.len() <= max_bytes {
            Some(trimmed.to_string())
        } else {
            Some(truncate_utf8_to_bytes(trimmed, max_bytes))
        }
    })
}

pub(super) fn bounded_operator_logs_context_string(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(normalize_operator_log_context_value(trimmed))
        }
    })
}

impl<I, V> RebornServices<I, V>
where
    I: ProductCapabilityInvoker + Clone + 'static,
    V: RebornViewProvider + Clone + 'static,
{
    pub(super) async fn invoke_operator_setup_run(
        &self,
        caller: ProductSurfaceCaller,
        input: serde_json::Value,
    ) -> Result<(), ProductSurfaceError> {
        let request: RebornOperatorSetupRequest =
            serde_json::from_value(input).map_err(|error| {
                tracing::debug!(?error, "failed to decode operator setup input");
                operator_setup_validation_error("input")
            })?;
        self.apply_operator_setup_request(caller.clone(), request)
            .await?;
        self.build_operator_setup_view(caller).await?;
        Ok(())
    }

    pub(super) async fn apply_operator_setup_request(
        &self,
        caller: ProductSurfaceCaller,
        request: RebornOperatorSetupRequest,
    ) -> Result<(), ProductSurfaceError> {
        if self.llm_config.is_none() {
            return Err(llm_config::llm_config_unavailable());
        }

        if request.model.is_some() && request.provider_id.is_none() {
            return Err(operator_setup_validation_error("model"));
        }
        if request.provider_id.is_none()
            && (request.adapter.is_some()
                || request.base_url.is_some()
                || request.api_key.is_some())
        {
            return Err(operator_setup_validation_error("provider_id"));
        }
        if request.base_url.is_some() && request.adapter.is_none() {
            return Err(operator_setup_validation_error("base_url"));
        }
        if request.api_key.is_some() && request.adapter.is_none() {
            return Err(operator_setup_validation_error("api_key"));
        }
        validate_llm_base_url(request.base_url.as_deref())?;
        let profile_id = validate_operator_setup_profile_id(request.profile_id.as_deref())?;
        let webui_access_token_updated =
            validate_operator_setup_webui_access_token(request.webui_access_token.as_ref())?;
        reject_unwired_operator_setup_host_mutation(profile_id, webui_access_token_updated)?;

        match (request.provider_id, request.adapter) {
            (Some(provider_id), Some(adapter)) => {
                let model = request.model;
                self.invoke_llm_provider_upsert(
                    caller.clone(),
                    UpsertLlmProviderRequest {
                        id: provider_id,
                        client_action_id: None,
                        name: None,
                        adapter,
                        base_url: request.base_url,
                        default_model: model.clone(),
                        api_key: request.api_key,
                        set_active: true,
                        model,
                    },
                )
                .await?;
            }
            (Some(provider_id), None) => {
                self.invoke_llm_active_set(
                    caller,
                    serde_json::json!({
                        "provider_id": provider_id,
                        "model": request.model,
                    }),
                )
                .await?;
            }
            (None, _) => {}
        }

        Ok(())
    }

    pub async fn run_operator_setup(
        &self,
        caller: ProductSurfaceCaller,
        request: RebornOperatorSetupRequest,
    ) -> Result<RebornOperatorSetupResponse, ProductSurfaceError> {
        self.apply_operator_setup_request(caller.clone(), request)
            .await?;
        self.build_operator_setup_view(caller).await
    }

    pub async fn run_operator_service_lifecycle(
        &self,
        caller: ProductSurfaceCaller,
        request: RebornOperatorServiceLifecycleRequest,
    ) -> Result<RebornOperatorCommandPlaneResponse, ProductSurfaceError> {
        let request = RebornServiceLifecycleRequest {
            action: match request.action {
                RebornOperatorServiceLifecycleAction::Install => {
                    RebornServiceLifecycleAction::Install
                }
                RebornOperatorServiceLifecycleAction::Start => RebornServiceLifecycleAction::Start,
                RebornOperatorServiceLifecycleAction::Stop => RebornServiceLifecycleAction::Stop,
                RebornOperatorServiceLifecycleAction::Status => {
                    RebornServiceLifecycleAction::Status
                }
            },
        };
        let service_lifecycle = self
            .operator_service_lifecycle
            .control_service(caller, request)
            .await?;
        let status = match service_lifecycle.state {
            RebornServiceLifecycleState::Installed
            | RebornServiceLifecycleState::Running
            | RebornServiceLifecycleState::Stopped
            | RebornServiceLifecycleState::Unknown => RebornOperatorSurfaceStatus::Available,
            RebornServiceLifecycleState::Unsupported | RebornServiceLifecycleState::Failed => {
                RebornOperatorSurfaceStatus::Unavailable
            }
        };
        Ok(RebornOperatorCommandPlaneResponse {
            area: RebornOperatorArea::ServiceLifecycle,
            status,
            message: service_lifecycle.message.clone(),
            operator_status: None,
            logs: None,
            service_lifecycle: Some(service_lifecycle),
            diagnostics: Vec::new(),
        })
    }
}
