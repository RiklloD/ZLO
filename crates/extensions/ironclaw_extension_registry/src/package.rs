use ironclaw_host_api::{
    approval::sha256_digest_token,
    capability::CapabilityDescriptor,
    ids::{ExtensionId, PackageId},
    path::VirtualPath,
    runtime::RuntimeKind,
    trust::{PackageIdentity, PackageSource, TrustPolicyInput},
};
use std::collections::{BTreeMap, BTreeSet, HashSet};

use crate::{
    CapabilityDeclV2, CapabilityManifest, ExtensionError, ExtensionManifest, ExtensionRuntime,
    ManifestSource, PackageRootBinding, PackageRootError,
};

/// Validated package rooted under `/system/extensions/<extension>`.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtensionPackage {
    pub id: ExtensionId,
    pub root_binding: PackageRootBinding,
    pub manifest: ExtensionManifest,
    pub capabilities: Vec<CapabilityDescriptor>,
    pub manifest_digest: Option<String>,
    pub descriptor_schema_mode: CapabilityDescriptorSchemaMode,
    hosted_mcp_tool_names: BTreeMap<ironclaw_host_api::ids::CapabilityId, String>,
    /// Hashes of the exact package files a consent grant covers. `None` for
    /// packages that need no consent (host-bundled, user-registered); `Some`
    /// binds every runtime read of a package file to the consented bytes
    /// (see [`Self::verify_asset`]).
    pub content_pins: Option<PackageContentPins>,
}

/// Consented per-file hashes of one package, keyed by package-relative path,
/// as `sha256:<hex>` tokens ([`sha256_digest_token`]). Carried down to the
/// runtime lanes on the package itself so a lane can refuse bytes that are
/// not the ones the user consented to, without depending on the consent
/// store that produced them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PackageContentPins {
    assets: BTreeMap<String, String>,
    /// The consent digest of the grant these pins came from (the
    /// `PackageConsentDigest` the per-principal decisions name), carried
    /// opaquely so every binding knows which consented generation it runs:
    /// dispatch admits a principal only when their own decision names
    /// exactly this digest. `None` for pins built without a grant (fixtures),
    /// which no principal's decision matches.
    consent_digest: Option<String>,
    binding: PinBinding,
}

/// What a pins value binds. A non-test build has exactly one variant, so no
/// production code can construct pins that skip verification.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum PinBinding {
    /// Every runtime read must match `assets`.
    #[default]
    Hashes,
    /// The explicit unverified test construction
    /// ([`crate::test_support::UnverifiedContentsForTest`]): binds nothing.
    #[cfg(any(test, feature = "test-support"))]
    UnverifiedForTest,
}

impl PackageContentPins {
    pub fn new(assets: BTreeMap<String, String>) -> Self {
        Self {
            assets,
            consent_digest: None,
            binding: PinBinding::Hashes,
        }
    }

    /// Stamp these pins with the consent digest of the grant they came from.
    pub fn with_consent_digest(mut self, digest: impl Into<String>) -> Self {
        self.consent_digest = Some(digest.into());
        self
    }

    /// The consent digest these pins were granted under, if any.
    pub fn consent_digest(&self) -> Option<&str> {
        self.consent_digest.as_deref()
    }

    /// True only for the explicit unverified test construction; always false
    /// in a build without the `test-support` feature.
    fn is_unverified_for_test(&self) -> bool {
        match self.binding {
            PinBinding::Hashes => false,
            #[cfg(any(test, feature = "test-support"))]
            PinBinding::UnverifiedForTest => true,
        }
    }

    /// Pins for an explicit file set (package-relative path -> bytes), hashed
    /// the way the consent grant hashes them. For callers that hold the exact
    /// files they bind a package to — the consent store and test fixtures.
    pub fn from_files<'a>(files: impl IntoIterator<Item = (&'a str, &'a [u8])>) -> Self {
        Self::new(
            files
                .into_iter()
                .map(|(path, bytes)| (path.to_string(), sha256_digest_token(bytes)))
                .collect(),
        )
    }

    pub fn assets(&self) -> &BTreeMap<String, String> {
        &self.assets
    }
}

/// Pins that bind nothing: the unverified test construction only
/// ([`crate::test_support::UnverifiedContentsForTest`]).
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn unverified_test_pins() -> PackageContentPins {
    PackageContentPins {
        binding: PinBinding::UnverifiedForTest,
        ..PackageContentPins::default()
    }
}

