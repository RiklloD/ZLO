//! Live, activation-tracking installation of package-declared hooks.
//!
//! Boot-time projection ([`super::projection`]) only sees the packages present
//! when the runtime was built. A package's hooks must follow its *lifecycle*
//! instead: they register when the extension is activated and disappear when
//! it is deactivated or removed. The source of truth for "active" is the
//! shared active extension registry the lifecycle publisher writes on
//! activation and clears on deactivation/removal; this module reads it once
//! per run, inside the per-run dispatcher factory, so:
//!
//! - a newly activated package's hooks fire from its next run on, without a
//!   restart;
//! - a deactivated or removed package's hooks are absent from the next run;
//! - an in-flight run keeps the dispatcher it started with.
//!
//! Per-user visibility: a run installs only packages whose installation is
//! visible to its user ([`InstallationOwnerSource`], the tool catalog's owner
//! projection); a package with no installation row is visible to nobody.
//!
//! Consent: a third-party package contributes hooks only when it was
//! published with its consent grant's per-file pins, and only to runs of
//! users whose own grant covers those files (the owner projection is
//! consent-filtered by the lifecycle manager). Each hook module is read once
//! and verified against its pin before it is cached or compiled; an unpinned
//! package or a module that differs from the consented bytes contributes no
//! hooks (quarantined and audited; the package's status reads `tampered`
//! on the next lifecycle list).
//!
//! Every package-declared hook still installs only through
//! [`HookRegistrar`], i.e. at the `Installed` tier with exactly that tier's
//! restrictions (restricted sinks — no `Allow`; no `Validation` /
//! `Authorization` phases; grants default-deny). Nothing here widens what an
//! installed hook may decide.
//!
//! WASM bodies compile from the package's `[[hooks]].module` assets, read
//! once per package content identity by [`read_hook_module`] (bounded by
//! [`MAX_HOOK_MODULE_BYTES`]) and held immutable next to the validated
//! entries; a run installs from the handle it admitted, so a concurrent prune
//! cannot invalidate it.
//!
//! Settings: when the package declares `[[settings]]`, the acting user's
//! effective values are read through [`ExtensionSettingsReader`] for the
//! run's scope and bound to the package's WASM hooks for that run (the
//! `settings` member of their context blob). On a read failure a gate or
//! mutator fails the run's host build (skipping it would fail open); after-
//! fact observers are skipped for that run only (failure isolation).

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use ironclaw_extension_contracts::runtime::ExtensionAssetPath;
use ironclaw_extension_manager::installation_visibility::InstallationOwnerSource;
use ironclaw_extension_manager::installation_visibility::installation_visible_to;
use ironclaw_extension_registry::settings::{ExtensionSettingsReader, effective_setting_values};
use ironclaw_extension_registry::{SharedExtensionRegistry, resolve_asset_under};
use ironclaw_filesystem::RootFilesystem;
use ironclaw_hooks::dispatch::HookDispatcherBuilder;
use ironclaw_hooks::evaluator::PredicateEvaluator;
use ironclaw_hooks::manifest::{HookManifestBody, HookManifestEntry, HookManifestKind};
use ironclaw_hooks::registrar::HookRegistrar;
use ironclaw_hooks::registry::HookRegistry;
use ironclaw_hooks::wasm::{HookModuleBytes, PinnedHookModules, WasmHookRuntime};
use ironclaw_host_api::ids::TenantId;
use ironclaw_host_api::path::VirtualPath;
use ironclaw_turn_runner::loop_driver_host::{
    HookDispatcherBuildRequest, RebornLoopDriverHostError,
};

use super::audit::{emit_hook_quarantined, emit_hook_skipped_for_run};
use super::factory::{ProjectedExtensionHooks, project_hook_entries};
use super::projection::{
    HookProjection, MAX_INSTALLED_EXTENSIONS_CONSIDERED, MAX_TOTAL_HOOKS_PER_TENANT,
    enforce_root_containment,
};

/// Most packages whose validated hook set (and module bytes, at most 2 MiB
/// per module) stay cached at once, across all users.
const MAX_CACHED_VALIDATIONS: usize = 64;

/// Largest hook WASM module a package may ship, in bytes.
pub(crate) const MAX_HOOK_MODULE_BYTES: usize = 2 * 1024 * 1024;

/// Live quarantines by extension id (reason), readable by the hook inventory
/// so a user sees why their plugin's hooks are off. Cleared when a package's
/// hooks next validate or it leaves the active set.
#[derive(Clone, Default)]
pub(crate) struct LiveHookQuarantines(Arc<Mutex<HashMap<String, String>>>);

