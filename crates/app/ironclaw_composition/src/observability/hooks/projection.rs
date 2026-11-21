//! Hook-only projection: structural containment of the per-tenant extension set
//! into hook metadata that can reach ONLY the hook factory, plus the
//! discovery/admission pipeline that decides which third-party packages are
//! merged.
//!
//! This module owns the structural containment unit ([`HookProjection`]) and
//! the hook-only registry ([`HookProjectionRegistry`]) — the #3951 P1
//! least-privilege boundary: the projection path holds only `[[hooks]]`
//! payloads plus the identity/trust/root needed to install + contain them, and
//! NOTHING from the capability / runtime / package surface.

use ironclaw_extension_registry::ExtensionRegistry;

use crate::error::RebornBuildError;

use super::HooksActivationConfig;

/// Maximum number of *installed* extension packages whose `[[hooks]]` will be
/// considered for projection in a single tenant build. Surplus extensions
/// beyond this count are quarantined (skipped + audited), never whole-build
/// failed. A tenant-wide DoS ceiling on top of the per-extension caps the
/// registrar already enforces (`MAX_HOOKS_PER_EXTENSION`).
pub const MAX_INSTALLED_EXTENSIONS_CONSIDERED: usize = 64;

/// Maximum number of hook bindings projected from third-party installed
/// extensions across the whole tenant. An extension whose hooks would push the
/// running total past this budget is quarantined (skipped + audited), not
/// whole-build failed. Builtin / host-bundled bindings do not count against
/// this third-party budget (they are trusted and fail-closed-whole-build).
pub(crate) const MAX_TOTAL_HOOKS_PER_TENANT: usize = 256;

/// The hook-only metadata extracted from ONE extension package: exactly the
/// fields the projection needs, and NOTHING from the capability / runtime /
/// package surface.
///
/// This is the structural containment unit (serrrfirat's #3951 P1): the
/// projection path holds only `[[hooks]]` payloads plus the identity/trust/root
/// needed to install + contain them. It literally CANNOT reach capabilities,
/// the runtime spec, schema refs, or anything else on `ExtensionPackage` —
/// because it does not hold them. Containment is by DATA SHAPE, stronger than a
/// "no conversion provided" newtype boundary.
#[derive(Debug, Clone)]
pub(super) struct HookProjection {
    pub(super) extension_id: ironclaw_host_api::ids::ExtensionId,
    pub(super) version: String,
    /// Trust posture (drives quarantine-vs-fail-closed). Copied off the
    /// manifest at extraction time; the capability surface is left behind.
    pub(super) source: ironclaw_extension_registry::ManifestSource,
    /// Package root, for the projection-layer containment check only.
    pub(super) root: ironclaw_host_api::path::VirtualPath,
    /// The declared `[[hooks]]` payloads.
    pub(super) hooks: Vec<ironclaw_extension_registry::HookSectionEntryV2>,
    /// The package's declared `[[settings]]`, so a run can bind the acting
    /// user's effective values to the package's WASM hooks. Plain,
    /// non-secret declarations — no capability or runtime surface.
    pub(super) settings: Vec<ironclaw_extension_registry::settings::ExtensionSettingDecl>,
    /// Content digest of the manifest record, when the package carries one;
    /// part of the live projection cache key.
    pub(super) manifest_digest: Option<String>,
    /// The consented per-file hashes the package was published with (third-
    /// party packages; `None` for host-bundled ones). Every hook module byte
    /// is verified against these before it is cached or compiled.
    pub(super) content_pins: Option<ironclaw_extension_registry::PackageContentPins>,
}

