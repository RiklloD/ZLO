use super::*;

/// The caller-membership axis of an installation.
///
/// `Users` is the only owner shape created by current lifecycle operations:
/// admins and ordinary users install/remove personal membership identically.
/// Tenant-scoped deployment configuration is deliberately not represented by
/// this enum. A future explicit required-extension policy must compose with
/// caller membership rather than overloading owner identity.
///
/// `Tenant` is retained only because persisted records that predate caller
/// ownership omit this field and deserialize through `#[serde(default)]`.
/// Composition narrows those compatibility rows to the configured operator at
/// restore before ordinary lifecycle operations. New code must not create a
/// `Tenant` row or interpret an admin's personal install as tenant-wide.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum InstallationOwner {
    #[default]
    Tenant,
    Users {
        user_ids: BTreeSet<UserId>,
    },
}

impl InstallationOwner {
    /// Singleton member set — what a single member's install produces.
    pub fn user(user_id: UserId) -> Self {
        Self::Users {
            user_ids: BTreeSet::from([user_id]),
        }
    }

    /// Member set; rejects an empty set (an installation must belong to the
    /// tenant or to at least one member — an empty set would be a row nobody
    /// can see, operate, or remove).
    pub fn users(user_ids: BTreeSet<UserId>) -> Result<Self, ExtensionInstallationError> {
        if user_ids.is_empty() {
            return Err(ExtensionInstallationError::EmptyOwnerMembers);
        }
        Ok(Self::Users { user_ids })
    }

    pub fn is_tenant(&self) -> bool {
        matches!(self, Self::Tenant)
    }

    /// The member set, if the installation is member-held.
    pub fn members(&self) -> Option<&BTreeSet<UserId>> {
        match self {
            Self::Users { user_ids } => Some(user_ids),
            Self::Tenant => None,
        }
    }

    /// Return the caller-membership rewrite needed to join `user_id`.
    ///
    /// `None` means the user is already a member and the operation is an
    /// idempotent no-op. A legacy `Tenant` compatibility row narrows to the
    /// first explicit caller; current lifecycle code never creates `Tenant`.
    pub fn joined_by(&self, user_id: &UserId) -> Result<Option<Self>, ExtensionInstallationError> {
        match self {
            Self::Tenant => Ok(Some(Self::user(user_id.clone()))),
            Self::Users { user_ids } if user_ids.contains(user_id) => Ok(None),
            Self::Users { user_ids } => {
                let mut joined = user_ids.clone();
                joined.insert(user_id.clone());
                Self::users(joined).map(Some)
            }
        }
    }

    /// Return the remaining caller-membership set after `user_id` leaves.
    /// `None` means no members remain and the shared runtime row may be torn
    /// down. Callers must authorize membership before invoking this method.
    /// A legacy tenant row must first be canonicalized to an explicit member;
    /// this transition never guesses which caller owns that shared row.
    pub fn without_member(
        &self,
        user_id: &UserId,
    ) -> Result<Option<Self>, ExtensionInstallationError> {
        match self {
            Self::Tenant => Err(ExtensionInstallationError::LegacyTenantOwnerNotCanonicalized),
            Self::Users { user_ids } => {
                let remaining = user_ids
                    .iter()
                    .filter(|member| *member != user_id)
                    .cloned()
                    .collect::<BTreeSet<_>>();
                if remaining.is_empty() {
                    Ok(None)
                } else {
                    Self::users(remaining).map(Some)
                }
            }
        }
    }

    /// Whether `caller` may see/use this installation: tenant-wide entries
    /// are visible to everyone, member-held entries only to their members.
    pub fn visible_to(&self, caller: &UserId) -> bool {
        match self {
            Self::Tenant => true,
            Self::Users { user_ids } => user_ids.contains(caller),
        }
    }
}

impl Serialize for InstallationOwner {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        InstallationOwnerWire::from(self.clone()).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for InstallationOwner {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        InstallationOwnerWire::deserialize(deserializer)?
            .try_into()
            .map_err(serde::de::Error::custom)
    }
}