/// Verify `bytes` read from `path` against `pins` for a package rooted at
/// `root` from `source` — the one rule every runtime read of a package file
/// follows (see [`ExtensionPackage::verify_asset`]). For callers that hold a
/// package's identity and pins without the whole package (the hook-only
/// projection).
pub fn verify_package_file(
    extension_id: &ExtensionId,
    source: ManifestSource,
    pins: Option<&PackageContentPins>,
    root: &VirtualPath,
    path: &VirtualPath,
    bytes: &[u8],
) -> Result<(), PackageContentPinError> {
    let Some(expected) = required_pin(extension_id, source, pins, root, path)? else {
        return Ok(());
    };
    if expected != sha256_digest_token(bytes) {
        return Err(PackageContentPinError::Mismatch {
            path: relative_to_root(root, path)
                .unwrap_or(path.as_str())
                .to_string(),
        });
    }
    Ok(())
}

fn required_pin<'a>(
    extension_id: &ExtensionId,
    source: ManifestSource,
    pins: Option<&'a PackageContentPins>,
    root: &VirtualPath,
    path: &VirtualPath,
) -> Result<Option<&'a str>, PackageContentPinError> {
    let Some(pins) = pins else {
        if source.requires_consent() {
            return Err(PackageContentPinError::NoGrant {
                extension_id: extension_id.as_str().to_string(),
            });
        }
        return Ok(None);
    };
    if pins.is_unverified_for_test() {
        return Ok(None);
    }
    let relative =
        relative_to_root(root, path).ok_or_else(|| PackageContentPinError::Unpinned {
            path: path.as_str().to_string(),
        })?;
    pins.assets
        .get(relative)
        .map(|hash| Some(hash.as_str()))
        .ok_or_else(|| PackageContentPinError::Unpinned {
            path: relative.to_string(),
        })
}

fn relative_to_root<'a>(root: &VirtualPath, path: &'a VirtualPath) -> Option<&'a str> {
    path.as_str()
        .strip_prefix(root.as_str().trim_end_matches('/'))?
        .strip_prefix('/')
}

/// A package file read at runtime is not the consented one.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PackageContentPinError {
    #[error("extension {extension_id} has no consent grant binding the files it runs")]
    NoGrant { extension_id: String },
    #[error("package file {path} is not covered by the extension's consent grant")]
    Unpinned { path: String },
    #[error("package file {path} changed after consent was granted")]
    Mismatch { path: String },
}

/// How package capability descriptor schemas are derived from the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityDescriptorSchemaMode {
    /// Descriptors must carry the manifest's `$ref` schema projection.
    ManifestRefs,
    /// Descriptors may carry inline schemas, but all non-schema fields must
    /// still match the manifest projection exactly.
    InlineDynamic,
}

impl ExtensionPackage {
    pub fn from_manifest(
        manifest: ExtensionManifest,
        root: VirtualPath,
    ) -> Result<Self, ExtensionError> {
        Self::from_manifest_with_digest(manifest, root, None)
    }

    pub fn from_manifest_toml(
        manifest: ExtensionManifest,
        root: VirtualPath,
        manifest_toml: &str,
    ) -> Result<Self, ExtensionError> {
        Self::from_manifest_with_digest(
            manifest,
            root,
            Some(sha256_digest_token(manifest_toml.as_bytes())),
        )
    }

    pub fn from_manifest_with_digest(
        manifest: ExtensionManifest,
        root: VirtualPath,
        manifest_digest: Option<String>,
    ) -> Result<Self, ExtensionError> {
        ensure_extension_root_matches(&manifest.id, &root)?;
        Self::from_manifest_with_binding(
            manifest,
            PackageRootBinding::Materialized(root),
            manifest_digest,
        )
    }

    /// Construct a remote-only user-registered package. Such a package has no
    /// filesystem tree and may never be routed through asset or schema I/O.
    pub fn from_virtual_manifest(
        manifest: ExtensionManifest,
        manifest_digest: Option<String>,
        capabilities: Vec<CapabilityDescriptor>,
    ) -> Result<Self, ExtensionError> {
        ensure_virtual_manifest(&manifest)?;
        let expected = capability_descriptors_from_manifest(&manifest)?;
        if !descriptors_match_except_schema(&capabilities, &expected) {
            return Err(ExtensionError::InvalidManifest {
                reason: "inline virtual capability descriptors do not match manifest declarations"
                    .to_string(),
            });
        }
        Ok(Self {
            id: manifest.id.clone(),
            root_binding: PackageRootBinding::Virtual,
            manifest,
            capabilities,
            manifest_digest,
            descriptor_schema_mode: CapabilityDescriptorSchemaMode::InlineDynamic,
            content_pins: None,
            hosted_mcp_tool_names: BTreeMap::new(),
        })
    }

