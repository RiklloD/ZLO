use super::*;

impl ExtensionInstallationStore {
    pub(super) async fn activate_v2_membership_row(
        &self,
        installation_id: &ExtensionInstallationId,
        user_id: &UserId,
        now: DateTime<Utc>,
    ) -> Result<bool, ExtensionInstallationError> {
        let path = self.v2_membership_scoped_path(installation_id, user_id)?;
        let installation_id = installation_id.clone();
        let user_id = user_id.clone();
        cas_update(
            self.scoped_filesystem.as_ref(),
            &ResourceScope::system(),
            &path,
            decode_v2_membership_body,
            entry_for_v2_membership,
            move |current: Option<V2MembershipRecord>| {
                let installation_id = installation_id.clone();
                let user_id = user_id.clone();
                async move {
                    if let Some(mut record) = current {
                        if record.installation_id != installation_id || record.user_id != user_id {
                            return Err(invalid_installation_error(
                                "v2 membership body identity did not match its key",
                            ));
                        }
                        let changed = !record.is_active();
                        record.updated_at = now;
                        record.removed_at = None;
                        return Ok(CasApply::new(record, changed));
                    }
                    Ok(CasApply::new(
                        V2MembershipRecord {
                            schema_version: EXTENSION_STATE_V2_SCHEMA.to_string(),
                            installation_id,
                            user_id,
                            installed_at: now,
                            updated_at: now,
                            removed_at: None,
                        },
                        true,
                    ))
                }
            },
        )
        .await
        .map_err(|error| map_extension_state_cas_error(error, "membership"))
    }

    pub(super) async fn deactivate_v2_membership_row(
        &self,
        installation_id: &ExtensionInstallationId,
        user_id: &UserId,
        now: DateTime<Utc>,
    ) -> Result<bool, ExtensionInstallationError> {
        let virtual_path = self.v2_membership_path(installation_id, user_id)?;
        let Some(current) = self
            .filesystem
            .get(&virtual_path)
            .await
            .map_err(store_unavailable("load v2 extension membership row"))?
        else {
            return Ok(false);
        };
        let current = parse_v2_membership_entry(current.entry, &virtual_path)?;
        if !current.is_active() {
            return Ok(false);
        }

        let path = self.v2_membership_scoped_path(installation_id, user_id)?;
        let installation_id = installation_id.clone();
        let user_id = user_id.clone();
        cas_update(
            self.scoped_filesystem.as_ref(),
            &ResourceScope::system(),
            &path,
            decode_v2_membership_body,
            entry_for_v2_membership,
            move |current: Option<V2MembershipRecord>| {
                let installation_id = installation_id.clone();
                let user_id = user_id.clone();
                async move {
                    let Some(mut record) = current else {
                        return Err(invalid_installation_error(
                            "v2 membership disappeared during deactivation",
                        ));
                    };
                    if record.installation_id != installation_id || record.user_id != user_id {
                        return Err(invalid_installation_error(
                            "v2 membership body identity did not match its key",
                        ));
                    }
                    if !record.is_active() {
                        return Ok(CasApply::new(record, false));
                    }
                    record.updated_at = now;
                    record.removed_at = Some(now);
                    Ok(CasApply::new(record, true))
                }
            },
        )
        .await
        .map_err(|error| map_extension_state_cas_error(error, "membership"))
    }

    pub(super) async fn activate_v2_credential_binding_row(
        &self,
        installation_id: &ExtensionInstallationId,
        binding: &ExtensionCredentialBinding,
        position: u32,
        now: DateTime<Utc>,
    ) -> Result<(), ExtensionInstallationError> {
        let path = self.v2_scoped_path(
            &self.v2_credential_binding_path(installation_id, binding.credential_handle())?,
        )?;
        let installation_id = installation_id.clone();
        let credential_handle = binding.credential_handle().clone();
        let secret_handle = binding.secret_handle().clone();
        cas_update(
            self.scoped_filesystem.as_ref(),
            &ResourceScope::system(),
            &path,
            decode_v2_credential_binding_body,
            entry_for_v2_credential_binding,
            move |current: Option<V2CredentialBindingRecord>| {
                let installation_id = installation_id.clone();
                let credential_handle = credential_handle.clone();
                let secret_handle = secret_handle.clone();
                async move {
                    if let Some(mut record) = current {
                        if record.installation_id != installation_id
                            || record.credential_handle != credential_handle
                        {
                            return Err(invalid_installation_error(
                                "v2 credential binding body identity did not match its key",
                            ));
                        }
                        record.secret_handle = secret_handle;
                        record.position = position;
                        record.updated_at = now;
                        record.removed_at = None;
                        return Ok(CasApply::new(record, ()));
                    }
                    Ok(CasApply::new(
                        V2CredentialBindingRecord {
                            schema_version: EXTENSION_STATE_V2_SCHEMA.to_string(),
                            installation_id,
                            credential_handle,
                            secret_handle,
                            position,
                            created_at: now,
                            updated_at: now,
                            removed_at: None,
                        },
                        (),
                    ))
                }
            },
        )
        .await
        .map_err(|error| map_extension_state_cas_error(error, "credential binding"))
    }

