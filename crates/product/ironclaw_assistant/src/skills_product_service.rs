//! The Skills page's product service over the deployment's skill stores:
//! scoped skill management (list, search, read), the "auto-activate
//! learned skills" switch the activation selector reads, and listing
//! management over the shared skill usage ledger. Composition constructs
//! it from the handles it assembled.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use ironclaw_host_api::{ids::InvocationId, resource::ResourceScope};
use ironclaw_product_contracts::product_wire::{
    RebornSkillContentResponse, RebornSkillInfo, RebornSkillListResponse,
    RebornSkillSearchResponse, RebornSkillSourceKind, RebornSkillTrustLevel,
};
use ironclaw_product_contracts::skill_listing::{
    RebornSkillListingPinRequest, RebornSkillListingResponse,
};
use ironclaw_product_contracts::surface::{
    ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceErrorCode, ProductSurfaceErrorKind,
};
use ironclaw_skills::usage::{SharedSkillUsage, SkillUsageOwner};
use ironclaw_skills::{ScopedSkillManagementError, ScopedSkillManagementPort};

use crate::SkillsProductService;

/// The skills view, search, content reads and listing management for one
/// deployment.
pub struct ScopedSkillsProductService {
    skill_management: Arc<ScopedSkillManagementPort>,
    // The activation selector's live master switch (composition's
    // `RebornRuntimeStores::skill_auto_activate_learned`); the read service
    // reports it for the skills view. Writes go through the first-party
    // `builtin.skill_auto_activate_learned_set` capability. `None` when no
    // flag-reading selector is wired (the production assembly) — the toggle then
    // reports unavailable instead of writing to a flag nothing reads.
    //
    // Process-global by design: this is a single-operator standalone switch, so it
    // is intentionally not scoped per caller. A future multi-user surface would
    // need a per-tenant flag.
    auto_activate_learned: Option<Arc<AtomicBool>>,
    /// The skill listing usage ledger and pins the harness and every CLI
    /// runtime list skills from.
    skill_usage: SharedSkillUsage,
}

impl ScopedSkillsProductService {
    pub fn new(
        skill_management: Arc<ScopedSkillManagementPort>,
        auto_activate_learned: Option<Arc<AtomicBool>>,
        skill_usage: SharedSkillUsage,
    ) -> Self {
        Self {
            skill_management,
            auto_activate_learned,
            skill_usage,
        }
    }
}

/// Whose skill listing the caller reads and pins: their own.
fn skill_usage_owner(caller: &ProductSurfaceCaller) -> SkillUsageOwner {
    SkillUsageOwner {
        tenant_id: caller.tenant_id.clone(),
        user_id: caller.user_id.clone(),
    }
}

#[async_trait]
impl SkillsProductService for ScopedSkillsProductService {
    async fn list_skills(
        &self,
        caller: ProductSurfaceCaller,
    ) -> Result<RebornSkillListResponse, ProductSurfaceError> {
        let scope = caller_skill_scope(caller);
        let skills = self
            .skill_management
            .list_for_scope(scope)
            .await
            .map_err(map_skill_management_error)?;
        Ok(skill_list_response(
            skills,
            self.auto_activate_learned
                .as_ref()
                .map(|flag| flag.load(Ordering::Relaxed))
                .unwrap_or(true),
        ))
    }

    async fn search_skills(
        &self,
        caller: ProductSurfaceCaller,
        query: String,
    ) -> Result<RebornSkillSearchResponse, ProductSurfaceError> {
        let scope = caller_skill_scope(caller);
        let result = self
            .skill_management
            .search_for_scope(scope, &query, 50)
            .await
            .map_err(map_skill_management_error)?;
        Ok(RebornSkillSearchResponse {
            catalog: Vec::new(),
            installed: result.skills.into_iter().map(skill_info).collect(),
            registry_url: String::new(),
            catalog_error: None,
        })
    }

    async fn read_skill_content(
        &self,
        caller: ProductSurfaceCaller,
        name: String,
    ) -> Result<RebornSkillContentResponse, ProductSurfaceError> {
        let scope = caller_skill_scope(caller);
        let content = self
            .skill_management
            .read_content_for_scope(scope, &name)
            .await
            .map_err(map_skill_management_error)?;
        Ok(RebornSkillContentResponse {
            name: content.name,
            content: content.content,
        })
    }

    async fn skill_listing(
        &self,
        caller: ProductSurfaceCaller,
    ) -> Result<RebornSkillListingResponse, ProductSurfaceError> {
        crate::skill_listing_page::skill_listing_response(
            self.skill_usage.store(),
            &skill_usage_owner(&caller),
        )
        .await
    }

    async fn set_skill_listing_pin(
        &self,
        caller: ProductSurfaceCaller,
        request: RebornSkillListingPinRequest,
    ) -> Result<RebornSkillListingResponse, ProductSurfaceError> {
        crate::skill_listing_page::set_skill_listing_pin(
            self.skill_usage.store(),
            &skill_usage_owner(&caller),
            request,
        )
        .await
    }
}

