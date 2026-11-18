//! The lifecycle manager's consent gate (see `crate::extension_consent` for
//! the model). Kept beside `product_lifecycle.rs` as a child module so the
//! manager's lock-ordering rules apply unchanged: the catalog is read before
//! `operation_lock`, never while holding it.

use std::collections::BTreeSet;
use std::sync::Arc;

use ironclaw_extension_contracts::state::InstallationState;
use ironclaw_extension_registry::ExtensionInstallationId;
use ironclaw_host_api::ids::{ExtensionId, UserId};
use ironclaw_product_contracts::error::ProductOperationFailure;
use ironclaw_product_contracts::package_consent::{LifecycleConsentSummary, PackageConsentDigest};
use ironclaw_product_contracts::package_lifecycle::{
    LifecyclePackageKind, LifecyclePackageRef, LifecycleProductPayload, LifecycleProductResponse,
};

use super::{
    ExtensionLifecycleManager, ensure_caller_may_mutate_tenant_installation,
    ensure_caller_may_operate, map_extension_installation_error, response_with_payload,
};
use crate::AvailableExtensionPackage;
use crate::extension_consent::{
    ExtensionConsentStatus, consent_summary, package_consent_digest, source_requires_consent,
};

impl ExtensionLifecycleManager {
    async fn consent_catalog_package(
        &self,
        package_ref: &LifecyclePackageRef,
    ) -> Result<Arc<AvailableExtensionPackage>, ProductOperationFailure> {
        let catalog = self.catalog.read().await;
        catalog.resolve(package_ref)
    }

    /// Consent status plus the summary to show, for a catalog package.
    pub(super) async fn consent_state(
        &self,
        available: &AvailableExtensionPackage,
    ) -> Result<(ExtensionConsentStatus, Option<LifecycleConsentSummary>), ProductOperationFailure>
    {
        let status = self.consent.status(available).await?;
        let summary = match status {
            ExtensionConsentStatus::Exempt => None,
            _ => Some(consent_summary(available)?),
        };
        Ok((status, summary))
    }

    /// The response an activation returns instead of activating, when the
    /// package lacks a grant for its current digest. Read before
    /// `operation_lock` (catalog-before-operation ordering).
    pub(super) async fn consent_blocked_activation(
        &self,
        package_ref: &LifecyclePackageRef,
    ) -> Result<Option<LifecycleProductResponse>, ProductOperationFailure> {
        let Ok(available) = self.consent_catalog_package(package_ref).await else {
            // Not in the catalog: the activation path reports that itself.
            return Ok(None);
        };
        let (status, summary) = self.consent_state(&available).await?;
        let Some(phase) = status.blocking_phase() else {
            return Ok(None);
        };
        tracing::debug!(
            extension_id = package_ref.id.as_str(),
            phase = phase.as_str(),
            "extension activation blocked until consent is granted"
        );
        let mut response = response_with_payload(
            Some(package_ref.clone()),
            phase,
            LifecycleProductPayload::ExtensionActivate {
                activated: false,
                visible_capability_ids: Vec::new(),
                connection_required: None,
            },
        );
        response.message = Some(consent_message(phase, package_ref.id.as_str()));
        response.consent = summary;
        Ok(Some(response))
    }

    /// Attach the consent summary to an install response, and report the
    /// blocking phase when the package still needs a grant.
    pub(super) async fn apply_consent_to_install_response(
        &self,
        available: &AvailableExtensionPackage,
        mut response: LifecycleProductResponse,
    ) -> Result<LifecycleProductResponse, ProductOperationFailure> {
        let (status, summary) = self.consent_state(available).await?;
        if let Some(phase) = status.blocking_phase() {
            response.phase = phase;
            if let Some(LifecycleProductPayload::ExtensionInstall { next_step, .. }) =
                response.payload.as_mut()
            {
                *next_step = consent_message(phase, available.package.id.as_str());
            }
        }
        response.consent = summary;
        Ok(response)
    }

    /// Fold consent into a projected installation phase (list/search/project):
    /// a package without a grant for its current digest is never reported as
    /// anything but blocked.
    pub(super) async fn consent_projected_phase(
        &self,
        available: &AvailableExtensionPackage,
        phase: InstallationState,
    ) -> Result<InstallationState, ProductOperationFailure> {
        if !source_requires_consent(available.source) {
            return Ok(phase);
        }
        Ok(self
            .consent
            .status(available)
            .await?
            .blocking_phase()
            .unwrap_or(phase))
    }

    /// The consent summary for a package, `None` when it is exempt.
    pub async fn consent_summary(
        &self,
        package_ref: &LifecyclePackageRef,
    ) -> Result<Option<LifecycleConsentSummary>, ProductOperationFailure> {
        let available = self.consent_catalog_package(package_ref).await?;
        Ok(self.consent_state(&available).await?.1)
    }

