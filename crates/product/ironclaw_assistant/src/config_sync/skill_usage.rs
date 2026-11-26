//! `skill_usage` domain handler: the person's skill pins and the usage
//! ledger (`ironclaw_skills::usage`), which decide how skills are listed to
//! every agent.
//!
//! Two-way. Uses merge (the larger count, the later use, per source and
//! skill), so work done on either computer counts on both. Pins follow the
//! source: applying its document sets its pins and clears the others; the
//! return leg leaves pins alone. A source's catalog (what it last offered)
//! is an observation of that computer and is not carried.

use async_trait::async_trait;
use ironclaw_product_contracts::config_sync::{
    ConfigSyncDomain, ConfigSyncDomainDigest, ConfigSyncDomainPayload, SkillUsageDomainPayload,
};
use ironclaw_skills::usage::{
    SharedSkillUsage, SkillLedger, SkillUsageDocument, SkillUsageError, SkillUsageOwner,
};

use super::{
    ConfigSyncContext, ConfigSyncDomainHandler, ConfigSyncError, DomainApplyResult,
    canonical_json_sha256,
};

const DOMAIN: ConfigSyncDomain = ConfigSyncDomain::SkillUsage;

pub struct SkillUsageDomainHandler {
    usage: SharedSkillUsage,
}

impl SkillUsageDomainHandler {
    pub fn new(usage: SharedSkillUsage) -> Self {
        Self { usage }
    }

    fn owner(ctx: &ConfigSyncContext) -> SkillUsageOwner {
        SkillUsageOwner {
            tenant_id: ctx.caller.tenant_id.clone(),
            user_id: ctx.caller.user_id.clone(),
        }
    }

    /// The owner's document as it travels: no catalogs, no source without
    /// uses. `None` when listings are not managed on this backend.
    async fn portable(
        &self,
        ctx: &ConfigSyncContext,
    ) -> Result<Option<SkillUsageDocument>, ConfigSyncError> {
        let Some(mut document) = self
            .usage
            .store()
            .load(&Self::owner(ctx))
            .await
            .map_err(store_error)?
        else {
            return Ok(None);
        };
        document.sources.retain(|_, record| {
            record.catalog.clear();
            !record.ledger.is_empty()
        });
        Ok(Some(document))
    }

    fn encode(
        document: &SkillUsageDocument,
    ) -> Result<(String, serde_json::Value), ConfigSyncError> {
        let value =
            serde_json::to_value(document).map_err(|source| ConfigSyncError::Serialization {
                domain: DOMAIN,
                source,
            })?;
        Ok((canonical_json_sha256(DOMAIN, &value)?, value))
    }
}

fn store_error(error: SkillUsageError) -> ConfigSyncError {
    ConfigSyncError::Store {
        domain: DOMAIN,
        reason: error.to_string(),
    }
}

fn parse(payload: &ConfigSyncDomainPayload) -> Result<SkillUsageDocument, ConfigSyncError> {
    let ConfigSyncDomainPayload::SkillUsage(p) = payload else {
        return Err(ConfigSyncError::validation(
            "skill_usage",
            "expected skill_usage payload",
        ));
    };
    serde_json::from_value(p.document.clone())
        .map_err(|error| ConfigSyncError::validation("skill_usage.document", error.to_string()))
}

#[async_trait]
impl ConfigSyncDomainHandler for SkillUsageDomainHandler {
    fn domain(&self) -> ConfigSyncDomain {
        DOMAIN
    }

    async fn manifest(
        &self,
        ctx: &ConfigSyncContext,
    ) -> Result<ConfigSyncDomainDigest, ConfigSyncError> {
        let document = self.portable(ctx).await?.unwrap_or_default();
        let (hash, _) = Self::encode(&document)?;
        Ok(ConfigSyncDomainDigest {
            hash,
            updated_at: None,
            item_count: (document.pins.len()
                + document
                    .sources
                    .values()
                    .map(|record| record.ledger.len())
                    .sum::<usize>()) as u64,
        })
    }

    async fn export(
        &self,
        ctx: &ConfigSyncContext,
    ) -> Result<ConfigSyncDomainPayload, ConfigSyncError> {
        let document = self.portable(ctx).await?.unwrap_or_default();
        let (hash, document) = Self::encode(&document)?;
        Ok(ConfigSyncDomainPayload::SkillUsage(
            SkillUsageDomainPayload { hash, document },
        ))
    }

    fn validate(&self, payload: &ConfigSyncDomainPayload) -> Result<(), ConfigSyncError> {
        parse(payload).map(|_| ())
    }

    async fn apply(
        &self,
        ctx: &ConfigSyncContext,
        payload: &ConfigSyncDomainPayload,
    ) -> Result<DomainApplyResult, ConfigSyncError> {
        let incoming = parse(payload)?;
        let owner = Self::owner(ctx);
        let store = self.usage.store();
        let Some(existing) = store.load(&owner).await.map_err(store_error)? else {
            return Ok(DomainApplyResult::skipped_with_reason(
                "skill listings are not managed on this backend",
                0,
            ));
        };
        let mut applied = 0u64;
        let mut warnings = Vec::new();
        for (source, record) in &incoming.sources {
            let ours = existing.sources.get(source).map(|r| &r.ledger);
            // Only the uses that would change something here.
            let news: SkillLedger = record
                .ledger
                .iter()
                .filter(|(name, used)| {
                    let held = ours.and_then(|l| l.get(*name)).copied().unwrap_or_default();
                    held.merged(**used) != held
                })
                .map(|(name, used)| (name.clone(), *used))
                .collect();
            if news.is_empty() {
                continue;
            }
            let count = news.len() as u64;
            match store.import_ledger(&owner, source, news).await {
                Ok(()) => applied += count,
                Err(error) => {
                    warnings.push(format!("uses of {} not merged: {error}", source.wire()))
                }
            }
        }
        if !ctx.merge_back {
            for (name, pin) in &incoming.pins {
                if existing.pins.get(name) != Some(pin) {
                    store
                        .set_pin(&owner, name, Some(*pin))
                        .await
                        .map_err(store_error)?;
                    applied += 1;
                }
            }
            for name in existing.pins.keys() {
                if !incoming.pins.contains_key(name) {
                    store
                        .set_pin(&owner, name, None)
                        .await
                        .map_err(store_error)?;
                    applied += 1;
                }
            }
        }
        let mut result = if applied == 0 {
            DomainApplyResult::unchanged(0)
        } else {
            DomainApplyResult::applied(applied, 0)
        };
        result.outcome.warnings = warnings;
        Ok(result)
    }
}
