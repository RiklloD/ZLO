//! `orchestrator_settings` domain handler (`orch_settings`).

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::model::Settings;
use ironclaw_product_contracts::config_sync::{
    ConfigSyncDomain, ConfigSyncDomainDigest, ConfigSyncDomainPayload,
    OrchestratorSettingsDomainPayload,
};

use super::{
    ConfigSyncContext, ConfigSyncDomainHandler, ConfigSyncError, DomainApplyResult,
    canonical_json_sha256,
};

pub struct OrchestratorSettingsDomainHandler {
    engine: Arc<Engine>,
}

impl OrchestratorSettingsDomainHandler {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self { engine }
    }

    /// The settings as their portable document.
    fn to_snapshot(settings: Settings) -> Result<serde_json::Value, ConfigSyncError> {
        serde_json::to_value(settings).map_err(|source| ConfigSyncError::Serialization {
            domain: ConfigSyncDomain::OrchestratorSettings,
            source,
        })
    }

    /// A received document as settings: the orchestrator's own decoding and
    /// its picker validation decide what is acceptable.
    fn from_snapshot(snapshot: &serde_json::Value) -> Result<Settings, ConfigSyncError> {
        let mut settings: Settings = serde_json::from_value(snapshot.clone()).map_err(|error| {
            ConfigSyncError::validation("orchestrator_settings", error.to_string())
        })?;
        settings.model_picker = settings.model_picker.validated().map_err(|reason| {
            ConfigSyncError::validation("orchestrator_settings.model_picker", reason)
        })?;
        Ok(settings)
    }

    fn hash_snapshot(snapshot: &serde_json::Value) -> Result<String, ConfigSyncError> {
        canonical_json_sha256(ConfigSyncDomain::OrchestratorSettings, snapshot)
    }
}

#[async_trait]
impl ConfigSyncDomainHandler for OrchestratorSettingsDomainHandler {
    fn domain(&self) -> ConfigSyncDomain {
        ConfigSyncDomain::OrchestratorSettings
    }

    async fn manifest(
        &self,
        _ctx: &ConfigSyncContext,
    ) -> Result<ConfigSyncDomainDigest, ConfigSyncError> {
        let current =
            self.engine
                .store()
                .settings()
                .await
                .map_err(|error| ConfigSyncError::Store {
                    domain: ConfigSyncDomain::OrchestratorSettings,
                    reason: error.to_string(),
                })?;
        let snapshot = Self::to_snapshot(current)?;
        let hash = Self::hash_snapshot(&snapshot)?;
        Ok(ConfigSyncDomainDigest {
            hash,
            updated_at: None,
            item_count: 1,
        })
    }

    async fn export(
        &self,
        _ctx: &ConfigSyncContext,
    ) -> Result<ConfigSyncDomainPayload, ConfigSyncError> {
        let current =
            self.engine
                .store()
                .settings()
                .await
                .map_err(|error| ConfigSyncError::Store {
                    domain: ConfigSyncDomain::OrchestratorSettings,
                    reason: error.to_string(),
                })?;
        let settings = Self::to_snapshot(current)?;
        let hash = Self::hash_snapshot(&settings)?;
        Ok(ConfigSyncDomainPayload::OrchestratorSettings(
            OrchestratorSettingsDomainPayload { hash, settings },
        ))
    }

    fn validate(&self, payload: &ConfigSyncDomainPayload) -> Result<(), ConfigSyncError> {
        let ConfigSyncDomainPayload::OrchestratorSettings(p) = payload else {
            return Err(ConfigSyncError::validation(
                "orchestrator_settings",
                "expected orchestrator_settings payload",
            ));
        };
        let _ = Self::from_snapshot(&p.settings)?;
        Ok(())
    }

    async fn apply(
        &self,
        _ctx: &ConfigSyncContext,
        payload: &ConfigSyncDomainPayload,
    ) -> Result<DomainApplyResult, ConfigSyncError> {
        let ConfigSyncDomainPayload::OrchestratorSettings(p) = payload else {
            return Err(ConfigSyncError::validation(
                "orchestrator_settings",
                "expected orchestrator_settings payload",
            ));
        };
        let desired = Self::from_snapshot(&p.settings)?;
        let current =
            self.engine
                .store()
                .settings()
                .await
                .map_err(|error| ConfigSyncError::Store {
                    domain: ConfigSyncDomain::OrchestratorSettings,
                    reason: error.to_string(),
                })?;
        if current == desired {
            return Ok(DomainApplyResult::unchanged(1));
        }

        let patch =
            serde_json::to_value(&desired).map_err(|source| ConfigSyncError::Serialization {
                domain: ConfigSyncDomain::OrchestratorSettings,
                source,
            })?;
        self.engine
            .store()
            .set_settings(&patch)
            .await
            .map_err(|error| ConfigSyncError::Store {
                domain: ConfigSyncDomain::OrchestratorSettings,
                reason: error.to_string(),
            })?;
        Ok(DomainApplyResult::applied(1, 0))
    }
}