    /// Grant consent for `digest`. Fails unless `digest` is the package's
    /// current digest. Does not activate — the caller activates next, through
    /// the ordinary activation path.
    pub async fn grant_consent(
        &self,
        package_ref: LifecyclePackageRef,
        digest: PackageConsentDigest,
        caller: &UserId,
    ) -> Result<LifecycleProductResponse, ProductOperationFailure> {
        let package_ref = package_ref.require_extension().map_err(|error| {
            ProductOperationFailure::InvalidBindingRequest {
                reason: error.to_string(),
            }
        })?;
        let available = self.consent_catalog_package(&package_ref).await?;
        if !source_requires_consent(available.source) {
            return Err(ProductOperationFailure::InvalidBindingRequest {
                reason: format!(
                    "extension {} ships with ZLO and needs no consent",
                    package_ref.id.as_str()
                ),
            });
        }
        let current = package_consent_digest(&available)?;
        if current != digest {
            return Err(ProductOperationFailure::InvalidBindingRequest {
                reason: format!(
                    "consent digest does not match the current files of extension {}; review its consent summary again",
                    package_ref.id.as_str()
                ),
            });
        }
        let extension_id = available.package.id.clone();
        let installation_id = ExtensionInstallationId::new(extension_id.as_str().to_string())
            .map_err(map_extension_installation_error)?;
        let _operation_guard = self.operation_lock.lock().await;
        let installation = self
            .installation_store
            .get_installation(&installation_id)
            .await
            .map_err(map_extension_installation_error)?;
        if let Some(installation) = installation.as_ref() {
            ensure_caller_may_operate(installation, caller)?;
            // Consent is what lets a shared install activate (at boot too), and
            // the grant may migrate the durable manifest below: the same
            // tenant-operator authority activation demands.
            ensure_caller_may_mutate_tenant_installation(
                installation,
                caller,
                &self.tenant_operator_user_id,
                "grant consent for",
            )?;
            // The files changed under an existing installation (the
            // `tampered` case): the durable manifest row must describe the
            // package the user just consented to, or activation would publish
            // the old declaration over the new files.
            let current_hash = crate::lifecycle_restore::available_manifest_hash(&available)?;
            if installation.manifest_ref().manifest_hash() != Some(&current_hash) {
                let plan =
                    crate::lifecycle_restore::prepare_manifest_migration(&available, installation)?;
                self.installation_store
                    .upsert_manifest_and_installation(plan.manifest_record, plan.installation)
                    .await
                    .map_err(map_extension_installation_error)?;
            }
        }
        self.consent.grant(&extension_id, &digest, caller).await?;
        let phase = if installation.is_none() {
            InstallationState::Installed
        } else if self
            .active_extensions
            .snapshot()
            .get_extension(&extension_id)
            .is_some()
        {
            InstallationState::Active
        } else {
            InstallationState::Installed
        };
        let mut response = response_with_payload(
            Some(package_ref),
            phase,
            LifecycleProductPayload::ExtensionGrantConsent { granted: true },
        );
        response.consent = Some(consent_summary(&available)?);
        Ok(response)
    }

    /// Every installed extension that must not activate at boot. Composition
    /// excludes these from the generic host's boot hydration, which rebuilds
    /// packages from the *persisted* manifest — so classification follows the
    /// persisted source, and a consent-requiring installation the catalog
    /// cannot vouch for (its files went missing or no longer parse, so the
    /// filesystem catalog skipped it) is blocked: fail closed.
    pub async fn consent_blocked_extension_ids(
        &self,
    ) -> Result<BTreeSet<String>, ProductOperationFailure> {
        let installations = self
            .installation_store
            .list_installations()
            .await
            .map_err(map_extension_installation_error)?;
        let mut blocked = BTreeSet::new();
        for installation in installations {
            let extension_id = installation.extension_id();
            let persisted_source = self
                .installation_store
                .get_manifest(extension_id)
                .await
                .map_err(map_extension_installation_error)?
                .map(|record| record.manifest().source);
            let resolved = match LifecyclePackageRef::new(
                LifecyclePackageKind::Extension,
                extension_id.as_str(),
            ) {
                Ok(package_ref) => self.consent_catalog_package(&package_ref).await.ok(),
                Err(_) => None,
            };
            let requires_consent = persisted_source.is_some_and(source_requires_consent)
                || resolved
                    .as_ref()
                    .is_some_and(|available| source_requires_consent(available.source));
            if !requires_consent {
                continue;
            }
            let is_blocked = match resolved {
                // The durable row says third-party but the catalog has no
                // verifiable package for it: nothing to check a grant against.
                None => true,
                // A catalog package that claims to need no consent for an
                // installation persisted as third-party is not trusted either.
                Some(available) if !source_requires_consent(available.source) => true,
                Some(available) => self
                    .consent
                    .status(&available)
                    .await?
                    .blocking_phase()
                    .is_some(),
            };
            if is_blocked {
                tracing::debug!(
                    extension_id = extension_id.as_str(),
                    "extension excluded from boot hydration until consent covers its files"
                );
                blocked.insert(extension_id.as_str().to_string());
            }
        }
        Ok(blocked)
    }

    /// Drop the grant once the installation is gone, so a later reinstall
    /// asks again.
    pub(super) async fn revoke_consent_after_removal(
        &self,
        extension_id: &ExtensionId,
    ) -> Result<(), ProductOperationFailure> {
        let installation_id = ExtensionInstallationId::new(extension_id.as_str().to_string())
            .map_err(map_extension_installation_error)?;
        let still_installed = self
            .installation_store
            .get_installation(&installation_id)
            .await
            .map_err(map_extension_installation_error)?
            .is_some();
        if still_installed {
            return Ok(());
        }
        self.consent.revoke(extension_id).await
    }
}

fn consent_message(phase: InstallationState, extension_id: &str) -> String {
    match phase {
        InstallationState::Tampered => format!(
            "The files of plugin \"{extension_id}\" changed after you allowed it, so it will not run. Review what it asks for and grant consent again to use it."
        ),
        _ => format!(
            "Plugin \"{extension_id}\" needs your consent before it can run. Review what it asks for, then grant consent for this digest."
        ),
    }
}