impl HookProjection {
    /// Extract the hook-only projection from an extension package, dropping
    /// everything else (capabilities, runtime, schema refs). Returns `None` for
    /// a package that declares no hooks (nothing to project).
    pub(super) fn from_package(
        package: &ironclaw_extension_registry::ExtensionPackage,
    ) -> Option<Self> {
        if package.manifest.hooks.is_empty() {
            return None;
        }
        // silent-ok: only virtual-root (hosted MCP) packages fail materialized_root();
        // they cannot declare filesystem hooks, so skipping projection is correct.
        let Ok(root) = package.materialized_root() else {
            return None;
        };
        let root = root.clone();
        Some(Self {
            extension_id: package.manifest.id.clone(),
            version: package.manifest.version.clone(),
            source: package.manifest.source,
            root,
            hooks: package.manifest.hooks.clone(),
            settings: package.manifest.settings.clone(),
            manifest_digest: package.manifest_digest.clone(),
            content_pins: package.content_pins.clone(),
        })
    }
}

/// A hook-projection registry: the hook-only metadata of every extension whose
/// declared `[[hooks]]` are projected into the hook dispatcher, AND NOTHING
/// ELSE.
///
/// # Structural containment (hook-only by data shape — #3951 P1)
///
/// Third-party installed extensions must contribute *hooks* without becoming
/// *capability providers*. The capability-dispatch path is fed by the
/// `Arc<ExtensionRegistry>` handed to
/// [`ironclaw_host_runtime::HostRuntimeServices::new`] (it becomes the
/// capability catalog + surface resolver). If a third-party registry reached
/// that constructor, those extensions would gain capability authority — exactly
/// what the hook-only projection model forbids.
///
/// This type carries `Vec<HookProjection>` — hook metadata ONLY. It does NOT
/// wrap an `ExtensionRegistry` or hold any `ExtensionPackage`, so there is no
/// `ExtensionRegistry` inside it to leak to the capability path: containment is
/// enforced by the DATA SHAPE, not by withholding a conversion. A developer
/// cannot feed this to `HostRuntimeServices::new` because it simply is not, and
/// cannot become, an `ExtensionRegistry`.
pub struct HookProjectionRegistry(Vec<HookProjection>);

impl HookProjectionRegistry {
    /// Build the hook-only registry from the per-package projections that
    /// survived discovery + admission. The full packages are consumed here and
    /// only their hook metadata is retained.
    pub(super) fn from_projections(projections: Vec<HookProjection>) -> Self {
        Self(projections)
    }

    /// Crate-private read-only view of the projected hook metadata, for the
    /// hook projection loop only.
    pub(super) fn projections(&self) -> impl Iterator<Item = &HookProjection> {
        self.0.iter()
    }
}

impl std::fmt::Debug for HookProjectionRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HookProjectionRegistry")
            .field("projection_count", &self.0.len())
            .finish()
    }
}

/// The fixed `/system/extensions` discovery root.
///
/// # Tenant isolation via the scoped filesystem (not the path)
///
/// The discovery layer ([`ironclaw_extension_registry::ExtensionDiscovery`] /
/// [`ironclaw_extension_registry::ExtensionPackage::from_manifest`]) hardcodes package
/// roots to `/system/extensions/<extension-id>` — exactly one segment under
/// this root. It does so **because the `RootFilesystem` it is handed is itself
/// the tenant scope boundary**: every other tenant-scoped resource in the
/// system (secrets, authorization leases, run state, …) is isolated by the
/// per-tenant [`ironclaw_filesystem::ScopedFilesystem`] / per-identity backend,
/// not by a tenant segment baked into the virtual path. Discovery follows the
/// same convention.
///
/// Consequently `tenant_extension_root` takes the authenticated `tenant_id`
/// (so the SIGNATURE pins that callers must supply identity, and the
/// containment defense knows the root) but returns the fixed `/system/extensions`
/// path. The isolation guarantee is: **the per-tenant `RootFilesystem` passed
/// to discovery resolves `/system/extensions/<id>` to that tenant's storage and
/// no other's.** In standalone (single-tenant, the only profile
/// `build_reborn_runtime` wires) the runtime's FS is constructed once per
/// identity in `build_runtime_substrate`, so it is per-identity by construction;
/// production wiring (a follow-up, since `build_reborn_runtime` only supports
/// standalone) must supply a tenant-scoped backend here.
///
/// **This makes the scoped FS the SOLE isolation boundary** — see the
/// FS-hardening gate on [`HooksActivationConfig`]: `HOOKS_THIRD_PARTY_ENABLED`
/// MUST NOT be enabled in multi-tenant production until
/// `openat2(RESOLVE_BENEATH)` / `O_NOFOLLOW` backend hardening lands, because
/// that hardening is precisely what protects the scoped-FS-is-the-boundary
/// property against symlink/`..` escapes below the virtual layer.
pub(crate) fn tenant_extension_root(
    _tenant_id: &ironclaw_host_api::ids::TenantId,
) -> Result<ironclaw_host_api::path::VirtualPath, RebornBuildError> {
    ironclaw_host_api::path::VirtualPath::new("/system/extensions").map_err(|error| {
        RebornBuildError::InvalidConfig {
            reason: format!("could not derive extension discovery root: {error}"),
        }
    })
}

