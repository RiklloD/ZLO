// untriaged discards: predate the workspace-wide deny (Cargo.toml [workspace.lints.clippy]).
#![allow(clippy::let_underscore_must_use)]
use super::*;

#[async_trait]
impl ExtensionInstallationStorePort for ExtensionInstallationStore {
    async fn admit_package_definition(
        &self,
        record: ExtensionManifestRecord,
    ) -> Result<PackageDefinitionAdmissionOutcome, ExtensionInstallationError> {
        let path = self.registered_definition_path(record.extension_id())?;
        match self
            .filesystem
            .put(
                &path,
                entry_for_registered_definition(&record)?,
                CasExpectation::Absent,
            )
            .await
        {
            Ok(_) => Ok(PackageDefinitionAdmissionOutcome::Created),
            Err(FilesystemError::VersionMismatch { .. }) => {
                let existing = self
                    .get_registered_package_definition(record.extension_id())
                    .await?
                    .ok_or_else(|| {
                        store_unavailable_error(
                            "registered package definition disappeared after CAS conflict",
                        )
                    })?;
                if existing == record {
                    Ok(PackageDefinitionAdmissionOutcome::ExactExisting)
                } else {
                    Err(ExtensionInstallationError::PackageDefinitionConflict {
                        extension_id: record.extension_id().clone(),
                    })
                }
            }
            Err(error) => Err(store_unavailable("admit package definition")(error)),
        }
    }

    async fn get_registered_package_definition(
        &self,
        extension_id: &ExtensionId,
    ) -> Result<Option<ExtensionManifestRecord>, ExtensionInstallationError> {
        let path = self.registered_definition_path(extension_id)?;
        let Some(row) = self
            .filesystem
            .get(&path)
            .await
            .map_err(store_unavailable("load registered package definition"))?
        else {
            return Ok(None);
        };
        ensure_entry_kind(&row.entry, REGISTERED_DEFINITION_RECORD_KIND, &path)?;
        let record = row
            .entry
            .parse_json::<WireManifestRecord>()
            .map_err(|error| {
                corrupt_row("deserialize registered package definition", &path, error)
            })?
            .into_manifest_record()?;
        if record.extension_id() != extension_id {
            return Err(corrupt_row(
                "validate registered package definition identity",
                &path,
                "row extension id does not match its path",
            ));
        }
        Ok(Some(record))
    }

    async fn list_registered_package_definitions(
        &self,
    ) -> Result<Vec<ExtensionManifestRecord>, ExtensionInstallationError> {
        let rows = query_all(
            &self.filesystem,
            &self.registered_definitions_root()?,
            &Filter::All,
        )
        .await?;
        let mut records = rows
            .into_iter()
            .map(|row| {
                ensure_entry_kind(&row.entry, REGISTERED_DEFINITION_RECORD_KIND, &row.path)?;
                let record = row
                    .entry
                    .parse_json::<WireManifestRecord>()
                    .map_err(|error| {
                        corrupt_row(
                            "deserialize registered package definition",
                            &row.path,
                            error,
                        )
                    })?
                    .into_manifest_record()?;
                Ok(record)
            })
            .collect::<Result<Vec<_>, ExtensionInstallationError>>()?;
        records.sort_by(|a, b| a.extension_id().cmp(b.extension_id()));
        Ok(records)
    }

    async fn list_manifests(
        &self,
    ) -> Result<Vec<ExtensionManifestRecord>, ExtensionInstallationError> {
        let rows = query_all(
            &self.filesystem,
            &self.v2_installations_root()?,
            &Filter::All,
        )
        .await?;
        let mut manifests = rows
            .into_iter()
            .map(|row| parse_v2_installation_entry(row.entry, &row.path))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            // The definition stays readable while a lease is held — an
            // in-flight membership mutation does not change what is pinned —
            // and while a removal-cleanup tombstone awaits convergence.
            .filter(V2InstallationRecord::manifest_is_authoritative)
            .map(|record| record.manifest.into_manifest_record())
            .collect::<Result<Vec<_>, _>>()?;
        manifests.sort_by(|a, b| a.extension_id().cmp(b.extension_id()));
        Ok(manifests)
    }

