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
//! Every package-declared hook still installs only through
//! [`HookRegistrar`], i.e. at the `Installed` tier with exactly that tier's
//! restrictions (restricted sinks — no `Allow`; no `Validation` /
//! `Authorization` phases; grants default-deny). Nothing here widens what an
//! installed hook may decide.
//!
//! WASM bodies load their module bytes from the package's own root (the
//! `[[hooks]].module` asset), bounded by [`MAX_HOOK_MODULE_BYTES`], into an
//! in-memory map the hook WASM runtime resolves from. The map is a cache over
//! the package files: it is pruned to the active set every run and refilled
//! from the filesystem on a miss.
//!
//! Settings: when the package declares `[[settings]]`, the acting user's
//! effective values are read through [`ExtensionSettingsReader`] for the
//! run's scope and bound to the package's WASM hooks for that run (the
//! `settings` member of their context blob). A settings read failure fails
//! the run's host build — skipping a gate hook because its configuration
//! could not be read would fail open.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, RwLock};

use ironclaw_extension_contracts::runtime::ExtensionAssetPath;
use ironclaw_extension_registry::settings::{ExtensionSettingsReader, effective_setting_values};
use ironclaw_extension_registry::{SharedExtensionRegistry, resolve_asset_under};
use ironclaw_filesystem::RootFilesystem;
use ironclaw_hooks::dispatch::HookDispatcherBuilder;
use ironclaw_hooks::evaluator::PredicateEvaluator;
use ironclaw_hooks::manifest::HookManifestEntry;
use ironclaw_hooks::registrar::HookRegistrar;
use ironclaw_hooks::registry::HookRegistry;
use ironclaw_hooks::wasm::{
    WasmHookModuleRequest, WasmHookModuleResolver, WasmHookRuntime, WasmHookRuntimeError,
};
use ironclaw_host_api::ids::TenantId;
use ironclaw_host_api::path::VirtualPath;
use ironclaw_turn_runner::loop_driver_host::{
    HookDispatcherBuildRequest, RebornLoopDriverHostError,
};

use super::audit::emit_hook_quarantined;
use super::factory::{ProjectedExtensionHooks, project_hook_entries};
use super::projection::{
    HookProjection, MAX_INSTALLED_EXTENSIONS_CONSIDERED, MAX_TOTAL_HOOKS_PER_TENANT,
    enforce_root_containment,
};

/// Largest hook WASM module a package may ship, in bytes.
pub(crate) const MAX_HOOK_MODULE_BYTES: usize = 2 * 1024 * 1024;

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
}

type FirstPartyInstaller = Arc<
    dyn Fn(HookDispatcherBuilder) -> Result<HookDispatcherBuilder, crate::error::RebornBuildError>
        + Send
        + Sync,
>;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ModuleKey {
    extension_id: String,
    version: String,
    hook_local_id: String,
}

/// Module bytes for the active packages' WASM hooks, keyed by hook identity.
#[derive(Default)]
struct LoadedHookModules {
    modules: RwLock<HashMap<ModuleKey, Arc<Vec<u8>>>>,
}

impl WasmHookModuleResolver for LoadedHookModules {
    fn resolve_module(
        &self,
        request: &WasmHookModuleRequest<'_>,
    ) -> Result<Vec<u8>, WasmHookRuntimeError> {
        let key = ModuleKey {
            extension_id: request.extension_id.as_str().to_string(),
            version: request.extension_version.to_string(),
            hook_local_id: request.hook_local_id.as_str().to_string(),
        };
        let modules = self.modules.read().map_err(|_| {
            WasmHookRuntimeError::Compilation("hook module table is unavailable".to_string())
        })?;
        modules
            .get(&key)
            .map(|bytes| bytes.as_ref().clone())
            .ok_or_else(|| {
                WasmHookRuntimeError::Compilation(format!(
                    "no module asset is loaded for hook `{}` of extension `{}`",
                    key.hook_local_id, key.extension_id
                ))
            })
    }
}

/// A package's hook set that passed projection + scratch validation, keyed by
/// what would change it.
#[derive(Clone, PartialEq, Eq)]
struct ValidationKey {
    version: String,
    manifest_digest: Option<String>,
    root: String,
}

