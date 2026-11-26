//! Assembles configuration sync (`ironclaw_assistant::config_sync`): one
//! handler per domain over the stores this host already has, behind the
//! product surface. Wiring only; the behavior lives in the handlers.

use std::sync::Arc;

use ironclaw_assistant::config_sync::approvals::ApprovalsDomainHandler;
use ironclaw_assistant::config_sync::cli_home::CliHomeDomainHandler;
use ironclaw_assistant::config_sync::config::ConfigDomainHandler;
use ironclaw_assistant::config_sync::memory::MemoryDomainHandler;
use ironclaw_assistant::config_sync::memory_base::SettingsMemorySyncBase;
use ironclaw_assistant::config_sync::orchestrator_settings::OrchestratorSettingsDomainHandler;
use ironclaw_assistant::config_sync::profiles::ProfilesDomainHandler;
use ironclaw_assistant::config_sync::prompts::PromptsDomainHandler;
use ironclaw_assistant::config_sync::skill_usage::SkillUsageDomainHandler;
use ironclaw_assistant::config_sync::skills::SkillsDomainHandler;
use ironclaw_assistant::config_sync::{ConfigSyncService, ConfigSyncSurface};
use ironclaw_product_contracts::surface::ProductSurface;

use crate::RebornRuntime;
use crate::operator_tool_catalog::ActiveRegistryOperatorToolCatalog;

/// `surface` with the config-sync views and command in front. A host
/// without a boot config (no data folder to name) is returned as it is.
pub(crate) fn wrap(
    surface: Arc<dyn ProductSurface>,
    runtime: &RebornRuntime,
) -> Arc<dyn ProductSurface> {
    let Some(boot) = runtime.webui_boot_config() else {
        return surface;
    };
    let home = boot.home();
    let user_home = std::env::var("HOME").unwrap_or_default(); // silent-ok: no HOME rewrites no home paths
    let mut service = ConfigSyncService::new(
        Arc::new(runtime.environment_access_store()),
        home.path().display().to_string(),
        user_home.clone(),
    )
    .with_handler(Arc::new(ConfigDomainHandler::new(home.config_file_path())))
    .with_handler(Arc::new(ApprovalsDomainHandler::new(
        runtime.auto_approve_settings.clone(),
        runtime.tool_permission_overrides.clone(),
        runtime.persistent_approval_policies.clone(),
        Arc::new(ActiveRegistryOperatorToolCatalog::new(
            runtime.shared_extension_registry.clone(),
            Vec::new(),
            Some(runtime.extension_management.clone()),
        )),
    )))
    .with_handler(Arc::new(SkillsDomainHandler::new(Arc::clone(
        &runtime.skill_management,
    ))))
    .with_handler(Arc::new(SkillUsageDomainHandler::new(
        runtime.skill_usage.clone(),
    )));
    // The coding-agent CLIs' own rules and skills files.
    if !user_home.is_empty() {
        service = service.with_handler(Arc::new(CliHomeDomainHandler::for_home(user_home.into())));
    }
    // Only the native provider's memory is documents this host can carry.
    if let Some(documents) = runtime.memory_page.documents.clone() {
        service = service.with_handler(Arc::new(MemoryDomainHandler::new(
            documents,
            Arc::new(SettingsMemorySyncBase::new(Arc::clone(
                &runtime.extension_filesystem,
            ))),
        )));
    }
    if let Some(engine) = runtime.orchestrator.as_ref() {
        service = service
            .with_handler(Arc::new(OrchestratorSettingsDomainHandler::new(
                Arc::clone(engine),
            )))
            .with_handler(Arc::new(ProfilesDomainHandler::new(Arc::clone(engine))));
    }
    if let Some((storage_root, prompt_path)) = runtime.system_prompt_paths.clone() {
        service = service.with_handler(Arc::new(PromptsDomainHandler::new(
            storage_root,
            prompt_path,
        )));
    }
    Arc::new(ConfigSyncSurface::new(surface, Arc::new(service)))
}