impl LiveHookQuarantines {
    fn record(&self, extension: &str, reason: Option<&str>) {
        if let Ok(mut map) = self.0.lock() {
            match reason {
                Some(reason) => map.insert(extension.to_string(), reason.to_string()),
                None => map.remove(extension),
            };
        }
    }

    fn retain(&self, live: &BTreeSet<String>) {
        if let Ok(mut map) = self.0.lock() {
            map.retain(|extension, _| live.contains(extension));
        }
    }

    /// The recorded quarantine reason for `extension`, if any.
    pub(crate) fn reason(&self, extension: &str) -> Option<String> {
        self.0.lock().ok()?.get(extension).cloned()
    }
}

/// The live inputs a run's hook set is derived from.
#[derive(Clone)]
pub(crate) struct LiveHookSources {
    /// The active extension set (written by activation, cleared by
    /// deactivation/removal).
    pub(crate) active: Arc<SharedExtensionRegistry>,
    /// The filesystem package roots resolve against (module bytes).
    pub(crate) filesystem: Arc<dyn RootFilesystem>,
    /// Per-user setting values.
    pub(crate) settings: Arc<dyn ExtensionSettingsReader>,
    /// Installation owners: which users each active package is visible to
    /// (consent-filtered in production).
    pub(crate) owners: Arc<dyn InstallationOwnerSource>,
    /// Where live quarantines are recorded for the hook inventory.
    pub(crate) quarantines: LiveHookQuarantines,
}

type FirstPartyInstaller = Arc<
    dyn Fn(HookDispatcherBuilder) -> Result<HookDispatcherBuilder, crate::error::RebornBuildError>
        + Send
        + Sync,
>;

/// A package's hook set that passed projection + scratch validation, keyed by
/// what would change it.
#[derive(Clone, PartialEq, Eq)]
struct ValidationKey {
    version: String,
    manifest_digest: Option<String>,
    root: String,
    /// A new grant (new pins) revalidates from freshly read, re-verified
    /// bytes.
    content_pins: Option<ironclaw_extension_registry::PackageContentPins>,
}

/// Entries that passed validation, with the exact module bytes they were
/// validated against.
#[derive(Clone)]
struct ValidatedHookSet {
    entries: Arc<Vec<HookManifestEntry>>,
    modules: HookModuleBytes,
}

#[derive(Clone)]
struct CachedValidation {
    key: ValidationKey,
    outcome: Result<ValidatedHookSet, String>,
}

pub(super) struct LiveHookInstaller {
    pub(super) third_party_enabled: bool,
    pub(super) tenant_id: TenantId,
    pub(super) tenant_root: VirtualPath,
    pub(super) sources: LiveHookSources,
    pub(super) evaluator: Arc<PredicateEvaluator>,
    pub(super) first_party: FirstPartyInstaller,
    /// Host-bundled sets validated once at composition (fail-closed there).
    pub(super) trusted: Vec<ProjectedExtensionHooks>,
    runtime: Arc<WasmHookRuntime>,
    validated: Mutex<HashMap<String, CachedValidation>>,
}

impl LiveHookInstaller {
    pub(super) fn new(
        third_party_enabled: bool,
        tenant_id: TenantId,
        tenant_root: VirtualPath,
        sources: LiveHookSources,
        evaluator: Arc<PredicateEvaluator>,
        first_party: FirstPartyInstaller,
        trusted: Vec<ProjectedExtensionHooks>,
    ) -> Result<Self, crate::error::RebornBuildError> {
        let runtime = Arc::new(
            // Every install brings the bytes its run admitted (`registrar_for`);
            // nothing resolves from shared mutable state.
            WasmHookRuntime::new(Arc::new(PinnedHookModules::none())).map_err(|error| {
                crate::error::RebornBuildError::InvalidConfig {
                    reason: format!("hook WASM runtime could not start: {error}"),
                }
            })?,
        );
        Ok(Self {
            third_party_enabled,
            tenant_id,
            tenant_root,
            sources,
            evaluator,
            first_party,
            trusted,
            runtime,
            validated: Mutex::new(HashMap::new()),
        })
    }

    fn registrar(&self) -> HookRegistrar {
        HookRegistrar::new(Arc::clone(&self.evaluator)).with_wasm_runtime(Arc::clone(&self.runtime))
    }