/// Defense-in-depth containment check applied to a discovered package's root
/// before its hooks are projected (Step 2 / FS-hardening v1).
///
/// The canonicalizing local backend is the primary defense; this projection
/// layer adds a strict-child / no-`..` / no-symlink-escape check so a package
/// whose resolved root escapes the tenant root is quarantined rather than
/// projected. Returns `Ok(())` when the package root is a strict child of
/// `tenant_root`; otherwise an error naming the violation (the caller turns
/// this into a quarantine, not a whole-build failure, for untrusted sources).
pub(super) fn enforce_root_containment(
    tenant_root: &ironclaw_host_api::path::VirtualPath,
    package_root: &ironclaw_host_api::path::VirtualPath,
) -> Result<(), String> {
    let root = tenant_root.as_str().trim_end_matches('/');
    let candidate = package_root.as_str();
    let prefix = format!("{root}/");
    if !candidate.starts_with(&prefix) {
        return Err(format!(
            "package root `{candidate}` is not a strict child of tenant root `{root}`"
        ));
    }
    // Reject any path traversal segment in the child portion. `VirtualPath`
    // already canonicalizes, but this is the explicit projection-layer
    // no-`..`/no-empty-segment guard the FS-hardening v1 posture documents.
    let child = &candidate[prefix.len()..];
    for segment in child.split('/') {
        // `VirtualPath` already canonicalizes `..`/`.`/empty (`//`) segments
        // out, but this is the explicit projection-layer no-`..`/no-empty-segment
        // guard the FS-hardening v1 posture documents — defense-in-depth that
        // does not rely on the path type's canonicalization (gemini #3951 #353).
        if segment == ".." || segment == "." || segment.is_empty() {
            return Err(format!(
                "package root `{candidate}` contains a traversal or empty segment `{segment}`"
            ));
        }
    }
    Ok(())
}

/// Assemble the boot-time hook-projection registry: the hook metadata of the
/// trusted (builtin / host-bundled) packages only.
///
/// Third-party packages are deliberately absent. Their hooks follow the
/// extension lifecycle and consent instead — they install per run from the
/// active extension registry through [`super::live`], only for users whose
/// own consent grant covers the package's files, and only from module bytes
/// verified against that grant. A boot-time filesystem discovery of
/// `/system/extensions` would be a second, ungated path for the same hooks,
/// so there is none.
pub async fn build_hook_projection_registry(
    builtin: ExtensionRegistry,
    _config: HooksActivationConfig,
) -> Result<HookProjectionRegistry, RebornBuildError> {
    // The builtin `ExtensionRegistry` is consumed here and dropped; only hook
    // projections survive into the hook-only registry (structural
    // containment). A builtin registry entry from any other source is not
    // trusted to contribute boot hooks.
    let projections: Vec<HookProjection> = builtin
        .extensions()
        .filter(|package| package.manifest.source.allows_first_party())
        .filter_map(HookProjection::from_package)
        .collect();
    Ok(HookProjectionRegistry::from_projections(projections))
}