    async fn get_manifest(
        &self,
        extension_id: &ExtensionId,
    ) -> Result<Option<ExtensionManifestRecord>, ExtensionInstallationError> {
        let Some(record) = self
            .load_v2_records_by_extension(extension_id)
            .await?
            .into_iter()
            .find(V2InstallationRecord::manifest_is_authoritative)
        else {
            return Ok(None);
        };
        record.manifest.into_manifest_record().map(Some)
    }

    async fn persist_removal_tombstone(
        &self,
        manifest: ExtensionManifestRecord,
    ) -> Result<(), ExtensionInstallationError> {
        let installation_id =
            ExtensionInstallationId::new(manifest.extension_id().as_str().to_string())
                .map_err(invalid_installation_error)?;
        let path = self.v2_scoped_path(&self.v2_installation_path(&installation_id)?)?;
        let wire = WireManifestRecord::from(&manifest);
        let extension_id = manifest.extension_id().clone();
        cas_update(
            self.scoped_filesystem.as_ref(),
            &ResourceScope::system(),
            &path,
            decode_v2_installation_body,
            entry_for_v2_installation,
            move |current: Option<V2InstallationRecord>| {
                let installation_id = installation_id.clone();
                let extension_id = extension_id.clone();
                let wire = wire.clone();
                async move {
                    if current.as_ref().is_some_and(|record| {
                        record.installation_id != installation_id
                            || record.extension_id != extension_id
                    }) {
                        return Err(invalid_installation_error(
                            "v2 installation body identity did not match its key",
                        ));
                    }
                    if let Some(record) = current.as_ref()
                        && !record.is_removed()
                    {
                        // A leased record is a peer's in-flight mutation, not
                        // an orphan: surface the retryable class so the
                        // caller waits it out instead of treating a live
                        // extension as cleanup debris.
                        if record.lease.is_some() {
                            return Err(ExtensionInstallationError::MembershipMutationInProgress {
                                installation_id: record.installation_id.clone(),
                            });
                        }
                        return Err(ExtensionInstallationError::InvalidInstallation {
                            reason: format!("extension {extension_id} still has installations"),
                        });
                    }
                    let now = Utc::now();
                    let record = V2InstallationRecord {
                        schema_version: EXTENSION_STATE_V2_SCHEMA.to_string(),
                        installation_id,
                        extension_id,
                        activation_state: current
                            .as_ref()
                            .and_then(|record| record.activation_state.clone()),
                        manifest: wire,
                        incarnation_id: current
                            .as_ref()
                            .and_then(|record| record.incarnation_id.clone()),
                        legacy_tenant_owner: current
                            .as_ref()
                            .map(|record| record.legacy_tenant_owner)
                            .unwrap_or(false),
                        updated_at: now,
                        removed_at: Some(
                            current
                                .as_ref()
                                .and_then(|record| record.removed_at)
                                .unwrap_or(now),
                        ),
                        lease: None,
                        removal_cleanup_pending: true,
                    };
                    Ok(CasApply::new(record, ()))
                }
            },
        )
        .await
        .map_err(|error| map_extension_state_cas_error(error, "installation"))?;
        // silent-ok: the v2 tombstone (the authority) is committed; the legacy
        // manifest projection mirrors it best-effort and startup repair
        // converges it.
        let _ = self.put_manifest(&manifest, CasExpectation::Any).await;
        Ok(())
    }

    async fn upsert_manifest_and_installation(
        &self,
        manifest: ExtensionManifestRecord,
        installation: ExtensionInstallation,
    ) -> Result<(), ExtensionInstallationError> {
        validate_installation_against_one_manifest(&manifest, &installation)?;
        self.put_v2_installation(&installation, Some(&manifest))
            .await?;
        // silent-ok: the merged v2 record (the authority) is committed, so
        // the install has succeeded; failing on a compatibility-projection
        // write would make callers compensate against a live install and
        // leave a ghost. Startup repairs the projections from v2.
        let _ = self.put_manifest(&manifest, CasExpectation::Any).await;
        let _ = self
            .put_installation(&installation, CasExpectation::Any)
            .await;
        Ok(())
    }

