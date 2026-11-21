//! Per-user visibility of active extensions, for surfaces that act on an
//! extension's behalf outside the capability surface (package hooks and
//! package settings).
//!
//! The active extension registry is tenant-global and carries no owner; who
//! may see an extension is the installation row's [`InstallationOwner`]
//! (#5459 P1). This is the same owner-by-extension projection the tool
//! catalog and the model-visible capability surface join
//! (`ExtensionLifecycleManager::installation_owners`), exposed as a narrow
//! port so callers can be tested without the whole lifecycle manager.
//!
//! The rule is fail-closed: an extension with no installation row is visible
//! to nobody (an install-backed package published without one is anomalous
//! and could be private); a tenant-shared row is visible to everyone; a
//! member-held row only to its members.

use std::collections::BTreeMap;

use async_trait::async_trait;
use ironclaw_extension_host::ExtensionLifecycleManager;
use ironclaw_extension_registry::{InstallationOwner, PackageContentPins};
use ironclaw_host_api::ids::{ExtensionId, UserId};
use ironclaw_product_contracts::error::ProductOperationFailure;

/// Owner of every installation, keyed by extension id, and the per-run
/// consent check for the exact package generation a caller would run.
#[async_trait]
pub trait InstallationOwnerSource: Send + Sync {
    async fn installation_owners(
        &self,
    ) -> Result<BTreeMap<ExtensionId, InstallationOwner>, ProductOperationFailure>;

    /// Whether `caller` may run the third-party package bound to `pins`
    /// under `owner` (its consent-filtered owner row): true only when the
    /// caller's governing principal holds a decision naming the consent
    /// digest those pins were granted under (see
    /// `ExtensionConsentStore::execution_consented`). Visibility alone is
    /// not enough — the owner projection may describe another generation.
    async fn execution_consented(
        &self,
        extension_id: &ExtensionId,
        pins: Option<&PackageContentPins>,
        owner: Option<&InstallationOwner>,
        caller: &UserId,
    ) -> Result<bool, ProductOperationFailure>;
}

#[async_trait]
impl InstallationOwnerSource for ExtensionLifecycleManager {
    async fn installation_owners(
        &self,
    ) -> Result<BTreeMap<ExtensionId, InstallationOwner>, ProductOperationFailure> {
        ExtensionLifecycleManager::installation_owners(self).await
    }

    async fn execution_consented(
        &self,
        extension_id: &ExtensionId,
        pins: Option<&PackageContentPins>,
        owner: Option<&InstallationOwner>,
        caller: &UserId,
    ) -> Result<bool, ProductOperationFailure> {
        self.consent_store()
            .execution_consented(extension_id, pins, owner, caller)
            .await
    }
}

/// Whether `caller` may see `extension` under `owners`. A missing owner row
/// is invisible (fail closed).
pub fn installation_visible_to(
    owners: &BTreeMap<ExtensionId, InstallationOwner>,
    extension: &ExtensionId,
    caller: &UserId,
) -> bool {
    owners
        .get(extension)
        .is_some_and(|owner| owner.visible_to(caller))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(id: &str) -> UserId {
        UserId::new(id).unwrap()
    }

    #[test]
    fn visibility_follows_the_owner_row_and_fails_closed_without_one() {
        let private = ExtensionId::new("private-ext").unwrap();
        let shared = ExtensionId::new("shared-ext").unwrap();
        let orphan = ExtensionId::new("orphan-ext").unwrap();
        let owners = BTreeMap::from([
            (private.clone(), InstallationOwner::user(user("alice"))),
            (shared.clone(), InstallationOwner::Tenant),
        ]);
        assert!(installation_visible_to(&owners, &private, &user("alice")));
        assert!(!installation_visible_to(&owners, &private, &user("bob")));
        assert!(installation_visible_to(&owners, &shared, &user("bob")));
        assert!(!installation_visible_to(&owners, &orphan, &user("alice")));
    }
}