    /// A registrar whose WASM installs compile exactly `modules`.
    fn registrar_for(
        &self,
        projection: &HookProjection,
        modules: HookModuleBytes,
    ) -> HookRegistrar {
        self.registrar()
            .with_module_resolver(Arc::new(PinnedHookModules::new(
                projection.extension_id.as_str(),
                projection.version.clone(),
                modules,
            )))
    }

    /// Mint one run's builder: first-party hooks, the host-bundled sets, then
    /// every *active* third-party package's hooks.
    pub(super) async fn build(
        &self,
        request: HookDispatcherBuildRequest,
    ) -> Result<HookDispatcherBuilder, RebornLoopDriverHostError> {
        let mut builder = (self.first_party)(HookDispatcherBuilder::new(HookRegistry::new()))
            .map_err(|error| replay_error(format!("first-party hook install failed: {error}")))?;
        let registrar = self.registrar();
        let mut installed_ids: BTreeSet<String> = BTreeSet::new();
        for set in &self.trusted {
            let (next, _) = registrar
                .install(
                    set.extension_id.clone(),
                    &set.extension_version,
                    &set.entries,
                    builder,
                )
                .map_err(|error| {
                    replay_error(format!(
                        "host-bundled hook install failed for `{}`: {error}",
                        set.extension_id.as_str()
                    ))
                })?;
            builder = next;
            installed_ids.insert(set.extension_id.as_str().to_string());
        }
        if !self.third_party_enabled {
            return Ok(builder);
        }

        let snapshot = self.sources.active.snapshot();
        // Owners are read once per run, lazily; unreadable fails closed.
        let mut owners = None;
        let mut considered = 0usize;
        let mut hook_total = 0usize;
        let mut live = BTreeSet::new();
        for package in snapshot.extensions() {
            if package.manifest.source.allows_first_party() {
                continue;
            }
            let Some(projection) = HookProjection::from_package(package) else {
                continue;
            };
            let extension = projection.extension_id.as_str().to_string();
            let hook_count = projection.hooks.len();
            // Cache retention follows activation, not this run's user.
            live.insert(extension.clone());
            if owners.is_none() {
                owners = Some(self.sources.owners.installation_owners().await.map_err(
                    |error| {
                        replay_error(format!(
                            "extension installation owners could not be read: {error}"
                        ))
                    },
                )?);
            }
            let visible = owners.as_ref().is_some_and(|owners| {
                installation_visible_to(owners, &projection.extension_id, &request.scope.user_id)
            });
            if !visible {
                tracing::debug!(
                    extension_id = %extension,
                    "package hooks not installed: installation not visible to the run's user"
                );
                continue;
            }
            if installed_ids.contains(&extension) {
                self.quarantine(
                    &extension,
                    "duplicate extension id collides with an already-installed hook set",
                    hook_count,
                );
                continue;
            }
            considered += 1;
            if considered > MAX_INSTALLED_EXTENSIONS_CONSIDERED {
                self.quarantine(
                    &extension,
                    "exceeded MAX_INSTALLED_EXTENSIONS_CONSIDERED",
                    hook_count,
                );
                continue;
            }
            if hook_total + hook_count > MAX_TOTAL_HOOKS_PER_TENANT {
                self.quarantine(
                    &extension,
                    "exceeded MAX_TOTAL_HOOKS_PER_TENANT",
                    hook_count,
                );
                continue;
            }
            if let Err(reason) = enforce_root_containment(&self.tenant_root, &projection.root) {
                self.quarantine(&extension, &reason, hook_count);
                continue;
            }
            // A package that needs consent runs hooks only from the files its
            // grant pinned; one published without pins is never trusted.
            if projection.source.requires_consent() && projection.content_pins.is_none() {
                self.quarantine(
                    &extension,
                    "no consent grant binds this package's files",
                    hook_count,
                );
                continue;
            }
            // This run holds the validated module bytes until its install.
            let validated = match self.validated_entries(&projection).await? {
                Ok(validated) => validated,
                Err(reason) => {
                    debug_quarantine_cached(&extension, &reason);
                    continue;
                }
            };
            let (entries, settings) = self
                .entries_with_settings(&request, &projection, &validated.entries)
                .await?;
            let installed_count = entries.len();
            let (next, _) = self
                .registrar_for(&projection, validated.modules)
                .install_with_settings(
                    projection.extension_id.clone(),
                    &projection.version,
                    &entries,
                    settings,
                    builder,
                )
                .map_err(|error| {
                    replay_error(format!(
                        "hook install failed for extension `{extension}` after validation: {error}"
                    ))
                })?;
            builder = next;
            hook_total += installed_count;
        }
        self.prune(&live)?;
        Ok(builder)
    }

