use super::*;

impl ExtensionInstallationStore {
    /// All merged records for one extension id. Installation ids equal
    /// package ids today so this is normally zero or one record, but legacy
    /// data may hold duplicates, so extension-level decisions must consider
    /// every record.
    pub(super) async fn load_v2_records_by_extension(
        &self,
        extension_id: &ExtensionId,
    ) -> Result<Vec<V2InstallationRecord>, ExtensionInstallationError> {
        let rows = query_all(
            &self.filesystem,
            &self.v2_installations_root()?,
            &Filter::Eq {
                key: index_key("extension_id")?,
                value: IndexValue::Text(extension_id.as_str().to_string()),
            },
        )
        .await?;
        let mut records = rows
            .into_iter()
            .map(|row| parse_v2_installation_entry(row.entry, &row.path))
            .collect::<Result<Vec<_>, _>>()?;
        records.retain(|record| &record.extension_id == extension_id);
        // Live records first so extension-level reads never answer from a
        // tombstone while a sibling record is still live.
        records.sort_by(|a, b| {
            a.is_removed()
                .cmp(&b.is_removed())
                .then_with(|| a.installation_id.cmp(&b.installation_id))
        });
        Ok(records)
    }

    pub(super) async fn load_v2_installation_record(
        &self,
        installation_id: &ExtensionInstallationId,
    ) -> Result<Option<(V2InstallationRecord, RecordVersion)>, ExtensionInstallationError> {
        let path = self.v2_installation_path(installation_id)?;
        let Some(entry) = self
            .filesystem
            .get(&path)
            .await
            .map_err(store_unavailable("load v2 extension installation row"))?
        else {
            return Ok(None);
        };
        let record = parse_v2_installation_entry(entry.entry, &path)?;
        if &record.installation_id != installation_id {
            return Err(invalid_installation_error(format!(
                "v2 extension installation row key {installation_id} contained installation {}",
                record.installation_id
            )));
        }
        Ok(Some((record, entry.version)))
    }

    /// Write the full aggregate. `manifest: Some` re-pins the embedded
    /// definition (install/update); `None` carries the existing row's
    /// definition forward (reactivation, restore, plain aggregate rewrites).
    pub(super) async fn put_v2_installation(
        &self,
        installation: &ExtensionInstallation,
        manifest: Option<&ExtensionManifestRecord>,
    ) -> Result<(), ExtensionInstallationError> {
        let prior = match self
            .load_v2_installation_record(installation.installation_id())
            .await?
        {
            Some((core, _)) if core.is_visible() => {
                let prior = self.reconstruct_v2_installation(&core).await?;
                self.take_v2_update_lease(installation.installation_id())
                    .await?;
                Some(prior)
            }
            _ => None,
        };
        // Every aggregate write sweeps child rows omitted from its member
        // and binding sets. Under the single-writer-per-root deployment
        // contract an omitted-but-active row can only be debris from an
        // earlier failed write (an interrupted install's membership row, a
        // crashed update's leftovers) — merging it in would grant membership
        // to a user whose install never succeeded.
        if let Err(error) = self
            .write_v2_installation_components(installation, manifest)
            .await
        {
            if let Some(prior) = prior
                && self
                    .write_v2_installation_components(&prior, None)
                    .await
                    .is_err()
            {
                return Err(store_unavailable_error(
                    "aggregate update failed and its prior installation could not be restored",
                ));
            }
            return Err(error);
        }
        Ok(())
    }