    fn from_manifest_with_binding(
        manifest: ExtensionManifest,
        root_binding: PackageRootBinding,
        manifest_digest: Option<String>,
    ) -> Result<Self, ExtensionError> {
        let capabilities = capability_descriptors_from_manifest(&manifest)?;

        Ok(Self {
            id: manifest.id.clone(),
            root_binding,
            manifest,
            capabilities,
            manifest_digest,
            descriptor_schema_mode: CapabilityDescriptorSchemaMode::ManifestRefs,
            content_pins: None,
            hosted_mcp_tool_names: BTreeMap::new(),
        })
    }

    pub fn from_host_bundled_manifest_with_inline_dynamic_schemas(
        manifest: ExtensionManifest,
        root: VirtualPath,
        manifest_digest: Option<String>,
        capabilities: Vec<CapabilityDescriptor>,
    ) -> Result<Self, ExtensionError> {
        if manifest.source != ManifestSource::HostBundled {
            return Err(ExtensionError::InvalidManifest {
                reason:
                    "inline dynamic descriptor schemas are only supported for host-bundled packages"
                        .to_string(),
            });
        }
        ensure_extension_root_matches(&manifest.id, &root)?;
        let expected = capability_descriptors_from_manifest(&manifest)?;
        if !descriptors_match_except_schema(&capabilities, &expected) {
            return Err(ExtensionError::InvalidManifest {
                reason: "inline dynamic capability descriptors do not match manifest declarations"
                    .to_string(),
            });
        }
        Ok(Self {
            id: manifest.id.clone(),
            root_binding: PackageRootBinding::Materialized(root),
            manifest,
            capabilities,
            manifest_digest,
            descriptor_schema_mode: CapabilityDescriptorSchemaMode::InlineDynamic,
            content_pins: None,
            hosted_mcp_tool_names: BTreeMap::new(),
        })
    }

    /// Bind the package to consented file hashes.
    pub fn with_content_pins(mut self, pins: PackageContentPins) -> Self {
        self.content_pins = Some(pins);
        self
    }

    /// Check bytes just read from `path` (an absolute path under the
    /// package root) against the consented hash for that file. Fails closed:
    /// a package that needs consent ([`ManifestSource::requires_consent`])
    /// and carries no pins is refused, as is a file the grant does not cover
    /// and bytes that hash differently. Only a package from a source that
    /// needs no consent (host-bundled, user-registered) runs unpinned.
    /// Callers verify the exact buffer they go on to use — never re-read
    /// after checking.
    pub fn verify_asset(
        &self,
        path: &VirtualPath,
        bytes: &[u8],
    ) -> Result<(), PackageContentPinError> {
        let Some(expected) = self.required_asset_pin(path)? else {
            return Ok(());
        };
        if expected != sha256_digest_token(bytes) {
            return Err(PackageContentPinError::Mismatch {
                path: self
                    .relative_asset_path(path)
                    .unwrap_or(path.as_str())
                    .to_string(),
            });
        }
        Ok(())
    }

    pub fn with_hosted_mcp_tool_names(
        mut self,
        tool_names: BTreeMap<ironclaw_host_api::ids::CapabilityId, String>,
    ) -> Result<Self, ExtensionError> {
        self.hosted_mcp_tool_names = tool_names;
        self.validate_consistency()?;
        Ok(self)
    }

    fn validate_hosted_mcp_tool_names(&self) -> Result<(), ExtensionError> {
        if self.hosted_mcp_tool_names.is_empty() {
            return Ok(());
        }
        let prefix = format!("{}.", self.id.as_str());
        if self.hosted_mcp_tool_names.len() != self.capabilities.len()
            || self.capabilities.iter().any(|descriptor| {
                !self.hosted_mcp_tool_names.contains_key(&descriptor.id)
                    || descriptor.runtime != RuntimeKind::Mcp
            })
            || self
                .hosted_mcp_tool_names
                .iter()
                .any(|(capability_id, wire_name)| {
                    let Some(suffix) = capability_id.as_str().strip_prefix(&prefix) else {
                        return true;
                    };
                    !ironclaw_extension_contracts::hosted_mcp::is_valid_mcp_tool_name(wire_name)
                        || wire_name.to_ascii_lowercase() != suffix
                })
        {
            return Err(ExtensionError::InvalidManifest {
                reason: "hosted MCP tool-name bindings do not match discovered capabilities"
                    .to_string(),
            });
        }
        Ok(())
    }

