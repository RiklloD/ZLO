use super::*;

/// Generic extension installation state store.
///
/// Implementations own product-agnostic manifest records, installation
/// activation state, opaque credential bindings, and
/// manifest-hash consistency. Domain crates validate domain-specific binding
/// semantics when projecting their host-api sections from these records.
#[async_trait]
pub trait ExtensionInstallationStorePort: Send + Sync {
    /// Immutably admit one package definition into the explicit catalog.
    /// Replaying the exact same definition is idempotent; a different
    /// definition for the same extension id is rejected without mutation.
    async fn admit_package_definition(
        &self,
        record: ExtensionManifestRecord,
    ) -> Result<PackageDefinitionAdmissionOutcome, ExtensionInstallationError> {
        let _ = record;
        Err(store_unavailable_error(
            "extension installation store does not implement package definition admission",
        ))
    }

    /// Read only definitions admitted through [`Self::admit_package_definition`].
    /// Legacy manifest rows and installation-embedded manifests are excluded.
    async fn get_registered_package_definition(
        &self,
        extension_id: &ExtensionId,
    ) -> Result<Option<ExtensionManifestRecord>, ExtensionInstallationError> {
        let _ = extension_id;
        Ok(None)
    }

    /// Enumerate every definition admitted through
    /// [`Self::admit_package_definition`], regardless of whether it currently
    /// has a live installation. Used at restart to repopulate the in-memory
    /// catalog for registered-but-never-installed (or install-then-removed)
    /// definitions. A store with no registrations returns an empty vec rather
    /// than an error.
    async fn list_registered_package_definitions(
        &self,
    ) -> Result<Vec<ExtensionManifestRecord>, ExtensionInstallationError> {
        Ok(Vec::new())
    }

    async fn list_manifests(
        &self,
    ) -> Result<Vec<ExtensionManifestRecord>, ExtensionInstallationError>;

    async fn get_manifest(
        &self,
        extension_id: &ExtensionId,
    ) -> Result<Option<ExtensionManifestRecord>, ExtensionInstallationError>;

    /// Durably retain `manifest` as removal-cleanup metadata for an extension
    /// with no live installation, so an interrupted orphan cleanup can retry
    /// without the catalog. The definition stays visible to `get_manifest`
    /// until `delete_manifest` marks the removal converged.
    async fn persist_removal_tombstone(
        &self,
        manifest: ExtensionManifestRecord,
    ) -> Result<(), ExtensionInstallationError>;

    async fn upsert_manifest_and_installation(
        &self,
        manifest: ExtensionManifestRecord,
        installation: ExtensionInstallation,
    ) -> Result<(), ExtensionInstallationError>;

    async fn list_installations(
        &self,
    ) -> Result<Vec<ExtensionInstallation>, ExtensionInstallationError>;

    async fn get_installation(
        &self,
        installation_id: &ExtensionInstallationId,
    ) -> Result<Option<ExtensionInstallation>, ExtensionInstallationError>;

    async fn upsert_installation(
        &self,
        installation: ExtensionInstallation,
    ) -> Result<(), ExtensionInstallationError>;

    /// Conditionally refresh only the manifest embedded in a live installation.
    /// The current installation row is read and retained by the CAS transform,
    /// so concurrent membership and credential state cannot be overwritten by
    /// a manifest refresh. A changed incarnation or manifest reference rejects
    /// the refresh.
    async fn upsert_manifest_only(
        &self,
        installation_id: &ExtensionInstallationId,
        expected_incarnation_id: Option<&InstallationIncarnationId>,
        expected_manifest_ref: &ExtensionManifestRef,
        expected_updated_at: DateTime<Utc>,
        manifest: ExtensionManifestRecord,
    ) -> Result<(), ExtensionInstallationError> {
        let _ = (
            installation_id,
            expected_incarnation_id,
            expected_manifest_ref,
            expected_updated_at,
            manifest,
        );
        Err(store_unavailable_error(
            "extension installation store does not implement manifest-only updates",
        ))
    }

    /// Conditionally commit a prepared manifest. Implementations must reject
    /// a stale incarnation or pending-manifest reference without publication.
    async fn finalize_preparation(
        &self,
        installation_id: &ExtensionInstallationId,
        incarnation_id: &InstallationIncarnationId,
        expected_pending_manifest_ref: &ExtensionManifestRef,
        finalized_manifest: ExtensionManifestRecord,
    ) -> Result<ExtensionInstallation, ExtensionInstallationError> {
        let _ = (
            installation_id,
            incarnation_id,
            expected_pending_manifest_ref,
            finalized_manifest,
        );
        Err(store_unavailable_error(
            "extension installation store does not implement preparation finalization",
        ))
    }

    /// Conditionally replace one pending manifest with another while keeping
    /// the installation pending and non-callable.
    async fn checkpoint_preparation(
        &self,
        installation_id: &ExtensionInstallationId,
        incarnation_id: &InstallationIncarnationId,
        expected_pending_manifest_ref: &ExtensionManifestRef,
        next_pending_manifest: ExtensionManifestRecord,
    ) -> Result<ExtensionInstallation, ExtensionInstallationError> {
        let _ = (
            installation_id,
            incarnation_id,
            expected_pending_manifest_ref,
            next_pending_manifest,
        );
        Err(store_unavailable_error(
            "extension installation store does not implement preparation checkpointing",
        ))
    }

    async fn activate_membership(
        &self,
        installation_id: &ExtensionInstallationId,
        user_id: &UserId,
    ) -> Result<ExtensionInstallation, ExtensionInstallationError>;

