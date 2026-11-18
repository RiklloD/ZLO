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
//! The decision is per principal ([`ConsentPrincipal`]); the package
//! definition and its execution pins are one per extension id. A
//! member-held installation runs, for each member, only on that member's own
//! grant; a tenant-owned one only on the tenant admin's. Every surface that
//! already narrows an active extension to the users its installation is
//! visible to (tool catalog, live hooks, UI slots, settings) narrows it by
//! consent too, through [`ExtensionConsentStore::consented_owner`].
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
    CapabilityVisibility, ExtensionRuntimeV2, InstallationOwner, ManifestSource, PackageContentPins,
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
/// v3 (2026-11-18): per-principal decisions plus one pins record per id.
/// Records of any other schema read as no grant.
const CONSENT_RECORD_SCHEMA: u32 = 3;

/// Whether a package from `source` must carry a consent grant to activate.
pub fn source_requires_consent(source: ManifestSource) -> bool {
    source.requires_consent()
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
    for slot in &resolved.ui_slots {
        requests.push(LifecycleConsentRequest {
            kind: LifecycleConsentRequestKind::UiSlot,
            title: slot.title.clone(),
            detail: format!(
                "Shows its own {} panel in ZLO, drawn by its {} tool.",
                slot.slot.as_str(),
                slot.render
            ),
        });
    }
    for setting in &resolved.settings {
        requests.push(LifecycleConsentRequest {
            kind: LifecycleConsentRequestKind::Setting,
            title: setting.label.clone(),
            detail: format!(
                "Adds the setting \"{}\"; its hooks and tools read the value you choose.",
                setting.id
            ),
        });
    }
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

/// Consent state of one package for one principal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtensionConsentStatus {
    /// Host-bundled or user-registered: no grant needed.
    Exempt,
    /// The principal granted consent for the current files, and the
    /// package's execution pins are those files.
    Granted,
    /// The principal has not granted consent.
    Required,
    /// The principal granted consent for other files: they changed.
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

/// One consent evaluation: the status plus the digest of the files it was
/// evaluated against (`None` when the package is exempt). Computed once per
/// package per operation and reused for every decision that operation makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsentEvaluation {
    pub status: ExtensionConsentStatus,
    pub digest: Option<PackageConsentDigest>,
}

impl ConsentEvaluation {
    pub const EXEMPT: Self = Self {
        status: ExtensionConsentStatus::Exempt,
        digest: None,
    };
}

/// Whose decision a consent grant is.
///
/// The package definition and its execution pins are one per extension id
/// (there is one set of files on disk); the *decision* to let it run is per
/// principal. A tenant-owned (shared) installation runs on the tenant admin's
/// decision; a member-held installation runs, for each member, on that
/// member's own decision — one user's consent never authorizes the package
/// for another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsentPrincipal {
    TenantAdmin,
    User(UserId),
}

impl ConsentPrincipal {
    /// The principal whose decision governs `caller`'s use of an
    /// installation held by `owner` (`None`: not installed yet, so a fresh
    /// install — always caller-private).
    pub fn for_caller(owner: Option<&InstallationOwner>, caller: &UserId) -> Self {
        match owner {
            Some(InstallationOwner::Tenant) => Self::TenantAdmin,
            _ => Self::User(caller.clone()),
        }
    }

    /// File-name key: never the raw user id (ids are not path-safe by
    /// contract), a hash of it.
    fn record_key(&self) -> String {
        match self {
            Self::TenantAdmin => "tenant-admin".to_string(),
            Self::User(user) => {
                let mut hex = String::with_capacity(64);
                for byte in Sha256::digest(user.as_str().as_bytes()).iter() {
                    // Writing to a String cannot fail.
                    let _written = write!(hex, "{byte:02x}");
                }
                format!("user-{hex}")
            }
        }
    }