    /// The consented hash a read of `path` must match: `Ok(None)` only for a
    /// package that needs no consent and carries no pins; `Ok(Some(hash))`
    /// when the grant covers the file; `Err` when the package needs consent
    /// but carries no pins, or is pinned and the grant does not cover the
    /// file. Lanes call this before any cache lookup keyed by the file, so a
    /// module compiled from other bytes is never reused for a pinned package.
    pub fn required_asset_pin(
        &self,
        path: &VirtualPath,
    ) -> Result<Option<&str>, PackageContentPinError> {
        let Ok(root) = self.root_binding.materialized_root() else {
            // A virtual (remote-only) package has no files to pin.
            return if self.requires_verified_contents() {
                Err(PackageContentPinError::NoGrant {
                    extension_id: self.id.as_str().to_string(),
                })
            } else {
                Ok(None)
            };
        };
        required_pin(
            &self.id,
            self.manifest.source,
            self.content_pins.as_ref(),
            root,
            path,
        )
    }

    /// True when this package's files must be verified against a consent
    /// grant before anything runs them: its source needs consent and it is
    /// not an explicit unverified test construction. A lane that cannot bind
    /// execution to verified bytes (the script runtime) refuses such a
    /// package outright.
    pub fn requires_verified_contents(&self) -> bool {
        self.manifest.source.requires_consent()
            && !self
                .content_pins
                .as_ref()
                .is_some_and(PackageContentPins::is_unverified_for_test)
    }

    /// The consented hash for the file at `path`, if this package is pinned
    /// and the grant covers that file.
    pub fn pinned_asset_hash(&self, path: &VirtualPath) -> Option<&str> {
        let pins = self
            .content_pins
            .as_ref()
            .filter(|pins| !pins.is_unverified_for_test())?;
        pins.assets
            .get(self.relative_asset_path(path)?)
            .map(String::as_str)
    }

    fn relative_asset_path<'a>(&self, path: &'a VirtualPath) -> Option<&'a str> {
        let root = self.root_binding.materialized_root().ok()?;
        path.as_str()
            .strip_prefix(root.as_str().trim_end_matches('/'))?
            .strip_prefix('/')
    }

    pub fn hosted_mcp_tool_names(&self) -> &BTreeMap<ironclaw_host_api::ids::CapabilityId, String> {
        &self.hosted_mcp_tool_names
    }

    pub fn manifest_digest(&self) -> Option<String> {
        self.manifest_digest.clone()
    }

    pub fn package_root_binding(&self) -> &PackageRootBinding {
        &self.root_binding
    }

    pub fn materialized_root(&self) -> Result<&VirtualPath, PackageRootError> {
        self.root_binding.materialized_root()
    }

    /// Return the immutable package source used for trust-policy evaluation.
    ///
    /// A materialized package is identified by its manifest path. A virtual
    /// package has no filesystem identity, so it is eligible only when it is
    /// the constrained direct HTTP MCP shape emitted by hosted-MCP discovery.
    /// `FabricateOnLoad` is a legacy loading sentinel, never an identity a
    /// trust policy may evaluate.
    pub fn trust_policy_source(&self) -> Result<PackageSource, ExtensionError> {
        match &self.root_binding {
            PackageRootBinding::Materialized(root) => Ok(PackageSource::LocalManifest {
                path: format!("{}/manifest.toml", root.as_str().trim_end_matches('/')),
            }),
            PackageRootBinding::Virtual => match &self.manifest.runtime {
                ExtensionRuntime::Mcp {
                    transport,
                    command: None,
                    args,
                    url: Some(endpoint),
                } if transport.eq_ignore_ascii_case("http") && args.is_empty() => {
                    Ok(PackageSource::DirectRemote {
                        endpoint: endpoint.clone(),
                    })
                }
                _ => Err(ExtensionError::InvalidManifest {
                    reason: "virtual package is not a direct HTTP MCP endpoint".to_string(),
                }),
            },
            PackageRootBinding::FabricateOnLoad => Err(ExtensionError::InvalidManifest {
                reason: "package root must be materialized before trust evaluation".to_string(),
            }),
        }
    }

    pub(crate) fn validate_consistency(&self) -> Result<(), ExtensionError> {
        if self.id != self.manifest.id {
            return Err(ExtensionError::InvalidManifest {
                reason: format!(
                    "package id {} does not match manifest id {}",
                    self.id, self.manifest.id
                ),
            });
        }
        match &self.root_binding {
            PackageRootBinding::Materialized(root) => {
                ensure_extension_root_matches(&self.manifest.id, root)?;
            }
            PackageRootBinding::Virtual => ensure_virtual_manifest(&self.manifest)?,
            PackageRootBinding::FabricateOnLoad => {
                return Err(ExtensionError::InvalidManifest {
                    reason: "package root must be materialized before packaging".to_string(),
                });
            }
        }
        let expected = capability_descriptors_from_manifest(&self.manifest)?;
        let consistent = match self.descriptor_schema_mode {
            CapabilityDescriptorSchemaMode::ManifestRefs => self.capabilities == expected,
            CapabilityDescriptorSchemaMode::InlineDynamic => {
                (self.manifest.source == ManifestSource::HostBundled
                    || matches!(self.root_binding, PackageRootBinding::Virtual))
                    && descriptors_match_except_schema(&self.capabilities, &expected)
            }
        };
        if !consistent {
            return Err(ExtensionError::InvalidManifest {
                reason: "package capability descriptors do not match manifest declarations"
                    .to_string(),
            });
        }
        self.validate_hosted_mcp_tool_names()?;
        Ok(())
    }

    /// Build the trust-policy identity for this package.
    ///
    /// `PackageId` and `ExtensionId` share the same underlying vocabulary in
    /// V1; the conversion still goes through the validated constructor so this
    /// crate does not rely on representation details.
    pub fn package_identity(
        &self,
        source: PackageSource,
        digest: Option<String>,
        signer: Option<String>,
    ) -> Result<PackageIdentity, ExtensionError> {
        crate::registry::validate_package_consistency(self)?;
        Ok(PackageIdentity::new(
            PackageId::new(self.manifest.id.as_str().to_string())?,
            source,
            digest,
            signer,
        ))
    }

    /// Build the trust-policy input for this package.
    ///
    /// Requested authority is the canonical set of capability ids declared by
    /// the package. The returned value is still untrusted input; callers must
    /// pass it to `ironclaw_trust::TrustPolicy::evaluate` to get an effective
    /// [`ironclaw_trust::TrustDecision`].
    pub fn trust_policy_input(
        &self,
        source: PackageSource,
        digest: Option<String>,
        signer: Option<String>,
    ) -> Result<TrustPolicyInput, ExtensionError> {
        Ok(TrustPolicyInput {
            identity: self.package_identity(source, digest, signer)?,
            requested_trust: self.manifest.requested_trust,
            requested_authority: self
                .capabilities
                .iter()
                .map(|descriptor| descriptor.id.clone())
                .collect::<BTreeSet<_>>(),
        })
    }
}

