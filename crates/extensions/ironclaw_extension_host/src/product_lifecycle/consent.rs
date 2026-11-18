//! The lifecycle manager's consent gate (see `crate::extension_consent` for
//! the model). Kept beside `product_lifecycle.rs` as a child module so the
//! manager's lock-ordering rules apply unchanged: the catalog is read before
//! `operation_lock`, never while holding it.
//!
//! Every decision here is made for a principal: the caller (or, on a
//! tenant-owned installation, the tenant admin). One user's grant never
//! unblocks another user's install, activation, join, or per-user surfaces.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use ironclaw_extension_contracts::runtime::ExtensionRuntime;
use ironclaw_extension_contracts::state::InstallationState;
use ironclaw_extension_registry::{
    ExtensionInstallation, ExtensionInstallationId, ExtensionPackage, InstallationOwner,
    ManifestSource,
};
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
    ConsentEvaluation, ConsentPrincipal, consent_summary, current_package_files,
    package_consent_digest, package_files_digest, package_files_pins, source_requires_consent,
};

impl ExtensionLifecycleManager {
    /// The durable consent grants, for the generic host's loader (which binds
    /// third-party packages to their grant before a runtime lane runs them).
    pub fn consent_store(&self) -> crate::extension_consent::ExtensionConsentStore {
        self.consent.clone()
    }

    async fn consent_catalog_package(
        &self,
        package_ref: &LifecyclePackageRef,
    ) -> Result<Arc<AvailableExtensionPackage>, ProductOperationFailure> {
        let catalog = self.catalog.read().await;
        catalog.resolve(package_ref)
    }

    async fn installation_for(
        &self,
        extension_id: &ExtensionId,
    ) -> Result<Option<ExtensionInstallation>, ProductOperationFailure> {
        let installation_id = ExtensionInstallationId::new(extension_id.as_str().to_string())
            .map_err(map_extension_installation_error)?;
        self.installation_store
            .get_installation(&installation_id)
            .await
            .map_err(map_extension_installation_error)
    }

    /// One consent evaluation of a catalog package for `caller` against
    /// `installation` (the caller's principal follows the installation's
    /// owner). Hashes the package files once.
    pub(super) async fn consent_evaluation(
        &self,
        available: &AvailableExtensionPackage,
        installation: Option<&ExtensionInstallation>,
        caller: &UserId,
    ) -> Result<ConsentEvaluation, ProductOperationFailure> {
        let principal =
            ConsentPrincipal::for_caller(installation.map(ExtensionInstallation::owner), caller);
        self.consent.evaluate(available, &principal).await
    }

