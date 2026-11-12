use super::*;

/// An exclusive mutation lease on an installation record — the cross-process
/// gate for multi-row transitions. While a lease is held the installation is
/// hidden from typed reads (its manifest stays readable) so readers never see
/// a torn aggregate. `member: None` is a whole-aggregate mutation (update or
/// full removal); `member: Some(user)` is that member's removal, recorded so
/// the same caller's retry re-enters its own lease and no other flow can
/// complete it. There is no expiry: a lease orphaned by a crash is resolved
/// by startup repair (restore from the compatibility snapshot, or finish the
/// removal).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct V2MutationLease {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) member: Option<UserId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) preparation: Option<V2PreparationLease>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct V2PreparationLease {
    pub(super) incarnation_id: InstallationIncarnationId,
    pub(super) expected_manifest_ref: ExtensionManifestRef,
}

impl V2MutationLease {
    pub(super) fn update() -> Self {
        Self {
            member: None,
            preparation: None,
        }
    }

    pub(super) fn member_removal(user_id: UserId) -> Self {
        Self {
            member: Some(user_id),
            preparation: None,
        }
    }

    pub(super) fn preparation(
        incarnation_id: InstallationIncarnationId,
        expected_manifest_ref: ExtensionManifestRef,
    ) -> Self {
        Self {
            member: None,
            preparation: Some(V2PreparationLease {
                incarnation_id,
                expected_manifest_ref,
            }),
        }
    }
}

/// The installation record: the deployment's install pin (the embedded,
/// hash-carrying manifest wire) plus instance lifecycle. Lifecycle state is
/// derived, not stored: `removed_at: Some` is the tombstone, `lease: Some` is
/// an in-flight exclusive mutation, and a record with neither is live.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct V2InstallationRecord {
    pub(super) schema_version: String,
    pub(super) installation_id: ExtensionInstallationId,
    pub(super) extension_id: ExtensionId,
    /// Written by 1.2.x builds, which added the field under this same
    /// `extension_state.v2` schema string. This release has no persisted
    /// activation concept -- `ExtensionActivationState` is a compatibility
    /// shim that always reports `Enabled` -- so the value is never read here.
    /// It is nonetheless carried forward rather than discarded like the
    /// diagnostic `health` field: `health` is regenerable, whereas this
    /// records operator intent that a rollback to 1.2 would otherwise
    /// silently reset to enabled. Absent on rows this release writes fresh,
    /// which 1.2 reads as its own `enabled` default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) activation_state: Option<String>,
    pub(super) manifest: WireManifestRecord,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) incarnation_id: Option<InstallationIncarnationId>,
    pub(super) legacy_tenant_owner: bool,
    pub(super) updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) removed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) lease: Option<V2MutationLease>,
    /// A tombstoned record whose removal cleanup has not yet converged. While
    /// set, the embedded definition stays authoritative for `get_manifest`
    /// readers so removal retries work without the catalog and fresh imports
    /// stay blocked until `delete_manifest` marks convergence. Fails open to
    /// `false` only for records written before the field existed, all of
    /// which had already converged.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(super) removal_cleanup_pending: bool,
}

impl V2InstallationRecord {
    pub(super) fn is_removed(&self) -> bool {
        self.removed_at.is_some()
    }

    /// Visible = reconstructable by typed reads: not removed, no lease held.
    pub(super) fn is_visible(&self) -> bool {
        self.removed_at.is_none() && self.lease.is_none()
    }

    /// Whether the embedded definition is authoritative for manifest readers:
    /// every non-removed record, plus removal-cleanup tombstones that have
    /// not yet converged.
    pub(super) fn manifest_is_authoritative(&self) -> bool {
        !self.is_removed() || self.removal_cleanup_pending
    }

    pub(super) fn manifest_ref(&self) -> ExtensionManifestRef {
        ExtensionManifestRef::new(
            self.extension_id.clone(),
            self.manifest.manifest_hash.clone(),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct V2MembershipRecord {
    pub(super) schema_version: String,
    pub(super) installation_id: ExtensionInstallationId,
    pub(super) user_id: UserId,
    pub(super) installed_at: DateTime<Utc>,
    pub(super) updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) removed_at: Option<DateTime<Utc>>,
}

impl V2MembershipRecord {
    pub(super) fn is_active(&self) -> bool {
        self.removed_at.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct V2CredentialBindingRecord {
    pub(super) schema_version: String,
    pub(super) installation_id: ExtensionInstallationId,
    pub(super) credential_handle: ExtensionCredentialHandle,
    pub(super) secret_handle: SecretHandle,
    pub(super) position: u32,
    pub(super) created_at: DateTime<Utc>,
    pub(super) updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) removed_at: Option<DateTime<Utc>>,
}

impl V2CredentialBindingRecord {
    pub(super) fn is_active(&self) -> bool {
        self.removed_at.is_none()
    }
}
