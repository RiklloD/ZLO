//! `profiles` domain handler (`orch_profiles`).
//!
//! - Export normalizes `working_folder` to `$ZLO_HOME` / `$HOME` tokens.
//! - Apply expands `$ZLO_HOME` / `$HOME` tokens to the target's folders,
//!   upserts by `profile_id` (source wins on conflict), preserves target-only
//!   profiles, and propagates archive (`archived_at`).
//! - Canonical hash excludes volatile timestamps (`updated_at` / `created_at`)
//!   and derived fields (`last_used_at` / `thread_count`) so identical profile
//!   sets hash identically across macOS and Linux.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::profiles::{
    ProfileOwner, ProfileSyncRecord, parse_profile_slug, validate_settings,
};
use ironclaw_product_contracts::config_sync::{
    ConfigSyncDomain, ConfigSyncDomainDigest, ConfigSyncDomainPayload, ProfileSyncItem,
    ProfilesDomainPayload,
};
use ironclaw_product_contracts::profiles::DEFAULT_PROFILE_ID;

use super::{
    ConfigSyncContext, ConfigSyncDomainHandler, ConfigSyncError, DomainApplyResult,
    canonical_json_sha256,
};

pub struct ProfilesDomainHandler {
    engine: Arc<Engine>,
}

impl ProfilesDomainHandler {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self { engine }
    }

    fn owner(ctx: &ConfigSyncContext) -> ProfileOwner {
        ProfileOwner {
            tenant_id: ctx.caller.tenant_id.clone(),
            user_id: ctx.caller.user_id.clone(),
        }
    }

    fn normalize_items(
        records: Vec<ProfileSyncRecord>,
        ctx: &ConfigSyncContext,
    ) -> (Vec<ProfileSyncItem>, Option<DateTime<Utc>>) {
        let rewrite = ctx.to_tokens_rewrite();
        let mut latest_ms: Option<i64> = None;
        let mut items: Vec<ProfileSyncItem> = records
            .into_iter()
            .map(|rec| {
                latest_ms = Some(latest_ms.map_or(rec.updated_at, |m| m.max(rec.updated_at)));
                let mut settings = rec.settings;
                if let Some(folder) = settings.working_folder.as_deref() {
                    settings.working_folder = Some(rewrite.rewrite_or_keep(folder));
                }
                ProfileSyncItem {
                    profile_id: rec.profile_id.as_str().to_string(),
                    updated_at: rec.updated_at,
                    archived_at: rec.archived_at,
                    settings,
                }
            })
            .collect();
        items.sort_by(|a, b| a.profile_id.cmp(&b.profile_id));
        let updated_at = latest_ms.and_then(DateTime::<Utc>::from_timestamp_millis);
        (items, updated_at)
    }

    fn hash_items(items: &[ProfileSyncItem]) -> Result<String, ConfigSyncError> {
        let mut canonical_rows: Vec<serde_json::Value> = items
            .iter()
            .map(|item| {
                serde_json::json!({
                    "profile_id": item.profile_id,
                    "archived": item.archived_at.is_some(),
                    "settings": item.settings,
                })
            })
            .collect();
        canonical_rows.sort_by(|a, b| {
            let ka = a.get("profile_id").and_then(serde_json::Value::as_str);
            let kb = b.get("profile_id").and_then(serde_json::Value::as_str);
            ka.cmp(&kb)
        });
        canonical_json_sha256(
            ConfigSyncDomain::Profiles,
            &serde_json::Value::Array(canonical_rows),
        )
    }
}

#[async_trait]
impl ConfigSyncDomainHandler for ProfilesDomainHandler {
    fn domain(&self) -> ConfigSyncDomain {
        ConfigSyncDomain::Profiles
    }

    async fn manifest(
        &self,
        ctx: &ConfigSyncContext,
    ) -> Result<ConfigSyncDomainDigest, ConfigSyncError> {
        let owner = Self::owner(ctx);
        let records = self
            .engine
            .store()
            .list_profiles_for_sync(&owner)
            .await
            .map_err(|error| ConfigSyncError::Store {
                domain: ConfigSyncDomain::Profiles,
                reason: error.to_string(),
            })?;
        let (items, updated_at) = Self::normalize_items(records, ctx);
        let hash = Self::hash_items(&items)?;
        Ok(ConfigSyncDomainDigest {
            hash,
            updated_at,
            item_count: items.len() as u64,
        })
    }

