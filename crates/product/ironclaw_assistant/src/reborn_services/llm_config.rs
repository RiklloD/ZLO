//! Product's half of the LLM configuration surface.
//!
//! The port itself — `LlmConfigService`, `ActiveModelReader`, and their request
//! and response DTOs — is declared in
//! `ironclaw_product_contracts::operator_llm`, because its implementation is
//! `ironclaw_operator`'s and a port belongs at the boundary its implementor
//! compiles against (PROPOSAL §6.1.3, §6.9.2; CHECKLIST WS5 operator row).
//!
//! What stays here is what product owns: the frozen `llm_config` view
//! descriptor, the fail-closed error for "no service wired", and the
//! `RebornServices` wiring that calls through the port.

use super::*;

use ironclaw_product_contracts::operator_llm::{
    SetUserModelPreferenceRequest, UserModelPreference,
};
use ironclaw_product_contracts::views::{RebornViewDescriptor, RebornViewProvider};

use ironclaw_product_contracts::surface::{
    ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceErrorCode, ProductSurfaceErrorKind,
    ProductSurfaceValidationCode,
};

use super::{ProductCapabilityInvoker, RebornServices};
use ironclaw_product_contracts::task_models::{
    TaskCompletionRequest, TaskCompletionResult, TaskModels,
};

use ironclaw_product_contracts::operator_llm::{
    LlmConfigSnapshot, SetActiveLlmRequest, SetUserModelPolicyRequest, UpsertLlmProviderRequest,
    UserModelCatalog,
};

pub const LLM_CONFIG_VIEW: RebornViewDescriptor = RebornViewDescriptor {
    id: "llm_config",
    paginated: false,
};

/// Error returned when an LLM-config method is invoked but no service is wired.
pub(super) fn llm_config_unavailable() -> ProductSurfaceError {
    ProductSurfaceError::from_status_kind(
        ProductSurfaceErrorCode::Unavailable,
        ProductSurfaceErrorKind::ServiceUnavailable,
        503,
        false,
    )
}

