use super::*;

impl ExtensionInstallationStore {
    /// Load every visible installation with a constant number of queries.
    ///
    /// The normalized layout stores each aggregate's members and credential
    /// bindings as child rows, so reconstructing them one installation at a
    /// time costs `1 + 2N` round trips. That is invisible on a local disk and
    /// dominates on network-attached storage, so both child collections are
    /// read once and joined in memory instead.
    pub(super) async fn query_v2_installations(
        &self,
        filter: &Filter,
    ) -> Result<Vec<ExtensionInstallation>, ExtensionInstallationError> {
        let rows = query_all(&self.filesystem, &self.v2_installations_root()?, filter).await?;
        let mut cores = Vec::new();
        for row in rows {
            let core = parse_v2_installation_entry(row.entry, &row.path)?;
            if core.is_visible() {
                cores.push(core);
            }
        }
        if cores.is_empty() {
            return Ok(Vec::new());
        }
        let mut memberships = self.query_v2_memberships_by_installation().await?;
        let mut bindings = self.query_v2_credential_bindings_by_installation().await?;
        let mut installations = Vec::with_capacity(cores.len());
        for core in &cores {
            installations.push(Self::assemble_v2_installation(
                core,
                memberships
                    .remove(&core.installation_id)
                    .unwrap_or_default(),
                bindings.remove(&core.installation_id).unwrap_or_default(),
            )?);
        }
        Ok(installations)
    }

    /// Import legacy two-row aggregates (manifest row + installation row)
    /// absent from v2 by joining each legacy installation with its legacy
    /// manifest. A legacy installation without its manifest could never have
    /// operated and fails the import loudly; an orphaned legacy manifest row
    /// with no installation is the legacy flow's durable removal-cleanup
    /// marker and imports as a cleanup-pending tombstone so the interrupted
    /// removal stays retryable after the migration.
    pub(super) async fn bootstrap_v2_from_compatibility_rows(
        &self,
    ) -> Result<(), ExtensionInstallationError> {
        let mut installed_extensions = BTreeSet::new();
        let legacy_installations = self.query_installations(&Filter::All).await?;
        for installation in legacy_installations {
            installed_extensions.insert(installation.extension_id().clone());
            if self
                .load_v2_installation_record(installation.installation_id())
                .await?
                .is_some()
            {
                continue;
            }
            let Some((manifest, _)) = self
                .load_manifest_entry(installation.extension_id())
                .await?
            else {
                return Err(invalid_installation_error(format!(
                    "legacy installation {} has no legacy manifest row to import",
                    installation.installation_id()
                )));
            };
            self.put_v2_installation(&installation, Some(&manifest))
                .await?;
        }
        let legacy_manifest_rows =
            query_all(&self.filesystem, &self.manifests_root()?, &Filter::All).await?;
        for row in legacy_manifest_rows {
            let manifest = self.parse_manifest_entry(row.entry, &row.path)?;
            if installed_extensions.contains(manifest.extension_id()) {
                continue;
            }
            if self
                .load_v2_records_by_extension(manifest.extension_id())
                .await?
                .is_empty()
            {
                self.persist_removal_tombstone(manifest).await?;
            }
        }
        Ok(())
    }

    pub(super) async fn repair_removed_v2_children(
        &self,
    ) -> Result<(), ExtensionInstallationError> {
        let installation_rows = query_all(
            &self.filesystem,
            &self.v2_installations_root()?,
            &Filter::All,
        )
        .await?;
        for row in installation_rows {
            let core = parse_v2_installation_entry(row.entry, &row.path)?;
            if !core.is_removed() {
                continue;
            }
            let removed_at = core.removed_at.unwrap_or(core.updated_at);
            for membership in self.query_v2_memberships(&core.installation_id).await? {
                if membership.is_active() {
                    self.deactivate_v2_membership_row(
                        &core.installation_id,
                        &membership.user_id,
                        removed_at,
                    )
                    .await?;
                }
            }
            for binding in self
                .query_v2_credential_bindings(&core.installation_id)
                .await?
            {
                if binding.is_active() {
                    self.deactivate_v2_credential_binding_row(
                        &core.installation_id,
                        &binding.credential_handle,
                        removed_at,
                    )
                    .await?;
                }
            }
        }
        Ok(())
    }