/// Wire shape of [`InstallationOwner`]. `user` is the read-only legacy kind
/// written by the slot iteration of #5459 P1 (a single owning user); it folds
/// into a singleton member set on load and is never written back.
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum InstallationOwnerWire {
    Tenant,
    User { user_id: UserId },
    Users { user_ids: BTreeSet<UserId> },
}

impl From<InstallationOwner> for InstallationOwnerWire {
    fn from(owner: InstallationOwner) -> Self {
        match owner {
            InstallationOwner::Tenant => Self::Tenant,
            InstallationOwner::Users { user_ids } => Self::Users { user_ids },
        }
    }
}

impl TryFrom<InstallationOwnerWire> for InstallationOwner {
    type Error = ExtensionInstallationError;

    fn try_from(wire: InstallationOwnerWire) -> Result<Self, Self::Error> {
        match wire {
            InstallationOwnerWire::Tenant => Ok(Self::Tenant),
            InstallationOwnerWire::User { user_id } => Ok(Self::user(user_id)),
            InstallationOwnerWire::Users { user_ids } => Self::users(user_ids),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExtensionInstallation {
    installation_id: ExtensionInstallationId,
    extension_id: ExtensionId,
    manifest_ref: ExtensionManifestRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    incarnation_id: Option<InstallationIncarnationId>,
    credential_bindings: Vec<ExtensionCredentialBinding>,
    updated_at: DateTime<Utc>,
    // `Tenant` is a read-only compatibility shape for records written before
    // caller-scoped installations. Restore narrows it to the configured
    // operator; new lifecycle operations never create it.
    #[serde(default, skip_serializing_if = "InstallationOwner::is_tenant")]
    owner: InstallationOwner,
}

/// All persisted fields needed to reconstruct an installation without
/// inventing fresh timestamp state.
#[derive(Debug)]
pub struct ExtensionInstallationPersistedParts {
    pub installation_id: ExtensionInstallationId,
    pub extension_id: ExtensionId,
    pub manifest_ref: ExtensionManifestRef,
    pub incarnation_id: Option<InstallationIncarnationId>,
    pub credential_bindings: Vec<ExtensionCredentialBinding>,
    pub updated_at: DateTime<Utc>,
    pub owner: InstallationOwner,
}

impl ExtensionInstallation {
    pub fn new(
        installation_id: ExtensionInstallationId,
        extension_id: ExtensionId,
        manifest_ref: ExtensionManifestRef,
        credential_bindings: Vec<ExtensionCredentialBinding>,
        updated_at: DateTime<Utc>,
        owner: InstallationOwner,
    ) -> Result<Self, ExtensionInstallationError> {
        Self::from_persisted_parts(ExtensionInstallationPersistedParts {
            installation_id,
            extension_id,
            manifest_ref,
            incarnation_id: Some(InstallationIncarnationId::fresh()),
            credential_bindings,
            updated_at,
            owner,
        })
    }

    /// Reconstruct an installation with all state read from persistence.
    ///
    /// Persistence adapters use this neutral constructor when they need to
    /// preserve an existing timestamp while changing the canonical
    /// installation identity.
    pub fn from_persisted_parts(
        parts: ExtensionInstallationPersistedParts,
    ) -> Result<Self, ExtensionInstallationError> {
        if parts.manifest_ref.extension_id() != &parts.extension_id {
            return Err(ExtensionInstallationError::ManifestExtensionMismatch {
                extension_id: parts.extension_id,
                manifest_extension_id: parts.manifest_ref.extension_id().clone(),
            });
        }
        validate_bindings_unique(&parts.credential_bindings)?;
        Ok(Self {
            installation_id: parts.installation_id,
            extension_id: parts.extension_id,
            manifest_ref: parts.manifest_ref,
            incarnation_id: parts.incarnation_id,
            credential_bindings: parts.credential_bindings,
            updated_at: parts.updated_at,
            owner: parts.owner,
        })
    }

    pub fn installation_id(&self) -> &ExtensionInstallationId {
        &self.installation_id
    }

    pub fn extension_id(&self) -> &ExtensionId {
        &self.extension_id
    }

    pub fn manifest_ref(&self) -> &ExtensionManifestRef {
        &self.manifest_ref
    }

    pub fn incarnation_id(&self) -> Option<&InstallationIncarnationId> {
        self.incarnation_id.as_ref()
    }

    pub fn credential_bindings(&self) -> &[ExtensionCredentialBinding] {
        &self.credential_bindings
    }

    pub fn updated_at(&self) -> DateTime<Utc> {
        self.updated_at
    }

    pub fn owner(&self) -> &InstallationOwner {
        &self.owner
    }

    /// Same installation with a replaced caller-membership set; refreshes
    /// `updated_at` like every other row mutation.
    pub fn with_owner(mut self, owner: InstallationOwner) -> Self {
        self.owner = owner;
        self.updated_at = Utc::now();
        self
    }
}

impl<'de> Deserialize<'de> for ExtensionInstallation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            installation_id: ExtensionInstallationId,
            extension_id: ExtensionId,
            manifest_ref: ExtensionManifestRef,
            #[serde(default)]
            incarnation_id: Option<InstallationIncarnationId>,
            credential_bindings: Vec<ExtensionCredentialBinding>,
            // The released aggregate row carries a diagnostic `health`
            // object. It is not modelled here -- the host's activation record
            // already owns extension failure state -- so it is accepted and
            // discarded rather than failing `deny_unknown_fields` on a row
            // written by the previous release.
            #[serde(default)]
            health: serde::de::IgnoredAny,
            updated_at: DateTime<Utc>,
            // Legacy records predate the owner field; they were all
            // tenant-visible, so absent == Tenant is behavior-preserving.
            #[serde(default)]
            owner: InstallationOwner,
        }
        let wire = Wire::deserialize(deserializer)?;
        if wire.manifest_ref.extension_id() != &wire.extension_id {
            return Err(serde::de::Error::custom(
                ExtensionInstallationError::ManifestExtensionMismatch {
                    extension_id: wire.extension_id,
                    manifest_extension_id: wire.manifest_ref.extension_id().clone(),
                },
            ));
        }
        validate_bindings_unique(&wire.credential_bindings).map_err(serde::de::Error::custom)?;
        let _ = wire.health;
        Ok(Self {
            installation_id: wire.installation_id,
            extension_id: wire.extension_id,
            manifest_ref: wire.manifest_ref,
            incarnation_id: wire.incarnation_id,
            credential_bindings: wire.credential_bindings,
            updated_at: wire.updated_at,
            owner: wire.owner,
        })
    }
}

