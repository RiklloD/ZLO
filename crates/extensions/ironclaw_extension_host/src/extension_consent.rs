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
use ironclaw_extension_registry::{CapabilityVisibility, ExtensionRuntimeV2, ManifestSource};
use ironclaw_filesystem::{FilesystemError, RootFilesystem};
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
const CONSENT_RECORD_SCHEMA: u32 = 1;

/// Whether a package from `source` must carry a consent grant to activate.
pub fn source_requires_consent(source: ManifestSource) -> bool {
    match source {
        ManifestSource::InstalledLocal | ManifestSource::RegistryInstalled => true,
        ManifestSource::HostBundled | ManifestSource::UserRegistered => false,
    }
}

/// SHA-256 over every file of the package, in path order, each framed by its
/// length so no two file sets can collide by concatenation. The manifest is
/// always included, even when the asset list does not carry it.
pub fn package_consent_digest(
    package: &AvailableExtensionPackage,
) -> Result<PackageConsentDigest, ProductOperationFailure> {
    let mut files: Vec<(&str, &[u8])> = package
        .assets
        .iter()
        .map(|asset| {
            let AvailableExtensionAssetContent::Bytes(bytes) = &asset.content;
            (asset.path.as_str(), bytes.as_ref())
        })
        .collect();
    if !files.iter().any(|(path, _)| *path == "manifest.toml") {
        files.push(("manifest.toml", package.manifest_toml.as_bytes()));
    }
    files.sort_by(|left, right| left.0.cmp(right.0));
    let mut hasher = Sha256::new();
    hash_part(&mut hasher, CONSENT_DIGEST_DOMAIN);
    hash_part(&mut hasher, package.package.id.as_str().as_bytes());
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

fn hash_part(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

/// Everything the package asks for, derived from the resolved manifest.
pub fn consent_summary(
    package: &AvailableExtensionPackage,
) -> Result<LifecycleConsentSummary, ProductOperationFailure> {
    Ok(LifecycleConsentSummary {
        digest: package_consent_digest(package)?,
        requests: consent_requests(package),
    })
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

    /// Persist a grant for `digest`, then read it back: success is reported
    /// only when the durable record names exactly this digest.
    pub async fn grant(
        &self,
        extension_id: &ExtensionId,
        digest: &PackageConsentDigest,
        granted_by: &UserId,
    ) -> Result<(), ProductOperationFailure> {
        let record = ExtensionConsentRecord {
            schema: CONSENT_RECORD_SCHEMA,
            extension_id: extension_id.as_str().to_string(),
            digest: digest.clone(),
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

    /// Evaluate `package` against its grant.
    pub async fn status(
        &self,
        package: &AvailableExtensionPackage,
    ) -> Result<ExtensionConsentStatus, ProductOperationFailure> {
        if !source_requires_consent(package.source) {
            return Ok(ExtensionConsentStatus::Exempt);
        }
        let current = package_consent_digest(package)?;
        Ok(match self.granted_digest(&package.package.id).await? {
            None => ExtensionConsentStatus::Required,
            Some(granted) if granted == current => ExtensionConsentStatus::Granted,
            Some(_) => ExtensionConsentStatus::Tampered,
        })
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
