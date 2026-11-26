//! `skills` domain handler: the person's own skills, whole bundles
//! (`SKILL.md`, scripts, references), through the skill management port.
//!
//! One-way, the source wins: a skill that differs is replaced as a whole,
//! one the receiver lacks is created, and a skill only the receiver has is
//! kept. System skills ship with the build and are never carried.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_host_api::resource::ResourceScope;
use ironclaw_product_contracts::config_sync::{
    ConfigSyncDomain, ConfigSyncDomainDigest, ConfigSyncDomainPayload,
};
use ironclaw_skills::{ManagedSkillSource, ScopedSkillManagementError, ScopedSkillManagementPort};

use super::{
    ConfigSyncContext, ConfigSyncDomainHandler, ConfigSyncError, DomainApplyResult, items,
};
use crate::skills_product_service::caller_skill_scope;

const DOMAIN: ConfigSyncDomain = ConfigSyncDomain::Skills;

pub struct SkillsDomainHandler {
    skills: Arc<ScopedSkillManagementPort>,
}

impl SkillsDomainHandler {
    pub fn new(skills: Arc<ScopedSkillManagementPort>) -> Self {
        Self { skills }
    }

    /// The person's skills here: name to bundle digest, and the skills
    /// held back because a file in them is a key or holds a secret.
    async fn index(
        &self,
        scope: &ResourceScope,
    ) -> Result<(BTreeMap<String, String>, Vec<String>), ConfigSyncError> {
        let mut out = BTreeMap::new();
        let mut held_back = Vec::new();
        for skill in self
            .skills
            .list_for_scope(scope.clone())
            .await
            .map_err(store_error)?
        {
            if skill.source == ManagedSkillSource::System {
                continue;
            }
            // A skill that cannot be read whole (its folder and its
            // manifest disagree on its name, or it went away) is left out;
            // the others still sync.
            let files = match self
                .skills
                .export_bundle_for_scope(scope.clone(), &skill.name)
                .await
            {
                Ok(files) => files,
                Err(error) => {
                    tracing::debug!(skill = %skill.name, %error, "config sync: skill not read");
                    continue;
                }
            };
            if let Some(path) = keeps_a_secret(&files) {
                held_back.push(format!(
                    "{} (`{path}` is a key or holds a secret)",
                    skill.name
                ));
                continue;
            }
            out.insert(skill.name, items::files_hash(&files));
        }
        Ok((out, held_back))
    }
}

/// The first file of a bundle that is a key file by name or holds a
/// secret: such a bundle is carried neither out nor in, whole (taking only
/// its other files would delete the key's copy on the receiver).
fn keeps_a_secret(files: &[(String, Vec<u8>)]) -> Option<&str> {
    files
        .iter()
        .find(|(path, bytes)| {
            let name = path.rsplit('/').next().unwrap_or(path);
            items::key_file_name(name) || items::secret_bytes(bytes)
        })
        .map(|(path, _)| path.as_str())
}

fn store_error(error: ScopedSkillManagementError) -> ConfigSyncError {
    ConfigSyncError::Store {
        domain: DOMAIN,
        reason: error.to_string(),
    }
}

#[async_trait]
impl ConfigSyncDomainHandler for SkillsDomainHandler {
    fn domain(&self) -> ConfigSyncDomain {
        DOMAIN
    }

    async fn manifest(
        &self,
        ctx: &ConfigSyncContext,
    ) -> Result<ConfigSyncDomainDigest, ConfigSyncError> {
        let (index, _) = self.index(&caller_skill_scope(ctx.caller.clone())).await?;
        Ok(ConfigSyncDomainDigest {
            hash: items::domain_hash(&index),
            updated_at: None,
            item_count: index.len() as u64,
        })
    }

    async fn export(
        &self,
        ctx: &ConfigSyncContext,
    ) -> Result<ConfigSyncDomainPayload, ConfigSyncError> {
        let scope = caller_skill_scope(ctx.caller.clone());
        let (index, held_back) = self.index(&scope).await?;
        let mut page = items::page(&index, &ctx.page, |name| {
            let skills = Arc::clone(&self.skills);
            let scope = scope.clone();
            async move {
                skills
                    .export_bundle_for_scope(scope, &name)
                    .await
                    .map(Some)
                    .map_err(store_error)
            }
        })
        .await?;
        // Said once, with the first page.
        if ctx.page.after.is_none() {
            let room = items::PAGE_NOTES.saturating_sub(page.not_carried.len());
            page.not_carried.extend(held_back.into_iter().take(room));
        }
        Ok(ConfigSyncDomainPayload::Skills(page))
    }

    fn validate(&self, payload: &ConfigSyncDomainPayload) -> Result<(), ConfigSyncError> {
        let ConfigSyncDomainPayload::Skills(page) = payload else {
            return Err(ConfigSyncError::validation(
                "skills",
                "expected skills payload",
            ));
        };
        items::validate(DOMAIN, page)
    }

    async fn apply(
        &self,
        ctx: &ConfigSyncContext,
        payload: &ConfigSyncDomainPayload,
    ) -> Result<DomainApplyResult, ConfigSyncError> {
        let ConfigSyncDomainPayload::Skills(page) = payload else {
            return Err(ConfigSyncError::validation(
                "skills",
                "expected skills payload",
            ));
        };
        // Skills go one way; a return leg has nothing to take.
        if ctx.merge_back {
            return Ok(DomainApplyResult::unchanged(page.items.len() as u64));
        }
        let scope = caller_skill_scope(ctx.caller.clone());
        let (held, kept_here) = self.index(&scope).await?;
        let (mut applied, mut skipped) = (0u64, 0u64);
        let mut warnings: Vec<String> = page
            .not_carried
            .iter()
            .map(|item| format!("skill {item} was not carried"))
            .collect();
        for item in &page.items {
            let Some(files) = items::decode(DOMAIN, item)? else {
                skipped += 1;
                continue;
            };
            if held.get(&item.key) == Some(&items::files_hash(&files)) {
                skipped += 1;
                continue;
            }
            if let Some(path) = keeps_a_secret(&files) {
                warnings.push(format!(
                    "skill {} was not synced: `{path}` is a key or holds a secret",
                    item.key
                ));
                skipped += 1;
                continue;
            }
            // This computer's skill of that name holds a secret of its
            // own: replacing the bundle would delete it.
            let prefix = format!("{} (", item.key);
            if kept_here.iter().any(|note| note.starts_with(&prefix)) {
                warnings.push(format!(
                    "skill {} was not synced: the one here holds a key or a secret",
                    item.key
                ));
                skipped += 1;
                continue;
            }
            match self
                .skills
                .replace_bundle_for_scope(scope.clone(), &item.key, files)
                .await
            {
                Ok(()) => applied += 1,
                Err(error) => {
                    tracing::debug!(skill = %item.key, %error, "config sync: skill not applied");
                    warnings.push(format!("skill {} was not synced: {error}", item.key));
                    skipped += 1;
                }
            }
        }
        let mut result = if applied == 0 {
            DomainApplyResult::unchanged(skipped)
        } else {
            DomainApplyResult::applied(applied, skipped)
        };
        result.outcome.warnings = warnings;
        Ok(result)
    }
}