    /// The entries this run installs for one package, with the user's
    /// effective settings bound when its WASM hooks read them.
    ///
    /// Only WASM bodies read settings. On a read failure, after-fact observer
    /// WASM hooks are dropped for this run (observer failure isolation) and
    /// the rest install without settings; a gate or mutator WASM hook fails
    /// the build closed, since skipping it would fail open.
    async fn entries_with_settings(
        &self,
        request: &HookDispatcherBuildRequest,
        projection: &HookProjection,
        entries: &Arc<Vec<HookManifestEntry>>,
    ) -> Result<
        (
            Arc<Vec<HookManifestEntry>>,
            Option<Arc<serde_json::Map<String, serde_json::Value>>>,
        ),
        RebornLoopDriverHostError,
    > {
        let extension = projection.extension_id.as_str();
        let wasm_kinds: Vec<HookManifestKind> = entries
            .iter()
            .filter(|entry| matches!(entry.body, HookManifestBody::Wasm { .. }))
            .map(|entry| entry.kind)
            .collect();
        if projection.settings.is_empty() || wasm_kinds.is_empty() {
            return Ok((Arc::clone(entries), None));
        }
        let error = match self
            .sources
            .settings
            .stored_values(&request.scope, &projection.extension_id)
            .await
        {
            Ok(stored) => {
                let effective = effective_setting_values(&projection.settings, &stored);
                return Ok((
                    Arc::clone(entries),
                    Some(Arc::new(effective.into_iter().collect())),
                ));
            }
            Err(error) => error,
        };
        if wasm_kinds.iter().any(|kind| !kind.is_after_fact()) {
            return Err(replay_error(format!(
                "settings for extension `{extension}` could not be read: {error}"
            )));
        }
        let kept: Vec<HookManifestEntry> = entries
            .iter()
            .filter(|entry| !matches!(entry.body, HookManifestBody::Wasm { .. }))
            .cloned()
            .collect();
        let reason = format!("settings could not be read: {error}");
        emit_hook_skipped_for_run(
            &self.tenant_id,
            extension,
            &reason,
            entries.len() - kept.len(),
        );
        Ok((Arc::new(kept), None))
    }

    /// The validated entries for one package, from cache or by reading its
    /// module assets and validating the whole set against a scratch builder.
    /// The outer `Result` is a host fault; the inner `Err` is a quarantine
    /// reason (cached until the package changes).
    async fn validated_entries(
        &self,
        projection: &HookProjection,
    ) -> Result<Result<ValidatedHookSet, String>, RebornLoopDriverHostError> {
        let extension = projection.extension_id.as_str().to_string();
        let key = ValidationKey {
            version: projection.version.clone(),
            manifest_digest: projection.manifest_digest.clone(),
            root: projection.root.as_str().to_string(),
            content_pins: projection.content_pins.clone(),
        };
        {
            let cache = self.lock_validated()?;
            if let Some(cached) = cache.get(&extension)
                && cached.key == key
            {
                return Ok(cached.outcome.clone());
            }
        }
        let outcome = self.validate(projection).await;
        match &outcome {
            Err(reason) => self.quarantine(&extension, reason, projection.hooks.len()),
            Ok(_) => self.sources.quarantines.record(&extension, None),
        }
        let mut cache = self.lock_validated()?;
        // Users with disjoint private packages each add entries here, so the
        // per-run admission caps do not bound the total. Past the cap an
        // arbitrary other entry goes: a build that admitted it holds its own
        // handle on the bytes, and the next build for it reads them again.
        if cache.len() >= MAX_CACHED_VALIDATIONS
            && !cache.contains_key(&extension)
            && let Some(evicted) = cache.keys().next().cloned()
        {
            cache.remove(&evicted);
        }
        cache.insert(
            extension,
            CachedValidation {
                key,
                outcome: outcome.clone(),
            },
        );
        drop(cache);
        Ok(outcome)
    }

    async fn validate(&self, projection: &HookProjection) -> Result<ValidatedHookSet, String> {
        let extension = projection.extension_id.as_str();
        let modules = self.read_modules(projection).await?;
        let entries = project_hook_entries(extension, &projection.hooks)?;
        let scratch = HookDispatcherBuilder::new(HookRegistry::new());
        // Validation only: the scratch builder is dropped; each run installs
        // the same entries, from the same bytes, into its own builder.
        let (_validated, _ids) = self
            .registrar_for(projection, Arc::clone(&modules))
            .install(
                projection.extension_id.clone(),
                &projection.version,
                &entries,
                scratch,
            )
            .map_err(|error| {
                format!("failed to install hooks declared by extension `{extension}`: {error}")
            })?;
        Ok(ValidatedHookSet {
            entries: Arc::new(entries),
            modules,
        })
    }

