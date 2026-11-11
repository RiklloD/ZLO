use super::*;

pub(super) const DEFAULT_INSTALLATION_STATE_PATH: &str = "/system/extensions/.installations";
pub(super) const MANIFEST_RECORD_KIND: &str = "extension_manifest_record";
pub(super) const REGISTERED_DEFINITION_RECORD_KIND: &str = "extension_registered_definition_record";
pub(super) const INSTALLATION_RECORD_KIND: &str = "extension_installation_record";
pub(super) const EXTENSION_STATE_V2_SCHEMA: &str = "extension_state.v2";
pub(super) const INSTALLATION_RECORD_KIND_V2: &str = "extension_installation_record_v2";
pub(super) const MEMBERSHIP_RECORD_KIND_V2: &str = "extension_membership_record_v2";
pub(super) const CREDENTIAL_BINDING_RECORD_KIND_V2: &str = "extension_credential_binding_record_v2";
pub(super) const FILESYSTEM_CAS_RETRIES: usize = 5;

pub(super) async fn query_all(
    filesystem: &Arc<dyn RootFilesystem>,
    prefix: &VirtualPath,
    filter: &Filter,
) -> Result<Vec<VersionedEntry>, ExtensionInstallationError> {
    let mut out = Vec::new();
    let mut offset: u64 = 0;
    loop {
        let page = Page::new(offset, Page::MAX_LIMIT);
        let rows = filesystem
            .query(prefix, filter, page)
            .await
            .map_err(store_unavailable("query extension installation rows"))?;
        let received = rows.len();
        out.extend(rows);
        if received < Page::MAX_LIMIT as usize {
            break;
        }
        offset = offset.saturating_add(received as u64);
    }
    Ok(out)
}

pub(super) fn entry_for_manifest(
    manifest: &ExtensionManifestRecord,
) -> Result<Entry, ExtensionInstallationError> {
    let payload = serde_json::to_value(WireManifestRecord::from(manifest))
        .map_err(invalid_installation_error)?;
    Ok(Entry::record(record_kind(MANIFEST_RECORD_KIND)?, &payload)
        .map_err(invalid_installation_error)?
        .with_indexed(
            index_key("extension_id")?,
            IndexValue::Text(manifest.extension_id().as_str().to_string()),
        )
        .with_indexed(
            index_key("manifest_source")?,
            IndexValue::Text(manifest_source_key(manifest.manifest().source).into()),
        ))
}

pub(super) fn entry_for_registered_definition(
    manifest: &ExtensionManifestRecord,
) -> Result<Entry, ExtensionInstallationError> {
    let payload = serde_json::to_value(WireManifestRecord::from(manifest))
        .map_err(invalid_installation_error)?;
    Entry::record(record_kind(REGISTERED_DEFINITION_RECORD_KIND)?, &payload)
        .map_err(invalid_installation_error)
}

pub(super) fn entry_for_installation(
    installation: &ExtensionInstallation,
) -> Result<Entry, ExtensionInstallationError> {
    let payload = serde_json::to_value(installation).map_err(invalid_installation_error)?;
    Ok(
        Entry::record(record_kind(INSTALLATION_RECORD_KIND)?, &payload)
            .map_err(invalid_installation_error)?
            .with_indexed(
                index_key("installation_id")?,
                IndexValue::Text(installation.installation_id().as_str().to_string()),
            )
            .with_indexed(
                index_key("extension_id")?,
                IndexValue::Text(installation.extension_id().as_str().to_string()),
            ),
    )
}

pub(super) fn parse_installation_entry(
    entry: Entry,
    path: &VirtualPath,
) -> Result<ExtensionInstallation, ExtensionInstallationError> {
    ensure_entry_kind(&entry, INSTALLATION_RECORD_KIND, path)?;
    entry
        .parse_json()
        .map_err(|error| corrupt_row("deserialize extension installation row", path, error))
}

pub(super) fn entry_for_v2_installation(
    record: &V2InstallationRecord,
) -> Result<Entry, ExtensionInstallationError> {
    entry_for_v2_record(
        INSTALLATION_RECORD_KIND_V2,
        record,
        [
            (
                "installation_id",
                IndexValue::Text(record.installation_id.as_str().to_string()),
            ),
            (
                "extension_id",
                IndexValue::Text(record.extension_id.as_str().to_string()),
            ),
        ],
    )
}

