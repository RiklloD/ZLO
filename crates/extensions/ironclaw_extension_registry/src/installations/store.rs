use super::*;

mod port_impl;
mod repair;
mod v2_lifecycle;
mod v2_membership;

#[cfg(test)]
mod tests;

/// Filesystem-backed extension installation state store.
///
/// Manifests, installation identity, user memberships, and credential
/// bindings are persisted as separate typed record rows under the configured
/// root path. Secondary indexes are declared on the row prefixes so scans that
/// gate lifecycle behavior can use the filesystem query API instead of loading
/// or rewriting a monolithic state snapshot.
pub struct ExtensionInstallationStore {
    filesystem: Arc<dyn RootFilesystem>,
    scoped_filesystem: Arc<ScopedFilesystem<dyn RootFilesystem>>,
    root: VirtualPath,
    host_ports: HostPortCatalog,
    contracts: HostApiContractRegistry,
}

impl fmt::Debug for ExtensionInstallationStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExtensionInstallationStore")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl ExtensionInstallationStore {
    pub async fn load_at(
        filesystem: Arc<dyn RootFilesystem>,
        root: VirtualPath,
        host_ports: HostPortCatalog,
        contracts: HostApiContractRegistry,
    ) -> Result<Self, ExtensionInstallationError> {
        let mount_view = MountView::new(vec![MountGrant::new(
            MountAlias::new("/extension-state").map_err(invalid_installation_error)?,
            root.clone(),
            MountPermissions::read_write_list_delete(),
        )])
        .map_err(invalid_installation_error)?;
        let scoped_filesystem = Arc::new(ScopedFilesystem::with_fixed_view(
            Arc::clone(&filesystem),
            mount_view,
        ));
        let store = Self {
            filesystem,
            scoped_filesystem,
            root,
            host_ports,
            contracts,
        };
        store.ensure_indexes().await?;
        store.migrate_legacy_manifest_rows().await?;
        store.ensure_v2_indexes().await?;
        store.bootstrap_v2_from_compatibility_rows().await?;
        store.repair_interrupted_v2_leases().await?;
        store.repair_removed_v2_children().await?;
        store.repair_compatibility_views().await?;
        Ok(store)
    }

    pub fn default_state_path() -> Result<VirtualPath, ExtensionInstallationError> {
        VirtualPath::new(DEFAULT_INSTALLATION_STATE_PATH).map_err(invalid_installation_error)
    }

    pub(super) async fn ensure_indexes(&self) -> Result<(), ExtensionInstallationError> {
        self.ensure_exact_index(
            &self.manifests_root()?,
            "extension_manifests_by_extension_id",
            "extension_id",
        )
        .await?;
        self.ensure_exact_index(
            &self.installations_root()?,
            "extension_installations_by_installation_id",
            "installation_id",
        )
        .await?;
        self.ensure_exact_index(
            &self.installations_root()?,
            "extension_installations_by_extension_id",
            "extension_id",
        )
        .await
    }

    pub(super) async fn ensure_v2_indexes(&self) -> Result<(), ExtensionInstallationError> {
        for (prefix, name, key) in [
            (
                self.v2_installations_root()?,
                "extension_v2_installations_by_installation_id",
                "installation_id",
            ),
            (
                self.v2_installations_root()?,
                "extension_v2_installations_by_extension_id",
                "extension_id",
            ),
            (
                self.v2_memberships_root()?,
                "extension_v2_memberships_by_installation_id",
                "installation_id",
            ),
            (
                self.v2_memberships_root()?,
                "extension_v2_memberships_by_user_id",
                "user_id",
            ),
            (
                self.v2_credential_bindings_root()?,
                "extension_v2_bindings_by_installation_id",
                "installation_id",
            ),
        ] {
            self.ensure_exact_index(&prefix, name, key).await?;
        }
        Ok(())
    }

    pub(super) async fn ensure_exact_index(
        &self,
        prefix: &VirtualPath,
        name: &'static str,
        key: &'static str,
    ) -> Result<(), ExtensionInstallationError> {
        let name = index_name(name)?;
        let key = index_key(key)?;
        let spec = IndexSpec::new(name, vec![key], IndexKind::Exact);
        self.filesystem
            .ensure_index(prefix, &spec)
            .await
            .map_err(store_unavailable(
                "ensure extension installation store index",
            ))
    }

    /// One-time compatibility compiler for rows written by the filesystem
    /// store before resolved manifests became authoritative. The migrated row
    /// is rewritten under CAS before `load_at` returns, so normal projection
    /// paths never reparse raw TOML.
    pub(super) async fn migrate_legacy_manifest_rows(
        &self,
    ) -> Result<(), ExtensionInstallationError> {
        let rows = query_all(&self.filesystem, &self.manifests_root()?, &Filter::All).await?;
        for row in rows {
            let mut complete = false;
            for _ in 0..=FILESYSTEM_CAS_RETRIES {
                let Some(current) = self
                    .filesystem
                    .get(&row.path)
                    .await
                    .map_err(store_unavailable("load legacy extension manifest row"))?
                else {
                    complete = true;
                    break;
                };
                ensure_entry_kind(&current.entry, MANIFEST_RECORD_KIND, &row.path)?;
                let wire: WireManifestRecord = current.entry.parse_json().map_err(|error| {
                    corrupt_row(
                        "deserialize legacy extension manifest row",
                        &row.path,
                        error,
                    )
                })?;
                if wire.resolved.is_some() {
                    complete = true;
                    break;
                }
                let record = ExtensionManifestRecord::from_toml_with_root_binding(
                    wire.raw_toml,
                    wire.source.into_manifest_source(),
                    &self.host_ports,
                    wire.manifest_hash,
                    &self.contracts,
                    // Legacy pre-REC-1 row: no root was ever persisted for it.
                    PackageRootBinding::FabricateOnLoad,
                )?
                .with_removal_cleanup_requirements(wire.removal_cleanup_requirements);
                match self
                    .filesystem
                    .put(
                        &row.path,
                        entry_for_manifest(&record)?,
                        CasExpectation::Version(current.version),
                    )
                    .await
                {
                    Ok(_) => {
                        complete = true;
                        break;
                    }
                    Err(FilesystemError::VersionMismatch { .. }) => continue,
                    Err(error) => {
                        return Err(store_unavailable("migrate extension manifest row")(error));
                    }
                }
            }
            if !complete {
                return Err(store_unavailable_error(
                    "legacy extension manifest row changed repeatedly while migrating",
                ));
            }
        }
        Ok(())
    }

    pub(super) fn manifests_root(&self) -> Result<VirtualPath, ExtensionInstallationError> {
        child_path(&self.root, "manifests")
    }

    pub(super) fn registered_definitions_root(
        &self,
    ) -> Result<VirtualPath, ExtensionInstallationError> {
        child_path(&self.root, "registered-definitions")
    }

    pub(super) fn installations_root(&self) -> Result<VirtualPath, ExtensionInstallationError> {
        child_path(&self.root, "installations")
    }

    pub(super) fn v2_root(&self) -> Result<VirtualPath, ExtensionInstallationError> {
        child_path(&self.root, "v2")
    }

    pub(super) fn v2_installations_root(&self) -> Result<VirtualPath, ExtensionInstallationError> {
        child_path(&self.v2_root()?, "installations")
    }

    pub(super) fn v2_memberships_root(&self) -> Result<VirtualPath, ExtensionInstallationError> {
        child_path(&self.v2_root()?, "memberships")
    }

    pub(super) fn v2_credential_bindings_root(
        &self,
    ) -> Result<VirtualPath, ExtensionInstallationError> {
        child_path(&self.v2_root()?, "credential-bindings")
    }

    pub(super) fn manifest_path(
        &self,
        extension_id: &ExtensionId,
    ) -> Result<VirtualPath, ExtensionInstallationError> {
        child_path(
            &self.manifests_root()?,
            &format!("{}.json", row_token(extension_id.as_str())),
        )
    }

    pub(super) fn registered_definition_path(
        &self,
        extension_id: &ExtensionId,
    ) -> Result<VirtualPath, ExtensionInstallationError> {
        child_path(
            &self.registered_definitions_root()?,
            &format!("{}.json", row_token(extension_id.as_str())),
        )
    }

    pub(super) fn installation_path(
        &self,
        installation_id: &ExtensionInstallationId,
    ) -> Result<VirtualPath, ExtensionInstallationError> {
        child_path(
            &self.installations_root()?,
            &format!("{}.json", row_token(installation_id.as_str())),
        )
    }

    pub(super) fn v2_installation_path(
        &self,
        installation_id: &ExtensionInstallationId,
    ) -> Result<VirtualPath, ExtensionInstallationError> {
        child_path(
            &self.v2_installations_root()?,
            &format!("{}.json", row_token(installation_id.as_str())),
        )
    }

    pub(super) fn v2_membership_root(
        &self,
        installation_id: &ExtensionInstallationId,
    ) -> Result<VirtualPath, ExtensionInstallationError> {
        child_path(
            &self.v2_memberships_root()?,
            &row_token(installation_id.as_str()),
        )
    }

    pub(super) fn v2_membership_path(
        &self,
        installation_id: &ExtensionInstallationId,
        user_id: &UserId,
    ) -> Result<VirtualPath, ExtensionInstallationError> {
        child_path(
            &self.v2_membership_root(installation_id)?,
            &format!("{}.json", row_token(user_id.as_str())),
        )
    }

    pub(super) fn v2_membership_scoped_path(
        &self,
        installation_id: &ExtensionInstallationId,
        user_id: &UserId,
    ) -> Result<ScopedPath, ExtensionInstallationError> {
        self.v2_scoped_path(&self.v2_membership_path(installation_id, user_id)?)
    }

    pub(super) fn v2_scoped_path(
        &self,
        virtual_path: &VirtualPath,
    ) -> Result<ScopedPath, ExtensionInstallationError> {
        let relative = virtual_path
            .as_str()
            .strip_prefix(self.root.as_str())
            .ok_or_else(|| {
                invalid_installation_error("v2 extension state path escaped the installation root")
            })?;
        ScopedPath::new(format!("/extension-state{relative}")).map_err(invalid_installation_error)
    }

    pub(super) fn v2_credential_binding_root(
        &self,
        installation_id: &ExtensionInstallationId,
    ) -> Result<VirtualPath, ExtensionInstallationError> {
        child_path(
            &self.v2_credential_bindings_root()?,
            &row_token(installation_id.as_str()),
        )
    }

    pub(super) fn v2_credential_binding_path(
        &self,
        installation_id: &ExtensionInstallationId,
        credential_handle: &ExtensionCredentialHandle,
    ) -> Result<VirtualPath, ExtensionInstallationError> {
        child_path(
            &self.v2_credential_binding_root(installation_id)?,
            &format!("{}.json", row_token(credential_handle.as_str())),
        )
    }

    pub(super) async fn load_manifest_entry(
        &self,
        extension_id: &ExtensionId,
    ) -> Result<Option<(ExtensionManifestRecord, RecordVersion)>, ExtensionInstallationError> {
        let path = self.manifest_path(extension_id)?;
        let Some(entry) = self
            .filesystem
            .get(&path)
            .await
            .map_err(store_unavailable("load extension manifest row"))?
        else {
            return Ok(None);
        };
        let manifest = self.parse_manifest_entry(entry.entry, &path)?;
        if manifest.extension_id() != extension_id {
            return Err(invalid_installation_error(format!(
                "extension manifest row key {extension_id} contained manifest {}",
                manifest.extension_id()
            )));
        }
        Ok(Some((manifest, entry.version)))
    }

    pub(super) async fn load_installation_entry(
        &self,
        installation_id: &ExtensionInstallationId,
    ) -> Result<Option<(ExtensionInstallation, RecordVersion)>, ExtensionInstallationError> {
        let path = self.installation_path(installation_id)?;
        let Some(entry) = self
            .filesystem
            .get(&path)
            .await
            .map_err(store_unavailable("load extension installation row"))?
        else {
            return Ok(None);
        };
        let installation = parse_installation_entry(entry.entry, &path)?;
        if installation.installation_id() != installation_id {
            return Err(invalid_installation_error(format!(
                "extension installation row key {installation_id} contained installation {}",
                installation.installation_id()
            )));
        }
        Ok(Some((installation, entry.version)))
    }

    pub(super) async fn query_installations(
        &self,
        filter: &Filter,
    ) -> Result<Vec<ExtensionInstallation>, ExtensionInstallationError> {
        let rows = query_all(&self.filesystem, &self.installations_root()?, filter).await?;
        rows.into_iter()
            .map(|entry| parse_installation_entry(entry.entry, &entry.path))
            .collect()
    }

    pub(super) fn parse_manifest_entry(
        &self,
        entry: Entry,
        path: &VirtualPath,
    ) -> Result<ExtensionManifestRecord, ExtensionInstallationError> {
        ensure_entry_kind(&entry, MANIFEST_RECORD_KIND, path)?;
        let wire: WireManifestRecord = entry
            .parse_json()
            .map_err(|error| corrupt_row("deserialize extension manifest row", path, error))?;
        wire.into_manifest_record()
    }

    pub(super) async fn put_manifest(
        &self,
        manifest: &ExtensionManifestRecord,
        cas: CasExpectation,
    ) -> Result<(), SaveRowError> {
        let path = self.manifest_path(manifest.extension_id())?;
        match self
            .filesystem
            .put(&path, entry_for_manifest(manifest)?, cas)
            .await
        {
            Ok(_) => Ok(()),
            Err(FilesystemError::VersionMismatch { .. }) => Err(SaveRowError::CasConflict),
            Err(error) => Err(SaveRowError::Store(store_unavailable(
                "save extension manifest row",
            )(error))),
        }
    }

    pub(super) async fn put_installation(
        &self,
        installation: &ExtensionInstallation,
        cas: CasExpectation,
    ) -> Result<(), SaveRowError> {
        let path = self.installation_path(installation.installation_id())?;
        match self
            .filesystem
            .put(&path, entry_for_installation(installation)?, cas)
            .await
        {
            Ok(_) => Ok(()),
            Err(FilesystemError::VersionMismatch { .. }) => Err(SaveRowError::CasConflict),
            Err(error) => Err(SaveRowError::Store(store_unavailable(
                "save extension installation row",
            )(error))),
        }
    }

    pub(super) async fn delete_installation_row(
        &self,
        installation_id: &ExtensionInstallationId,
        version: RecordVersion,
    ) -> Result<(), SaveRowError> {
        let path = self.installation_path(installation_id)?;
        match self.filesystem.delete_if_version(&path, version).await {
            Ok(()) => Ok(()),
            Err(FilesystemError::VersionMismatch { .. }) => Err(SaveRowError::CasConflict),
            Err(FilesystemError::NotFound { .. }) => Err(SaveRowError::NotFound),
            Err(error) => Err(SaveRowError::Store(store_unavailable(
                "delete extension installation row",
            )(error))),
        }
    }

    pub(super) async fn delete_manifest_row(
        &self,
        extension_id: &ExtensionId,
        version: RecordVersion,
    ) -> Result<(), SaveRowError> {
        let path = self.manifest_path(extension_id)?;
        match self.filesystem.delete_if_version(&path, version).await {
            Ok(()) => Ok(()),
            Err(FilesystemError::VersionMismatch { .. }) => Err(SaveRowError::CasConflict),
            Err(FilesystemError::NotFound { .. }) => Err(SaveRowError::NotFound),
            Err(error) => Err(SaveRowError::Store(store_unavailable(
                "delete extension manifest row",
            )(error))),
        }
    }
}