    /// Read every WASM hook module of `projection`, once.
    async fn read_modules(&self, projection: &HookProjection) -> Result<HookModuleBytes, String> {
        let mut modules = HashMap::new();
        for hook in &projection.hooks {
            let Some(module) = &hook.module else {
                continue;
            };
            let bytes = read_hook_module(
                self.sources.filesystem.as_ref(),
                projection,
                &hook.local_id,
                module,
            )
            .await?;
            modules.insert(hook.local_id.clone(), bytes);
        }
        Ok(Arc::new(modules))
    }

    /// Drop cached validations (and with them the shared handle on their
    /// module bytes) for packages that are no longer active, so a
    /// deactivated package leaves nothing resident once its admitted runs
    /// finish installing.
    fn prune(&self, live: &BTreeSet<String>) -> Result<(), RebornLoopDriverHostError> {
        self.lock_validated()?
            .retain(|extension, _| live.contains(extension));
        self.sources.quarantines.retain(live);
        Ok(())
    }

    fn lock_validated(
        &self,
    ) -> Result<
        std::sync::MutexGuard<'_, HashMap<String, CachedValidation>>,
        RebornLoopDriverHostError,
    > {
        self.validated
            .lock()
            .map_err(|_| replay_error("hook validation cache is unavailable".to_string()))
    }

    fn quarantine(&self, extension: &str, reason: &str, hooks: usize) {
        emit_hook_quarantined(&self.tenant_id, extension, reason, hooks);
        self.sources.quarantines.record(extension, Some(reason));
    }
}

/// Read the exact bytes one WASM hook will compile from: the package-relative
/// `module` asset, resolved under the package root and bounded by
/// [`MAX_HOOK_MODULE_BYTES`]. This is the only place hook module bytes enter
/// the host; the returned buffer is what validation and every admitting run
/// compile — nothing reads the file a second time.
async fn read_hook_module(
    filesystem: &dyn RootFilesystem,
    projection: &HookProjection,
    hook_local_id: &str,
    module: &str,
) -> Result<Arc<[u8]>, String> {
    let asset = ExtensionAssetPath::new(module.to_string())
        .map_err(|error| format!("hook `{hook_local_id}` module path is invalid: {error}"))?;
    let path = resolve_asset_under(&asset, &projection.root)
        .map_err(|error| format!("hook `{hook_local_id}` module path is invalid: {error}"))?;
    let bytes = filesystem
        .read_file_bounded(&path, MAX_HOOK_MODULE_BYTES)
        .await
        .map_err(|error| {
            format!(
                "hook `{hook_local_id}` module `{module}` could not be read (at most \
                 {MAX_HOOK_MODULE_BYTES} bytes): {error}"
            )
        })?
        .ok_or_else(|| {
            format!(
                "hook `{hook_local_id}` module `{module}` exceeds {MAX_HOOK_MODULE_BYTES} bytes"
            )
        })?;
    // Consent binds the bytes that run: verify this exact buffer against the
    // hash the grant pinned for this file before it is cached or compiled.
    // A host-bundled package carries no pins and needs none; a package that
    // needs consent and has no pin for the module is refused too.
    ironclaw_extension_registry::verify_package_file(
        &projection.extension_id,
        projection.source,
        projection.content_pins.as_ref(),
        &projection.root,
        &path,
        &bytes,
    )
    .map_err(|error| format!("hook `{hook_local_id}` module `{module}` is tampered: {error}"))?;
    Ok(Arc::from(bytes))
}

fn debug_quarantine_cached(extension: &str, reason: &str) {
    tracing::debug!(
        extension_id = %extension,
        reason = %reason,
        "skipping quarantined extension hook set"
    );
}

fn replay_error(reason: String) -> RebornLoopDriverHostError {
    RebornLoopDriverHostError::InvalidRequest { reason }
}

/// Test seam: the extension ids whose module bytes the shared cache holds.
#[cfg(test)]
impl LiveHookInstaller {
    pub(super) fn resident_module_extensions(&self) -> BTreeSet<String> {
        self.validated
            .lock()
            .map(|cache| {
                cache
                    .iter()
                    .filter(|(_, cached)| {
                        cached
                            .outcome
                            .as_ref()
                            .is_ok_and(|validated| !validated.modules.is_empty())
                    })
                    .map(|(extension, _)| extension.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
}