    pub(super) async fn write_v2_installation_components(
        &self,
        installation: &ExtensionInstallation,
        manifest: Option<&ExtensionManifestRecord>,
    ) -> Result<(), ExtensionInstallationError> {
        let desired_members = installation.owner().members().cloned().unwrap_or_default();
        for user_id in &desired_members {
            self.activate_v2_membership_row(
                installation.installation_id(),
                user_id,
                installation.updated_at(),
            )
            .await?;
        }
        let existing_memberships = self
            .query_v2_memberships(installation.installation_id())
            .await?;
        for membership in existing_memberships {
            if desired_members.contains(&membership.user_id) {
                continue;
            }
            if membership.is_active() {
                self.deactivate_v2_membership_row(
                    installation.installation_id(),
                    &membership.user_id,
                    installation.updated_at(),
                )
                .await?;
            }
        }

        let desired_binding_handles = installation
            .credential_bindings()
            .iter()
            .map(|binding| binding.credential_handle().clone())
            .collect::<BTreeSet<_>>();
        for (position, binding) in installation.credential_bindings().iter().enumerate() {
            let position = u32::try_from(position).map_err(|_| {
                invalid_installation_error("extension credential binding position overflow")
            })?;
            self.activate_v2_credential_binding_row(
                installation.installation_id(),
                binding,
                position,
                installation.updated_at(),
            )
            .await?;
        }
        let existing_bindings = self
            .query_v2_credential_bindings(installation.installation_id())
            .await?;
        for binding in existing_bindings {
            if desired_binding_handles.contains(&binding.credential_handle) {
                continue;
            }
            if binding.is_active() {
                self.deactivate_v2_credential_binding_row(
                    installation.installation_id(),
                    &binding.credential_handle,
                    installation.updated_at(),
                )
                .await?;
            }
        }

        self.put_v2_installation_core(installation, manifest)
            .await
            .map(|_| ())
    }

    pub(super) async fn take_v2_update_lease(
        &self,
        installation_id: &ExtensionInstallationId,
    ) -> Result<(), ExtensionInstallationError> {
        let path = self.v2_scoped_path(&self.v2_installation_path(installation_id)?)?;
        let installation_id = installation_id.clone();
        cas_update(
            self.scoped_filesystem.as_ref(),
            &ResourceScope::system(),
            &path,
            decode_v2_installation_body,
            entry_for_v2_installation,
            move |current: Option<V2InstallationRecord>| {
                let installation_id = installation_id.clone();
                async move {
                    let Some(mut record) = current else {
                        return Err(ExtensionInstallationError::InstallationNotFound {
                            installation_id,
                        });
                    };
                    if record.installation_id != installation_id {
                        return Err(invalid_installation_error(
                            "v2 installation body identity did not match its key",
                        ));
                    }
                    if !record.is_visible() {
                        return Err(ExtensionInstallationError::MembershipMutationInProgress {
                            installation_id,
                        });
                    }
                    record.lease = Some(V2MutationLease::update());
                    record.updated_at = Utc::now();
                    Ok(CasApply::new(record, ()))
                }
            },
        )
        .await
        .map_err(|error| map_extension_state_cas_error(error, "installation"))
    }

    pub(super) async fn take_v2_preparation_lease(
        &self,
        installation_id: &ExtensionInstallationId,
        incarnation_id: &InstallationIncarnationId,
        expected_manifest_ref: &ExtensionManifestRef,
    ) -> Result<(), ExtensionInstallationError> {
        let path = self.v2_scoped_path(&self.v2_installation_path(installation_id)?)?;
        let installation_id = installation_id.clone();
        let incarnation_id = incarnation_id.clone();
        let expected_manifest_ref = expected_manifest_ref.clone();
        cas_update(
            self.scoped_filesystem.as_ref(),
            &ResourceScope::system(),
            &path,
            decode_v2_installation_body,
            entry_for_v2_installation,
            move |current: Option<V2InstallationRecord>| {
                let installation_id = installation_id.clone();
                let incarnation_id = incarnation_id.clone();
                let expected_manifest_ref = expected_manifest_ref.clone();
                async move {
                    let Some(mut record) = current else {
                        return Err(ExtensionInstallationError::InstallationNotFound {
                            installation_id,
                        });
                    };
                    if record.installation_id != installation_id {
                        return Err(invalid_installation_error(
                            "v2 installation body identity did not match its key",
                        ));
                    }
                    if !record.is_visible() {
                        return Err(ExtensionInstallationError::MembershipMutationInProgress {
                            installation_id,
                        });
                    }
                    if record
                        .manifest
                        .resolved
                        .as_ref()
                        .is_none_or(|resolved| resolved.has_model_visible_capabilities())
                        || record.incarnation_id.as_ref() != Some(&incarnation_id)
                        || record.manifest_ref() != expected_manifest_ref
                    {
                        return Err(
                            ExtensionInstallationError::PreparationFinalizationRejected {
                                installation_id,
                            },
                        );
                    }
                    record.lease = Some(V2MutationLease::preparation(
                        incarnation_id,
                        expected_manifest_ref,
                    ));
                    record.updated_at = Utc::now();
                    Ok(CasApply::new(record, ()))
                }
            },
        )
        .await
        .map_err(|error| map_extension_state_cas_error(error, "installation"))
    }