fn ensure_virtual_manifest(manifest: &ExtensionManifest) -> Result<(), ExtensionError> {
    if manifest.source != ManifestSource::UserRegistered {
        return Err(ExtensionError::InvalidManifest {
            reason: "virtual packages require user-registered provenance".to_string(),
        });
    }
    if matches!(manifest.runtime, ExtensionRuntime::Wasm { .. }) {
        return Err(ExtensionError::InvalidManifest {
            reason: "virtual packages cannot declare a filesystem-backed runtime".to_string(),
        });
    }
    if manifest
        .capabilities
        .iter()
        .any(|capability| capability.prompt_doc_ref.is_some())
    {
        return Err(ExtensionError::InvalidManifest {
            reason: "virtual packages cannot declare filesystem-backed prompt documents"
                .to_string(),
        });
    }
    Ok(())
}

fn descriptors_match_except_schema(
    actual: &[CapabilityDescriptor],
    expected: &[CapabilityDescriptor],
) -> bool {
    actual.len() == expected.len()
        && actual.iter().zip(expected).all(|(actual, expected)| {
            let mut normalized = actual.clone();
            normalized.parameters_schema = expected.parameters_schema.clone();
            normalized == *expected
        })
}

fn ensure_extension_root_matches(
    id: &ExtensionId,
    root: &VirtualPath,
) -> Result<(), ExtensionError> {
    let expected = extension_id_from_package_root(root)?;
    if &expected != id {
        return Err(ExtensionError::ManifestIdMismatch {
            root: root.clone(),
            expected,
            actual: id.clone(),
        });
    }
    Ok(())
}

fn extension_id_from_package_root(root: &VirtualPath) -> Result<ExtensionId, ExtensionError> {
    let Some(extension_id) = root.as_str().strip_prefix("/system/extensions/") else {
        return Err(invalid_package_root(root));
    };
    if extension_id.is_empty() || extension_id.contains('/') {
        return Err(invalid_package_root(root));
    }
    Ok(ExtensionId::new(extension_id.to_string())?)
}