    async fn list_installations(
        &self,
    ) -> Result<Vec<ExtensionInstallation>, ExtensionInstallationError> {
        let mut installations = self.query_v2_installations(&Filter::All).await?;
        installations.sort_by(|a, b| a.installation_id().cmp(b.installation_id()));
        Ok(installations)
    }

    async fn get_installation(
        &self,
        installation_id: &ExtensionInstallationId,
    ) -> Result<Option<ExtensionInstallation>, ExtensionInstallationError> {
        let Some((core, _)) = self.load_v2_installation_record(installation_id).await? else {
            return Ok(None);
        };
        if !core.is_visible() {
            return Ok(None);
        }
        self.reconstruct_v2_installation(&core).await.map(Some)
    }

    async fn upsert_installation(
        &self,
        installation: ExtensionInstallation,
    ) -> Result<(), ExtensionInstallationError> {
        // Validate against the record's own pinned definition — a removed
        // record retains it, which is what lets a reactivating upsert
        // revalidate without re-supplying the manifest. A row that does not
        // exist yet (a legacy-shaped additional installation of an already
        // pinned extension) falls back to the extension's authoritative
        // definition, which that new row then embeds.
        let (manifest, provide_manifest) = match self
            .load_v2_installation_record(installation.installation_id())
            .await?
        {
            Some((record, _)) => (record.manifest.into_manifest_record()?, false),
            None => (
                self.get_manifest(installation.extension_id())
                    .await?
                    .ok_or_else(|| ExtensionInstallationError::UnknownManifest {
                        extension_id: installation.extension_id().clone(),
                    })?,
                true,
            ),
        };
        validate_installation_against_one_manifest(&manifest, &installation)?;
        self.put_v2_installation(&installation, provide_manifest.then_some(&manifest))
            .await?;
        // silent-ok: v2 is authoritative and committed; see
        // upsert_manifest_and_installation for why the projection write must
        // not fail the operation. Startup repairs the projection from v2.
        let _ = self
            .put_installation(&installation, CasExpectation::Any)
            .await;
        Ok(())
    }