    pub(super) async fn finish_v2_preparation(
        &self,
        installation_id: &ExtensionInstallationId,
        incarnation_id: &InstallationIncarnationId,
        expected_manifest_ref: &ExtensionManifestRef,
        finalized_manifest: &ExtensionManifestRecord,
    ) -> Result<V2InstallationRecord, ExtensionInstallationError> {
        let path = self.v2_scoped_path(&self.v2_installation_path(installation_id)?)?;
        let installation_id = installation_id.clone();
        let incarnation_id = incarnation_id.clone();
        let expected_manifest_ref = expected_manifest_ref.clone();
        let finalized_wire = WireManifestRecord::from(finalized_manifest);
        cas_update(
            self.scoped_filesystem.as_ref(),
            &ResourceScope::system(),
            &path,
            decode_v2_installation_body,
            entry_for_v2_installation,
            move |current: Option<V2InstallationRecord>| {
                let installation_id = installation_id.clone();
                let incarnation_id = incarnation_id.clone();
                let expected_manifest_ref = expected_manifest_ref.clone();
                let finalized_wire = finalized_wire.clone();
                async move {
                    let Some(mut record) = current else {
                        return Err(ExtensionInstallationError::InstallationNotFound {
                            installation_id,
                        });
                    };
                    let lease_matches = record.lease.as_ref().and_then(|lease| {
                        lease.preparation.as_ref().map(|preparation| {
                            preparation.incarnation_id == incarnation_id
                                && preparation.expected_manifest_ref == expected_manifest_ref
                        })
                    }) == Some(true);
                    let finalized_extension_id = finalized_wire
                        .resolved
                        .as_ref()
                        .ok_or_else(|| {
                            invalid_installation_error("finalized manifest was not resolved")
                        })?
                        .id
                        .clone();
                    let finalized_is_ready = finalized_wire
                        .resolved
                        .as_ref()
                        .is_some_and(|resolved| resolved.has_model_visible_capabilities());
                    if record.installation_id != installation_id
                        || record.extension_id != finalized_extension_id
                        || !lease_matches
                        || record
                            .manifest
                            .resolved
                            .as_ref()
                            .is_none_or(|resolved| resolved.has_model_visible_capabilities())
                        || record.incarnation_id.as_ref() != Some(&incarnation_id)
                        || record.manifest_ref() != expected_manifest_ref
                        || !finalized_is_ready
                    {
                        return Err(
                            ExtensionInstallationError::PreparationFinalizationRejected {
                                installation_id,
                            },
                        );
                    }
                    record.manifest = finalized_wire;
                    record.lease = None;
                    record.updated_at = Utc::now();
                    Ok(CasApply::new(record.clone(), record))
                }
            },
        )
        .await
        .map_err(|error| map_extension_state_cas_error(error, "installation"))
    }

    pub(super) async fn finish_v2_preparation_checkpoint(
        &self,
        installation_id: &ExtensionInstallationId,
        incarnation_id: &InstallationIncarnationId,
        expected_manifest_ref: &ExtensionManifestRef,
        next_pending_manifest: &ExtensionManifestRecord,
    ) -> Result<V2InstallationRecord, ExtensionInstallationError> {
        let path = self.v2_scoped_path(&self.v2_installation_path(installation_id)?)?;
        let installation_id = installation_id.clone();
        let incarnation_id = incarnation_id.clone();
        let expected_manifest_ref = expected_manifest_ref.clone();
        let next_wire = WireManifestRecord::from(next_pending_manifest);
        cas_update(
            self.scoped_filesystem.as_ref(),
            &ResourceScope::system(),
            &path,
            decode_v2_installation_body,
            entry_for_v2_installation,
            move |current: Option<V2InstallationRecord>| {
                let installation_id = installation_id.clone();
                let incarnation_id = incarnation_id.clone();
                let expected_manifest_ref = expected_manifest_ref.clone();
                let next_wire = next_wire.clone();
                async move {
                    let Some(mut record) = current else {
                        return Err(ExtensionInstallationError::InstallationNotFound {
                            installation_id,
                        });
                    };
                    let lease_matches = record
                        .lease
                        .as_ref()
                        .and_then(|lease| lease.preparation.as_ref())
                        .is_some_and(|preparation| {
                            preparation.incarnation_id == incarnation_id
                                && preparation.expected_manifest_ref == expected_manifest_ref
                        });
                    let current_resolved = record.manifest.resolved.as_ref().ok_or_else(|| {
                        invalid_installation_error("pending manifest was not resolved")
                    })?;
                    let next_resolved = next_wire.resolved.as_ref().ok_or_else(|| {
                        invalid_installation_error("checkpoint manifest was not resolved")
                    })?;
                    if record.installation_id != installation_id
                        || record.extension_id != next_resolved.id
                        || record.manifest.source != next_wire.source
                        || current_resolved.root_binding != next_resolved.root_binding
                        || !lease_matches
                        || current_resolved.has_model_visible_capabilities()
                        || next_resolved.has_model_visible_capabilities()
                        || record.incarnation_id.as_ref() != Some(&incarnation_id)
                        || record.manifest_ref() != expected_manifest_ref
                    {
                        return Err(
                            ExtensionInstallationError::PreparationFinalizationRejected {
                                installation_id,
                            },
                        );
                    }
                    record.manifest = next_wire;
                    record.lease = None;
                    record.updated_at = Utc::now();
                    Ok(CasApply::new(record.clone(), record))
                }
            },
        )
        .await
        .map_err(|error| map_extension_state_cas_error(error, "installation"))
    }