fn capability_descriptors_from_manifest(
    manifest: &ExtensionManifest,
) -> Result<Vec<CapabilityDescriptor>, ExtensionError> {
    let expected_prefix = format!("{}.", manifest.id.as_str());
    // Descriptor-layer mirror of the parse-time provider-prefix rule. The one
    // extra namespace: a HOST-BUNDLED manifest may declare tools under the
    // reserved stable memory-tool namespace (`ironclaw.memory.*`), so a
    // swapped memory backend keeps the stable tool ids. The primary
    // enforcement is the v3 parser (`[memory]` requires a first_party runtime,
    // which requires a host-bundled source); this check keeps the namespace
    // closed to every non-host-bundled package as defense in depth.
    let reserved_memory_prefix = format!(
        "{}.",
        ironclaw_extension_contracts::memory::MEMORY_TOOL_ID_NAMESPACE
    );
    let mut seen_capabilities = HashSet::new();
    manifest
        .capabilities
        .iter()
        .map(|capability| {
            let in_reserved_memory_namespace = manifest.source == ManifestSource::HostBundled
                && capability.id.as_str().starts_with(&reserved_memory_prefix);
            if !capability.id.as_str().starts_with(&expected_prefix)
                && !in_reserved_memory_namespace
            {
                return Err(ExtensionError::InvalidManifest {
                    reason: format!(
                        "capability id {} must be provider-prefixed with {}",
                        capability.id.as_str(),
                        expected_prefix
                    ),
                });
            }
            if !seen_capabilities.insert(capability.id.clone()) {
                return Err(ExtensionError::DuplicateCapability {
                    id: capability.id.clone(),
                });
            }
            Ok(CapabilityDescriptor {
                id: capability.id.clone(),
                provider: manifest.id.clone(),
                runtime: manifest.runtime_kind(),
                trust_ceiling: manifest.descriptor_trust_default,
                description: composed_capability_description(capability),
                parameters_schema: descriptor_schema_ref(capability),
                effects: capability.effects.clone(),
                default_permission: capability.default_permission,
                runtime_credentials: capability.runtime_credentials.clone(),
                network_targets: capability.network_targets.clone(),
                max_egress_bytes: capability.max_egress_bytes,
                resource_profile: capability.resource_profile.clone(),
                origin_gate_matrix: capability.origin_gate_matrix.clone(),
                standard_op: capability.standard_op,
            })
        })
        .collect()
}

/// Compose a capability's model-visible description at descriptor build.
///
/// Standard messaging bindings use the host-authored operation core followed
/// by the extension's optional vendor addendum. Bespoke declarations preserve
/// their manifest description verbatim. Composition happens here rather than
/// in persisted manifest data so host wording updates take effect on restart
/// without changing the manifest digest.
pub fn composed_capability_description(decl: &CapabilityDeclV2) -> String {
    match decl.standard_op.and_then(|op| op.contract()) {
        Some(contract) => {
            let addendum = decl.description.trim();
            if addendum.is_empty() {
                contract.description_core.trim().to_string()
            } else {
                format!("{}\n{}", contract.description_core.trim(), addendum)
            }
        }
        None => decl.description.clone(),
    }
}

fn invalid_package_root(root: &VirtualPath) -> ExtensionError {
    ExtensionError::InvalidManifest {
        reason: format!(
            "extension package root {} must be /system/extensions/<extension>",
            root.as_str()
        ),
    }
}

fn descriptor_schema_ref(capability: &CapabilityManifest) -> serde_json::Value {
    serde_json::json!({ "$ref": capability.input_schema_ref.as_str() })
}

#[cfg(test)]
mod tests {
    use ironclaw_host_api::host_port::HostPortCatalog;

    use super::*;
    use crate::{CapabilityProviderHostApiContract, HostApiContractRegistry};

    const VIRTUAL_MANIFEST: &str = r#"
schema_version = "reborn.extension_manifest.v2"
id = "remote-tools"
name = "Remote Tools"
version = "0.1.0"
description = "Remote-only tool provider"
trust = "untrusted"

[runtime]
kind = "script"
runner = "remote"
command = "invoke"

[[host_api]]
id = "ironclaw.capability_provider/v1"
section = "capability_provider.tools"

[capability_provider.tools]

[[capability_provider.tools.capabilities]]
id = "remote-tools.invoke"
description = "Invoke a remote tool"
effects = ["dispatch_capability"]
default_permission = "ask"
visibility = "model"
input_schema_ref = "schemas/remote-tools/invoke.input.v1.json"
"#;

    const DIRECT_REMOTE_VIRTUAL_MANIFEST: &str = r#"
schema_version = "reborn.extension_manifest.v2"
id = "remote-tools"
name = "Remote Tools"
version = "0.1.0"
description = "Remote-only tool provider"
trust = "untrusted"

[runtime]
kind = "mcp"
transport = "http"
url = "https://mcp.example.test/mcp"

[[host_api]]
id = "ironclaw.capability_provider/v1"
section = "capability_provider.tools"

[capability_provider.tools]

[[capability_provider.tools.capabilities]]
id = "remote-tools.invoke"
description = "Invoke a remote tool"
effects = ["network"]
default_permission = "ask"
visibility = "model"
input_schema_ref = "schemas/remote-tools/invoke.input.v1.json"
"#;