pub(super) fn entry_for_v2_membership(
    record: &V2MembershipRecord,
) -> Result<Entry, ExtensionInstallationError> {
    entry_for_v2_record(
        MEMBERSHIP_RECORD_KIND_V2,
        record,
        [
            (
                "installation_id",
                IndexValue::Text(record.installation_id.as_str().to_string()),
            ),
            (
                "user_id",
                IndexValue::Text(record.user_id.as_str().to_string()),
            ),
        ],
    )
}

pub(super) fn entry_for_v2_credential_binding(
    record: &V2CredentialBindingRecord,
) -> Result<Entry, ExtensionInstallationError> {
    entry_for_v2_record(
        CREDENTIAL_BINDING_RECORD_KIND_V2,
        record,
        [
            (
                "installation_id",
                IndexValue::Text(record.installation_id.as_str().to_string()),
            ),
            (
                "credential_handle",
                IndexValue::Text(record.credential_handle.as_str().to_string()),
            ),
        ],
    )
}

pub(super) fn entry_for_v2_record<T, const N: usize>(
    kind: &'static str,
    record: &T,
    indexed: [(&'static str, IndexValue); N],
) -> Result<Entry, ExtensionInstallationError>
where
    T: Serialize,
{
    let payload = serde_json::to_value(record).map_err(invalid_installation_error)?;
    let mut entry =
        Entry::record(record_kind(kind)?, &payload).map_err(invalid_installation_error)?;
    for (key, value) in indexed {
        entry = entry.with_indexed(index_key(key)?, value);
    }
    Ok(entry)
}

pub(super) fn parse_v2_installation_entry(
    entry: Entry,
    path: &VirtualPath,
) -> Result<V2InstallationRecord, ExtensionInstallationError> {
    parse_v2_entry(entry, path, INSTALLATION_RECORD_KIND_V2, "installation")
}

pub(super) fn parse_v2_membership_entry(
    entry: Entry,
    path: &VirtualPath,
) -> Result<V2MembershipRecord, ExtensionInstallationError> {
    parse_v2_entry(entry, path, MEMBERSHIP_RECORD_KIND_V2, "membership")
}

pub(super) fn decode_v2_installation_body(
    body: &[u8],
) -> Result<V2InstallationRecord, ExtensionInstallationError> {
    decode_v2_record_body(body, "installation")
}

pub(super) fn decode_v2_membership_body(
    body: &[u8],
) -> Result<V2MembershipRecord, ExtensionInstallationError> {
    decode_v2_record_body(body, "membership")
}

pub(super) fn parse_v2_credential_binding_entry(
    entry: Entry,
    path: &VirtualPath,
) -> Result<V2CredentialBindingRecord, ExtensionInstallationError> {
    parse_v2_entry(
        entry,
        path,
        CREDENTIAL_BINDING_RECORD_KIND_V2,
        "credential binding",
    )
}

pub(super) fn decode_v2_credential_binding_body(
    body: &[u8],
) -> Result<V2CredentialBindingRecord, ExtensionInstallationError> {
    decode_v2_record_body(body, "credential binding")
}

pub(super) fn decode_v2_record_body<T>(
    body: &[u8],
    label: &'static str,
) -> Result<T, ExtensionInstallationError>
where
    T: serde::de::DeserializeOwned,
{
    let value: serde_json::Value =
        serde_json::from_slice(body).map_err(invalid_installation_error)?;
    if value
        .get("schema_version")
        .and_then(serde_json::Value::as_str)
        != Some(EXTENSION_STATE_V2_SCHEMA)
    {
        return Err(invalid_installation_error(format!(
            "v2 extension {label} row had unsupported schema version"
        )));
    }
    serde_json::from_value(value).map_err(invalid_installation_error)
}

pub(super) fn parse_v2_entry<T>(
    entry: Entry,
    path: &VirtualPath,
    expected_kind: &'static str,
    label: &'static str,
) -> Result<T, ExtensionInstallationError>
where
    T: serde::de::DeserializeOwned,
{
    ensure_entry_kind(&entry, expected_kind, path)?;
    let record: T = entry
        .parse_json()
        .map_err(|error| corrupt_row("deserialize v2 extension state row", path, error))?;
    let value = serde_json::from_slice::<serde_json::Value>(&entry.body)
        .map_err(|error| corrupt_row("inspect v2 extension state schema", path, error))?;
    if value
        .get("schema_version")
        .and_then(serde_json::Value::as_str)
        != Some(EXTENSION_STATE_V2_SCHEMA)
    {
        return Err(invalid_installation_error(format!(
            "v2 extension {label} row had unsupported schema version"
        )));
    }
    Ok(record)
}

pub(super) fn ensure_entry_kind(
    entry: &Entry,
    expected: &'static str,
    path: &VirtualPath,
) -> Result<(), ExtensionInstallationError> {
    match entry.kind.as_ref().map(RecordKind::as_str) {
        Some(actual) if actual == expected => Ok(()),
        _ => Err(invalid_installation_error(format!(
            "extension installation store row at {} had unexpected record kind",
            path.as_str()
        ))),
    }
}

pub(super) fn child_path(
    root: &VirtualPath,
    child: &str,
) -> Result<VirtualPath, ExtensionInstallationError> {
    VirtualPath::new(format!("{}/{}", root.as_str().trim_end_matches('/'), child))
        .map_err(invalid_installation_error)
}

pub(super) fn row_token(value: &str) -> String {
    sha256_digest_token(value.as_bytes()).replace(':', "_")
}

pub(super) fn manifest_source_key(source: ManifestSource) -> &'static str {
    match source {
        ManifestSource::HostBundled => "host_bundled",
        ManifestSource::InstalledLocal => "installed_local",
        ManifestSource::RegistryInstalled => "registry_installed",
        ManifestSource::UserRegistered => "user_registered",
    }
}

pub(super) fn record_kind(value: &'static str) -> Result<RecordKind, ExtensionInstallationError> {
    RecordKind::new(value).map_err(invalid_installation_error)
}

pub(super) fn index_name(value: &'static str) -> Result<IndexName, ExtensionInstallationError> {
    IndexName::new(value).map_err(invalid_installation_error)
}

pub(super) fn index_key(value: &'static str) -> Result<IndexKey, ExtensionInstallationError> {
    IndexKey::new(value).map_err(invalid_installation_error)
}

pub(super) fn corrupt_row(
    operation: &'static str,
    path: &VirtualPath,
    error: impl fmt::Display,
) -> ExtensionInstallationError {
    let _ = path;
    invalid_installation_error(format!("{operation}: {error}"))
}

pub(super) fn store_unavailable(
    operation: &'static str,
) -> impl FnOnce(FilesystemError) -> ExtensionInstallationError {
    move |error| {
        let _ = error;
        store_unavailable_error(operation)
    }
}

pub(super) fn invalid_installation_error(error: impl fmt::Display) -> ExtensionInstallationError {
    ExtensionInstallationError::InvalidInstallation {
        reason: error.to_string(),
    }
}

pub(super) fn store_unavailable_error(error: impl fmt::Display) -> ExtensionInstallationError {
    ExtensionInstallationError::StoreUnavailable {
        reason: error.to_string(),
    }
}

pub(super) fn map_extension_state_cas_error(
    error: CasUpdateError<ExtensionInstallationError>,
    record: &'static str,
) -> ExtensionInstallationError {
    match error {
        CasUpdateError::Apply(error) => error,
        CasUpdateError::Timeout => {
            store_unavailable_error(format!("v2 extension {record} CAS update timed out"))
        }
        CasUpdateError::RetriesExhausted => {
            store_unavailable_error(format!("v2 extension {record} changed repeatedly"))
        }
        CasUpdateError::CasUnsupported => {
            store_unavailable_error(format!("v2 extension {record} store does not support CAS"))
        }
        CasUpdateError::Backend(_) => {
            store_unavailable_error(format!("v2 extension {record} backend operation failed"))
        }
    }
}