    pub(super) async fn put_v2_installation_core(
        &self,
        installation: &ExtensionInstallation,
        manifest: Option<&ExtensionManifestRecord>,
    ) -> Result<V2InstallationRecord, ExtensionInstallationError> {
        let path =
            self.v2_scoped_path(&self.v2_installation_path(installation.installation_id())?)?;
        let installation = installation.clone();
        let provided_wire = manifest.map(WireManifestRecord::from);
        cas_update(
            self.scoped_filesystem.as_ref(),
            &ResourceScope::system(),
            &path,
            decode_v2_installation_body,
            entry_for_v2_installation,
            move |current: Option<V2InstallationRecord>| {
                let installation = installation.clone();
                let provided_wire = provided_wire.clone();
                async move {
                    if current.as_ref().is_some_and(|current| {
                        current.installation_id != *installation.installation_id()
                    }) {
                        return Err(invalid_installation_error(
                            "v2 installation body identity did not match its key",
                        ));
                    }
                    // The commit either re-pins a provided definition or
                    // carries the existing row's forward; a fresh id with no
                    // definition to embed cannot become installed.
                    let Some(wire) = provided_wire
                        .or_else(|| current.as_ref().map(|record| record.manifest.clone()))
                    else {
                        return Err(ExtensionInstallationError::UnknownManifest {
                            extension_id: installation.extension_id().clone(),
                        });
                    };
                    let record = V2InstallationRecord {
                        schema_version: EXTENSION_STATE_V2_SCHEMA.to_string(),
                        installation_id: installation.installation_id().clone(),
                        extension_id: installation.extension_id().clone(),
                        activation_state: current
                            .as_ref()
                            .and_then(|record| record.activation_state.clone()),
                        manifest: wire,
                        incarnation_id: installation.incarnation_id().cloned(),
                        legacy_tenant_owner: installation.owner().is_tenant(),
                        updated_at: installation.updated_at(),
                        removed_at: None,
                        lease: None,
                        removal_cleanup_pending: false,
                    };
                    Ok(CasApply::new(record.clone(), record))
                }
            },
        )
        .await
        .map_err(|error| map_extension_state_cas_error(error, "installation"))
    }