impl<I, V> RebornServices<I, V>
where
    I: ProductCapabilityInvoker + Clone + 'static,
    V: RebornViewProvider + Clone + 'static,
{
    pub(super) async fn task_models(
        &self,
        caller: ProductSurfaceCaller,
    ) -> Result<TaskModels, ProductSurfaceError> {
        let service = self
            .llm_config
            .as_ref()
            .ok_or_else(llm_config_unavailable)?;
        service
            .task_models(caller)
            .await
            .map_err(ProductSurfaceError::from)
    }

    pub(super) async fn set_task_models(
        &self,
        caller: ProductSurfaceCaller,
        models: TaskModels,
    ) -> Result<TaskModels, ProductSurfaceError> {
        let service = self
            .llm_config
            .as_ref()
            .ok_or_else(llm_config_unavailable)?;
        service
            .set_task_models(caller, models)
            .await
            .map_err(ProductSurfaceError::from)
    }

    pub(super) async fn complete_task(
        &self,
        caller: ProductSurfaceCaller,
        request: TaskCompletionRequest,
    ) -> Result<TaskCompletionResult, ProductSurfaceError> {
        let service = self
            .llm_config
            .as_ref()
            .ok_or_else(llm_config_unavailable)?;
        service
            .complete_task(caller, request)
            .await
            .map_err(ProductSurfaceError::from)
    }

    pub(super) async fn invoke_llm_provider_upsert(
        &self,
        caller: ProductSurfaceCaller,
        request: UpsertLlmProviderRequest,
    ) -> Result<(), ProductSurfaceError> {
        let service = self
            .llm_config
            .as_ref()
            .ok_or_else(llm_config_unavailable)?;
        super::validate_llm_base_url(request.base_url.as_deref())?;
        service
            .upsert_provider(caller, request)
            .await
            .map_err(ProductSurfaceError::from)?;
        Ok(())
    }

    pub(super) async fn invoke_llm_provider_delete(
        &self,
        caller: ProductSurfaceCaller,
        input: serde_json::Value,
    ) -> Result<(), ProductSurfaceError> {
        let service = self
            .llm_config
            .as_ref()
            .ok_or_else(llm_config_unavailable)?;
        let provider_id = input
            .as_object()
            .and_then(|object| object.get("provider_id"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| llm_config_input_error("provider_id"))?
            .to_string();
        service
            .delete_provider(caller, provider_id)
            .await
            .map_err(ProductSurfaceError::from)?;
        Ok(())
    }

    pub(super) async fn invoke_llm_active_set(
        &self,
        caller: ProductSurfaceCaller,
        input: serde_json::Value,
    ) -> Result<(), ProductSurfaceError> {
        let service = self
            .llm_config
            .as_ref()
            .ok_or_else(llm_config_unavailable)?;
        let request: SetActiveLlmRequest =
            serde_json::from_value(input).map_err(|_| llm_config_input_error("input"))?;
        service
            .set_active(caller, request)
            .await
            .map_err(ProductSurfaceError::from)?;
        Ok(())
    }

    pub(super) async fn invoke_user_model_policy_set(
        &self,
        caller: ProductSurfaceCaller,
        input: serde_json::Value,
    ) -> Result<(), ProductSurfaceError> {
        let service = self
            .llm_config
            .as_ref()
            .ok_or_else(llm_config_unavailable)?;
        let request: SetUserModelPolicyRequest =
            serde_json::from_value(input).map_err(|_| llm_config_input_error("input"))?;
        service
            .set_user_model_policy(caller, request)
            .await
            .map_err(ProductSurfaceError::from)?;
        Ok(())
    }

    pub(super) async fn invoke_user_model_preference_set(
        &self,
        caller: ProductSurfaceCaller,
        input: serde_json::Value,
    ) -> Result<(), ProductSurfaceError> {
        let service = self
            .llm_config
            .as_ref()
            .ok_or_else(llm_config_unavailable)?;
        let request: SetUserModelPreferenceRequest =
            serde_json::from_value(input).map_err(|_| llm_config_input_error("input"))?;
        service
            .set_user_model_preference(caller, request)
            .await
            .map_err(ProductSurfaceError::from)?;
        Ok(())
    }

    pub(super) async fn build_llm_config_view(
        &self,
        caller: ProductSurfaceCaller,
    ) -> Result<LlmConfigSnapshot, ProductSurfaceError> {
        let service = self
            .llm_config
            .as_ref()
            .ok_or_else(llm_config_unavailable)?;
        service
            .snapshot(caller)
            .await
            .map_err(ProductSurfaceError::from)
    }

    pub(super) async fn build_user_model_catalog_view(
        &self,
        caller: ProductSurfaceCaller,
    ) -> Result<UserModelCatalog, ProductSurfaceError> {
        let Some(service) = self.llm_config.as_ref() else {
            return Ok(UserModelCatalog::disabled());
        };
        service
            .user_model_catalog(caller)
            .await
            .map_err(ProductSurfaceError::from)
    }

    pub(super) async fn build_user_model_preference_view(
        &self,
        caller: ProductSurfaceCaller,
    ) -> Result<UserModelPreference, ProductSurfaceError> {
        let Some(service) = self.llm_config.as_ref() else {
            return Ok(UserModelPreference { model: None });
        };
        service
            .user_model_preference(caller)
            .await
            .map_err(ProductSurfaceError::from)
    }
}

fn llm_config_input_error(field: &'static str) -> ProductSurfaceError {
    ProductSurfaceError::validation(field, ProductSurfaceValidationCode::InvalidValue)
}

pub const LLM_PROVIDER_UPSERT_CAPABILITY_ID: &str = "builtin.llm_provider_upsert";

pub const LLM_PROVIDER_UPSERT_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(LLM_PROVIDER_UPSERT_CAPABILITY_ID);

pub const LLM_PROVIDER_DELETE_CAPABILITY_ID: &str = "builtin.llm_provider_delete";

pub const LLM_PROVIDER_DELETE_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(LLM_PROVIDER_DELETE_CAPABILITY_ID);

pub const LLM_ACTIVE_SET_CAPABILITY_ID: &str = "builtin.llm_active_set";

pub const LLM_ACTIVE_SET_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(LLM_ACTIVE_SET_CAPABILITY_ID);

pub const LLM_TEST_CONNECTION_COMMAND_ID: &str = "llm.test_connection";

pub const LLM_TEST_CONNECTION_COMMAND: ProductSurfaceCommandDescriptor<
    serde_json::Value,
    LlmProbeResult,
> = ProductSurfaceCommandDescriptor::new(LLM_TEST_CONNECTION_COMMAND_ID);

pub const LLM_LIST_MODELS_COMMAND_ID: &str = "llm.list_models";

pub const LLM_LIST_MODELS_COMMAND: ProductSurfaceCommandDescriptor<
    serde_json::Value,
    LlmModelsResult,
> = ProductSurfaceCommandDescriptor::new(LLM_LIST_MODELS_COMMAND_ID);

pub const LLM_NEARAI_LOGIN_COMMAND_ID: &str = "llm.nearai.login";

pub const LLM_NEARAI_LOGIN_COMMAND: ProductSurfaceCommandDescriptor<
    serde_json::Value,
    NearAiLoginStart,
> = ProductSurfaceCommandDescriptor::new(LLM_NEARAI_LOGIN_COMMAND_ID);

pub const LLM_NEARAI_WALLET_LOGIN_COMMAND_ID: &str = "llm.nearai.wallet_login";

pub const LLM_NEARAI_WALLET_LOGIN_COMMAND: ProductSurfaceCommandDescriptor<
    serde_json::Value,
    NearAiWalletLoginResult,
> = ProductSurfaceCommandDescriptor::new(LLM_NEARAI_WALLET_LOGIN_COMMAND_ID);

pub const LLM_CODEX_LOGIN_COMMAND_ID: &str = "llm.codex.login";

pub const LLM_CODEX_LOGIN_COMMAND: ProductSurfaceCommandDescriptor<
    EmptyProductCommandInput,
    CodexLoginStart,
> = ProductSurfaceCommandDescriptor::new(LLM_CODEX_LOGIN_COMMAND_ID);

pub(super) const LLM_BASE_URL_MAX_BYTES: usize = 2048;

/// Validate an operator-supplied LLM `base_url` before it is persisted or
/// probed.
///
/// Mirrors the `AllowPrivateNetwork` posture used at the model-discovery egress
/// point (`ironclaw_llm`'s `check_models_url`) and the binary's
/// `validate_operator_base_url`: a self-hosted provider on a loopback or private
/// address (Ollama, vLLM) is the primary local use case and must be allowed.
/// Only the never-legitimate classes — cloud metadata / link-local, multicast,
/// and the unspecified address — are rejected here. DNS-name hosts are resolved,
/// re-validated, and pinned by the egress guard; this syntactic check only
/// screens literal IPs.
pub(super) fn validate_llm_base_url(base_url: Option<&str>) -> Result<(), ProductSurfaceError> {
    let Some(raw) = base_url else {
        return Ok(());
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > LLM_BASE_URL_MAX_BYTES {
        return Err(operator_setup_validation_error("base_url"));
    }
    let parsed = Url::parse(trimmed).map_err(|error| {
        tracing::debug!(%error, "failed to parse operator setup base URL");
        operator_setup_validation_error("base_url")
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(operator_setup_validation_error("base_url"));
    }
    let Some(host) = parsed.host_str() else {
        return Err(operator_setup_validation_error("base_url"));
    };
    // `localhost` and loopback/private literals are intentionally allowed —
    // pointing the operator's provider at a self-hosted endpoint is the main
    // reason this field exists. Only literal IPs in the always-blocked classes
    // are rejected.
    let normalized_host = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = normalized_host.parse::<IpAddr>()
        && forbidden_llm_base_url_ip(ip)
    {
        return Err(operator_setup_validation_error("base_url"));
    }
    Ok(())
}

pub(super) fn forbidden_llm_base_url_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => forbidden_llm_base_url_ipv4(ip),
        IpAddr::V6(ip) => forbidden_llm_base_url_ipv6(ip),
    }
}

/// Always-blocked IPv4 classes: the unspecified address, multicast, and
/// link-local (which includes the cloud-metadata endpoint 169.254.169.254).
/// Loopback and private ranges are allowed so self-hosted providers work.
pub(super) fn forbidden_llm_base_url_ipv4(ip: Ipv4Addr) -> bool {
    ip.is_unspecified() || ip.is_multicast() || ip.is_link_local()
}

/// Always-blocked IPv6 classes: unspecified, multicast, and link-local.
/// Loopback (`::1`) and unique-local are allowed so self-hosted providers work.
/// Embedded-IPv4 forms (`::ffff:a.b.c.d` and `::a.b.c.d`) are unwrapped so an
/// IPv4-compatible metadata address can't slip through as a "plain" v6 host.
pub(super) fn forbidden_llm_base_url_ipv6(ip: Ipv6Addr) -> bool {
    if ip.is_unspecified() || ip.is_multicast() || ip.is_unicast_link_local() {
        return true;
    }
    if let Some(v4) = ip.to_ipv4() {
        return forbidden_llm_base_url_ipv4(v4);
    }
    false
}

impl<I, V> RebornServices<I, V>
where
    I: ProductCapabilityInvoker + Clone + 'static,
    V: RebornViewProvider + Clone + 'static,
{
    pub async fn test_llm_connection(
        &self,
        caller: ProductSurfaceCaller,
        request: LlmProbeRequest,
    ) -> Result<LlmProbeResult, ProductSurfaceError> {
        let service = self
            .llm_config
            .as_ref()
            .ok_or_else(llm_config::llm_config_unavailable)?;
        validate_llm_base_url(request.base_url.as_deref())?;
        service
            .test_connection(caller, request)
            .await
            .map_err(ProductSurfaceError::from)
    }

    pub async fn list_llm_models(
        &self,
        caller: ProductSurfaceCaller,
        request: LlmProbeRequest,
    ) -> Result<LlmModelsResult, ProductSurfaceError> {
        let service = self
            .llm_config
            .as_ref()
            .ok_or_else(llm_config::llm_config_unavailable)?;
        validate_llm_base_url(request.base_url.as_deref())?;
        service
            .list_models(caller, request)
            .await
            .map_err(ProductSurfaceError::from)
    }

    pub async fn start_nearai_login(
        &self,
        caller: ProductSurfaceCaller,
        request: NearAiLoginRequest,
    ) -> Result<NearAiLoginStart, ProductSurfaceError> {
        let service = self
            .llm_config
            .as_ref()
            .ok_or_else(llm_config::llm_config_unavailable)?;
        service
            .start_nearai_login(caller, request)
            .await
            .map_err(ProductSurfaceError::from)
    }

    pub async fn start_codex_login(
        &self,
        caller: ProductSurfaceCaller,
    ) -> Result<CodexLoginStart, ProductSurfaceError> {
        let service = self
            .llm_config
            .as_ref()
            .ok_or_else(llm_config::llm_config_unavailable)?;
        service
            .start_codex_login(caller)
            .await
            .map_err(ProductSurfaceError::from)
    }

    pub async fn complete_nearai_wallet_login(
        &self,
        caller: ProductSurfaceCaller,
        request: NearAiWalletLoginRequest,
    ) -> Result<NearAiWalletLoginResult, ProductSurfaceError> {
        let service = self
            .llm_config
            .as_ref()
            .ok_or_else(llm_config::llm_config_unavailable)?;
        service
            .complete_nearai_wallet_login(caller, request)
            .await
            .map_err(ProductSurfaceError::from)
    }
}
