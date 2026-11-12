//! Skill install/update/remove, search and content reads, auto-activation
//! settings, and the activation recorder/clearer ports (`skills` charter row).

use super::*;

pub(super) type SkillActivationRecorder =
    dyn Fn(&TurnScope, &AcceptedMessageRef, &str) -> Result<(), ProductSurfaceError> + Send + Sync;

pub(super) type SkillActivationClearer =
    dyn Fn(&TurnScope, &AcceptedMessageRef) -> Result<(), ProductSurfaceError> + Send + Sync;

pub const SKILL_INSTALL_CAPABILITY_ID: &str = "builtin.skill_install";

pub const SKILL_INSTALL_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(SKILL_INSTALL_CAPABILITY_ID);

pub const SKILL_UPDATE_CAPABILITY_ID: &str = "builtin.skill_update";

pub const SKILL_UPDATE_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(SKILL_UPDATE_CAPABILITY_ID);

pub const SKILL_REMOVE_CAPABILITY_ID: &str = "builtin.skill_remove";

pub const SKILL_REMOVE_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(SKILL_REMOVE_CAPABILITY_ID);

pub const SKILL_AUTO_ACTIVATE_SET_CAPABILITY_ID: &str = "builtin.skill_auto_activate_set";

pub const SKILL_AUTO_ACTIVATE_SET_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(SKILL_AUTO_ACTIVATE_SET_CAPABILITY_ID);

pub const SKILL_AUTO_ACTIVATE_LEARNED_SET_CAPABILITY_ID: &str =
    "builtin.skill_auto_activate_learned_set";

pub const SKILL_AUTO_ACTIVATE_LEARNED_SET_CAPABILITY: ProductCapabilityDescriptor =
    ProductCapabilityDescriptor::api_only(SKILL_AUTO_ACTIVATE_LEARNED_SET_CAPABILITY_ID);

pub const SKILLS_VIEW: ProductView<serde_json::Value, RebornSkillListResponse> =
    ProductView::unpaginated("skills");

pub const SKILL_SEARCH_VIEW: ProductView<serde_json::Value, RebornSkillSearchResponse> =
    ProductView::unpaginated("skill_search");

pub const SKILL_CONTENT_VIEW: ProductView<serde_json::Value, RebornSkillContentResponse> =
    ProductView::unpaginated("skill_content");

#[async_trait]
pub trait SkillsProductService: Send + Sync {
    async fn list_skills(
        &self,
        caller: ProductSurfaceCaller,
    ) -> Result<RebornSkillListResponse, ProductSurfaceError> {
        let _ = caller;
        Err(ProductSurfaceError::service_unavailable(false))
    }

    async fn search_skills(
        &self,
        caller: ProductSurfaceCaller,
        query: String,
    ) -> Result<RebornSkillSearchResponse, ProductSurfaceError> {
        let _ = (caller, query);
        Err(ProductSurfaceError::service_unavailable(false))
    }

    async fn read_skill_content(
        &self,
        caller: ProductSurfaceCaller,
        name: String,
    ) -> Result<RebornSkillContentResponse, ProductSurfaceError> {
        let _ = (caller, name);
        Err(ProductSurfaceError::service_unavailable(false))
    }
}

#[derive(Debug, Default)]
pub struct UnsupportedSkillsProductService;

impl UnsupportedSkillsProductService {
    pub fn new_static() -> Self {
        Self
    }
}

#[async_trait]
impl SkillsProductService for UnsupportedSkillsProductService {}
