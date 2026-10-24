//! Profiles: list (most recently used first), read, create, update,
//! archive, start a thread on one, and the one-time Hermes import.
//!
//! Profile routes answer a correctable failure with HTTP 200 and the
//! orchestrator's `{"ok": false, "error", "message"}` document; it comes back
//! here as [`ClientError::Api`] carrying the backend's message.

use ironclaw_product_contracts::profiles::{
    HermesImportResponse, ProfileInfo, ProfileListResponse, ProfileSettings,
    ProfileThreadCreateResponse, ProfileWriteRequest,
};
use reqwest::Method;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::api::{Client, V2, client_action_id, seg};
use crate::error::{ApiErrorBody, ClientError};

pub(crate) fn document<T: DeserializeOwned>(value: Value) -> Result<T, ClientError> {
    if value.get("ok").and_then(Value::as_bool) == Some(false) {
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_string);
        return Err(ClientError::Api {
            status: 200,
            message: text("message").unwrap_or_else(|| "Request failed".into()),
            body: Box::new(ApiErrorBody {
                error: text("error"),
                ..ApiErrorBody::default()
            }),
        });
    }
    serde_json::from_value(value).map_err(|e| ClientError::Decode(e.to_string()))
}

impl Client {
    pub async fn profiles(&self) -> Result<ProfileListResponse, ClientError> {
        document(self.get(&format!("{V2}/profiles")).await?)
    }

    pub async fn profile(&self, profile_id: &str) -> Result<ProfileInfo, ClientError> {
        document(
            self.get(&format!("{V2}/profiles/{}", seg(profile_id)))
                .await?,
        )
    }

    pub async fn create_profile(
        &self,
        profile_id: &str,
        settings: ProfileSettings,
    ) -> Result<ProfileInfo, ClientError> {
        let body = ProfileWriteRequest {
            profile_id: profile_id.to_string(),
            settings,
        };
        document(self.post(&format!("{V2}/profiles"), &body).await?)
    }

    pub async fn update_profile(
        &self,
        profile_id: &str,
        settings: &ProfileSettings,
    ) -> Result<ProfileInfo, ClientError> {
        document(
            self.post(&format!("{V2}/profiles/{}", seg(profile_id)), settings)
                .await?,
        )
    }

    /// Archives the profile; its threads keep it.
    pub async fn delete_profile(&self, profile_id: &str) -> Result<(), ClientError> {
        let path = format!("{V2}/profiles/{}", seg(profile_id));
        let value: Value = self.send(self.request(Method::DELETE, &path)).await?;
        document::<Value>(value).map(|_| ())
    }

    /// A new thread that belongs to `profile_id` for life (the inbox picker).
    pub async fn create_profile_thread(
        &self,
        profile_id: &str,
    ) -> Result<ProfileThreadCreateResponse, ClientError> {
        let body = serde_json::json!({ "client_action_id": client_action_id() });
        let path = format!("{V2}/profiles/{}/threads", seg(profile_id));
        document(self.post(&path, &body).await?)
    }

    pub async fn import_hermes_profiles(&self) -> Result<HermesImportResponse, ClientError> {
        document(
            self.post(
                &format!("{V2}/profiles/import/hermes"),
                &serde_json::json!({}),
            )
            .await?,
        )
    }
}
