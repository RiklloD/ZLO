//! Consent for third-party extension packages ("plugins").
//!
//! A package the binary did not compile in ([`ManifestSource::InstalledLocal`]
//! or [`ManifestSource::RegistryInstalled`]) may not activate until the user
//! grants consent to what it asks for. The grant is pinned to
//! [`package_consent_digest`], a SHA-256 over **every file of the package**
//! (not only the manifest), so a package whose files change after the grant
//! reports [`ExtensionConsentStatus::Tampered`] and stops activating — at
//! boot as well as on explicit activation — until consent is granted again
//! for the new digest.
//!
//! Exempt: host-bundled packages (compiled into the binary, first-party
//! trust) and user-registered hosted-MCP definitions, whose registration
//! request is already an explicit user act and whose tool catalog passes the
//! MCP catalog admission policy. No trust rule changes here: a consented
//! third-party package keeps exactly the trust it had before; consent only
//! adds a gate in front of activation.
//!
//! Grants are durable records under [`CONSENT_RECORD_ROOT`] on the same
//! root filesystem as the installation store (`/system/extensions`). The
//! directory name starts with a dot, so it can never be an extension id and
//! the filesystem catalog loader never mistakes it for a package.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::sync::Arc;

use ironclaw_extension_contracts::state::InstallationState;
use ironclaw_extension_registry::{
    CapabilityVisibility, ExtensionRuntimeV2, ManifestSource, PackageContentPins,
};
use ironclaw_filesystem::{FilesystemError, RootFilesystem};
use ironclaw_host_api::approval::sha256_digest_token;
use ironclaw_host_api::capability::{EffectKind, PermissionMode};
use ironclaw_host_api::ids::{ExtensionId, UserId};
use ironclaw_host_api::path::VirtualPath;
use ironclaw_product_contracts::error::ProductOperationFailure;
use ironclaw_product_contracts::package_consent::{
    LifecycleConsentRequest, LifecycleConsentRequestKind, LifecycleConsentSummary,
    PackageConsentDigest,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::available_extensions::{AvailableExtensionAssetContent, AvailableExtensionPackage};

/// Where consent grants live (one JSON record per extension id).
pub const CONSENT_RECORD_ROOT: &str = "/system/extensions/.consent";

const CONSENT_DIGEST_DOMAIN: &[u8] = b"zlo-extension-package-consent-v1";
const CONSENT_RECORD_SCHEMA: u32 = 2;

/// Whether a package from `source` must carry a consent grant to activate.
pub fn source_requires_consent(source: ManifestSource) -> bool {
    match source {
        ManifestSource::InstalledLocal | ManifestSource::RegistryInstalled => true,
        ManifestSource::HostBundled | ManifestSource::UserRegistered => false,
    }
}

/// One package's files as the consent digest sees them: package-relative
/// path to bytes, the manifest always included.
pub(crate) type PackageFiles = std::collections::BTreeMap<String, Vec<u8>>;

/// The files the catalog holds for `package` (the bytes read when it was
/// loaded or imported).
pub(crate) fn catalog_package_files(package: &AvailableExtensionPackage) -> PackageFiles {
    let mut files: PackageFiles = package
        .assets
        .iter()
        .map(|asset| {
            let AvailableExtensionAssetContent::Bytes(bytes) = &asset.content;
            (asset.path.clone(), bytes.to_vec())
        })
        .collect();
    files
        .entry("manifest.toml".to_string())
        .or_insert_with(|| package.manifest_toml.as_bytes().to_vec());
    files
}

/// The files of `package` as they are on disk now, when its root is
/// materialized there; the catalog's bytes otherwise (an uploaded package
/// not yet installed, or a removed one). Reading the disk — not the catalog's
/// cached copy — is what lets a long-running process notice files that
/// changed after it loaded them.
pub(crate) async fn current_package_files(
    filesystem: &dyn RootFilesystem,
    package: &AvailableExtensionPackage,
) -> Result<PackageFiles, ProductOperationFailure> {
    let Ok(root) = package.package.materialized_root() else {
        return Ok(catalog_package_files(package));
    };
    match filesystem.list_dir(root).await {
        Ok(_) => {}
        Err(FilesystemError::NotFound { .. }) | Err(FilesystemError::MountNotFound { .. }) => {
            return Ok(catalog_package_files(package));
        }
        Err(error) => {
            return Err(ProductOperationFailure::Transient {
                reason: format!("failed to read extension package files: {error}"),
            });
        }
    }
    let assets =
        crate::available_extension_import::inline_extension_dir_assets(filesystem, root).await?;
    Ok(assets
        .into_iter()
        .map(|asset| {
            let AvailableExtensionAssetContent::Bytes(bytes) = asset.content;
            (asset.path, bytes.into_owned())
        })
        .collect())
}

/// SHA-256 over the catalog's copy of every file of the package; see
/// [`package_files_digest`].
pub fn package_consent_digest(
    package: &AvailableExtensionPackage,
) -> Result<PackageConsentDigest, ProductOperationFailure> {
    package_files_digest(&package.package.id, &catalog_package_files(package))
}

/// SHA-256 over every file of a package, in path order, each framed by its
/// length so no two file sets can collide by concatenation.
pub(crate) fn package_files_digest(
    extension_id: &ExtensionId,
    files: &PackageFiles,
) -> Result<PackageConsentDigest, ProductOperationFailure> {
    let mut hasher = Sha256::new();
    hash_part(&mut hasher, CONSENT_DIGEST_DOMAIN);
    hash_part(&mut hasher, extension_id.as_str().as_bytes());
    for (path, bytes) in files {
        hash_part(&mut hasher, path.as_bytes());
        hash_part(&mut hasher, bytes);
    }
    let mut hex = String::with_capacity(64);
    for byte in hasher.finalize().iter() {
        // Writing to a String cannot fail.
        let _written = write!(hex, "{byte:02x}");
    }
    PackageConsentDigest::new(hex).map_err(|error| ProductOperationFailure::Transient {
        reason: format!("package consent digest is malformed: {error}"),
    })
}

/// Per-file hashes of a file set, in the form the runtime lanes verify
/// ([`PackageContentPins`]).
pub(crate) fn package_files_pins(files: &PackageFiles) -> PackageContentPins {
    PackageContentPins::new(
        files
            .iter()
            .map(|(path, bytes)| (path.clone(), sha256_digest_token(bytes)))
            .collect(),
    )
}

fn hash_part(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

/// Everything the package asks for, derived from the resolved manifest,
/// pinned to `digest` (the digest of the files a grant would cover).
pub fn consent_summary(
    package: &AvailableExtensionPackage,
    digest: PackageConsentDigest,
) -> LifecycleConsentSummary {
    LifecycleConsentSummary {
        digest,
        requests: consent_requests(package),
    }
}

fn consent_requests(package: &AvailableExtensionPackage) -> Vec<LifecycleConsentRequest> {
    let resolved = package.resolved_manifest.as_ref();
    let mut requests = Vec::new();
    let mut hosts = BTreeSet::new();
    let mut credentials = BTreeSet::new();

    for tool in &resolved.tools {
        if tool.visibility == CapabilityVisibility::HostInternal {
            continue;
        }
        requests.push(LifecycleConsentRequest {
            kind: LifecycleConsentRequestKind::Tool,
            title: tool.id.as_str().to_string(),
            detail: tool_detail(&tool.description, &tool.effects, tool.default_permission),
        });
        for target in &tool.network_targets {
            hosts.insert(target.host_pattern.clone());
        }
        for credential in &tool.runtime_credentials {
            hosts.insert(credential.audience.host_pattern.clone());
            credentials.insert(credential.handle.as_str().to_string());
        }
    }

    match &resolved.runtime {
        ExtensionRuntimeV2::Mcp { url: Some(url), .. } => {
            if let Some(host) = url_host(url) {
                hosts.insert(host);
            }
        }
        ExtensionRuntimeV2::Script { runner, .. } => requests.push(LifecycleConsentRequest {
            kind: LifecycleConsentRequestKind::Service,
            title: format!("{runner} process"),
            detail: "Runs the plugin's own program in a sandboxed process when a tool is used."
                .to_string(),
        }),
        _ => {}
    }
    if let Some(mcp) = &resolved.mcp {
        // The server's tool list is only known after activation, so consent
        // covers the ceiling every listed tool must fit inside.
        let server_host = url_host(&mcp.server).unwrap_or_else(|| mcp.server.clone());
        requests.push(LifecycleConsentRequest {
            kind: LifecycleConsentRequestKind::Tool,
            title: format!("{}.* (listed by the server)", mcp.namespace),
            detail: tool_detail(
                &format!("Up to {} tools listed by {server_host}", mcp.max_tools),
                &mcp.effects,
                mcp.default_permission,
            ),
        });
        hosts.insert(server_host);
        for handle in &mcp.credential_handles {
            credentials.insert(handle.as_str().to_string());
        }
    }
    if let Some(channel) = &resolved.channel {
        let direction = match (channel.supports_inbound(), channel.supports_outbound()) {
            (true, true) => "receives and sends messages",
            (true, false) => "receives messages",
            (false, true) => "sends messages",
            (false, false) => "connects a channel",
        };
        requests.push(LifecycleConsentRequest {
            kind: LifecycleConsentRequestKind::Service,
            title: format!("{} channel", resolved.name),
            detail: format!(
                "Keeps a channel open that {direction} on your behalf while it is active."
            ),
        });
    }

    for host in hosts {
        requests.push(LifecycleConsentRequest {
            kind: LifecycleConsentRequestKind::Network,
            detail: format!("Sends and receives data over the network from {host}."),
            title: host,
        });
    }
    for surface in &resolved.auth {
        credentials.insert(surface.vendor.as_str().to_string());
    }
    for credential in credentials {
        requests.push(LifecycleConsentRequest {
            kind: LifecycleConsentRequestKind::Credential,
            detail: format!(
                "Uses your {credential} credential; ZLO injects it when a request leaves, so the plugin never reads the secret."
            ),
            title: credential,
        });
    }
    for hook in &resolved.hooks {
        requests.push(LifecycleConsentRequest {
            kind: LifecycleConsentRequestKind::Hook,
            title: hook.local_id.clone(),
            detail: "Runs on agent events and can observe or change what the assistant does."
                .to_string(),
        });
    }
    // `ui_slot` and `setting` requests are reserved kinds: they are derived
    // here once the manifest declares those sections.
    requests
}

fn tool_detail(description: &str, effects: &[EffectKind], permission: PermissionMode) -> String {
    let mut phrases: Vec<&str> = effects
        .iter()
        .map(|effect| effect_phrase(*effect))
        .collect();
    phrases.dedup();
    let can = if phrases.is_empty() {
        "has no declared side effects".to_string()
    } else {
        format!("can {}", phrases.join(", "))
    };
    let ask = match permission {
        PermissionMode::Allow => "runs without asking",
        PermissionMode::Deny => "is blocked unless you allow it",
        _ => "asks you before each use",
    };
    let description = description.trim().trim_end_matches('.');
    if description.is_empty() {
        format!("This tool {can} and {ask}.")
    } else {
        format!("{description}; it {can} and {ask}.")
    }
}

fn effect_phrase(effect: EffectKind) -> &'static str {
    match effect {
        EffectKind::ReadFilesystem => "read files",
        EffectKind::WriteFilesystem => "write files",
        EffectKind::DeleteFilesystem => "delete files",
        EffectKind::Network => "use the network",
        EffectKind::UseSecret => "use a stored credential",
        EffectKind::ExecuteCode => "run code",
        EffectKind::SpawnProcess => "start processes",
        EffectKind::DispatchCapability => "call other tools",
        EffectKind::ModifyExtension => "change extensions",
        EffectKind::ModifyApproval => "change approvals",
        EffectKind::ModifyBudget => "change budgets",
        EffectKind::ExternalWrite => "change data in outside services",
        EffectKind::Financial => "spend money",
    }
}

fn url_host(raw: &str) -> Option<String> {
    url::Url::parse(raw)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
}

/// Consent state of one package against its durable grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionConsentStatus {
    /// Host-bundled or user-registered: no grant needed.
    Exempt,
    /// A grant exists for the current digest.
    Granted,
    /// No grant exists.
    Required,
    /// A grant exists, for a different digest: the files changed.
    Tampered,
}