    pub(super) async fn canonicalize_v2_installation_owner(
        &self,
        installation_id: &ExtensionInstallationId,
        now: DateTime<Utc>,
    ) -> Result<V2InstallationRecord, ExtensionInstallationError> {
        let path = self.v2_scoped_path(&self.v2_installation_path(installation_id)?)?;
        let installation_id = installation_id.clone();
        cas_update(
            self.scoped_filesystem.as_ref(),
            &ResourceScope::system(),
            &path,
            decode_v2_installation_body,
            entry_for_v2_installation,
            move |current: Option<V2InstallationRecord>| {
                let installation_id = installation_id.clone();
                async move {
                    let Some(mut record) = current else {
                        return Err(ExtensionInstallationError::InstallationNotFound {
                            installation_id,
                        });
                    };
                    if record.installation_id != installation_id {
                        return Err(invalid_installation_error(
                            "v2 installation body identity did not match its key",
                        ));
                    }
                    if record.is_removed() {
                        return Err(ExtensionInstallationError::InstallationNotFound {
                            installation_id,
                        });
                    }
                    if record.lease.is_some() {
                        return Err(ExtensionInstallationError::MembershipMutationInProgress {
                            installation_id,
                        });
                    }
                    let mut changed = false;
                    if record.legacy_tenant_owner {
                        record.legacy_tenant_owner = false;
                        changed = true;
                    }
                    // Incarnations were added after v2 installation rows
                    // shipped. A lifecycle write is the safe compatibility
                    // point to give an older live row its stable identity;
                    // without one, hosted-MCP preparation cannot checkpoint
                    // or finalize the discovery that follows installation.
                    if record.incarnation_id.is_none() {
                        record.incarnation_id = Some(InstallationIncarnationId::fresh());
                        changed = true;
                    }
                    if changed {
                        record.updated_at = now;
                    }
                    Ok(CasApply::new(record.clone(), record))
                }
            },
        )
        .await
        .map_err(|error| map_extension_state_cas_error(error, "installation"))
    }

    pub(super) async fn reserve_v2_membership_removal(
        &self,
        installation_id: &ExtensionInstallationId,
        user_id: &UserId,
        now: DateTime<Utc>,
    ) -> Result<V2InstallationRecord, ExtensionInstallationError> {
        let path = self.v2_scoped_path(&self.v2_installation_path(installation_id)?)?;
        let installation_id = installation_id.clone();
        let user_id = user_id.clone();
        cas_update(
            self.scoped_filesystem.as_ref(),
            &ResourceScope::system(),
            &path,
            decode_v2_installation_body,
            entry_for_v2_installation,
            move |current: Option<V2InstallationRecord>| {
                let installation_id = installation_id.clone();
                let user_id = user_id.clone();
                async move {
                    let Some(mut record) = current else {
                        return Err(ExtensionInstallationError::InstallationNotFound {
                            installation_id,
                        });
                    };
                    if record.installation_id != installation_id {
                        return Err(invalid_installation_error(
                            "v2 installation body identity did not match its key",
                        ));
                    }
                    if record.is_removed() {
                        return Err(ExtensionInstallationError::InstallationNotFound {
                            installation_id,
                        });
                    }
                    if let Some(lease) = record.lease.as_ref() {
                        if lease.member.as_ref() == Some(&user_id) {
                            return Ok(CasApply::new(record.clone(), record));
                        }
                        return Err(ExtensionInstallationError::MembershipMutationInProgress {
                            installation_id,
                        });
                    }
                    record.lease = Some(V2MutationLease::member_removal(user_id));
                    record.updated_at = now;
                    Ok(CasApply::new(record.clone(), record))
                }
            },
        )
        .await
        .map_err(|error| map_extension_state_cas_error(error, "installation"))
    }

    pub(super) async fn begin_v2_installation_removal(
        &self,
        installation_id: &ExtensionInstallationId,
        now: DateTime<Utc>,
    ) -> Result<(), ExtensionInstallationError> {
        let path = self.v2_scoped_path(&self.v2_installation_path(installation_id)?)?;
        let installation_id = installation_id.clone();
        cas_update(
            self.scoped_filesystem.as_ref(),
            &ResourceScope::system(),
            &path,
            decode_v2_installation_body,
            entry_for_v2_installation,
            move |current: Option<V2InstallationRecord>| {
                let installation_id = installation_id.clone();
                async move {
                    let Some(mut record) = current else {
                        return Err(ExtensionInstallationError::InstallationNotFound {
                            installation_id,
                        });
                    };
                    if record.installation_id != installation_id {
                        return Err(invalid_installation_error(
                            "v2 installation body identity did not match its key",
                        ));
                    }
                    // A visible record takes the exclusive lease; an existing
                    // lease (this removal's own retry, or the member-removal
                    // lease this final removal continues) and an existing
                    // tombstone both pass through so removal stays idempotent.
                    if record.is_visible() {
                        record.lease = Some(V2MutationLease::update());
                        record.updated_at = now;
                    }
                    Ok(CasApply::new(record, ()))
                }
            },
        )
        .await
        .map_err(|error| map_extension_state_cas_error(error, "installation"))
    }