    fn wasm_package(source: ManifestSource) -> ExtensionPackage {
        let manifest = ExtensionManifest::parse(
            &DIRECT_REMOTE_VIRTUAL_MANIFEST.replace(
                "kind = \"mcp\"\ntransport = \"http\"\nurl = \"https://mcp.example.test/mcp\"",
                "kind = \"wasm\"\nmodule = \"wasm/tool.wasm\"",
            ),
            source,
            &HostPortCatalog::empty(),
            &contracts(),
        )
        .expect("manifest parses");
        ExtensionPackage::from_manifest(
            manifest,
            VirtualPath::new("/system/extensions/remote-tools").expect("root"),
        )
        .expect("package")
    }

    /// Absent pins are acceptable only for a package that needs no consent;
    /// a consent-requiring package without pins is refused, as is a pinned
    /// package's file the grant does not cover — before any bytes are read.
    #[test]
    fn content_pins_fail_closed_for_packages_that_need_consent() {
        let module = VirtualPath::new("/system/extensions/remote-tools/wasm/tool.wasm").unwrap();
        let bytes = b"module bytes".as_slice();

        let bundled = wasm_package(ManifestSource::HostBundled);
        assert_eq!(bundled.required_asset_pin(&module), Ok(None));
        assert_eq!(bundled.verify_asset(&module, bytes), Ok(()));

        for source in [
            ManifestSource::InstalledLocal,
            ManifestSource::RegistryInstalled,
        ] {
            let unpinned = wasm_package(source);
            assert!(unpinned.requires_verified_contents());
            assert!(matches!(
                unpinned.required_asset_pin(&module),
                Err(PackageContentPinError::NoGrant { .. })
            ));
            assert!(matches!(
                unpinned.verify_asset(&module, bytes),
                Err(PackageContentPinError::NoGrant { .. })
            ));

            let uncovered = wasm_package(source).with_content_pins(PackageContentPins::from_files(
                [("manifest.toml", b"m".as_slice())],
            ));
            assert!(matches!(
                uncovered.required_asset_pin(&module),
                Err(PackageContentPinError::Unpinned { .. })
            ));

            let pinned = wasm_package(source)
                .with_content_pins(PackageContentPins::from_files([("wasm/tool.wasm", bytes)]));
            assert_eq!(
                pinned.required_asset_pin(&module),
                Ok(Some(sha256_digest_token(bytes).as_str()))
            );
            assert_eq!(pinned.verify_asset(&module, bytes), Ok(()));
            assert!(matches!(
                pinned.verify_asset(&module, b"other bytes"),
                Err(PackageContentPinError::Mismatch { .. })
            ));

            let test_only =
                crate::test_support::UnverifiedContentsForTest::with_unverified_contents_for_test(
                    wasm_package(source),
                );
            assert!(!test_only.requires_verified_contents());
            assert_eq!(test_only.verify_asset(&module, b"anything"), Ok(()));
            assert_eq!(test_only.pinned_asset_hash(&module), None);
        }
    }

    fn contracts() -> HostApiContractRegistry {
        let mut contracts = HostApiContractRegistry::new();
        contracts
            .register(std::sync::Arc::new(
                CapabilityProviderHostApiContract::new().expect("contract"),
            ))
            .expect("register contract");
        contracts
    }

    #[test]
    fn virtual_package_requires_inline_descriptors_and_never_exposes_a_root() {
        let manifest = ExtensionManifest::parse(
            VIRTUAL_MANIFEST,
            ManifestSource::UserRegistered,
            &HostPortCatalog::empty(),
            &contracts(),
        )
        .expect("manifest");
        let mut capabilities =
            capability_descriptors_from_manifest(&manifest).expect("descriptors");
        capabilities[0].parameters_schema = serde_json::json!({"type": "object"});

        let package = ExtensionPackage::from_virtual_manifest(manifest, None, capabilities)
            .expect("virtual package");
        assert_eq!(package.root_binding, PackageRootBinding::Virtual);
        assert_eq!(package.materialized_root(), Err(PackageRootError::Virtual));
        package.validate_consistency().expect("consistent package");
    }

    #[test]
    fn virtual_package_rejects_non_user_registered_provenance() {
        let manifest = ExtensionManifest::parse(
            VIRTUAL_MANIFEST,
            ManifestSource::InstalledLocal,
            &HostPortCatalog::empty(),
            &contracts(),
        )
        .expect("manifest");
        let capabilities = capability_descriptors_from_manifest(&manifest).expect("descriptors");
        let error = ExtensionPackage::from_virtual_manifest(manifest, None, capabilities)
            .expect_err("local provenance cannot become virtual");
        assert!(matches!(
            error,
            ExtensionError::InvalidManifest { reason }
                if reason.contains("user-registered provenance")
        ));
    }