    pub(super) async fn repair_interrupted_v2_leases(
        &self,
    ) -> Result<(), ExtensionInstallationError> {
        let rows = query_all(
            &self.filesystem,
            &self.v2_installations_root()?,
            &Filter::All,
        )
        .await?;
        for row in rows {
            let core = parse_v2_installation_entry(row.entry, &row.path)?;
            if core.lease.is_none() || core.is_removed() {
                continue;
            }
            if let Some((installation, _)) =
                self.load_installation_entry(&core.installation_id).await?
            {
                self.put_v2_installation(&installation, None).await?;
                continue;
            }
            // A missing compatibility snapshot is NOT proof of removal:
            // successful v2 writes deliberately tolerate failed projection
            // writes, so a live record can legitimately have no snapshot.
            // The child rows are the authority. Surviving active membership
            // (or a legacy tenant owner) means the record was live — clear
            // the lease over the children as they stand. Zero active members
            // can only mean a final removal already tombstoned every child,
            // so only then does the record roll forward to removed.
            let has_live_owner = core.legacy_tenant_owner
                || self
                    .query_v2_memberships(&core.installation_id)
                    .await?
                    .iter()
                    .any(V2MembershipRecord::is_active);
            if has_live_owner {
                self.clear_v2_lease(&core.installation_id).await?;
            } else {
                self.finish_v2_installation_removal(&core.installation_id, Utc::now())
                    .await?;
            }
        }
        Ok(())
    }

    /// Release a dangling lease without touching any other state, leaving the
    /// record live over its child rows as they stand. Startup lease recovery
    /// uses this when no compatibility snapshot exists to roll back to.
    pub(super) async fn clear_v2_lease(
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
                    if record.lease.is_some() {
                        record.lease = None;
                        record.updated_at = Utc::now();
                    }
                    Ok(CasApply::new(record, ()))
                }
            },
        )
        .await
        .map_err(|error| map_extension_state_cas_error(error, "installation"))
    }

    /// Clear exactly the preparation lease `token` names. A no-op when the
    /// record no longer holds that lease — including a newer lease with the
    /// same incarnation and manifest (told apart by the persisted lease id),
    /// and a legacy lease without an id, which only startup repair clears.
    pub(super) async fn clear_v2_preparation_lease(
        &self,
        token: &PreparationLeaseToken,
    ) -> Result<(), ExtensionInstallationError> {
        let path = self.v2_scoped_path(&self.v2_installation_path(&token.installation_id)?)?;
        clear_preparation_lease_at(self.scoped_filesystem.as_ref(), &path, token).await
    }

    /// Release the lease an operation took after that operation was rejected.
    ///
    /// The rejection stays the error the caller sees. If the release itself
    /// fails, the failure is attached as the error's source and a bounded,
    /// identity-checked repair is scheduled in-process, so the installation
    /// does not stay hidden (`is_visible()` requires no lease) until the next
    /// store-construction repair pass.
    pub(super) async fn release_preparation_lease_after_rejection(
        &self,
        token: PreparationLeaseToken,
        rejection: ExtensionInstallationError,
    ) -> ExtensionInstallationError {
        let cleanup = match self.clear_v2_preparation_lease(&token).await {
            Ok(()) => return rejection,
            Err(cleanup) => cleanup,
        };
        tracing::debug!(
            installation_id = token.installation_id.as_str(),
            error = %cleanup,
            "releasing a rejected operation's preparation lease failed; scheduling repair"
        );
        self.schedule_preparation_lease_repair(token);
        ExtensionInstallationError::RejectedWithLeaseCleanupPending {
            rejection: Box::new(rejection),
            cleanup: Box::new(cleanup),
        }
    }

    fn schedule_preparation_lease_repair(&self, token: PreparationLeaseToken) {
        let path = match self.v2_installation_path(&token.installation_id) {
            Ok(path) => path,
            Err(error) => {
                tracing::debug!(%error, "cannot address a stranded preparation lease for repair");
                return;
            }
        };
        let path = match self.v2_scoped_path(&path) {
            Ok(path) => path,
            Err(error) => {
                tracing::debug!(%error, "cannot address a stranded preparation lease for repair");
                return;
            }
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            // silent-ok: no async runtime to repair on; the store-construction
            // repair pass (repair_interrupted_v2_leases) still clears it.
            tracing::debug!("no async runtime for preparation lease repair; deferring to startup");
            return;
        };
        let filesystem = Arc::clone(&self.scoped_filesystem);
        runtime.spawn(async move {
            repair_preparation_lease(filesystem.as_ref(), &path, &token).await;
        });
    }

    pub(super) async fn repair_compatibility_views(
        &self,
    ) -> Result<(), ExtensionInstallationError> {
        let installation_rows = query_all(
            &self.filesystem,
            &self.v2_installations_root()?,
            &Filter::All,
        )
        .await?;
        for row in installation_rows {
            let core = parse_v2_installation_entry(row.entry, &row.path)?;
            if core.lease.is_some() && !core.is_removed() {
                // A lease observed here was taken by a live peer between the
                // recovery pass and this one. Failing the whole store open
                // over one mid-mutation record would trade correctness for
                // availability; skip it — the holder releases it, or the
                // next startup recovers it.
                continue;
            }
            if core.is_removed() {
                if let Some((_, version)) =
                    self.load_installation_entry(&core.installation_id).await?
                {
                    self.delete_installation_row(&core.installation_id, version)
                        .await
                        .map_err(SaveRowError::into_installation_error)?;
                }
                if core.removal_cleanup_pending {
                    // The definition is still authoritative for cleanup
                    // retries, so its legacy projection stays in place.
                    let manifest = core.manifest.clone().into_manifest_record()?;
                    self.put_manifest(&manifest, CasExpectation::Any)
                        .await
                        .map_err(SaveRowError::into_installation_error)?;
                } else if let Some((_, version)) =
                    self.load_manifest_entry(&core.extension_id).await?
                {
                    self.delete_manifest_row(&core.extension_id, version)
                        .await
                        .map_err(SaveRowError::into_installation_error)?;
                }
            } else {
                let manifest = core.manifest.clone().into_manifest_record()?;
                self.put_manifest(&manifest, CasExpectation::Any)
                    .await
                    .map_err(SaveRowError::into_installation_error)?;
                let installation = self.reconstruct_v2_installation(&core).await?;
                self.put_installation(&installation, CasExpectation::Any)
                    .await
                    .map_err(SaveRowError::into_installation_error)?;
            }
        }
        Ok(())
    }
}