    fn wire(&self) -> ConsentPrincipalWire {
        match self {
            Self::TenantAdmin => ConsentPrincipalWire::TenantAdmin,
            Self::User(user) => ConsentPrincipalWire::User {
                user_id: user.as_str().to_string(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ConsentPrincipalWire {
    TenantAdmin,
    User { user_id: String },
}

/// The package's execution pins: the files the latest authorized grant
/// covered. One per extension id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PackagePinsRecord {
    schema: u32,
    extension_id: String,
    digest: PackageConsentDigest,
    /// Package-relative path -> `sha256:<hex>` of every consented file: what
    /// the runtime lanes verify the bytes they execute against.
    assets: std::collections::BTreeMap<String, String>,
}

/// One principal's decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ConsentDecisionRecord {
    schema: u32,
    extension_id: String,
    principal: ConsentPrincipalWire,
    digest: PackageConsentDigest,
    granted_by: String,
    granted_at: chrono::DateTime<chrono::Utc>,
}

/// The raw durable consent records of one extension, for restoring them
/// after a failed replacement.
#[derive(Debug, Clone, Default)]
pub(crate) struct ConsentSnapshot {
    files: Vec<(VirtualPath, Vec<u8>)>,
}

/// Durable consent: one pins record per extension id
/// (`<root>/<id>.json`) and one decision record per principal
/// (`<root>/grants/<id>/<principal-key>.json`).
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

fn consent_path(raw: String) -> Result<VirtualPath, ProductOperationFailure> {
    VirtualPath::new(raw).map_err(|error| ProductOperationFailure::InvalidBindingRequest {
        reason: format!("consent record path is invalid: {error}"),
    })
}

impl ExtensionConsentStore {
    pub fn new(filesystem: Arc<dyn RootFilesystem>) -> Self {
        Self { filesystem }
    }

    fn pins_path(extension_id: &ExtensionId) -> Result<VirtualPath, ProductOperationFailure> {
        consent_path(format!(
            "{CONSENT_RECORD_ROOT}/{}.json",
            extension_id.as_str()
        ))
    }

    fn decisions_dir(extension_id: &ExtensionId) -> Result<VirtualPath, ProductOperationFailure> {
        consent_path(format!(
            "{CONSENT_RECORD_ROOT}/grants/{}",
            extension_id.as_str()
        ))
    }

    fn decision_path(
        extension_id: &ExtensionId,
        principal: &ConsentPrincipal,
    ) -> Result<VirtualPath, ProductOperationFailure> {
        consent_path(format!(
            "{CONSENT_RECORD_ROOT}/grants/{}/{}.json",
            extension_id.as_str(),
            principal.record_key()
        ))
    }

    async fn read_raw(
        &self,
        path: &VirtualPath,
    ) -> Result<Option<Vec<u8>>, ProductOperationFailure> {
        match self.filesystem.read_file(path).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(FilesystemError::NotFound { .. }) | Err(FilesystemError::MountNotFound { .. }) => {
                Ok(None)
            }
            Err(error) => Err(ProductOperationFailure::Transient {
                reason: format!("failed to read extension consent record: {error}"),
            }),
        }
    }

    /// Read one record of the current schema. A record of another schema
    /// (the pre-principal v2 grant keyed by extension id only, or anything
    /// older) cannot say whose decision it is: it reads as no grant, so the
    /// user is asked again (fail closed).
    async fn read_record<T: serde::de::DeserializeOwned>(
        &self,
        path: &VirtualPath,
        extension_id: &ExtensionId,
    ) -> Result<Option<T>, ProductOperationFailure> {
        let Some(bytes) = self.read_raw(path).await? else {
            return Ok(None);
        };
        let value = serde_json::from_slice::<serde_json::Value>(&bytes).ok();
        let schema = value
            .as_ref()
            .and_then(|value| value.get("schema").and_then(serde_json::Value::as_u64));
        let record_id = value.as_ref().and_then(|value| {
            value
                .get("extension_id")
                .and_then(serde_json::Value::as_str)
        });
        if schema != Some(u64::from(CONSENT_RECORD_SCHEMA)) {
            tracing::debug!(
                extension_id = extension_id.as_str(),
                ?schema,
                "ignoring an extension consent record of another schema"
            );
            return Ok(None);
        }
        if record_id != Some(extension_id.as_str()) {
            return Err(ProductOperationFailure::InvalidBindingRequest {
                reason: "extension consent record names a different extension".to_string(),
            });
        }
        serde_json::from_slice(&bytes).map(Some).map_err(|error| {
            ProductOperationFailure::InvalidBindingRequest {
                reason: format!("extension consent record is malformed: {error}"),
            }
        })
    }

    async fn write_record<T: Serialize>(
        &self,
        path: &VirtualPath,
        record: &T,
    ) -> Result<(), ProductOperationFailure> {
        let bytes =
            serde_json::to_vec(record).map_err(|error| ProductOperationFailure::Transient {
                reason: format!("failed to encode extension consent record: {error}"),
            })?;
        self.filesystem
            .write_file(path, &bytes)
            .await
            .map_err(|error| ProductOperationFailure::Transient {
                reason: format!("failed to write extension consent record: {error}"),
            })
    }

    async fn delete_record(&self, path: &VirtualPath) -> Result<(), ProductOperationFailure> {
        match self.filesystem.delete(path).await {
            Ok(())
            | Err(FilesystemError::NotFound { .. })
            | Err(FilesystemError::MountNotFound { .. }) => Ok(()),
            Err(error) => Err(ProductOperationFailure::Transient {
                reason: format!("failed to delete extension consent record: {error}"),
            }),
        }
    }

    async fn package_pins_record(
        &self,
        extension_id: &ExtensionId,
    ) -> Result<Option<PackagePinsRecord>, ProductOperationFailure> {
        self.read_record(&Self::pins_path(extension_id)?, extension_id)
            .await
    }

    /// The package's execution pins (digest and per-file hashes), if a grant
    /// established them.
    pub async fn granted_package(
        &self,
        extension_id: &ExtensionId,
    ) -> Result<Option<(PackageConsentDigest, PackageContentPins)>, ProductOperationFailure> {
        Ok(self
            .package_pins_record(extension_id)
            .await?
            .map(|record| (record.digest, PackageContentPins::new(record.assets))))
    }

    /// The package's execution pins, for the loaders and publication.
    pub async fn granted_pins(
        &self,
        extension_id: &ExtensionId,
    ) -> Result<Option<PackageContentPins>, ProductOperationFailure> {
        Ok(self
            .granted_package(extension_id)
            .await?
            .map(|(_, pins)| pins))
    }

    /// The digest `principal` consented to, if any.
    pub async fn decision(
        &self,
        extension_id: &ExtensionId,
        principal: &ConsentPrincipal,
    ) -> Result<Option<PackageConsentDigest>, ProductOperationFailure> {
        let record: Option<ConsentDecisionRecord> = self
            .read_record(&Self::decision_path(extension_id, principal)?, extension_id)
            .await?;
        Ok(record
            .filter(|record| record.principal == principal.wire())
            .map(|record| record.digest))
    }

    /// Persist `principal`'s grant for `digest`, making `pins` (the files
    /// that digest covers) the package's execution pins, then read both back:
    /// success is reported only when the durable records name exactly this
    /// digest. Callers authorize the principal first.
    pub async fn grant(
        &self,
        extension_id: &ExtensionId,
        principal: &ConsentPrincipal,
        digest: &PackageConsentDigest,
        pins: &PackageContentPins,
        granted_by: &UserId,
    ) -> Result<(), ProductOperationFailure> {
        let pins_record = PackagePinsRecord {
            schema: CONSENT_RECORD_SCHEMA,
            extension_id: extension_id.as_str().to_string(),
            digest: digest.clone(),
            assets: pins.assets().clone(),
        };
        let pins_path = Self::pins_path(extension_id)?;
        if self.package_pins_record(extension_id).await?.as_ref() != Some(&pins_record) {
            self.write_record(&pins_path, &pins_record).await?;
        }
        let decision = ConsentDecisionRecord {
            schema: CONSENT_RECORD_SCHEMA,
            extension_id: extension_id.as_str().to_string(),
            principal: principal.wire(),
            digest: digest.clone(),
            granted_by: granted_by.as_str().to_string(),
            granted_at: chrono::Utc::now(),
        };
        self.write_record(&Self::decision_path(extension_id, principal)?, &decision)
            .await?;
        let pins_back = self.granted_package(extension_id).await?;
        let decision_back = self.decision(extension_id, principal).await?;
        match (pins_back, decision_back) {
            (Some((pinned, _)), Some(decided)) if &pinned == digest && &decided == digest => Ok(()),
            _ => Err(ProductOperationFailure::Transient {
                reason: "extension consent record did not read back after the grant".to_string(),
            }),
        }
    }

    /// Drop one principal's decision (a member leaving a shared
    /// installation). Absent records are fine.
    pub async fn revoke_principal(
        &self,
        extension_id: &ExtensionId,
        principal: &ConsentPrincipal,
    ) -> Result<(), ProductOperationFailure> {
        self.delete_record(&Self::decision_path(extension_id, principal)?)
            .await
    }

    async fn decision_paths(
        &self,
        extension_id: &ExtensionId,
    ) -> Result<Vec<VirtualPath>, ProductOperationFailure> {
        match self
            .filesystem
            .list_dir(&Self::decisions_dir(extension_id)?)
            .await
        {
            Ok(entries) => Ok(entries.into_iter().map(|entry| entry.path).collect()),
            Err(FilesystemError::NotFound { .. }) | Err(FilesystemError::MountNotFound { .. }) => {
                Ok(Vec::new())
            }
            Err(error) => Err(ProductOperationFailure::Transient {
                reason: format!("failed to list extension consent decisions: {error}"),
            }),
        }
    }

    /// Drop the package pins and every decision (on final removal), so a
    /// later reinstall asks everyone again. Absent records are fine.
    pub async fn revoke(&self, extension_id: &ExtensionId) -> Result<(), ProductOperationFailure> {
        for path in self.decision_paths(extension_id).await? {
            self.delete_record(&path).await?;
        }
        self.delete_record(&Self::pins_path(extension_id)?).await
    }

    /// The raw durable records of one extension.
    pub(crate) async fn snapshot(
        &self,
        extension_id: &ExtensionId,
    ) -> Result<ConsentSnapshot, ProductOperationFailure> {
        let mut paths = self.decision_paths(extension_id).await?;
        paths.push(Self::pins_path(extension_id)?);
        let mut files = Vec::new();
        for path in paths {
            if let Some(bytes) = self.read_raw(&path).await? {
                files.push((path, bytes));
            }
        }
        Ok(ConsentSnapshot { files })
    }

    /// Write a snapshot back verbatim.
    pub(crate) async fn restore(
        &self,
        snapshot: &ConsentSnapshot,
    ) -> Result<(), ProductOperationFailure> {
        for (path, bytes) in &snapshot.files {
            self.filesystem
                .write_file(path, bytes)
                .await
                .map_err(|error| ProductOperationFailure::Transient {
                    reason: format!("failed to restore extension consent record: {error}"),
                })?;
        }
        Ok(())
    }

    /// Evaluate `package` for `principal`, hashing the files as they are
    /// now (see [`current_package_files`]) exactly once.
    pub async fn evaluate(
        &self,
        package: &AvailableExtensionPackage,
        principal: &ConsentPrincipal,
    ) -> Result<ConsentEvaluation, ProductOperationFailure> {
        if !source_requires_consent(package.source) {
            return Ok(ConsentEvaluation::EXEMPT);
        }
        let files = current_package_files(self.filesystem.as_ref(), package).await?;
        let current = package_files_digest(&package.package.id, &files)?;
        let pinned = self
            .granted_package(&package.package.id)
            .await?
            .map(|(digest, _)| digest);
        let decision = self.decision(&package.package.id, principal).await?;
        Ok(ConsentEvaluation {
            status: status_for(&current, pinned.as_ref(), decision.as_ref()),
            digest: Some(current),
        })
    }

    /// Evaluate `package` for an installation as a whole — boot hydration,
    /// which publishes for every member at once: it may run when its files
    /// are the pinned ones and at least one principal that governs `owner`
    /// consented to them. Per-user use is then narrowed by
    /// [`Self::consented_owner`].
    pub async fn evaluate_for_owner(
        &self,
        package: &AvailableExtensionPackage,
        owner: &InstallationOwner,
    ) -> Result<ConsentEvaluation, ProductOperationFailure> {
        if !source_requires_consent(package.source) {
            return Ok(ConsentEvaluation::EXEMPT);
        }
        let files = current_package_files(self.filesystem.as_ref(), package).await?;
        let current = package_files_digest(&package.package.id, &files)?;
        let pinned = self
            .granted_package(&package.package.id)
            .await?
            .map(|(digest, _)| digest);
        let mut status = ExtensionConsentStatus::Required;
        for principal in owner_principals(owner) {
            let decision = self.decision(&package.package.id, &principal).await?;
            match status_for(&current, pinned.as_ref(), decision.as_ref()) {
                ExtensionConsentStatus::Granted => {
                    status = ExtensionConsentStatus::Granted;
                    break;
                }
                ExtensionConsentStatus::Tampered => status = ExtensionConsentStatus::Tampered,
                _ => {}
            }
        }
        Ok(ConsentEvaluation {
            status,
            digest: Some(current),
        })
    }

    /// `owner` narrowed to the principals whose decision matches the
    /// package's execution pins: the tenant row survives only on the tenant
    /// admin's grant, a member set keeps only the members who consented
    /// themselves. `None` when nobody remains. Reads small records only (no
    /// file hashing): what executes is bound to the pins by the lanes, which
    /// verify every byte they read against them.
    pub async fn consented_owner(
        &self,
        extension_id: &ExtensionId,
        owner: &InstallationOwner,
    ) -> Result<Option<InstallationOwner>, ProductOperationFailure> {
        let Some((pinned, _)) = self.granted_package(extension_id).await? else {
            return Ok(None);
        };
        match owner {
            InstallationOwner::Tenant => Ok(self
                .decision(extension_id, &ConsentPrincipal::TenantAdmin)
                .await?
                .filter(|decided| decided == &pinned)
                .map(|_| InstallationOwner::Tenant)),
            InstallationOwner::Users { user_ids } => {
                let mut consented = BTreeSet::new();
                for user in user_ids {
                    let principal = ConsentPrincipal::User(user.clone());
                    if self.decision(extension_id, &principal).await?.as_ref() == Some(&pinned) {
                        consented.insert(user.clone());
                    }
                }
                Ok(InstallationOwner::users(consented).ok())
            }
        }
    }

    pub(crate) fn filesystem(&self) -> &dyn RootFilesystem {
        self.filesystem.as_ref()
    }
}

fn owner_principals(owner: &InstallationOwner) -> Vec<ConsentPrincipal> {
    match owner {
        InstallationOwner::Tenant => vec![ConsentPrincipal::TenantAdmin],
        InstallationOwner::Users { user_ids } => user_ids
            .iter()
            .cloned()
            .map(ConsentPrincipal::User)
            .collect(),
    }
}

/// A decision counts only when it covers the current files *and* those are
/// the package's execution pins (what the lanes verify against).
fn status_for(
    current: &PackageConsentDigest,
    pinned: Option<&PackageConsentDigest>,
    decision: Option<&PackageConsentDigest>,
) -> ExtensionConsentStatus {
    match (decision, pinned) {
        (None, _) | (Some(_), None) => ExtensionConsentStatus::Required,
        (Some(decided), Some(pinned)) if decided == current && pinned == current => {
            ExtensionConsentStatus::Granted
        }
        (Some(_), Some(_)) => ExtensionConsentStatus::Tampered,
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