    pub(super) async fn deactivate_v2_credential_binding_row(
        &self,
        installation_id: &ExtensionInstallationId,
        credential_handle: &ExtensionCredentialHandle,
        now: DateTime<Utc>,
    ) -> Result<(), ExtensionInstallationError> {
        let path = self.v2_scoped_path(
            &self.v2_credential_binding_path(installation_id, credential_handle)?,
        )?;
        let installation_id = installation_id.clone();
        let credential_handle = credential_handle.clone();
        cas_update(
            self.scoped_filesystem.as_ref(),
            &ResourceScope::system(),
            &path,
            decode_v2_credential_binding_body,
            entry_for_v2_credential_binding,
            move |current: Option<V2CredentialBindingRecord>| {
                let installation_id = installation_id.clone();
                let credential_handle = credential_handle.clone();
                async move {
                    let Some(mut record) = current else {
                        return Err(invalid_installation_error(
                            "v2 credential binding disappeared during deactivation",
                        ));
                    };
                    if record.installation_id != installation_id
                        || record.credential_handle != credential_handle
                    {
                        return Err(invalid_installation_error(
                            "v2 credential binding body identity did not match its key",
                        ));
                    }
                    if record.is_active() {
                        record.updated_at = now;
                        record.removed_at = Some(now);
                    }
                    Ok(CasApply::new(record, ()))
                }
            },
        )
        .await
        .map_err(|error| map_extension_state_cas_error(error, "credential binding"))
    }

    pub(super) async fn query_v2_memberships(
        &self,
        installation_id: &ExtensionInstallationId,
    ) -> Result<Vec<V2MembershipRecord>, ExtensionInstallationError> {
        let rows = query_all(
            &self.filesystem,
            &self.v2_membership_root(installation_id)?,
            &Filter::All,
        )
        .await?;
        rows.into_iter()
            .map(|row| {
                let record = parse_v2_membership_entry(row.entry, &row.path)?;
                if &record.installation_id != installation_id {
                    return Err(invalid_installation_error(
                        "v2 membership row installation id did not match its parent",
                    ));
                }
                Ok(record)
            })
            .collect()
    }

    pub(super) async fn query_v2_credential_bindings(
        &self,
        installation_id: &ExtensionInstallationId,
    ) -> Result<Vec<V2CredentialBindingRecord>, ExtensionInstallationError> {
        let rows = query_all(
            &self.filesystem,
            &self.v2_credential_binding_root(installation_id)?,
            &Filter::All,
        )
        .await?;
        rows.into_iter()
            .map(|row| {
                let record = parse_v2_credential_binding_entry(row.entry, &row.path)?;
                if &record.installation_id != installation_id {
                    return Err(invalid_installation_error(
                        "v2 credential binding installation id did not match its parent",
                    ));
                }
                Ok(record)
            })
            .collect()
    }

    pub(super) async fn deactivate_reserved_v2_membership(
        &self,
        installation_id: &ExtensionInstallationId,
        user_id: &UserId,
        now: DateTime<Utc>,
    ) -> Result<MembershipDeactivation, ExtensionInstallationError> {
        let Some((_core, _)) = self.load_v2_installation_record(installation_id).await? else {
            return Err(ExtensionInstallationError::InstallationNotFound {
                installation_id: installation_id.clone(),
            });
        };
        let active_memberships = self
            .query_v2_memberships(installation_id)
            .await?
            .into_iter()
            .filter(|membership| membership.is_active())
            .collect::<Vec<_>>();
        let caller_is_active = active_memberships
            .iter()
            .any(|record| &record.user_id == user_id);
        let other_active_members = active_memberships
            .iter()
            .any(|record| &record.user_id != user_id);
        if !other_active_members {
            return Ok(MembershipDeactivation::FinalMemberReserved);
        }
        if caller_is_active {
            self.deactivate_v2_membership_row(installation_id, user_id, now)
                .await?;
        }
        let other_active_after_deactivation = self
            .query_v2_memberships(installation_id)
            .await?
            .into_iter()
            .any(|record| record.is_active() && &record.user_id != user_id);
        if !other_active_after_deactivation {
            self.activate_v2_membership_row(installation_id, user_id, now)
                .await?;
            return Ok(MembershipDeactivation::FinalMemberReserved);
        }
        let core = self
            .finish_v2_membership_removal(installation_id, user_id, now)
            .await?;
        let installation = self.reconstruct_v2_installation(&core).await?;
        self.put_installation(&installation, CasExpectation::Any)
            .await
            .map_err(SaveRowError::into_installation_error)?;
        Ok(MembershipDeactivation::MembershipRemoved(Box::new(
            installation,
        )))
    }