#[derive(Clone)]
struct CachedValidation {
    key: ValidationKey,
    outcome: Result<Arc<Vec<HookManifestEntry>>, String>,
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
    modules: Arc<LoadedHookModules>,
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
        let modules = Arc::new(LoadedHookModules::default());
        let runtime = Arc::new(
            WasmHookRuntime::new(Arc::clone(&modules) as Arc<dyn WasmHookModuleResolver>).map_err(
                |error| crate::error::RebornBuildError::InvalidConfig {
                    reason: format!("hook WASM runtime could not start: {error}"),
                },
            )?,
        );
        Ok(Self {
            third_party_enabled,
            tenant_id,
            tenant_root,
            sources,
            evaluator,
            first_party,
            trusted,
            modules,
            runtime,
            validated: Mutex::new(HashMap::new()),
        })
    }

    fn registrar(&self) -> HookRegistrar {
        HookRegistrar::new(Arc::clone(&self.evaluator)).with_wasm_runtime(Arc::clone(&self.runtime))
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
            live.insert(extension.clone());
            let entries = match self.validated_entries(&projection).await? {
                Ok(entries) => entries,
                Err(reason) => {
                    debug_quarantine_cached(&extension, &reason);
                    continue;
                }
            };
            let settings = if projection.settings.is_empty() {
                None
            } else {
                let stored = self
                    .sources
                    .settings
                    .stored_values(&request.scope, &projection.extension_id)
                    .await
                    .map_err(|error| {
                        replay_error(format!(
                            "settings for extension `{extension}` could not be read: {error}"
                        ))
                    })?;
                let effective = effective_setting_values(&projection.settings, &stored);
                Some(Arc::new(effective.into_iter().collect()))
            };
            let (next, _) = registrar
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
            hook_total += hook_count;
        }
        self.prune(&live)?;
        Ok(builder)
    }

    /// The validated entries for one package, from cache or by loading its
    /// module assets and validating the whole set against a scratch builder.
    /// The outer `Result` is a host fault; the inner `Err` is a quarantine
    /// reason (cached until the package changes).
    async fn validated_entries(
        &self,
        projection: &HookProjection,
    ) -> Result<Result<Arc<Vec<HookManifestEntry>>, String>, RebornLoopDriverHostError> {
        let extension = projection.extension_id.as_str().to_string();
        let key = ValidationKey {
            version: projection.version.clone(),
            manifest_digest: projection.manifest_digest.clone(),
            root: projection.root.as_str().to_string(),
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
        if let Err(reason) = &outcome {
            self.quarantine(&extension, reason, projection.hooks.len());
        }
        self.lock_validated()?.insert(
            extension,
            CachedValidation {
                key,
                outcome: outcome.clone(),
            },
        );
        Ok(outcome)
    }

    async fn validate(
        &self,
        projection: &HookProjection,
    ) -> Result<Arc<Vec<HookManifestEntry>>, String> {
        let extension = projection.extension_id.as_str();
        self.load_modules(projection).await?;
        let entries = project_hook_entries(extension, &projection.hooks)?;
        let scratch = HookDispatcherBuilder::new(HookRegistry::new());
        // Validation only: the scratch builder is dropped; the per-run build
        // installs the same entries into its own builder.
        let (_validated, _ids) = self
            .registrar()
            .install(
                projection.extension_id.clone(),
                &projection.version,
                &entries,
                scratch,
            )
            .map_err(|error| {
                format!("failed to install hooks declared by extension `{extension}`: {error}")
            })?;
        Ok(Arc::new(entries))
    }

    async fn load_modules(&self, projection: &HookProjection) -> Result<(), String> {
        let extension = projection.extension_id.as_str();
        let mut loaded = Vec::new();
        for hook in &projection.hooks {
            let Some(module) = &hook.module else {
                continue;
            };
            let asset = ExtensionAssetPath::new(module.clone()).map_err(|error| {
                format!("hook `{}` module path is invalid: {error}", hook.local_id)
            })?;
            let path = resolve_asset_under(&asset, &projection.root).map_err(|error| {
                format!("hook `{}` module path is invalid: {error}", hook.local_id)
            })?;
            let bytes = self
                .sources
                .filesystem
                .read_file_bounded(&path, MAX_HOOK_MODULE_BYTES)
                .await
                .map_err(|error| {
                    format!(
                        "hook `{}` module `{module}` could not be read (at most \
                         {MAX_HOOK_MODULE_BYTES} bytes): {error}",
                        hook.local_id
                    )
                })?
                .ok_or_else(|| {
                    format!(
                        "hook `{}` module `{module}` exceeds {MAX_HOOK_MODULE_BYTES} bytes",
                        hook.local_id
                    )
                })?;
            loaded.push((
                ModuleKey {
                    extension_id: extension.to_string(),
                    version: projection.version.clone(),
                    hook_local_id: hook.local_id.clone(),
                },
                Arc::new(bytes),
            ));
        }
        let mut modules = self
            .modules
            .modules
            .write()
            .map_err(|_| "hook module table is unavailable".to_string())?;
        modules.retain(|key, _| key.extension_id != extension);
        modules.extend(loaded);
        Ok(())
    }

    /// Drop cached validations and module bytes for packages that are no
    /// longer active, so a deactivated package leaves nothing resident.
    fn prune(&self, live: &BTreeSet<String>) -> Result<(), RebornLoopDriverHostError> {
        self.lock_validated()?
            .retain(|extension, _| live.contains(extension));
        self.modules
            .modules
            .write()
            .map_err(|_| replay_error("hook module table is unavailable".to_string()))?
            .retain(|key, _| live.contains(&key.extension_id));
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
    }
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

/// Test seam: the extension ids whose module bytes are resident.
#[cfg(test)]
impl LiveHookInstaller {
    pub(super) fn resident_module_extensions(&self) -> BTreeSet<String> {
        self.modules
            .modules
            .read()
            .map(|modules| modules.keys().map(|key| key.extension_id.clone()).collect())
            .unwrap_or_default()
    }
}
