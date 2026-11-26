//! The Supermemory API key as a Settings → Keys row
//! (`ironclaw_product_contracts::service_keys`): its descriptor and a
//! verifier that proves a pasted key with one authenticated call.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_product_contracts::service_keys::{
    ServiceKeyDescriptor, ServiceKeyVerifier, ServiceKeyVerifyError,
};
use ironclaw_product_contracts::surface::ProductSurfaceCaller;
use secrecy::SecretString;

use crate::binding::SupermemoryCaller;
use crate::config::SUPERMEMORY_API_KEY_SECRET;
use crate::error::SupermemoryError;
use crate::service::Supermemory;

/// The environment variable Supermemory's own tools and the Hermes
/// `supermemory` plugin keep the key in.
pub const SUPERMEMORY_API_KEY_ENV: &str = "SUPERMEMORY_API_KEY";

pub fn supermemory_service_key() -> ServiceKeyDescriptor {
    ServiceKeyDescriptor {
        id: SUPERMEMORY_API_KEY_SECRET.to_string(),
        label: "Supermemory".to_string(),
        purpose: "Cloud memory: agents recall from and save to your Supermemory account \
                  instead of local memory."
            .to_string(),
        docs_url: Some("https://console.supermemory.ai".to_string()),
        import_env_var: Some(SUPERMEMORY_API_KEY_ENV.to_string()),
    }
}

pub struct SupermemoryKeyVerifier {
    supermemory: Arc<Supermemory>,
}

impl SupermemoryKeyVerifier {
    pub fn new(supermemory: Arc<Supermemory>) -> Self {
        Self { supermemory }
    }
}

#[async_trait]
impl ServiceKeyVerifier for SupermemoryKeyVerifier {
    async fn verify(
        &self,
        caller: &ProductSurfaceCaller,
        key: &SecretString,
    ) -> Result<(), ServiceKeyVerifyError> {
        let caller = SupermemoryCaller {
            tenant_id: caller.tenant_id.clone(),
            user_id: caller.user_id.clone(),
            agent_id: caller.agent_id.clone(),
            project_id: caller.project_id.clone(),
            thread_id: None,
        };
        match self.supermemory.client().verify_key(&caller, key).await {
            Ok(()) => Ok(()),
            Err(SupermemoryError::Unauthorized { .. }) => Err(ServiceKeyVerifyError::Rejected),
            Err(error) => Err(ServiceKeyVerifyError::Unreachable {
                reason: error.to_string(),
            }),
        }
    }

    async fn key_changed(&self) {
        self.supermemory.key_changed();
    }
}