    pub(super) async fn reconstruct_v2_installation(
        &self,
        core: &V2InstallationRecord,
    ) -> Result<ExtensionInstallation, ExtensionInstallationError> {
        if !core.is_visible() {
            return Err(invalid_installation_error(
                "a removed or leased v2 installation cannot be reconstructed as live",
            ));
        }
        let all_memberships = self.query_v2_memberships(&core.installation_id).await?;
        let all_bindings = self
            .query_v2_credential_bindings(&core.installation_id)
            .await?;
        Self::assemble_v2_installation(core, all_memberships, all_bindings)
    }

    /// Group every membership row under the shared prefix by its installation.
    ///
    /// One query for the whole collection instead of one per installation:
    /// listing is a hot read and the per-installation shape made it O(N)
    /// round trips.
    pub(super) async fn query_v2_memberships_by_installation(
        &self,
    ) -> Result<HashMap<ExtensionInstallationId, Vec<V2MembershipRecord>>, ExtensionInstallationError>
    {
        let rows = query_all(&self.filesystem, &self.v2_memberships_root()?, &Filter::All).await?;
        let mut grouped: HashMap<ExtensionInstallationId, Vec<V2MembershipRecord>> = HashMap::new();
        for row in rows {
            let path = row.path.clone();
            let record = parse_v2_membership_entry(row.entry, &path)?;
            // Same integrity check the per-installation read makes: the body's
            // installation id must agree with the parent the row is filed under.
            // Match the full directory boundary, not a bare string prefix.
            // Today's row tokens are fixed-length hashes so a prefix collision
            // is unreachable, but that is a property of the path scheme rather
            // than of this check -- pin the boundary so the check stays exact
            // if the scheme ever changes.
            let expected_root = format!("{}/", self.v2_membership_root(&record.installation_id)?);
            if !path.as_str().starts_with(&expected_root) {
                return Err(invalid_installation_error(
                    "v2 membership row installation id did not match its parent",
                ));
            }
            grouped
                .entry(record.installation_id.clone())
                .or_default()
                .push(record);
        }
        Ok(grouped)
    }

    /// Credential-binding counterpart of
    /// [`Self::query_v2_memberships_by_installation`].
    pub(super) async fn query_v2_credential_bindings_by_installation(
        &self,
    ) -> Result<
        HashMap<ExtensionInstallationId, Vec<V2CredentialBindingRecord>>,
        ExtensionInstallationError,
    > {
        let rows = query_all(
            &self.filesystem,
            &self.v2_credential_bindings_root()?,
            &Filter::All,
        )
        .await?;
        let mut grouped: HashMap<ExtensionInstallationId, Vec<V2CredentialBindingRecord>> =
            HashMap::new();
        for row in rows {
            let path = row.path.clone();
            let record = parse_v2_credential_binding_entry(row.entry, &path)?;
            let expected_root = format!(
                "{}/",
                self.v2_credential_binding_root(&record.installation_id)?
            );
            if !path.as_str().starts_with(&expected_root) {
                return Err(invalid_installation_error(
                    "v2 credential binding installation id did not match its parent",
                ));
            }
            grouped
                .entry(record.installation_id.clone())
                .or_default()
                .push(record);
        }
        Ok(grouped)
    }

    /// Rebuild one aggregate from a core row and its already-loaded children.
    ///
    /// Pure assembly: the caller decides whether the children came from a
    /// per-installation read or a batched one, so a list can load every child
    /// collection once.
    pub(super) fn assemble_v2_installation(
        core: &V2InstallationRecord,
        all_memberships: Vec<V2MembershipRecord>,
        all_bindings: Vec<V2CredentialBindingRecord>,
    ) -> Result<ExtensionInstallation, ExtensionInstallationError> {
        let membership_updated_at = all_memberships.iter().map(|record| record.updated_at).max();
        let memberships = all_memberships
            .into_iter()
            .filter(|membership| membership.is_active())
            .collect::<Vec<_>>();
        let owner = if core.legacy_tenant_owner {
            InstallationOwner::Tenant
        } else {
            InstallationOwner::users(
                memberships
                    .iter()
                    .map(|record| record.user_id.clone())
                    .collect(),
            )?
        };
        let binding_updated_at = all_bindings.iter().map(|record| record.updated_at).max();
        let mut bindings = all_bindings
            .into_iter()
            .filter(|binding| binding.is_active())
            .collect::<Vec<_>>();
        bindings.sort_by(|left, right| {
            left.position
                .cmp(&right.position)
                .then_with(|| left.credential_handle.cmp(&right.credential_handle))
        });
        let credential_bindings = bindings
            .iter()
            .map(|record| {
                ExtensionCredentialBinding::new(
                    record.credential_handle.clone(),
                    record.secret_handle.clone(),
                )
            })
            .collect::<Vec<_>>();
        let updated_at = membership_updated_at
            .into_iter()
            .chain(binding_updated_at)
            .fold(core.updated_at, std::cmp::max);
        ExtensionInstallation::from_persisted_parts(ExtensionInstallationPersistedParts {
            installation_id: core.installation_id.clone(),
            extension_id: core.extension_id.clone(),
            manifest_ref: core.manifest_ref(),
            incarnation_id: core.incarnation_id.clone(),
            credential_bindings,
            updated_at,
            owner,
        })
    }
}