    async fn deactivate_membership(
        &self,
        installation_id: &ExtensionInstallationId,
        user_id: &UserId,
    ) -> Result<MembershipDeactivation, ExtensionInstallationError>;

    async fn delete_installation(
        &self,
        installation_id: &ExtensionInstallationId,
    ) -> Result<(), ExtensionInstallationError>;

    async fn delete_manifest(
        &self,
        extension_id: &ExtensionId,
    ) -> Result<(), ExtensionInstallationError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MembershipDeactivation {
    MembershipRemoved(Box<ExtensionInstallation>),
    FinalMemberReserved,
}

#[async_trait]
impl<T> ExtensionInstallationStorePort for Arc<T>
where
    T: ExtensionInstallationStorePort + ?Sized,
{
    async fn admit_package_definition(
        &self,
        record: ExtensionManifestRecord,
    ) -> Result<PackageDefinitionAdmissionOutcome, ExtensionInstallationError> {
        (**self).admit_package_definition(record).await
    }

    async fn get_registered_package_definition(
        &self,
        extension_id: &ExtensionId,
    ) -> Result<Option<ExtensionManifestRecord>, ExtensionInstallationError> {
        (**self)
            .get_registered_package_definition(extension_id)
            .await
    }

    async fn list_registered_package_definitions(
        &self,
    ) -> Result<Vec<ExtensionManifestRecord>, ExtensionInstallationError> {
        (**self).list_registered_package_definitions().await
    }

    async fn list_manifests(
        &self,
    ) -> Result<Vec<ExtensionManifestRecord>, ExtensionInstallationError> {
        (**self).list_manifests().await
    }

    async fn get_manifest(
        &self,
        extension_id: &ExtensionId,
    ) -> Result<Option<ExtensionManifestRecord>, ExtensionInstallationError> {
        (**self).get_manifest(extension_id).await
    }

    async fn persist_removal_tombstone(
        &self,
        manifest: ExtensionManifestRecord,
    ) -> Result<(), ExtensionInstallationError> {
        (**self).persist_removal_tombstone(manifest).await
    }

    async fn upsert_manifest_and_installation(
        &self,
        manifest: ExtensionManifestRecord,
        installation: ExtensionInstallation,
    ) -> Result<(), ExtensionInstallationError> {
        (**self)
            .upsert_manifest_and_installation(manifest, installation)
            .await
    }

    async fn list_installations(
        &self,
    ) -> Result<Vec<ExtensionInstallation>, ExtensionInstallationError> {
        (**self).list_installations().await
    }

    async fn get_installation(
        &self,
        installation_id: &ExtensionInstallationId,
    ) -> Result<Option<ExtensionInstallation>, ExtensionInstallationError> {
        (**self).get_installation(installation_id).await
    }

    async fn upsert_installation(
        &self,
        installation: ExtensionInstallation,
    ) -> Result<(), ExtensionInstallationError> {
        (**self).upsert_installation(installation).await
    }

    async fn upsert_manifest_only(
        &self,
        installation_id: &ExtensionInstallationId,
        expected_incarnation_id: Option<&InstallationIncarnationId>,
        expected_manifest_ref: &ExtensionManifestRef,
        expected_updated_at: DateTime<Utc>,
        manifest: ExtensionManifestRecord,
    ) -> Result<(), ExtensionInstallationError> {
        (**self)
            .upsert_manifest_only(
                installation_id,
                expected_incarnation_id,
                expected_manifest_ref,
                expected_updated_at,
                manifest,
            )
            .await
    }

    async fn finalize_preparation(
        &self,
        installation_id: &ExtensionInstallationId,
        incarnation_id: &InstallationIncarnationId,
        expected_pending_manifest_ref: &ExtensionManifestRef,
        finalized_manifest: ExtensionManifestRecord,
    ) -> Result<ExtensionInstallation, ExtensionInstallationError> {
        (**self)
            .finalize_preparation(
                installation_id,
                incarnation_id,
                expected_pending_manifest_ref,
                finalized_manifest,
            )
            .await
    }

    async fn checkpoint_preparation(
        &self,
        installation_id: &ExtensionInstallationId,
        incarnation_id: &InstallationIncarnationId,
        expected_pending_manifest_ref: &ExtensionManifestRef,
        next_pending_manifest: ExtensionManifestRecord,
    ) -> Result<ExtensionInstallation, ExtensionInstallationError> {
        (**self)
            .checkpoint_preparation(
                installation_id,
                incarnation_id,
                expected_pending_manifest_ref,
                next_pending_manifest,
            )
            .await
    }

    async fn activate_membership(
        &self,
        installation_id: &ExtensionInstallationId,
        user_id: &UserId,
    ) -> Result<ExtensionInstallation, ExtensionInstallationError> {
        (**self).activate_membership(installation_id, user_id).await
    }

    async fn deactivate_membership(
        &self,
        installation_id: &ExtensionInstallationId,
        user_id: &UserId,
    ) -> Result<MembershipDeactivation, ExtensionInstallationError> {
        (**self)
            .deactivate_membership(installation_id, user_id)
            .await
    }

    async fn delete_installation(
        &self,
        installation_id: &ExtensionInstallationId,
    ) -> Result<(), ExtensionInstallationError> {
        (**self).delete_installation(installation_id).await
    }

    async fn delete_manifest(
        &self,
        extension_id: &ExtensionId,
    ) -> Result<(), ExtensionInstallationError> {
        (**self).delete_manifest(extension_id).await
    }
}
