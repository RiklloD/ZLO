//! The Skills page's listing management over the skill usage ledger
//! (`ironclaw_skills::usage`): the per-user summary as wire DTOs, and pin
//! writes read back as the listing the store committed.

use ironclaw_product_contracts::skill_listing::{
    RebornSkillListingEntry, RebornSkillListingPinRequest, RebornSkillListingResponse,
    RebornSkillListingState,
};
use ironclaw_product_contracts::surface::{ProductSurfaceError, ProductSurfaceValidationCode};
use ironclaw_skills::usage::{
    SkillListing, SkillListingPolicy, SkillPin, SkillUsageDocument, SkillUsageName,
    SkillUsageOwner, SkillUsageStore, summarize,
};

/// The `skill_listing` view for `owner`, from the ledger's summary.
pub async fn skill_listing_response(
    store: &dyn SkillUsageStore,
    owner: &SkillUsageOwner,
) -> Result<RebornSkillListingResponse, ProductSurfaceError> {
    let document = store
        .load(owner)
        .await
        .map_err(ProductSurfaceError::internal_from)?;
    Ok(listing_response(document.as_ref()))
}

/// Set or clear one pin of `owner`'s; returns the listing as written.
pub async fn set_skill_listing_pin(
    store: &dyn SkillUsageStore,
    owner: &SkillUsageOwner,
    request: RebornSkillListingPinRequest,
) -> Result<RebornSkillListingResponse, ProductSurfaceError> {
    let name = SkillUsageName::new(request.name).map_err(|error| {
        tracing::debug!(%error, "skill listing pin: invalid skill name");
        ProductSurfaceError::validation("name", ProductSurfaceValidationCode::InvalidValue)
    })?;
    // An unmanaged deployment keeps no pins: say so instead of failing inside.
    if store
        .load(owner)
        .await
        .map_err(ProductSurfaceError::internal_from)?
        .is_none()
    {
        return Err(ProductSurfaceError::service_unavailable(false));
    }
    let pin = request.pin.map(|pin| match pin {
        RebornSkillListingState::Full => SkillPin::AlwaysFull,
        RebornSkillListingState::NameOnly => SkillPin::NameOnly,
        RebornSkillListingState::Off => SkillPin::Off,
    });
    let written = store
        .set_pin(owner, &name, pin)
        .await
        .map_err(ProductSurfaceError::internal_from)?;
    Ok(listing_response(Some(&written)))
}

fn listing_response(document: Option<&SkillUsageDocument>) -> RebornSkillListingResponse {
    let Some(document) = document else {
        return RebornSkillListingResponse {
            managed: false,
            skills: Vec::new(),
        };
    };
    let now_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0); // silent-ok: a clock before 1970 only ages every use
    let skills = summarize(&SkillListingPolicy::default(), document, now_ms)
        .into_iter()
        .map(|summary| RebornSkillListingEntry {
            name: summary.name.into_inner(),
            uses: summary.uses,
            last_used_ms: summary.last_used_ms,
            listing: wire_listing(summary.state.listing),
            pin: summary.pin.map(|pin| wire_listing(pin.listing())),
            sources: summary
                .sources
                .into_iter()
                .map(|source| source.source.wire())
                .collect(),
        })
        .collect();
    RebornSkillListingResponse {
        managed: true,
        skills,
    }
}

fn wire_listing(listing: SkillListing) -> RebornSkillListingState {
    match listing {
        SkillListing::Full => RebornSkillListingState::Full,
        SkillListing::NameOnly => RebornSkillListingState::NameOnly,
        SkillListing::Off => RebornSkillListingState::Off,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_host_api::ids::{TenantId, UserId};
    use ironclaw_skills::usage::UnmanagedSkillUsage;

    fn owner() -> SkillUsageOwner {
        SkillUsageOwner {
            tenant_id: TenantId::new("tenant-a").unwrap(),
            user_id: UserId::new("user-a").unwrap(),
        }
    }

    #[tokio::test]
    async fn bad_names_and_unmanaged_deployments_are_refused() {
        let error = set_skill_listing_pin(
            &UnmanagedSkillUsage,
            &owner(),
            RebornSkillListingPinRequest {
                name: "two words".into(),
                pin: None,
            },
        )
        .await
        .unwrap_err();
        assert_eq!(error.status_code, 400);
        let unmanaged = set_skill_listing_pin(
            &UnmanagedSkillUsage,
            &owner(),
            RebornSkillListingPinRequest {
                name: "pdf".into(),
                pin: Some(RebornSkillListingState::Off),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(unmanaged.status_code, 503);
        let listing = skill_listing_response(&UnmanagedSkillUsage, &owner())
            .await
            .unwrap();
        assert!(!listing.managed && listing.skills.is_empty());
    }
}