pub(crate) fn caller_skill_scope(caller: ProductSurfaceCaller) -> ResourceScope {
    ResourceScope {
        tenant_id: caller.tenant_id,
        user_id: caller.user_id,
        agent_id: caller.agent_id,
        project_id: caller.project_id,
        mission_id: None,
        thread_id: None,
        invocation_id: InvocationId::new(),
    }
}

fn skill_list_response(
    skills: Vec<ironclaw_skills::SkillSummary>,
    auto_activate_learned: bool,
) -> RebornSkillListResponse {
    let skills: Vec<_> = skills.into_iter().map(skill_info).collect();
    RebornSkillListResponse {
        count: skills.len(),
        skills,
        auto_activate_learned,
    }
}

fn skill_info(skill: ironclaw_skills::SkillSummary) -> RebornSkillInfo {
    let source_kind = match skill.source {
        ironclaw_skills::ManagedSkillSource::System => RebornSkillSourceKind::System,
        ironclaw_skills::ManagedSkillSource::User => RebornSkillSourceKind::User,
        ironclaw_skills::ManagedSkillSource::Installed => RebornSkillSourceKind::Installed,
    };
    let can_manage = matches!(
        source_kind,
        RebornSkillSourceKind::User | RebornSkillSourceKind::Installed
    );
    RebornSkillInfo {
        name: skill.name.clone(),
        description: skill.description,
        version: skill.version,
        trust: if source_kind == RebornSkillSourceKind::Installed {
            RebornSkillTrustLevel::Installed
        } else {
            RebornSkillTrustLevel::Trusted
        },
        source: source_kind,
        source_kind,
        keywords: skill.keywords,
        usage_hint: Some(format!(
            "Type `/{}` in chat to force-activate this skill.",
            skill.name
        )),
        setup_hint: None,
        bundle_path: None,
        install_source_url: None,
        // Both were hardcoded `false`, so the Skills page could not show what a skill contains --
        // the WebUI has rendered `requirements`/`scripts/` chips since #6194 and the wire fields have
        // existed since #7002, but nothing ever set them. This PR makes agent-authored skills with
        // scripts possible, so the page has to reflect it.
        has_requirements: !skill.requires_skills.is_empty(),
        has_scripts: skill.has_scripts,
        can_edit: can_manage,
        can_delete: can_manage,
        auto_activate: skill.auto_activate,
    }
}

fn map_skill_management_error(error: ScopedSkillManagementError) -> ProductSurfaceError {
    match error {
        ScopedSkillManagementError::InvalidContext { .. } => internal_skill_error(),
        ScopedSkillManagementError::Skill(error) => match error.kind() {
            ironclaw_skills::SkillManagementErrorKind::NotFound => ProductSurfaceError {
                code: ProductSurfaceErrorCode::NotFound,
                kind: ProductSurfaceErrorKind::NotFound,
                status_code: 404,
                retryable: false,
                field: None,
                validation_code: None,
            },
            ironclaw_skills::SkillManagementErrorKind::Conflict => ProductSurfaceError {
                code: ProductSurfaceErrorCode::Conflict,
                kind: ProductSurfaceErrorKind::Conflict,
                status_code: 409,
                retryable: false,
                field: None,
                validation_code: None,
            },
            ironclaw_skills::SkillManagementErrorKind::Resource => ProductSurfaceError {
                code: ProductSurfaceErrorCode::Unavailable,
                kind: ProductSurfaceErrorKind::ServiceUnavailable,
                status_code: 503,
                retryable: true,
                field: None,
                validation_code: None,
            },
            ironclaw_skills::SkillManagementErrorKind::FilesystemDenied => ProductSurfaceError {
                code: ProductSurfaceErrorCode::Forbidden,
                kind: ProductSurfaceErrorKind::ParticipantDenied,
                status_code: 403,
                retryable: false,
                field: None,
                validation_code: None,
            },
            ironclaw_skills::SkillManagementErrorKind::InvalidInput
            | ironclaw_skills::SkillManagementErrorKind::InvalidSkill => invalid_skill_request(),
        },
    }
}

fn invalid_skill_request() -> ProductSurfaceError {
    ProductSurfaceError {
        code: ProductSurfaceErrorCode::InvalidRequest,
        kind: ProductSurfaceErrorKind::Validation,
        status_code: 400,
        retryable: false,
        field: None,
        validation_code: None,
    }
}

fn internal_skill_error() -> ProductSurfaceError {
    ProductSurfaceError {
        code: ProductSurfaceErrorCode::Internal,
        kind: ProductSurfaceErrorKind::Internal,
        status_code: 500,
        retryable: false,
        field: None,
        validation_code: None,
    }
}

#[cfg(test)]
mod tests;
