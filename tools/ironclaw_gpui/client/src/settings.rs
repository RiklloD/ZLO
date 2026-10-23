//! Typed calls for settings (models, tool permissions), extensions and skills.

use std::collections::BTreeMap;

use ironclaw_extension_contracts::hosted_mcp::HostedMcpAuthSelection;
use ironclaw_product_contracts::inbound_requests::ProductSetupExtensionRequest;
use ironclaw_product_contracts::operator_llm::{
    CodexLoginStart, LlmConfigSnapshot, LlmModelsResult, LlmProbeResult, LlmProviderView,
    SetActiveLlmRequest, SetUserModelPolicyRequest, SetUserModelPreferenceRequest,
    UserModelCatalog, UserModelPreference,
};
use ironclaw_product_contracts::package_lifecycle::LifecyclePackageRef;
use ironclaw_product_contracts::product_wire::{
    RebornAccountLoginLinkResponse, RebornAccountTracesResponse, RebornExtensionActionResponse,
    RebornExtensionRegistryResponse, RebornOperatorConfigGetResponse,
    RebornOperatorConfigListResponse, RebornSetupExtensionResponse, RebornSkillActionResponse,
    RebornSkillContentResponse, RebornSkillListResponse, RebornSkillSearchResponse,
    RebornTraceCreditsResponse, RebornTraceHoldAuthorizeResponse, SettingsToolPermissionState,
};
use reqwest::Method;
use serde_json::json;

use crate::api::{Client, V2, client_action_id, seg};
use crate::error::ClientError;
use crate::wire::{ExtensionList, HostedMcpRegistered, PairingCode, PairingStatus};

impl Client {
    // --- tool permissions ---

    /// `agent.auto_approve_tools` plus one `tool.<capability>` entry per tool.
    pub async fn tool_settings(&self) -> Result<RebornOperatorConfigListResponse, ClientError> {
        self.get(&format!("{V2}/settings/tools")).await
    }

    pub async fn set_auto_approve(
        &self,
        enabled: bool,
    ) -> Result<RebornOperatorConfigGetResponse, ClientError> {
        self.post(
            &format!("{V2}/settings/tools"),
            &json!({ "enabled": enabled }),
        )
        .await
    }

    pub async fn set_tool_permission(
        &self,
        capability_id: &str,
        state: SettingsToolPermissionState,
    ) -> Result<RebornOperatorConfigGetResponse, ClientError> {
        self.post(
            &format!("{V2}/settings/tools/{}", seg(capability_id)),
            &json!({ "state": state }),
        )
        .await
    }

    // --- models ---

    pub async fn llm_config(&self) -> Result<LlmConfigSnapshot, ClientError> {
        self.get(&format!("{V2}/llm/providers")).await
    }

    pub async fn set_active_llm(
        &self,
        provider_id: &str,
        model: Option<String>,
    ) -> Result<LlmConfigSnapshot, ClientError> {
        let body = SetActiveLlmRequest {
            provider_id: provider_id.to_string(),
            model,
        };
        self.post(&format!("{V2}/llm/active"), &body).await
    }

    // --- quick model switching (policy + per-user preference) ---

    /// The models a user may switch between; `selection_enabled` is false
    /// until an operator saved a list for the active provider.
    pub async fn model_catalog(&self) -> Result<UserModelCatalog, ClientError> {
        self.get(&format!("{V2}/llm/models")).await
    }

    /// Replace the switchable list for the active provider (operator only).
    pub async fn set_model_policy(
        &self,
        workspace_default: String,
        allowed_models: Vec<String>,
    ) -> Result<UserModelCatalog, ClientError> {
        let body = SetUserModelPolicyRequest {
            workspace_default,
            allowed_models,
            model_entries: None,
        };
        self.send(
            self.request(Method::PUT, &format!("{V2}/llm/model-policy"))
                .json(&body),
        )
        .await
    }

    pub async fn model_preference(&self) -> Result<UserModelPreference, ClientError> {
        self.get(&format!("{V2}/llm/model-preference")).await
    }

    /// `None` goes back to the workspace default.
    pub async fn set_model_preference(
        &self,
        model: Option<String>,
    ) -> Result<UserModelPreference, ClientError> {
        let body = SetUserModelPreferenceRequest { model };
        self.send(
            self.request(Method::PUT, &format!("{V2}/llm/model-preference"))
                .json(&body),
        )
        .await
    }

