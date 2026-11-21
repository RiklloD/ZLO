use std::sync::Arc;

use ironclaw_filesystem::CompositeRootFilesystem;
use ironclaw_skills::usage::{FilesystemSkillUsageStore, SkillUsageStore};

/// The skill listing usage ledger (`ironclaw_skills::usage`) over the
/// composition-owned filesystem's per-user `/skill-usage` mount. One
/// instance serves the harness, every CLI runtime and the product surface.
pub(crate) fn build_skill_usage_store(
    filesystem: Arc<CompositeRootFilesystem>,
) -> Arc<dyn SkillUsageStore> {
    #[allow(clippy::disallowed_methods)]
    let store = FilesystemSkillUsageStore::new(crate::wrap_scoped(filesystem));
    Arc::new(store)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_assistant::skill_listing_page::{set_skill_listing_pin, skill_listing_response};
    use ironclaw_filesystem::InMemoryBackend;
    use ironclaw_host_api::ids::{TenantId, UserId};
    use ironclaw_product_contracts::skill_listing::{
        RebornSkillListingBasis, RebornSkillListingPinRequest, RebornSkillListingState,
        RebornSkillPin,
    };
    use ironclaw_skills::usage::SkillUsageOwner;
    use ironclaw_skills::usage::{SkillUsageName, SkillUsageObservation, SkillUsageSource};

    fn owner() -> SkillUsageOwner {
        SkillUsageOwner {
            tenant_id: TenantId::new("tenant-a").unwrap(),
            user_id: UserId::new("user-a").unwrap(),
        }
    }

    /// The production mount view: the store writes under the caller's own
    /// `/skill-usage`, and a pin reads back through the listing.
    #[tokio::test]
    async fn a_pin_reads_back_in_the_listing_over_the_production_mount() {
        let root = Arc::new(InMemoryBackend::new());
        #[allow(clippy::disallowed_methods)]
        let store: Arc<dyn SkillUsageStore> = Arc::new(FilesystemSkillUsageStore::new(
            crate::wrap_scoped(Arc::clone(&root)),
        ));
        store
            .record(
                &owner(),
                &SkillUsageSource::cli_runtime("codex").unwrap(),
                SkillUsageObservation {
                    catalog: Some(vec![SkillUsageName::new("pdf").unwrap()]),
                    used: Vec::new(),
                },
                1,
            )
            .await
            .unwrap();
        let before = skill_listing_response(store.as_ref(), &owner())
            .await
            .unwrap();
        assert!(before.managed);
        assert_eq!(before.skills[0].listing, RebornSkillListingState::NameOnly);
        assert_eq!(before.skills[0].sources[0].source, "cli:codex");

        let written = set_skill_listing_pin(
            store.as_ref(),
            &owner(),
            RebornSkillListingPinRequest {
                name: "pdf".into(),
                pin: Some(RebornSkillPin::AlwaysFull),
            },
        )
        .await
        .unwrap();
        assert_eq!(written.skills[0].listing, RebornSkillListingState::Full);
        assert_eq!(written.skills[0].basis, RebornSkillListingBasis::Pinned);
        let read_back = skill_listing_response(store.as_ref(), &owner())
            .await
            .unwrap();
        assert_eq!(read_back, written);
        let path = ironclaw_host_api::path::VirtualPath::new(
            "/tenants/tenant-a/users/user-a/skill-usage/listing.json",
        )
        .unwrap();
        assert!(
            ironclaw_filesystem::RootFilesystem::get(root.as_ref(), &path)
                .await
                .unwrap()
                .is_some(),
            "the document lives under the caller's own mount"
        );
    }
}
