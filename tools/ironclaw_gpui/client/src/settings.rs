//! Typed calls for settings (models, tool permissions), extensions and skills.

use std::collections::BTreeMap;

use ironclaw_product_contracts::inbound_requests::ProductSetupExtensionRequest;
use ironclaw_product_contracts::operator_llm::{
    LlmConfigSnapshot, LlmModelsResult, LlmProviderView, SetActiveLlmRequest,
    SetUserModelPolicyRequest, SetUserModelPreferenceRequest, UserModelCatalog,
    UserModelPreference,
};
use ironclaw_product_contracts::package_lifecycle::LifecyclePackageRef;
use ironclaw_product_contracts::product_wire::{
    RebornExtensionActionResponse, RebornExtensionRegistryResponse,
    RebornOperatorConfigGetResponse, RebornOperatorConfigListResponse,
    RebornSetupExtensionResponse, RebornSkillActionResponse, RebornSkillContentResponse,
    RebornSkillListResponse, RebornSkillSearchResponse, SettingsToolPermissionState,
};
use reqwest::Method;
use serde_json::json;

use crate::api::{Client, V2, client_action_id, seg};
use crate::error::ClientError;
use crate::wire::ExtensionList;

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
}