impl ExtensionConsentStatus {
    /// The lifecycle phase that blocks activation, if any.
    pub fn blocking_phase(self) -> Option<InstallationState> {
        match self {
            Self::Exempt | Self::Granted => None,
            Self::Required => Some(InstallationState::ConsentRequired),
            Self::Tampered => Some(InstallationState::Tampered),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ExtensionConsentRecord {
    schema: u32,
    extension_id: String,
    digest: PackageConsentDigest,
    /// Package-relative path -> `sha256:<hex>` of every consented file: what
    /// the runtime lanes verify the bytes they execute against.
    assets: std::collections::BTreeMap<String, String>,
    granted_by: String,
    granted_at: chrono::DateTime<chrono::Utc>,
}

/// Durable consent grants, one record per extension id.
#[derive(Clone)]
pub struct ExtensionConsentStore {
    filesystem: Arc<dyn RootFilesystem>,
}

impl std::fmt::Debug for ExtensionConsentStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtensionConsentStore")
            .finish_non_exhaustive()
    }
}

impl ExtensionConsentStore {
    pub fn new(filesystem: Arc<dyn RootFilesystem>) -> Self {
        Self { filesystem }
    }

    fn record_path(extension_id: &ExtensionId) -> Result<VirtualPath, ProductOperationFailure> {
        VirtualPath::new(format!(
            "{CONSENT_RECORD_ROOT}/{}.json",
            extension_id.as_str()
        ))
        .map_err(|error| ProductOperationFailure::InvalidBindingRequest {
            reason: format!("consent record path is invalid: {error}"),
        })
    }

