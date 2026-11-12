//! Standalone skill-context source assembly and its env-driven selector
//! configuration (`IRONCLAW_REBORN_SKILL_INJECTION` / `_ACTIVATION`).

use super::*;

pub(super) struct ComposedSkillContextSource {
    pub(super) bundle_source: Arc<FilesystemSkillBundleSource<CompositeRootFilesystem>>,
    pub(super) source: Arc<dyn HostSkillContextSource>,
    pub(super) activation_source: Arc<ComposedSelectableSkillContextSource>,
    pub(super) execution_adapter: Arc<ComposedSkillExecutionAdapter>,
}

const MAX_SKILL_CONTEXT_TOKENS: usize = 6000;

/// Build the [`SkillActivationSelectorConfig`] used by the standalone
/// filesystem skill context source. Extracted from
/// [`filesystem_skill_context_source`] so the wiring of the
/// `regex_skill_activation_enabled` flag from [`RebornRuntimeInput`] is
/// covered by a unit test (see `tests::standalone_selector_config_*`).
/// Without this seam the propagation was tested only indirectly through
/// the full [`build_reborn_runtime`] path, where an accidental
/// `..SkillActivationSelectorConfig::default()` regression would slip
/// through silently.
pub(super) fn skill_activation_selector_config(
    regex_skill_activation_enabled: bool,
    injection_mode: SkillInjectionMode,
    activation_strategy: ironclaw_skills::activation_strategy::ActivationStrategy,
    process_execution_available: bool,
) -> SkillActivationSelectorConfig {
    SkillActivationSelectorConfig {
        max_context_tokens: MAX_SKILL_CONTEXT_TOKENS,
        // `selection_mode` is deliberately NOT set here: it inherits
        // `SkillActivationSelectorConfig`'s default, which this PR makes
        // `ExplicitOnly`.
        //
        // The model decides which skill applies. It sees every installed skill in
        // the one-line listing and calls `skill_activate` for the one it wants;
        // the host does not keyword-match on its behalf. A scoring function
        // guessing from the user's wording is strictly worse at this than the
        // model reading the same listing, and a wrong guess spends the skill
        // budget on the wrong body.
        //
        // Pinning `ExplicitAndCriteria` here previously made that default
        // unreachable on the Reborn path — the value in `activation.rs` was dead
        // code as far as any real user was concerned, so changing it looked like a
        // behaviour change and was not one. `reborn_skill_selection_is_model_decided`
        // fails if it is re-pinned. Criteria selection is still available to
        // callers that opt in via `set_selection_mode`, and explicit `$name` /
        // `/name` mentions force-activate under either mode.
        regex_activation_enabled: regex_skill_activation_enabled,
        injection_mode,
        activation_strategy,
        process_execution_available,
        ..SkillActivationSelectorConfig::default()
    }
}

/// Parse the Reborn skill-injection mode from the
/// `IRONCLAW_REBORN_SKILL_INJECTION` env switch. Defaults to `listing`
/// (one-line skill listing; bodies load on `builtin.skill_activate`);
/// `full` restores the legacy inject-bodies-by-score behavior.
fn skill_injection_mode_env() -> Result<SkillInjectionMode, RebornRuntimeError> {
    skill_injection_mode_from_env_value(std::env::var(SKILL_INJECTION_MODE_ENV_KEY))
}

/// The decision itself, split from the lookup so every branch is testable. `remove_var` is not an
/// option: these tests run in-process and in parallel, so unsetting the key races every other test
/// reading it.
pub(super) fn skill_injection_mode_from_env_value(
    value: Result<String, std::env::VarError>,
) -> Result<SkillInjectionMode, RebornRuntimeError> {
    match value {
        Ok(value) => skill_injection_mode_from(&value),
        Err(std::env::VarError::NotPresent) => Ok(DEFAULT_SKILL_INJECTION_MODE),
        Err(error) => Err(RebornRuntimeError::InvalidArgument {
            reason: format!("could not read {SKILL_INJECTION_MODE_ENV_KEY}: {error}"),
        }),
    }
}

const SKILL_INJECTION_MODE_ENV_KEY: &str = "IRONCLAW_REBORN_SKILL_INJECTION";

/// Binding for the `skill.activation.v1` profile.
const SKILL_ACTIVATION_ENV_KEY: &str = "IRONCLAW_REBORN_SKILL_ACTIVATION";

/// Default stays `CriteriaOnly` — behavior-preserving.
///
/// `name_and_description` is the opt-in (`IRONCLAW_REBORN_SKILL_ACTIVATION=name_and_description`):
/// a skill matches on name and description, not only `activation.keywords`/`tags`/`patterns`. On
/// nearai/benchmarks#287, 0 of 30 agent-authored skills carried an `activation` block, so under
/// criteria scoring they never auto-activate.
///
/// A floor-score strategy (`always_available`) was tried and REMOVED: listing membership is decided
/// by visibility, not selection, so the floor only reordered a listing the model already saw.
pub(super) const DEFAULT_SKILL_ACTIVATION:
    ironclaw_skills::activation_strategy::ActivationStrategy =
    ironclaw_skills::activation_strategy::ActivationStrategy::CriteriaOnly;

/// Resolve the activation binding from the env, failing closed on an unknown id.
fn skill_activation_env()
-> Result<ironclaw_skills::activation_strategy::ActivationStrategy, RebornRuntimeError> {
    match std::env::var(SKILL_ACTIVATION_ENV_KEY) {
        Ok(value) => ironclaw_skills::activation_strategy::ActivationStrategy::parse(&value)
            .map_err(|error| RebornRuntimeError::InvalidArgument {
                reason: format!("{SKILL_ACTIVATION_ENV_KEY}: {error}"),
            }),
        Err(std::env::VarError::NotPresent) => Ok(DEFAULT_SKILL_ACTIVATION),
        Err(error) => Err(RebornRuntimeError::InvalidArgument {
            reason: format!("could not read {SKILL_ACTIVATION_ENV_KEY}: {error}"),
        }),
    }
}