    async fn upsert_manifest_only(
        &self,
        installation_id: &ExtensionInstallationId,
        expected_incarnation_id: Option<&InstallationIncarnationId>,
        expected_manifest_ref: &ExtensionManifestRef,
        expected_updated_at: DateTime<Utc>,
        manifest: ExtensionManifestRecord,
    ) -> Result<(), ExtensionInstallationError> {
        if manifest.extension_id() != expected_manifest_ref.extension_id() {
            return Err(ExtensionInstallationError::ManifestExtensionMismatch {
                extension_id: expected_manifest_ref.extension_id().clone(),
                manifest_extension_id: manifest.extension_id().clone(),
            });
        }
        let path = self.v2_scoped_path(&self.v2_installation_path(installation_id)?)?;
        let installation_id = installation_id.clone();
        let expected_incarnation_id = expected_incarnation_id.cloned();
        let expected_manifest_ref = expected_manifest_ref.clone();
        let manifest_wire = WireManifestRecord::from(&manifest);
        cas_update(
            self.scoped_filesystem.as_ref(),
            &ResourceScope::system(),
            &path,
            decode_v2_installation_body,
            entry_for_v2_installation,
            move |current: Option<V2InstallationRecord>| {
                let installation_id = installation_id.clone();
                let expected_incarnation_id = expected_incarnation_id.clone();
                let expected_manifest_ref = expected_manifest_ref.clone();
                let manifest_wire = manifest_wire.clone();
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
                    // The expected timestamp is the aggregate maximum across
                    // this core and its child rows. Child-only changes can make
                    // it newer than the core and must not block this update;
                    // only a core newer than the observed aggregate is stale.
                    if record.lease.is_some()
                        || record.incarnation_id != expected_incarnation_id
                        || record.manifest_ref() != expected_manifest_ref
                        || record.updated_at > expected_updated_at
                    {
                        return Err(
                            ExtensionInstallationError::PreparationFinalizationRejected {
                                installation_id,
                            },
                        );
                    }
                    let manifest_extension_id = manifest_wire
                        .resolved
                        .as_ref()
                        .ok_or_else(|| {
                            invalid_installation_error("manifest-only refresh was not resolved")
                        })?
                        .id
                        .clone();
                    if record.extension_id != manifest_extension_id {
                        return Err(ExtensionInstallationError::ManifestExtensionMismatch {
                            extension_id: record.extension_id,
                            manifest_extension_id,
                        });
                    }
                    record.manifest = manifest_wire;
                    record.updated_at = Utc::now();
                    Ok(CasApply::new(record, ()))
                }
            },
        )
        .await
        .map_err(|error| map_extension_state_cas_error(error, "installation"))?;
        // The v2 row is authoritative. Keep the legacy projection best effort;
        // startup repair converges it if this compatibility write races.
        let _ = self.put_manifest(&manifest, CasExpectation::Any).await;
        Ok(())
    }

    async fn finalize_preparation(
        &self,
        installation_id: &ExtensionInstallationId,
        incarnation_id: &InstallationIncarnationId,
        expected_pending_manifest_ref: &ExtensionManifestRef,
        finalized_manifest: ExtensionManifestRecord,
    ) -> Result<ExtensionInstallation, ExtensionInstallationError> {
        self.take_v2_preparation_lease(
            installation_id,
            incarnation_id,
            expected_pending_manifest_ref,
        )
        .await?;
        let core = match self
            .finish_v2_preparation(
                installation_id,
                incarnation_id,
                expected_pending_manifest_ref,
                &finalized_manifest,
            )
            .await
        {
            Ok(core) => core,
            Err(rejection) => {
                // The lease was committed by take_v2_preparation_lease above;
                // finish_v2_preparation's real validation rejected the
                // finalize, so the aggregate would otherwise be stranded
                // under a held lease (is_visible() requires lease.is_none())
                // until the next store-construction repair pass. Release it
                // here so the aggregate stays visible immediately.
                // silent-ok: best-effort release racing a concurrent repair
                // or retry; the matching lease is cleared, while a newer lease
                // is preserved, and the original rejection remains the error
                // the caller must see.
                let _ = self
                    .clear_v2_preparation_lease(
                        installation_id,
                        incarnation_id,
                        expected_pending_manifest_ref,
                    )
                    .await;
                return Err(rejection);
            }
        };
        let installation = self.reconstruct_v2_installation(&core).await?;
        // The v2 aggregate has committed. Compatibility rows are repaired at
        // next open, so a projection failure must not make callers undo the
        // winner or expose a partial publication.
        let _ = self
            .put_manifest(&finalized_manifest, CasExpectation::Any)
            .await;
        let _ = self
            .put_installation(&installation, CasExpectation::Any)
            .await;
        Ok(installation)
    }

    async fn checkpoint_preparation(
        &self,
        installation_id: &ExtensionInstallationId,
        incarnation_id: &InstallationIncarnationId,
        expected_pending_manifest_ref: &ExtensionManifestRef,
        next_pending_manifest: ExtensionManifestRecord,
    ) -> Result<ExtensionInstallation, ExtensionInstallationError> {
        let next_wire = WireManifestRecord::from(&next_pending_manifest);
        let Some((current, _)) = self.load_v2_installation_record(installation_id).await? else {
            return Err(ExtensionInstallationError::InstallationNotFound {
                installation_id: installation_id.clone(),
            });
        };
        let current_resolved = current
            .manifest
            .resolved
            .as_ref()
            .ok_or_else(|| invalid_installation_error("pending manifest was not resolved"))?;
        let next_resolved = next_wire
            .resolved
            .as_ref()
            .ok_or_else(|| invalid_installation_error("checkpoint manifest was not resolved"))?;
        if !current.is_visible()
            || current_resolved.has_model_visible_capabilities()
            || next_resolved.has_model_visible_capabilities()
            || current.incarnation_id.as_ref() != Some(incarnation_id)
            || current.manifest_ref() != *expected_pending_manifest_ref
            || current.extension_id != next_resolved.id
            || current.manifest.source != next_wire.source
            || current_resolved.root_binding != next_resolved.root_binding
        {
            return Err(
                ExtensionInstallationError::PreparationFinalizationRejected {
                    installation_id: installation_id.clone(),
                },
            );
        }
        self.take_v2_preparation_lease(
            installation_id,
            incarnation_id,
            expected_pending_manifest_ref,
        )
        .await?;
        let core = match self
            .finish_v2_preparation_checkpoint(
                installation_id,
                incarnation_id,
                expected_pending_manifest_ref,
                &next_pending_manifest,
            )
            .await
        {
            Ok(core) => core,
            Err(rejection) => {
                // The lease was committed above. Keep the pending aggregate
                // immediately visible if the checkpoint CAS fails.
                let _ = self
                    .clear_v2_preparation_lease(
                        installation_id,
                        incarnation_id,
                        expected_pending_manifest_ref,
                    )
                    .await;
                return Err(rejection);
            }
        };
        let installation = self.reconstruct_v2_installation(&core).await?;
        let _ = self
            .put_manifest(&next_pending_manifest, CasExpectation::Any)
            .await;
        let _ = self
            .put_installation(&installation, CasExpectation::Any)
            .await;
        Ok(installation)
    }

    async fn activate_membership(
        &self,
        installation_id: &ExtensionInstallationId,
        user_id: &UserId,
    ) -> Result<ExtensionInstallation, ExtensionInstallationError> {
        let Some((core, _)) = self.load_v2_installation_record(installation_id).await? else {
            return Err(ExtensionInstallationError::InstallationNotFound {
                installation_id: installation_id.clone(),
            });
        };
        if core.is_removed() {
            return Err(ExtensionInstallationError::InstallationNotFound {
                installation_id: installation_id.clone(),
            });
        }
        if core.lease.is_some() {
            return Err(ExtensionInstallationError::MembershipMutationInProgress {
                installation_id: installation_id.clone(),
            });
        }
        let now = Utc::now();
        let changed = self
            .activate_v2_membership_row(installation_id, user_id, now)
            .await?;
        let Some((observed_core, _)) = self.load_v2_installation_record(installation_id).await?
        else {
            if changed {
                self.deactivate_v2_membership_row(installation_id, user_id, now)
                    .await?;
            }
            return Err(ExtensionInstallationError::InstallationNotFound {
                installation_id: installation_id.clone(),
            });
        };
        if !observed_core.is_visible() {
            // Deliberately roll the new row back only for a tombstoned
            // record. While a removal lease is held, unconditionally
            // deactivating our row can interleave with the lease holder's
            // member-count recheck and strand a live record with zero
            // members. Leaving the row active is the safe at-least-once
            // outcome: a final removal tombstones every active row, and the
            // caller's transient error invites the retry that converges.
            if changed && observed_core.is_removed() {
                self.deactivate_v2_membership_row(installation_id, user_id, now)
                    .await?;
            }
            return if observed_core.is_removed() {
                Err(ExtensionInstallationError::InstallationNotFound {
                    installation_id: installation_id.clone(),
                })
            } else {
                Err(ExtensionInstallationError::MembershipMutationInProgress {
                    installation_id: installation_id.clone(),
                })
            };
        }
        let core = self
            .canonicalize_v2_installation_owner(installation_id, now)
            .await?;
        let installation = self.reconstruct_v2_installation(&core).await?;
        // silent-ok: the membership row (the authority) is committed; a
        // projection-write failure must not report the join as failed.
        // Startup repairs the projection from v2.
        let _ = self
            .put_installation(&installation, CasExpectation::Any)
            .await;
        Ok(installation)
    }

    async fn deactivate_membership(
        &self,
        installation_id: &ExtensionInstallationId,
        user_id: &UserId,
    ) -> Result<MembershipDeactivation, ExtensionInstallationError> {
        let Some((current_core, _)) = self.load_v2_installation_record(installation_id).await?
        else {
            return Err(ExtensionInstallationError::InstallationNotFound {
                installation_id: installation_id.clone(),
            });
        };
        if current_core.legacy_tenant_owner {
            return Err(ExtensionInstallationError::LegacyTenantOwnerNotCanonicalized);
        }
        if !current_core.is_visible() {
            return Err(ExtensionInstallationError::MembershipMutationInProgress {
                installation_id: installation_id.clone(),
            });
        }
        let prior = self.reconstruct_v2_installation(&current_core).await?;
        let now = Utc::now();
        if let Err(error) = self
            .reserve_v2_membership_removal(installation_id, user_id, now)
            .await
        {
            if !matches!(error, ExtensionInstallationError::StoreUnavailable { .. }) {
                return Err(error);
            }
            let v2_restored = self.write_v2_installation_components(&prior, None).await;
            let compatibility_restored = self.put_installation(&prior, CasExpectation::Any).await;
            if v2_restored.is_err() || compatibility_restored.is_err() {
                return Err(store_unavailable_error(
                    "membership reservation failed and its prior installation could not be restored",
                ));
            }
            return Err(error);
        }
        match self
            .deactivate_reserved_v2_membership(installation_id, user_id, now)
            .await
        {
            Ok(result) => Ok(result),
            Err(error) => {
                let v2_restored = self.write_v2_installation_components(&prior, None).await;
                let compatibility_restored =
                    self.put_installation(&prior, CasExpectation::Any).await;
                if v2_restored.is_err() || compatibility_restored.is_err() {
                    return Err(store_unavailable_error(
                        "membership mutation failed and its prior installation could not be restored",
                    ));
                }
                Err(error)
            }
        }
    }

    async fn delete_installation(
        &self,
        installation_id: &ExtensionInstallationId,
    ) -> Result<(), ExtensionInstallationError> {
        let now = Utc::now();
        self.begin_v2_installation_removal(installation_id, now)
            .await?;
        for membership in self.query_v2_memberships(installation_id).await? {
            if membership.is_active() {
                self.deactivate_v2_membership_row(installation_id, &membership.user_id, now)
                    .await?;
            }
        }
        for binding in self.query_v2_credential_bindings(installation_id).await? {
            if binding.is_active() {
                self.deactivate_v2_credential_binding_row(
                    installation_id,
                    &binding.credential_handle,
                    now,
                )
                .await?;
            }
        }
        self.finish_v2_installation_removal(installation_id, now)
            .await?;
        // The legacy manifest projection deliberately stays: the tombstone is
        // cleanup-pending and its definition remains authoritative until
        // `delete_manifest` marks convergence and retires both.
        if let Some((_, version)) = self.load_installation_entry(installation_id).await? {
            self.delete_installation_row(installation_id, version)
                .await
                .map_err(SaveRowError::into_installation_error)?;
        }
        Ok(())
    }

    /// The definition is tombstoned by `delete_installation`; this method is
    /// the convergence marker — it errors while the definition is still live,
    /// clears the cleanup-pending state, and retires the legacy manifest
    /// projection.
    async fn delete_manifest(
        &self,
        extension_id: &ExtensionId,
    ) -> Result<(), ExtensionInstallationError> {
        let records = self.load_v2_records_by_extension(extension_id).await?;
        if records.is_empty() {
            return Err(ExtensionInstallationError::ManifestNotFound {
                extension_id: extension_id.clone(),
            });
        }
        if let Some(leased) = records
            .iter()
            .find(|record| record.lease.is_some() && !record.is_removed())
        {
            return Err(ExtensionInstallationError::MembershipMutationInProgress {
                installation_id: leased.installation_id.clone(),
            });
        }
        if records.iter().any(|record| !record.is_removed()) {
            return Err(ExtensionInstallationError::InvalidInstallation {
                reason: format!("extension {extension_id} still has installations"),
            });
        }
        for record in records
            .iter()
            .filter(|record| record.removal_cleanup_pending)
        {
            self.mark_v2_removal_converged(&record.installation_id)
                .await?;
        }
        if let Some((_, version)) = self.load_manifest_entry(extension_id).await? {
            self.delete_manifest_row(extension_id, version)
                .await
                .map_err(SaveRowError::into_installation_error)?;
        }
        Ok(())
    }
}