    #[test]
    fn packaged_fabrication_state_fails_consistency() {
        let manifest = ExtensionManifest::parse(
            VIRTUAL_MANIFEST,
            ManifestSource::InstalledLocal,
            &HostPortCatalog::empty(),
            &contracts(),
        )
        .expect("manifest");
        let root = VirtualPath::new("/system/extensions/remote-tools").expect("root");
        let mut package = ExtensionPackage::from_manifest(manifest, root).expect("package");
        package.root_binding = PackageRootBinding::FabricateOnLoad;
        let error = package
            .validate_consistency()
            .expect_err("fabrication cannot survive packaging");
        assert!(matches!(error, ExtensionError::InvalidManifest { .. }));
    }

    #[test]
    fn package_consistency_rejects_stale_hosted_mcp_tool_name_bindings() {
        let manifest = ExtensionManifest::parse(
            DIRECT_REMOTE_VIRTUAL_MANIFEST,
            ManifestSource::UserRegistered,
            &HostPortCatalog::empty(),
            &contracts(),
        )
        .expect("manifest");
        let mut capabilities =
            capability_descriptors_from_manifest(&manifest).expect("capability descriptors");
        capabilities[0].parameters_schema = serde_json::json!({"type": "object"});
        let capability_id = capabilities[0].id.clone();
        let mut package = ExtensionPackage::from_virtual_manifest(manifest, None, capabilities)
            .expect("virtual package")
            .with_hosted_mcp_tool_names(BTreeMap::from([(capability_id, "invoke".to_string())]))
            .expect("complete binding is consistent");

        let mut added_manifest = package.manifest.capabilities[0].clone();
        added_manifest.id = ironclaw_host_api::ids::CapabilityId::new("remote-tools.second")
            .expect("second capability id");
        let mut added_descriptor = package.capabilities[0].clone();
        added_descriptor.id = added_manifest.id.clone();
        package.manifest.capabilities.push(added_manifest);
        package.capabilities.push(added_descriptor);

        let error = package
            .validate_consistency()
            .expect_err("stale non-empty bindings must fail package admission");
        assert!(matches!(
            error,
            ExtensionError::InvalidManifest { reason }
                if reason.contains("tool-name bindings do not match")
        ));
    }

    #[test]
    fn trust_policy_source_uses_the_package_root_binding() {
        let materialized_manifest = ExtensionManifest::parse(
            VIRTUAL_MANIFEST,
            ManifestSource::InstalledLocal,
            &HostPortCatalog::empty(),
            &contracts(),
        )
        .expect("manifest");
        let materialized = ExtensionPackage::from_manifest(
            materialized_manifest,
            VirtualPath::new("/system/extensions/remote-tools").expect("root"),
        )
        .expect("package");
        assert_eq!(
            materialized.trust_policy_source().expect("source"),
            PackageSource::LocalManifest {
                path: "/system/extensions/remote-tools/manifest.toml".to_string(),
            }
        );

        let virtual_manifest = ExtensionManifest::parse(
            DIRECT_REMOTE_VIRTUAL_MANIFEST,
            ManifestSource::UserRegistered,
            &HostPortCatalog::empty(),
            &contracts(),
        )
        .expect("manifest");
        let capabilities =
            capability_descriptors_from_manifest(&virtual_manifest).expect("descriptors");
        let virtual_package =
            ExtensionPackage::from_virtual_manifest(virtual_manifest, None, capabilities)
                .expect("virtual package");
        assert_eq!(
            virtual_package.trust_policy_source().expect("source"),
            PackageSource::DirectRemote {
                endpoint: "https://mcp.example.test/mcp".to_string(),
            }
        );

        let non_direct_manifest = ExtensionManifest::parse(
            VIRTUAL_MANIFEST,
            ManifestSource::UserRegistered,
            &HostPortCatalog::empty(),
            &contracts(),
        )
        .expect("manifest");
        let non_direct_capabilities =
            capability_descriptors_from_manifest(&non_direct_manifest).expect("descriptors");
        let non_direct = ExtensionPackage::from_virtual_manifest(
            non_direct_manifest,
            None,
            non_direct_capabilities,
        )
        .expect("virtual package");
        assert!(matches!(
            non_direct.trust_policy_source(),
            Err(ExtensionError::InvalidManifest { .. })
        ));

        let mut fabricated = materialized;
        fabricated.root_binding = PackageRootBinding::FabricateOnLoad;
        assert!(matches!(
            fabricated.trust_policy_source(),
            Err(ExtensionError::InvalidManifest { .. })
        ));
    }
}