    /// Uses the provider's stored key; nothing secret leaves the app.
    pub async fn list_models(&self, p: &LlmProviderView) -> Result<LlmModelsResult, ClientError> {
        let body = json!({
            "provider_id": p.id,
            "adapter": p.adapter,
            "base_url": p.base_url,
        });
        self.post(&format!("{V2}/llm/list-models"), &body).await
    }

    /// Probe the provider with its stored key and default model.
    pub async fn test_llm_connection(
        &self,
        p: &LlmProviderView,
    ) -> Result<LlmProbeResult, ClientError> {
        let body = json!({
            "provider_id": p.id,
            "adapter": p.adapter,
            "base_url": p.base_url,
            "model": p.active_model.as_ref().unwrap_or(&p.default_model),
        });
        self.post(&format!("{V2}/llm/test-connection"), &body).await
    }

    /// Remove an operator-defined provider and its stored key.
    pub async fn delete_llm_provider(&self, id: &str) -> Result<LlmConfigSnapshot, ClientError> {
        self.post(
            &format!("{V2}/llm/providers/{}/delete", seg(id)),
            &json!({}),
        )
        .await
    }

    /// Start a ChatGPT device-code login. The gateway finishes it in the
    /// background and makes Codex active once the code is entered.
    pub async fn start_codex_login(&self) -> Result<CodexLoginStart, ClientError> {
        self.post(&format!("{V2}/llm/codex/login"), &json!({}))
            .await
    }

    /// Store a new API key for an existing provider. The gateway encrypts it;
    /// the request is the only place the key exists outside the secret store.
    pub async fn set_provider_key(
        &self,
        p: &LlmProviderView,
        api_key: &str,
    ) -> Result<LlmConfigSnapshot, ClientError> {
        let body = json!({
            "id": p.id,
            "client_action_id": client_action_id(),
            "adapter": p.adapter,
            "base_url": p.base_url,
            "default_model": p.default_model,
            "api_key": api_key,
        });
        self.post(&format!("{V2}/llm/providers"), &body).await
    }

    // --- extensions ---

    pub async fn extensions(&self) -> Result<ExtensionList, ClientError> {
        self.get(&format!("{V2}/extensions")).await
    }

    pub async fn extension_registry(&self) -> Result<RebornExtensionRegistryResponse, ClientError> {
        self.get(&format!("{V2}/extensions/registry")).await
    }

    pub async fn install_extension(
        &self,
        package_ref: &LifecyclePackageRef,
    ) -> Result<RebornExtensionActionResponse, ClientError> {
        let body = json!({ "package_ref": package_ref, "client_action_id": client_action_id() });
        self.post(&format!("{V2}/extensions/install"), &body).await
    }

    pub async fn remove_extension(
        &self,
        id: &str,
    ) -> Result<RebornExtensionActionResponse, ClientError> {
        let body = json!({ "client_action_id": client_action_id() });
        self.post(&format!("{V2}/extensions/{}/remove", seg(id)), &body)
            .await
    }

    pub async fn extension_setup(
        &self,
        id: &str,
    ) -> Result<RebornSetupExtensionResponse, ClientError> {
        let body = ProductSetupExtensionRequest {
            client_action_id: Some(client_action_id()),
            action: None,
            payload: None,
        };
        self.post(&format!("{V2}/extensions/{}/setup", seg(id)), &body)
            .await
    }

    pub async fn submit_extension_setup(
        &self,
        id: &str,
        secrets: BTreeMap<String, String>,
        fields: BTreeMap<String, String>,
    ) -> Result<RebornSetupExtensionResponse, ClientError> {
        let body = ProductSetupExtensionRequest {
            client_action_id: Some(client_action_id()),
            action: Some("submit".to_string()),
            payload: Some(json!({ "secrets": secrets, "fields": fields })),
        };
        self.post(&format!("{V2}/extensions/{}/setup", seg(id)), &body)
            .await
    }

    /// Channel pairing: a short code the user sends to the bot, plus a deep
    /// link that pre-fills it.
    pub async fn mint_pairing_code(&self, id: &str) -> Result<PairingCode, ClientError> {
        self.post(
            &format!("{V2}/extensions/{}/pairing/mint", seg(id)),
            &json!({}),
        )
        .await
    }

    pub async fn pairing_status(&self, id: &str) -> Result<PairingStatus, ClientError> {
        self.get(&format!("{V2}/extensions/{}/pairing/status", seg(id)))
            .await
    }

