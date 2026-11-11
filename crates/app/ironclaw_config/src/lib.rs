//! Boot configuration contracts for the standalone IronClaw Reborn binary.
//!
//! This crate is intentionally small and has no IronClaw workspace dependencies.
//! It owns process/environment boot configuration that must be shared by the
//! `ironclaw-reborn` binary and later Reborn runtime composition without pulling
//! in the v1 root application.
//!
//! Four boot-time surfaces live here:
//!
//! - [`RebornBootConfig`] — home + profile resolved from env vars at
//!   process start. The original API; unchanged.
//! - [`RebornConfigFile`] — the operator-edited TOML at
//!   `$ZLO_HOME/config.toml`. Read once at process start;
//!   provides the *selection* layer of the three-layer config model
//!   (catalog → selection → runtime config). See `config_file.rs`.
//! - Provider catalog — lives in `$ZLO_HOME/providers.json`
//!   in the v1 `providers.json` shape. This crate exposes the path via
//!   [`RebornHome::providers_file_path`]; loading the file goes through
//!   `ironclaw_llm::ProviderRegistry` in the composition root (this
//!   crate has no workspace deps, per boundary rules).
//! - [`seed_default_config_file_if_missing`] — first-run seeding for the
//!   sparse runtime `config.toml` written by stateful Reborn commands.

mod boot;
mod budget;
mod capability_remediation;
mod config_file;
mod config_seed;
mod doctor;
mod home;
mod home_migration;
mod profile;
mod remote;
mod retired_sections;
mod secrets_guard;
mod update;
mod zlo_env;

pub use boot::RebornBootConfig;
pub use budget::{
    BACKGROUND_JOB_DEFAULT_USD_ENV, BUDGET_DEFAULT_TZ_ENV, BUDGET_OVERESTIMATE_FACTOR_ENV,
    BUDGET_PAUSE_AT_ENV, BUDGET_WARN_AT_ENV, BudgetDefaults, BudgetDefaultsError,
    HEARTBEAT_PER_TICK_USD_ENV, MISSION_PER_TICK_USD_ENV, PROJECT_DAILY_USD_ENV,
    ROUTINE_LIGHTWEIGHT_USD_ENV, ROUTINE_STANDARD_USD_ENV, USER_DAILY_USD_ENV,
};
pub use capability_remediation::{
    HostRemediationText, apply_step_text, google_backend_auth_text, google_not_configured_text,
    google_remediation_text, google_setup_steps_text,
};
pub use config_file::{
    BootSection, BudgetSection, DefaultLlmSlotUpdate, DefaultLlmSlotUpdateSession, DriversSection,
    EmbeddingsSection, GoogleFieldUpdate, GoogleOauthConfigUpdate, GoogleOauthConfigUpdateSession,
    GoogleSection, HarnessSection, IdentitySection, LlmSlotFieldUpdate, LlmSlotSelection,
    MemoryAdminOverride, MemorySection, PolicySection, REBORN_CONFIG_API_VERSION, RebornConfigFile,
    RebornConfigFileError, RebornConfigFileUpdateError, RunnerSection, StorageBackend,
    StorageSection, TriggerPollerConfigSection, begin_default_llm_slot_update,
    begin_google_oauth_config_update, update_default_llm_slot, update_google_oauth_config,
};
pub use config_seed::{
    RebornConfigSeedError, RebornConfigSeedOutcome, seed_default_config_file_if_missing,
};
pub use doctor::RebornDoctorReport;
pub use home::{
    DEFAULT_HOME_DIR_NAME, LEGACY_REBORN_HOME_ENV, REBORN_HOME_ENV, RebornConfigError, RebornHome,
    RebornHomeSource,
};
pub use home_migration::{
    HomeMigration, MIGRATION_PENDING_MARKER, legacy_home_path, migrate_legacy_home,
};
pub use profile::{
    LEGACY_REBORN_PROFILE_ENV, REBORN_PROFILE_ENV, RebornProfile, profile_env_value,
};
pub use remote::{
    DEFAULT_REMOTE_HTTPS_PORT, DEFAULT_REMOTE_IDENTITY_PORT, RemoteSection, RemoteSettings,
    remove_remote_section, write_remote_section,
};
pub use retired_sections::{RetiredSectionError, RetiredSections, retired_config_key_guidance};
pub use secrets_guard::{InlineSecretError, reject_inline_secret};
pub use update::{
    SourceUpdateSettings, UpdateMethodSetting, UpdateSection, is_safe_branch_name,
    write_update_section,
};
pub use zlo_env::{
    LEGACY_WEBUI_TOKEN_ENV, LEGACY_WEBUI_USER_ID_ENV, WEBUI_TOKEN_ENV, WEBUI_USER_ID_ENV,
    active_env_name, var_os_with_legacy,
};