/// Identity of one preparation lease: which aggregate, the incarnation and
/// manifest it covers, and the random lease id its take persisted. A newer
/// lease with the same incarnation and manifest has a different id (even if
/// taken at the same instant), so a late release or repair for this lease
/// can never clear that one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PreparationLeaseToken {
    pub(super) installation_id: ExtensionInstallationId,
    pub(super) incarnation_id: InstallationIncarnationId,
    pub(super) expected_manifest_ref: ExtensionManifestRef,
    /// The persisted identity the release matches on.
    pub(super) lease_id: uuid::Uuid,
    /// When the lease was taken; diagnostics only, never identity.
    pub(super) taken_at: DateTime<Utc>,
}

/// Retry delays for the in-process lease repair (five attempts, ~1.5 s total).
const PREPARATION_LEASE_REPAIR_BACKOFF_MS: [u64; 5] = [50, 100, 200, 400, 800];

async fn repair_preparation_lease(
    filesystem: &ScopedFilesystem<dyn RootFilesystem>,
    path: &ScopedPath,
    token: &PreparationLeaseToken,
) {
    for (attempt, delay_ms) in PREPARATION_LEASE_REPAIR_BACKOFF_MS.into_iter().enumerate() {
        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
        match clear_preparation_lease_at(filesystem, path, token).await {
            Ok(()) => {
                tracing::debug!(
                    installation_id = token.installation_id.as_str(),
                    attempt,
                    "repaired a stranded preparation lease"
                );
                return;
            }
            Err(error) => tracing::debug!(
                installation_id = token.installation_id.as_str(),
                attempt,
                %error,
                "preparation lease repair attempt failed"
            ),
        }
    }
    tracing::debug!(
        installation_id = token.installation_id.as_str(),
        "preparation lease repair exhausted its retries; startup repair will clear it"
    );
}

async fn clear_preparation_lease_at(
    filesystem: &ScopedFilesystem<dyn RootFilesystem>,
    path: &ScopedPath,
    token: &PreparationLeaseToken,
) -> Result<(), ExtensionInstallationError> {
    let token = token.clone();
    cas_update(
        filesystem,
        &ResourceScope::system(),
        path,
        decode_v2_installation_body,
        entry_for_v2_installation,
        move |current: Option<V2InstallationRecord>| {
            let token = token.clone();
            async move {
                let Some(mut record) = current else {
                    return Err(ExtensionInstallationError::InstallationNotFound {
                        installation_id: token.installation_id,
                    });
                };
                if record.installation_id != token.installation_id {
                    return Err(invalid_installation_error(
                        "v2 installation body identity did not match its key",
                    ));
                }
                let lease_matches = record
                    .lease
                    .as_ref()
                    .and_then(|lease| lease.preparation.as_ref())
                    .is_some_and(|preparation| {
                        preparation.lease_id == Some(token.lease_id)
                            && preparation.incarnation_id == token.incarnation_id
                            && preparation.expected_manifest_ref == token.expected_manifest_ref
                    });
                if !lease_matches {
                    return Ok(CasApply::no_op(record, ()));
                }
                record.lease = None;
                record.updated_at = Utc::now();
                Ok(CasApply::new(record, ()))
            }
        },
    )
    .await
    .map_err(|error| map_extension_state_cas_error(error, "installation"))
}