#[cfg(test)]
mod tests {

    use ironclaw_host_api::ids::ExtensionId;

    use super::*;

    use crate::installations::test_support::*;

    #[test]
    fn legacy_installation_wire_loads_without_an_incarnation() {
        let installation = installation("fixture", Some("hash-1"));
        let mut wire = serde_json::to_value(&installation).expect("serialize installation");
        wire.as_object_mut()
            .expect("object wire")
            .remove("incarnation_id");

        let restored: ExtensionInstallation =
            serde_json::from_value(wire).expect("legacy installation wire");
        assert_eq!(restored.incarnation_id(), None);
    }

    #[test]
    fn fresh_pending_installations_have_distinct_opaque_incarnations() {
        let first = pending_installation("fixture", Some("hash-1"));
        let second = pending_installation("fixture", Some("hash-1"));
        assert_ne!(first.incarnation_id(), second.incarnation_id());
    }

    /// #5459 P1: legacy persisted rows predate the `owner` field and were all
    /// tenant-visible; a record without it MUST deserialize as `Tenant` (no
    /// migration). The inverse also holds: a TENANT-owned record serializes
    /// WITHOUT the field, keeping the exact pre-#5459 byte shape so a rollback
    /// to an older binary (`deny_unknown_fields` wire struct) still loads a
    /// state.json holding no private installs. A user-owned record must
    /// round-trip its owner.
    #[test]
    fn installation_owner_defaults_to_tenant_for_legacy_rows_and_round_trips() {
        let current = installation("fixture", Some("hash-1"));
        let json = serde_json::to_value(&current).expect("serialize installation");
        assert!(
            json.get("owner").is_none(),
            "tenant-owned rows must keep the pre-#5459 shape (rollback compat): {json}"
        );
        let legacy: ExtensionInstallation =
            serde_json::from_value(json).expect("legacy row without owner deserializes");
        assert_eq!(legacy.owner(), &InstallationOwner::Tenant);

        let alice = ironclaw_host_api::ids::UserId::new("alice").expect("user id");
        let private = ExtensionInstallation::new(
            ExtensionInstallationId::new("fixture".to_string()).expect("installation id"),
            ExtensionId::new("fixture".to_string()).expect("extension id"),
            ExtensionManifestRef::new(ExtensionId::new("fixture".to_string()).unwrap(), None),
            Vec::new(),
            Utc::now(),
            InstallationOwner::user(alice.clone()),
        )
        .expect("installation");
        let json = serde_json::to_string(&private).expect("serialize");
        assert!(
            json.contains(r#""kind":"users""#),
            "member-held rows serialize the set shape: {json}"
        );
        let restored: ExtensionInstallation = serde_json::from_str(&json).expect("round-trip");
        assert!(restored.owner().visible_to(&alice));
        assert_eq!(
            restored.owner().members().map(BTreeSet::len),
            Some(1),
            "singleton member set round-trips"
        );
    }

    /// Membership pivot (2026-07-08): rows written by the slot iteration
    /// carry `{"kind": "user", "user_id": …}` — they MUST keep loading, as a
    /// singleton member set; an empty member set is rejected on the wire and
    /// at construction (a row nobody could see, operate, or remove).
    #[test]
    fn slot_iteration_user_owner_rows_load_as_singleton_member_set() {
        let alice = ironclaw_host_api::ids::UserId::new("alice").expect("user id");
        let bob = ironclaw_host_api::ids::UserId::new("bob").expect("user id");
        let legacy: InstallationOwner =
            serde_json::from_str(r#"{"kind":"user","user_id":"alice"}"#)
                .expect("slot-iteration owner row loads");
        assert!(legacy.visible_to(&alice));
        assert!(!legacy.visible_to(&bob));
        assert_eq!(legacy, InstallationOwner::user(alice.clone()));

        let set: InstallationOwner =
            serde_json::from_str(r#"{"kind":"users","user_ids":["alice","bob"]}"#)
                .expect("member set loads");
        assert!(set.visible_to(&alice) && set.visible_to(&bob));

        serde_json::from_str::<InstallationOwner>(r#"{"kind":"users","user_ids":[]}"#)
            .expect_err("empty member set is rejected on the wire");
        InstallationOwner::users(BTreeSet::new()).expect_err("empty member set is unconstructable");
    }

    #[test]
    fn caller_membership_join_and_leave_are_idempotent_domain_transitions() {
        let alice = ironclaw_host_api::ids::UserId::new("alice").expect("user id");
        let bob = ironclaw_host_api::ids::UserId::new("bob").expect("user id");

        assert_eq!(
            InstallationOwner::Tenant
                .without_member(&alice)
                .expect_err("legacy tenant rows must be narrowed before removal"),
            ExtensionInstallationError::LegacyTenantOwnerNotCanonicalized,
            "a caller must never tear down a legacy shared row directly"
        );

        let alice_only = InstallationOwner::Tenant
            .joined_by(&alice)
            .expect("legacy owner narrows")
            .expect("owner changes");
        assert_eq!(alice_only, InstallationOwner::user(alice.clone()));
        assert_eq!(
            alice_only.joined_by(&alice).expect("same-member retry"),
            None,
            "joining an existing member must not rewrite the row"
        );

        let alice_and_bob = alice_only
            .joined_by(&bob)
            .expect("Bob joins")
            .expect("owner changes");
        assert!(alice_and_bob.visible_to(&alice));
        assert!(alice_and_bob.visible_to(&bob));

        let bob_only = alice_and_bob
            .without_member(&alice)
            .expect("Alice leaves")
            .expect("Bob remains");
        assert!(!bob_only.visible_to(&alice));
        assert!(bob_only.visible_to(&bob));
        assert_eq!(
            bob_only.without_member(&bob).expect("Bob leaves"),
            None,
            "the last member tears down the aggregate"
        );
    }
}