    pub(super) async fn finish_v2_membership_removal(
        &self,
        installation_id: &ExtensionInstallationId,
        user_id: &UserId,
        now: DateTime<Utc>,
    ) -> Result<V2InstallationRecord, ExtensionInstallationError> {
        let path = self.v2_scoped_path(&self.v2_installation_path(installation_id)?)?;
        let installation_id = installation_id.clone();
        let user_id = user_id.clone();
        cas_update(
            self.scoped_filesystem.as_ref(),
            &ResourceScope::system(),
            &path,
            decode_v2_installation_body,
            entry_for_v2_installation,
            move |current: Option<V2InstallationRecord>| {
                let installation_id = installation_id.clone();
                let user_id = user_id.clone();
                async move {
                    let Some(mut record) = current else {
                        return Err(ExtensionInstallationError::InstallationNotFound {
                            installation_id,
                        });
                    };
                    if record.installation_id != installation_id {
                        return Err(invalid_installation_error(
                            "v2 installation body identity did not match its key",
                        ));
                    }
                    if record.is_removed()
                        || record
                            .lease
                            .as_ref()
                            .is_none_or(|lease| lease.member.as_ref() != Some(&user_id))
                    {
                        return Err(ExtensionInstallationError::MembershipMutationInProgress {
                            installation_id,
                        });
                    }
                    record.lease = None;
                    record.updated_at = now;
                    Ok(CasApply::new(record.clone(), record))
                }
            },
        )
        .await
        .map_err(|error| map_extension_state_cas_error(error, "installation"))
    }

    pub(super) async fn finish_v2_installation_removal(
        &self,
        installation_id: &ExtensionInstallationId,
        now: DateTime<Utc>,
    ) -> Result<(), ExtensionInstallationError> {
        let path = self.v2_scoped_path(&self.v2_installation_path(installation_id)?)?;
        let installation_id = installation_id.clone();
        cas_update(
            self.scoped_filesystem.as_ref(),
            &ResourceScope::system(),
            &path,
            decode_v2_installation_body,
            entry_for_v2_installation,
            move |current: Option<V2InstallationRecord>| {
                let installation_id = installation_id.clone();
                async move {
                    let Some(mut record) = current else {
                        return Err(ExtensionInstallationError::InstallationNotFound {
                            installation_id,
                        });
                    };
                    if record.installation_id != installation_id {
                        return Err(invalid_installation_error(
                            "v2 installation body identity did not match its key",
                        ));
                    }
                    if !record.is_removed() {
                        record.removed_at = Some(now);
                        record.updated_at = now;
                        // The embedded definition stays authoritative for
                        // cleanup retries until `delete_manifest` marks the
                        // removal converged.
                        record.removal_cleanup_pending = true;
                    }
                    record.lease = None;
                    Ok(CasApply::new(record, ()))
                }
            },
        )
        .await
        .map_err(|error| map_extension_state_cas_error(error, "installation"))
    }

    /// Clear the removal-cleanup-pending marker on a tombstoned record:
    /// `delete_manifest` calls this when removal cleanup has converged, after
    /// which the embedded definition stops being authoritative for manifest
    /// readers and fresh imports are unblocked.
    pub(super) async fn mark_v2_removal_converged(
        &self,
        installation_id: &ExtensionInstallationId,
    ) -> Result<(), ExtensionInstallationError> {
        let path = self.v2_scoped_path(&self.v2_installation_path(installation_id)?)?;
        let installation_id = installation_id.clone();
        cas_update(
            self.scoped_filesystem.as_ref(),
            &ResourceScope::system(),
            &path,
            decode_v2_installation_body,
            entry_for_v2_installation,
            move |current: Option<V2InstallationRecord>| {
                let installation_id = installation_id.clone();
                async move {
                    let Some(mut record) = current else {
                        return Err(ExtensionInstallationError::InstallationNotFound {
                            installation_id,
                        });
                    };
                    if record.installation_id != installation_id {
                        return Err(invalid_installation_error(
                            "v2 installation body identity did not match its key",
                        ));
                    }
                    if record.is_removed() && record.removal_cleanup_pending {
                        record.removal_cleanup_pending = false;
                        record.updated_at = Utc::now();
                    }
                    Ok(CasApply::new(record, ()))
                }
            },
        )
        .await
        .map_err(|error| map_extension_state_cas_error(error, "installation"))
    }
}
