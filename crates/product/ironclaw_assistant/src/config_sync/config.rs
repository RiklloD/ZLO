//! `config` domain handler: portable sections of `config.toml`.
//!
//! Machine-local sections (`[boot]`, `[identity]`, `[storage]`, `[webui]`,
//! `[remote]`, `[update]`) are never exported and never overwritten on apply.

use std::collections::BTreeMap;
use std::path::PathBuf;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ironclaw_config::{
    PortableConfigSections, RebornConfigFile, apply_portable_config_file,
    extract_portable_sections, validate_portable_sections,
};
use ironclaw_product_contracts::config_sync::{
    ConfigDomainPayload, ConfigSyncDomain, ConfigSyncDomainDigest, ConfigSyncDomainPayload,
    DomainApplyOutcome, DomainApplyStatus,
};
use serde_json::Value;

use super::{
    ConfigSyncContext, ConfigSyncDomainHandler, ConfigSyncError, DomainApplyResult,
    canonical_json_sha256, rewrite_json_strings,
};

pub struct ConfigDomainHandler {
    config_path: PathBuf,
}

impl ConfigDomainHandler {
    pub fn new(config_path: PathBuf) -> Self {
        Self { config_path }
    }

    fn read_portable_sections(
        &self,
    ) -> Result<(PortableConfigSections, Option<DateTime<Utc>>), ConfigSyncError> {
        let updated_at = std::fs::metadata(&self.config_path)
            .ok()
            .and_then(|meta| meta.modified().ok())
            .map(DateTime::<Utc>::from);
        let file =
            RebornConfigFile::load(&self.config_path).map_err(|error| ConfigSyncError::Store {
                domain: ConfigSyncDomain::Config,
                reason: error.to_string(),
            })?;
        let portable = match file.as_ref() {
            Some(file) => {
                extract_portable_sections(file).map_err(|error| ConfigSyncError::Store {
                    domain: ConfigSyncDomain::Config,
                    reason: error.to_string(),
                })?
            }
            None => PortableConfigSections::default(),
        };
        Ok((portable, updated_at))
    }

    fn portable_to_normalized_map(
        portable: &PortableConfigSections,
        ctx: &ConfigSyncContext,
    ) -> Result<BTreeMap<String, Value>, ConfigSyncError> {
        let value =
            serde_json::to_value(portable).map_err(|source| ConfigSyncError::Serialization {
                domain: ConfigSyncDomain::Config,
                source,
            })?;
        let rewritten = rewrite_json_strings(&value, &ctx.to_tokens_rewrite());
        let Some(obj) = rewritten.as_object() else {
            return Ok(BTreeMap::new());
        };
        let mut out = BTreeMap::new();
        for (k, v) in obj {
            // Drop null or empty tables so absent vs empty compare identically.
            if v.is_null() || matches!(v, Value::Object(m) if m.is_empty()) {
                continue;
            }
            out.insert(k.clone(), v.clone());
        }
        Ok(out)
    }

    fn sections_map_to_portable(
        sections: &BTreeMap<String, Value>,
        ctx: Option<&ConfigSyncContext>,
    ) -> Result<PortableConfigSections, ConfigSyncError> {
        let mut value =
            serde_json::to_value(sections).map_err(|source| ConfigSyncError::Serialization {
                domain: ConfigSyncDomain::Config,
                source,
            })?;
        if let Some(ctx) = ctx {
            value = rewrite_json_strings(&value, &ctx.from_tokens_rewrite());
        }
        serde_json::from_value(value).map_err(|error| {
            ConfigSyncError::validation(
                "config.sections",
                format!("invalid or non-portable config section: {error}"),
            )
        })
    }
}

#[async_trait]
impl ConfigSyncDomainHandler for ConfigDomainHandler {
    fn domain(&self) -> ConfigSyncDomain {
        ConfigSyncDomain::Config
    }

    async fn manifest(
        &self,
        ctx: &ConfigSyncContext,
    ) -> Result<ConfigSyncDomainDigest, ConfigSyncError> {
        let (portable, updated_at) = self.read_portable_sections()?;
        let sections = Self::portable_to_normalized_map(&portable, ctx)?;
        let value =
            serde_json::to_value(&sections).map_err(|source| ConfigSyncError::Serialization {
                domain: ConfigSyncDomain::Config,
                source,
            })?;
        let hash = canonical_json_sha256(ConfigSyncDomain::Config, &value)?;
        Ok(ConfigSyncDomainDigest {
            hash,
            updated_at,
            item_count: sections.len() as u64,
        })
    }

    async fn export(
        &self,
        ctx: &ConfigSyncContext,
    ) -> Result<ConfigSyncDomainPayload, ConfigSyncError> {
        let (portable, _) = self.read_portable_sections()?;
        let sections = Self::portable_to_normalized_map(&portable, ctx)?;
        let value =
            serde_json::to_value(&sections).map_err(|source| ConfigSyncError::Serialization {
                domain: ConfigSyncDomain::Config,
                source,
            })?;
        let hash = canonical_json_sha256(ConfigSyncDomain::Config, &value)?;
        Ok(ConfigSyncDomainPayload::Config(ConfigDomainPayload {
            hash,
            sections,
        }))
    }

    fn validate(&self, payload: &ConfigSyncDomainPayload) -> Result<(), ConfigSyncError> {
        let ConfigSyncDomainPayload::Config(config_payload) = payload else {
            return Err(ConfigSyncError::validation(
                "config",
                "expected config payload",
            ));
        };
        let portable = Self::sections_map_to_portable(&config_payload.sections, None)?;
        validate_portable_sections(&portable)
            .map_err(|error| ConfigSyncError::validation("config.sections", error.to_string()))
    }

    async fn apply(
        &self,
        ctx: &ConfigSyncContext,
        payload: &ConfigSyncDomainPayload,
    ) -> Result<DomainApplyResult, ConfigSyncError> {
        let ConfigSyncDomainPayload::Config(config_payload) = payload else {
            return Err(ConfigSyncError::validation(
                "config",
                "expected config payload",
            ));
        };
        let incoming = Self::sections_map_to_portable(&config_payload.sections, Some(ctx))?;
        let result = apply_portable_config_file(&self.config_path, &incoming).map_err(|error| {
            ConfigSyncError::Store {
                domain: ConfigSyncDomain::Config,
                reason: error.to_string(),
            }
        })?;

        if !result.changed {
            let mut unchanged = DomainApplyResult::unchanged(config_payload.sections.len() as u64);
            unchanged.outcome.warnings = result.warnings;
            return Ok(unchanged);
        }

        let applied_count =
            (result.changes.live.len() + result.changes.restart_required.len()) as u64;
        Ok(DomainApplyResult {
            outcome: DomainApplyOutcome {
                status: DomainApplyStatus::Applied,
                reason: None,
                applied_count: applied_count.max(1),
                skipped_count: 0,
                warnings: result.warnings,
            },
            restart_required: result.changes.restart_required,
        })
    }
}