/// Default skill-injection mode. `Listing` shows a one-line menu and loads a body only on `$name`
/// or `builtin.skill_activate`; `Full` injects scored bodies.
///
/// On the 31-task subset in nearai/benchmarks#287 (`deepseek-v4-flash`), `Listing` leaves skills
/// nearly inert -- `skill_list` called in 30/30 runs, `skill_activate` in 3/30, a body read in
/// 0/30 -- and scores 79.8% against 78.5% for no skills at all, where `Full` scores 85.6%.
///
/// **Left at `Listing` anyway**: three `local_dev_*` tests drive a mock expecting the listing
/// candidate and HANG under `Full`, so flipping the product default needs those updated first. Opt
/// in with `IRONCLAW_REBORN_SKILL_INJECTION=full`.
pub(super) const DEFAULT_SKILL_INJECTION_MODE: SkillInjectionMode = SkillInjectionMode::Listing;

pub(super) fn skill_injection_mode_from(
    value: &str,
) -> Result<SkillInjectionMode, RebornRuntimeError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "listing" => Ok(SkillInjectionMode::Listing),
        "full" => Ok(SkillInjectionMode::Full),
        other => Err(RebornRuntimeError::InvalidArgument {
            reason: format!(
                "{SKILL_INJECTION_MODE_ENV_KEY} must be \"listing\" or \"full\", got {other:?}"
            ),
        }),
    }
}

pub(super) fn filesystem_skill_context_runtime(
    runtime: &RebornRuntimeStores,
) -> Option<&RebornRuntimeStores> {
    Some(runtime)
}

pub(super) fn filesystem_skill_context_source(
    runtime: &RebornRuntimeStores,
    tenant_id: &TenantId,
    regex_skill_activation_enabled: bool,
) -> Result<ComposedSkillContextSource, RebornRuntimeError> {
    let skill_filesystem = &runtime.skill_filesystem;
    let workspace_filesystem = &runtime.workspace_filesystem;
    let skill_auto_activate_learned = &runtime.skill_auto_activate_learned;
    let extension = FirstPartySkillsExtension::new(
        Arc::clone(skill_filesystem),
        FirstPartySkillsExtensionHandles::without_tenant_shared().map_err(|reason| {
            RebornRuntimeError::InvalidArgument {
                reason: format!("first-party skills extension handles: {reason}"),
            }
        })?,
        tenant_id.clone(),
    )
    .map_err(|reason| RebornRuntimeError::InvalidArgument {
        reason: format!("first-party skills extension source: {reason}"),
    })?;
    // Whether this deployment can execute a process at all. Under `ProcessBackendKind::None` (hosted
    // multi-tenant + secure default) a skill that says "run scripts/foo.py" is instructing the model to
    // do something impossible, and it does not degrade gracefully -- see
    // `SkillActivationSelectorConfig::process_execution_available`.
    //
    // MULTI-TENANT ENABLEMENT: this is one of the two places that change when the tenant sandbox
    // lands. Once `HostedMultiTenant` + `SecureDefault` resolves to `ProcessBackendKind::TenantSandbox`
    // instead of `None`, this returns true on its own and skills stop being told they cannot run
    // anything. See docs/skills/multi_tenant_enablement.md.
    //
    // `unwrap_or(true)` is not a fail-open: `build_runtime` rejects a services input with no
    // resolved policy long before this, so only local test harnesses (which do have a shell) can
    // observe the fallback.
    let process_execution_available = runtime
        .runtime_policy
        .as_ref()
        .map(|policy| {
            policy.process_backend != ironclaw_host_api::runtime_policy::ProcessBackendKind::None
        })
        .unwrap_or(true);
    let selector_config = skill_activation_selector_config(
        regex_skill_activation_enabled,
        skill_injection_mode_env()?,
        skill_activation_env()?,
        process_execution_available,
    );
    // Staging needs a READ-WRITE workspace handle: `workspace_filesystem` beside it is deliberately
    // read-only (it backs setup-marker reads) and fails closed on write. Same recipe the inbound
    // attachment lander uses, and the same reason -- under a per-caller policy it addresses the
    // caller's own subtree rather than the shared root.
    let staging_filesystem = crate::runtime_mounts::read_write_workspace_filesystem(
        &runtime.extension_filesystem,
        &runtime.workspace_mounts,
    );
    let selectable_skills = match staging_filesystem {
        Some(staging_filesystem) => extension.selectable_skill_runtime_with_staging(
            selector_config,
            Arc::clone(workspace_filesystem),
            staging_filesystem,
            Arc::clone(skill_auto_activate_learned),
        ),
        // No writable workspace (hosted multi-tenant today) -- skills still activate, and a body that
        // promises execution gets the "cannot execute processes" note instead of a staged path.
        None => extension.selectable_skill_runtime_with_setup_markers(
            selector_config,
            Arc::clone(workspace_filesystem),
            Arc::clone(skill_auto_activate_learned),
        ),
    };
    let bundle_source = extension.bundle_source();
    Ok(ComposedSkillContextSource {
        source: selectable_skills.host_skill_context_source(),
        activation_source: selectable_skills.activation_source(),
        execution_adapter: selectable_skills.execution_adapter(),
        bundle_source,
    })
}