    /// The response an activation returns instead of activating, when the
    /// package lacks the caller's grant for its current digest — or declares
    /// a runtime a plugin may not use. Read before `operation_lock`
    /// (catalog-before-operation ordering).
    pub(super) async fn consent_blocked_activation(
        &self,
        package_ref: &LifecyclePackageRef,
        caller: &UserId,
    ) -> Result<Option<LifecycleProductResponse>, ProductOperationFailure> {
        let Ok(available) = self.consent_catalog_package(package_ref).await else {
            // Not in the catalog: the activation path reports that itself.
            return Ok(None);
        };
        reject_unsupported_plugin_runtime(&available.package)?;
        let installation = self.installation_for(&available.package.id).await?;
        let evaluation = self
            .consent_evaluation(&available, installation.as_ref(), caller)
            .await?;
        let Some(phase) = evaluation.status.blocking_phase() else {
            return Ok(None);
        };
        tracing::debug!(
            extension_id = package_ref.id.as_str(),
            phase = phase.as_str(),
            "extension activation blocked until the caller grants consent"
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
        response.consent = summary_for(&available, &evaluation);
        Ok(Some(response))
    }

    /// Attach the caller's consent summary to an install (or join) response,
    /// and report the blocking phase when the caller still needs to grant.
    pub(super) async fn apply_consent_to_install_response(
        &self,
        available: &AvailableExtensionPackage,
        mut response: LifecycleProductResponse,
        caller: &UserId,
    ) -> Result<LifecycleProductResponse, ProductOperationFailure> {
        let installation = self.installation_for(&available.package.id).await?;
        let evaluation = self
            .consent_evaluation(available, installation.as_ref(), caller)
            .await?;
        if let Some(phase) = evaluation.status.blocking_phase() {
            response.phase = phase;
            if let Some(LifecycleProductPayload::ExtensionInstall { next_step, .. }) =
                response.payload.as_mut()
            {
                *next_step = consent_message(phase, available.package.id.as_str());
            }
        }
        response.consent = summary_for(available, &evaluation);
        Ok(response)
    }

    /// The consent summary for a package as `caller` would see it, `None`
    /// when it is exempt.
    pub async fn consent_summary(
        &self,
        package_ref: &LifecyclePackageRef,
        caller: &UserId,
    ) -> Result<Option<LifecycleConsentSummary>, ProductOperationFailure> {
        let available = self.consent_catalog_package(package_ref).await?;
        let installation = self
            .installation_for(&available.package.id)
            .await?
            .filter(|installation| installation.owner().visible_to(caller));
        let evaluation = self
            .consent_evaluation(&available, installation.as_ref(), caller)
            .await?;
        Ok(summary_for(&available, &evaluation))
    }

    /// Grant the caller's consent for `digest`. Fails unless `digest` is the
    /// package's current digest. On a tenant-owned installation the grant is
    /// the tenant admin's and only the tenant admin may make it; otherwise it
    /// is the caller's own decision and covers nobody else. Does not
    /// activate — the caller activates next, through the ordinary activation
    /// path.
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
        reject_unsupported_plugin_runtime(&available.package)?;
        // The grant covers the files as they are on disk now, and they must
        // still be the files the catalog loaded: consent is shown for the
        // catalog's declaration, and activation publishes it.
        let files = current_package_files(self.consent.filesystem(), &available).await?;
        let current = package_files_digest(&available.package.id, &files)?;
        if current != digest {
            return Err(ProductOperationFailure::InvalidBindingRequest {
                reason: format!(
                    "consent digest does not match the current files of extension {}; review its consent summary again",
                    package_ref.id.as_str()
                ),
            });
        }
        if package_consent_digest(&available)? != current {
            return Err(ProductOperationFailure::InvalidBindingRequest {
                reason: format!(
                    "the files of extension {} changed after ZLO loaded them; restart ZLO to review the new version before granting consent",
                    package_ref.id.as_str()
                ),
            });
        }
        let pins = package_files_pins(&files);
        let extension_id = available.package.id.clone();
        let _operation_guard = self.operation_lock.lock().await;
        let installation = self.installation_for(&extension_id).await?;
        if let Some(installation) = installation.as_ref() {
            ensure_caller_may_operate(installation, caller)?;
            // A tenant-owned install runs on the tenant admin's decision (at
            // boot too), and the grant may migrate the durable manifest
            // below: the same tenant-operator authority activation demands.
            ensure_caller_may_mutate_tenant_installation(
                installation,
                caller,
                &self.tenant_operator_user_id,
                "grant consent for",
            )?;
            // The files changed under an existing installation (the
            // `tampered` case): the durable manifest row must describe the
            // package the user just consented to, or activation would publish
            // the old declaration over the new files. Every other member's
            // earlier decision then no longer matches the pins, so each of
            // them is asked again.
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
        let principal =
            ConsentPrincipal::for_caller(installation.as_ref().map(|row| row.owner()), caller);
        self.consent
            .grant(&extension_id, &principal, &digest, &pins, caller)
            .await?;
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
        response.consent = Some(consent_summary(&available, digest));
        Ok(response)
    }

    /// Every installed extension that must not activate at boot. Composition
    /// excludes these from the generic host's boot hydration, which rebuilds
    /// packages from the *persisted* manifest — so classification follows the
    /// persisted source, and a consent-requiring installation the catalog
    /// cannot vouch for (its files went missing or no longer parse, so the
    /// filesystem catalog skipped it) is blocked: fail closed. An
    /// installation hydrates when its files are the pinned ones and at least
    /// one of its principals consented to them; per-user use is narrowed
    /// afterwards by [`Self::installation_owners`].
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
                Some(available) => {
                    plugin_runtime_unsupported(&available.package)
                        || self
                            .consent
                            .evaluate_for_owner(&available, installation.owner())
                            .await?
                            .status
                            .blocking_phase()
                            .is_some()
                }
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

    /// `owners` (every installation's owner, keyed by extension id) narrowed
    /// by consent: for a package that needs consent, only the principals
    /// whose own grant matches its execution pins remain, and a package
    /// nobody remains for is dropped — invisible to everyone, exactly like a
    /// missing owner row. This is the one per-user filter the tool catalog,
    /// live hooks, UI slots, and settings already apply.
    pub(super) async fn consent_filtered_owners(
        &self,
        owners: BTreeMap<ExtensionId, InstallationOwner>,
    ) -> Result<BTreeMap<ExtensionId, InstallationOwner>, ProductOperationFailure> {
        let sources: BTreeMap<ExtensionId, ManifestSource> = self
            .installation_store
            .list_manifests()
            .await
            .map_err(map_extension_installation_error)?
            .into_iter()
            .map(|record| (record.extension_id().clone(), record.manifest().source))
            .collect();
        let mut filtered = BTreeMap::new();
        for (extension_id, owner) in owners {
            // No durable definition: the source is unknown, so consent cannot
            // be ruled out; fail closed.
            let Some(source) = sources.get(&extension_id).copied() else {
                tracing::debug!(
                    extension_id = extension_id.as_str(),
                    "installation without a manifest record is visible to nobody"
                );
                continue;
            };
            if !source_requires_consent(source) {
                filtered.insert(extension_id, owner);
                continue;
            }
            if let Some(consented) = self.consent.consented_owner(&extension_id, &owner).await? {
                filtered.insert(extension_id, consented);
            }
        }
        Ok(filtered)
    }

    /// `package` bound to its execution pins when it needs consent — what
    /// publication must carry so the model-facing catalog and every lane
    /// verify the bytes they read. Refuses a consent-requiring package with
    /// no pins: it never reaches the active registry unbound.
    pub(super) async fn pinned_for_publication(
        &self,
        package: ExtensionPackage,
    ) -> Result<ExtensionPackage, ProductOperationFailure> {
        pin_for_publication(&self.consent, package).await
    }

    /// Drop consent once the installation is gone (every decision and the
    /// pins), so a later reinstall asks again; when only `leaving` left a
    /// still-held installation, drop just their own decision.
    pub(super) async fn revoke_consent_after_removal(
        &self,
        extension_id: &ExtensionId,
        leaving: Option<&UserId>,
    ) -> Result<(), ProductOperationFailure> {
        let still_installed = self.installation_for(extension_id).await?;
        match still_installed {
            None => self.consent.revoke(extension_id).await,
            Some(installation) => match leaving {
                Some(user) if !installation.owner().visible_to(user) => {
                    self.consent
                        .revoke_principal(extension_id, &ConsentPrincipal::User(user.clone()))
                        .await
                }
                _ => Ok(()),
            },
        }
    }
}

/// See [`ExtensionLifecycleManager::pinned_for_publication`]; shared with
/// boot restore.
pub(crate) async fn pin_for_publication(
    consent: &crate::extension_consent::ExtensionConsentStore,
    package: ExtensionPackage,
) -> Result<ExtensionPackage, ProductOperationFailure> {
    if !source_requires_consent(package.manifest.source) {
        return Ok(package);
    }
    let pins = consent.granted_pins(&package.id).await?.ok_or_else(|| {
        ProductOperationFailure::InvalidBindingRequest {
            reason: format!(
                "extension {} has no consent grant binding its files",
                package.id.as_str()
            ),
        }
    })?;
    Ok(package.with_content_pins(pins))
}

/// A consent-requiring package may not declare a runtime that executes its
/// files outside a verified snapshot (today: the script/process runtime).
pub(crate) fn plugin_runtime_unsupported(package: &ExtensionPackage) -> bool {
    source_requires_consent(package.manifest.source)
        && matches!(package.manifest.runtime, ExtensionRuntime::Script { .. })
}

/// The plain refusal for [`plugin_runtime_unsupported`], used at admission
/// (import/install) and at activation.
pub(crate) fn reject_unsupported_plugin_runtime(
    package: &ExtensionPackage,
) -> Result<(), ProductOperationFailure> {
    if plugin_runtime_unsupported(package) {
        return Err(ProductOperationFailure::InvalidBindingRequest {
            reason: format!(
                "plugin {} runs its own program (a script runtime); ZLO cannot run plugin programs yet, only WASM plugins",
                package.id.as_str()
            ),
        });
    }
    Ok(())
}

/// Fold a consent evaluation into a projected installation phase: a package
/// without the caller's grant for its current digest is never reported as
/// anything but blocked.
pub(super) fn consent_projected_phase(
    evaluation: &ConsentEvaluation,
    phase: InstallationState,
) -> InstallationState {
    evaluation.status.blocking_phase().unwrap_or(phase)
}

pub(super) fn summary_for(
    available: &AvailableExtensionPackage,
    evaluation: &ConsentEvaluation,
) -> Option<LifecycleConsentSummary> {
    evaluation
        .digest
        .clone()
        .map(|digest| consent_summary(available, digest))
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