    async fn read(
        &self,
        extension_id: &ExtensionId,
    ) -> Result<Option<ExtensionConsentRecord>, ProductOperationFailure> {
        let path = Self::record_path(extension_id)?;
        let bytes = match self.filesystem.read_file(&path).await {
            Ok(bytes) => bytes,
            Err(FilesystemError::NotFound { .. }) | Err(FilesystemError::MountNotFound { .. }) => {
                return Ok(None);
            }
            Err(error) => {
                return Err(ProductOperationFailure::Transient {
                    reason: format!("failed to read extension consent record: {error}"),
                });
            }
        };
        // A record from an older schema (no per-file hashes) cannot bind
        // execution to the consented bytes: treat it as no grant, so the user
        // is asked again.
        let schema = serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|value| value.get("schema").and_then(serde_json::Value::as_u64));
        if schema != Some(u64::from(CONSENT_RECORD_SCHEMA)) {
            tracing::debug!(
                extension_id = extension_id.as_str(),
                ?schema,
                "ignoring an extension consent record of another schema"
            );
            return Ok(None);
        }
        let record: ExtensionConsentRecord = serde_json::from_slice(&bytes).map_err(|error| {
            ProductOperationFailure::InvalidBindingRequest {
                reason: format!("extension consent record is malformed: {error}"),
            }
        })?;
        if record.extension_id != extension_id.as_str() {
            return Err(ProductOperationFailure::InvalidBindingRequest {
                reason: "extension consent record names a different extension".to_string(),
            });
        }
        Ok(Some(record))
    }