    pub async fn unpair_extension(&self, id: &str) -> Result<(), ClientError> {
        self.send_unit(self.request(
            Method::POST,
            &format!("{V2}/extensions/{}/pairing/unpair", seg(id)),
        ))
        .await
    }

    /// Register a remote MCP server as an extension. The gateway probes the
    /// endpoint; credentials are entered afterwards in setup.
    pub async fn register_hosted_mcp(
        &self,
        id: &str,
        name: &str,
        endpoint: &str,
        auth: HostedMcpAuthSelection,
    ) -> Result<HostedMcpRegistered, ClientError> {
        let body = json!({
            "desired_id": id,
            "desired_name": name,
            "endpoint": endpoint,
            "auth_selection": auth,
        });
        self.post(&format!("{V2}/extensions/register-hosted-mcp"), &body)
            .await
    }

    /// Upload a tool bundle zip (operator only).
    pub async fn import_extension(
        &self,
        bundle: Vec<u8>,
    ) -> Result<RebornExtensionActionResponse, ClientError> {
        self.send(
            self.request(Method::POST, &format!("{V2}/extensions/import"))
                .header(reqwest::header::CONTENT_TYPE, "application/zip")
                .body(bundle),
        )
        .await
    }

    // --- skills ---

    pub async fn skills(&self) -> Result<RebornSkillListResponse, ClientError> {
        self.get(&format!("{V2}/skills")).await
    }

    pub async fn skill_content(
        &self,
        name: &str,
    ) -> Result<RebornSkillContentResponse, ClientError> {
        self.get(&format!("{V2}/skills/{}", seg(name))).await
    }

    pub async fn set_skill_auto_activate(
        &self,
        name: &str,
        enabled: bool,
    ) -> Result<RebornSkillActionResponse, ClientError> {
        self.post(
            &format!("{V2}/skills/{}/auto-activate", seg(name)),
            &json!({ "enabled": enabled }),
        )
        .await
    }

    /// Replace an editable skill's SKILL.md; the gateway re-validates it.
    pub async fn update_skill(
        &self,
        name: &str,
        content: &str,
    ) -> Result<RebornSkillActionResponse, ClientError> {
        self.send(
            self.request(Method::PUT, &format!("{V2}/skills/{}", seg(name)))
                .json(&json!({ "content": content })),
        )
        .await
    }

    /// Master switch for criteria-based auto-activation of all skills.
    pub async fn set_auto_activate_learned(
        &self,
        enabled: bool,
    ) -> Result<RebornSkillActionResponse, ClientError> {
        self.post(
            &format!("{V2}/skills/auto-activate-learned"),
            &json!({ "enabled": enabled }),
        )
        .await
    }

    pub async fn remove_skill(&self, name: &str) -> Result<RebornSkillActionResponse, ClientError> {
        self.send(self.request(Method::DELETE, &format!("{V2}/skills/{}", seg(name))))
            .await
    }

    pub async fn search_skills(
        &self,
        query: &str,
    ) -> Result<RebornSkillSearchResponse, ClientError> {
        self.post(&format!("{V2}/skills/search"), &json!({ "query": query }))
            .await
    }

    /// Install a catalog skill by name; the gateway fetches and scans it.
    pub async fn install_skill(
        &self,
        name: &str,
    ) -> Result<RebornSkillActionResponse, ClientError> {
        self.post(&format!("{V2}/skills/install"), &json!({ "name": name }))
            .await
    }

    // --- Trace Commons ---

    /// Local credit summary; the unenrolled zero-state is not an error.
    pub async fn trace_credits(&self) -> Result<RebornTraceCreditsResponse, ClientError> {
        self.get(&format!("{V2}/traces/credit")).await
    }

    pub async fn account_traces(&self) -> Result<RebornAccountTracesResponse, ClientError> {
        self.get(&format!("{V2}/traces/account")).await
    }

    /// Submit a trace held for manual privacy review as-is.
    pub async fn authorize_trace_hold(
        &self,
        submission_id: &str,
    ) -> Result<RebornTraceHoldAuthorizeResponse, ClientError> {
        let path = format!("{V2}/traces/holds/{}/authorize", seg(submission_id));
        self.post(&path, &json!({})).await
    }

    /// Mint a one-time login link. Every call burns a link server-side, and the
    /// URL is an account credential: open it, never log or store it.
    pub async fn trace_account_login_link(
        &self,
    ) -> Result<RebornAccountLoginLinkResponse, ClientError> {
        self.post(&format!("{V2}/traces/account-login-link"), &json!({}))
            .await
    }
}