    async fn export(
        &self,
        ctx: &ConfigSyncContext,
    ) -> Result<ConfigSyncDomainPayload, ConfigSyncError> {
        let owner = Self::owner(ctx);
        let records = self
            .engine
            .store()
            .list_profiles_for_sync(&owner)
            .await
            .map_err(|error| ConfigSyncError::Store {
                domain: ConfigSyncDomain::Profiles,
                reason: error.to_string(),
            })?;
        let (items, _) = Self::normalize_items(records, ctx);
        let hash = Self::hash_items(&items)?;
        Ok(ConfigSyncDomainPayload::Profiles(ProfilesDomainPayload {
            hash,
            items,
        }))
    }

    fn validate(&self, payload: &ConfigSyncDomainPayload) -> Result<(), ConfigSyncError> {
        let ConfigSyncDomainPayload::Profiles(p) = payload else {
            return Err(ConfigSyncError::validation(
                "profiles",
                "expected profiles payload",
            ));
        };
        let mut seen = std::collections::HashSet::with_capacity(p.items.len());
        for item in &p.items {
            parse_profile_slug(&item.profile_id).map_err(|error| {
                ConfigSyncError::validation("profiles.items", error.to_string())
            })?;
            if !seen.insert(item.profile_id.as_str()) {
                return Err(ConfigSyncError::validation(
                    "profiles.items",
                    format!("duplicate profile_id `{}`", item.profile_id),
                ));
            }
            if item.profile_id == DEFAULT_PROFILE_ID && item.archived_at.is_some() {
                return Err(ConfigSyncError::validation(
                    "profiles.items",
                    "the default profile cannot be archived",
                ));
            }
            // Temporarily expand `$HOME`/`$ZLO_HOME` to a dummy absolute prefix
            // so `validate_settings`'s path check validates normalized tokens.
            let mut probe_settings = item.settings.clone();
            if let Some(folder) = probe_settings.working_folder.as_deref() {
                let probe_rewrite =
                    ironclaw_config::HomePathRewrite::from_tokens("/zlo-home", "/home-dir");
                probe_settings.working_folder = Some(probe_rewrite.rewrite_or_keep(folder));
            }
            validate_settings(&probe_settings).map_err(|error| {
                ConfigSyncError::validation(
                    format!("profiles.items.{}", item.profile_id),
                    error.to_string(),
                )
            })?;
        }
        Ok(())
    }

    async fn apply(
        &self,
        ctx: &ConfigSyncContext,
        payload: &ConfigSyncDomainPayload,
    ) -> Result<DomainApplyResult, ConfigSyncError> {
        let ConfigSyncDomainPayload::Profiles(p) = payload else {
            return Err(ConfigSyncError::validation(
                "profiles",
                "expected profiles payload",
            ));
        };
        let owner = Self::owner(ctx);
        let expand = ctx.from_tokens_rewrite();
        let mut records = Vec::with_capacity(p.items.len());
        for item in &p.items {
            let profile_id = parse_profile_slug(&item.profile_id).map_err(|error| {
                ConfigSyncError::validation("profiles.items", error.to_string())
            })?;
            let mut settings = item.settings.clone();
            if let Some(folder) = settings.working_folder.as_deref() {
                settings.working_folder = Some(expand.rewrite_or_keep(folder));
            }
            records.push(ProfileSyncRecord {
                profile_id,
                settings,
                created_at: item.updated_at,
                updated_at: item.updated_at,
                archived_at: item.archived_at,
            });
        }

        let counts = self
            .engine
            .store()
            .apply_profiles_sync(&owner, &records)
            .await
            .map_err(|error| ConfigSyncError::Store {
                domain: ConfigSyncDomain::Profiles,
                reason: error.to_string(),
            })?;

        if counts.applied == 0 {
            Ok(DomainApplyResult::unchanged(counts.skipped_unchanged))
        } else {
            Ok(DomainApplyResult::applied(
                counts.applied,
                counts.skipped_unchanged,
            ))
        }
    }
}