    /// The grant's digest, if one is recorded.
    pub async fn granted_digest(
        &self,
        extension_id: &ExtensionId,
    ) -> Result<Option<PackageConsentDigest>, ProductOperationFailure> {
        Ok(self.read(extension_id).await?.map(|record| record.digest))
    }

    /// The consented per-file hashes, if a grant is recorded.
    pub async fn granted_pins(
        &self,
        extension_id: &ExtensionId,
    ) -> Result<Option<PackageContentPins>, ProductOperationFailure> {
        Ok(self
            .read(extension_id)
            .await?
            .map(|record| PackageContentPins::new(record.assets)))
    }

    /// The whole grant (digest and per-file hashes), for restoring it after
    /// a failed replacement.
    pub(crate) async fn granted(
        &self,
        extension_id: &ExtensionId,
    ) -> Result<Option<(PackageConsentDigest, PackageContentPins)>, ProductOperationFailure> {
        Ok(self
            .read(extension_id)
            .await?
            .map(|record| (record.digest, PackageContentPins::new(record.assets))))
    }

    /// Persist a grant for `digest` covering exactly the files hashed in
    /// `pins`, then read it back: success is reported only when the durable
    /// record names exactly this digest.
    pub async fn grant(
        &self,
        extension_id: &ExtensionId,
        digest: &PackageConsentDigest,
        pins: &PackageContentPins,
        granted_by: &UserId,
    ) -> Result<(), ProductOperationFailure> {
        let record = ExtensionConsentRecord {
            schema: CONSENT_RECORD_SCHEMA,
            extension_id: extension_id.as_str().to_string(),
            digest: digest.clone(),
            assets: pins.assets().clone(),
            granted_by: granted_by.as_str().to_string(),
            granted_at: chrono::Utc::now(),
        };
        let bytes =
            serde_json::to_vec(&record).map_err(|error| ProductOperationFailure::Transient {
                reason: format!("failed to encode extension consent record: {error}"),
            })?;
        let path = Self::record_path(extension_id)?;
        self.filesystem
            .write_file(&path, &bytes)
            .await
            .map_err(|error| ProductOperationFailure::Transient {
                reason: format!("failed to write extension consent record: {error}"),
            })?;
        match self.granted_digest(extension_id).await? {
            Some(stored) if &stored == digest => Ok(()),
            _ => Err(ProductOperationFailure::Transient {
                reason: "extension consent record did not read back after the grant".to_string(),
            }),
        }
    }

    /// Drop the grant (on removal). Absent records are fine.
    pub async fn revoke(&self, extension_id: &ExtensionId) -> Result<(), ProductOperationFailure> {
        let path = Self::record_path(extension_id)?;
        match self.filesystem.delete(&path).await {
            Ok(()) | Err(FilesystemError::NotFound { .. }) => Ok(()),
            Err(FilesystemError::MountNotFound { .. }) => Ok(()),
            Err(error) => Err(ProductOperationFailure::Transient {
                reason: format!("failed to delete extension consent record: {error}"),
            }),
        }
    }

    /// Evaluate `package` against its grant, hashing the files as they are
    /// now (see [`current_package_files`]).
    pub async fn status(
        &self,
        package: &AvailableExtensionPackage,
    ) -> Result<ExtensionConsentStatus, ProductOperationFailure> {
        Ok(self.evaluate(package).await?.0)
    }

    /// Status plus the digest of the files it was evaluated against
    /// (`None` when the package is exempt).
    pub async fn evaluate(
        &self,
        package: &AvailableExtensionPackage,
    ) -> Result<(ExtensionConsentStatus, Option<PackageConsentDigest>), ProductOperationFailure>
    {
        if !source_requires_consent(package.source) {
            return Ok((ExtensionConsentStatus::Exempt, None));
        }
        let files = current_package_files(self.filesystem.as_ref(), package).await?;
        let current = package_files_digest(&package.package.id, &files)?;
        let status = match self.granted_digest(&package.package.id).await? {
            None => ExtensionConsentStatus::Required,
            Some(granted) if granted == current => ExtensionConsentStatus::Granted,
            Some(_) => ExtensionConsentStatus::Tampered,
        };
        Ok((status, Some(current)))
    }

    pub(crate) fn filesystem(&self) -> &dyn RootFilesystem {
        self.filesystem.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_detail_reads_as_one_sentence() {
        assert_eq!(
            tool_detail(
                "Echo text back.",
                &[EffectKind::Network, EffectKind::ExternalWrite],
                PermissionMode::Ask,
            ),
            "Echo text back; it can use the network, change data in outside services and asks you before each use."
        );
        assert_eq!(
            tool_detail("", &[], PermissionMode::Allow),
            "This tool has no declared side effects and runs without asking."
        );
    }

    #[test]
    fn only_third_party_sources_need_consent() {
        assert!(source_requires_consent(ManifestSource::InstalledLocal));
        assert!(source_requires_consent(ManifestSource::RegistryInstalled));
        assert!(!source_requires_consent(ManifestSource::HostBundled));
        assert!(!source_requires_consent(ManifestSource::UserRegistered));
    }
}
