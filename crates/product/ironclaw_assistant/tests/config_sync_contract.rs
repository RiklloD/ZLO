//! Configuration sync at the service seam: two hosts ("mac" and "box") with
//! their own `config.toml` and `SYSTEM.md`, one snapshot exported from the
//! first and applied to the second. Covers the file-backed domains
//! (`config`, `prompts`); the store-backed ones are covered where their
//! stores are assembled.

#![deny(clippy::let_underscore_must_use)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_assistant::config_sync::ConfigSyncService;
use ironclaw_assistant::config_sync::config::ConfigDomainHandler;
use ironclaw_assistant::config_sync::prompts::PromptsDomainHandler;
use ironclaw_host_api::ids::{TenantId, UserId};
use ironclaw_product_contracts::config_sync::{
    ConfigSyncApplyRequest, ConfigSyncDomain, ConfigSyncExportRequest, DomainApplyStatus,
};
use ironclaw_product_contracts::environment_access::{
    DeviceGrant, DeviceId, DeviceList, EnvironmentAccessError, EnvironmentAccessService,
    EnvironmentId, EnvironmentIdentity, PairClient, PairedDevice, PairingCodeIssued,
    PairingCodeRequest,
};
use ironclaw_product_contracts::surface::ProductSurfaceCaller;

/// Only the installation's identity is asked of it here.
struct Identity(&'static str);

#[async_trait]
impl EnvironmentAccessService for Identity {
    async fn environment(&self) -> Result<EnvironmentIdentity, EnvironmentAccessError> {
        Ok(EnvironmentIdentity {
            environment_id: EnvironmentId::new(self.0).expect("environment id"),
            label: "test".into(),
        })
    }
    async fn issue_pairing_code(
        &self,
        _: PairingCodeRequest,
    ) -> Result<PairingCodeIssued, EnvironmentAccessError> {
        Err(EnvironmentAccessError::InvalidCode)
    }
    async fn redeem_pairing_code(
        &self,
        _: &str,
        _: PairClient,
    ) -> Result<PairedDevice, EnvironmentAccessError> {
        Err(EnvironmentAccessError::InvalidCode)
    }
    async fn pair_tailnet_owner(
        &self,
        _: &str,
        _: PairClient,
    ) -> Result<PairedDevice, EnvironmentAccessError> {
        Err(EnvironmentAccessError::InvalidCode)
    }
    async fn authenticate_device(
        &self,
        _: &str,
    ) -> Result<Option<DeviceGrant>, EnvironmentAccessError> {
        Ok(None)
    }
    async fn list_devices(
        &self,
        _: Option<&DeviceId>,
    ) -> Result<DeviceList, EnvironmentAccessError> {
        Err(EnvironmentAccessError::DeviceNotFound)
    }
    async fn revoke_device(&self, _: &DeviceId) -> Result<(), EnvironmentAccessError> {
        Err(EnvironmentAccessError::DeviceNotFound)
    }
}

struct Host {
    _dir: tempfile::TempDir,
    config: PathBuf,
    prompt: PathBuf,
    service: ConfigSyncService,
}

fn host(environment_id: &'static str, config_toml: &str, system_md: &str) -> Host {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = dir.path().join("config.toml");
    std::fs::write(&config, config_toml).expect("config");
    let prompt = dir.path().join("system/prompts/default-system.md");
    std::fs::create_dir_all(prompt.parent().expect("parent")).expect("dirs");
    std::fs::write(&prompt, system_md).expect("prompt");
    let service = ConfigSyncService::new(
        Arc::new(Identity(environment_id)),
        dir.path().display().to_string(),
        dir.path().join("home").display().to_string(),
    )
    .with_handler(Arc::new(ConfigDomainHandler::new(config.clone())))
    .with_handler(Arc::new(PromptsDomainHandler::new(
        dir.path().to_path_buf(),
        prompt.clone(),
    )));
    Host {
        _dir: dir,
        config,
        prompt,
        service,
    }
}

fn caller() -> ProductSurfaceCaller {
    ProductSurfaceCaller::new(
        TenantId::new("tenant-a").expect("tenant"),
        UserId::new("alice").expect("user"),
        None,
        None,
    )
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).expect("read")
}

const MAC: &str = "0f6c2d0e-3b0a-4a51-9b7e-6b1c8f1f2a10";
const BOX: &str = "1a7d3e1f-4c1b-4b62-8c8f-7c2d9a2a3b21";

#[tokio::test]
async fn a_snapshot_from_one_host_lands_on_the_other_and_keeps_its_local_sections() {
    let mac = host(
        MAC,
        "[webui]\nlisten_port = 3000\n\n[budget]\nuser_daily_usd = 25.0\n\n[runner]\nworker_count = 4\n",
        "You are the Mac's assistant.\n",
    );
    let boxed = host(
        BOX,
        "[webui]\nlisten_port = 9090\n\n[remote]\nenabled = true\nowner_logins = [\"box@example.com\"]\ntailnet_url = \"https://box.tail0000.ts.net\"\n\n[budget]\nuser_daily_usd = 5.0\n",
        "You are the box's assistant.\n",
    );

    let before = boxed.service.manifest(caller()).await.expect("manifest");
    let source = mac.service.manifest(caller()).await.expect("manifest");
    assert_ne!(
        before.domains[&ConfigSyncDomain::Config].hash,
        source.domains[&ConfigSyncDomain::Config].hash
    );
    assert_eq!(source.environment_id, MAC);

    let snapshot = mac
        .service
        .export(caller(), ConfigSyncExportRequest::default())
        .await
        .expect("export");
    assert_eq!(snapshot.source_environment_id, MAC);
    let report = boxed
        .service
        .apply(
            caller(),
            ConfigSyncApplyRequest {
                snapshot: snapshot.clone(),
                domains: Vec::new(),
            },
        )
        .await
        .expect("apply");
    assert_eq!(report.target_environment_id, BOX);
    for domain in [ConfigSyncDomain::Config, ConfigSyncDomain::Prompts] {
        assert_eq!(
            report.domains[&domain].status,
            DomainApplyStatus::Applied,
            "{domain}"
        );
    }
    assert!(
        report.restart_required.iter().any(|s| s == "budget"),
        "a changed budget needs a restart: {:?}",
        report.restart_required
    );

    // The portable sections arrived; the box's own listener and remote
    // access are what they were.
    let text = read(&boxed.config);
    assert!(text.contains("user_daily_usd = 25.0"), "{text}");
    assert!(text.contains("worker_count = 4"), "{text}");
    assert!(text.contains("listen_port = 9090"), "{text}");
    assert!(text.contains("box@example.com"), "{text}");
    assert!(!text.contains("listen_port = 3000"), "{text}");
    assert_eq!(read(&boxed.prompt), "You are the Mac's assistant.\n");
    // The source is untouched.
    assert!(read(&mac.config).contains("listen_port = 3000"));

    // Now equal: the manifests agree, and a second apply changes nothing.
    let after = boxed.service.manifest(caller()).await.expect("manifest");
    for domain in [ConfigSyncDomain::Config, ConfigSyncDomain::Prompts] {
        assert_eq!(after.domains[&domain].hash, source.domains[&domain].hash);
    }
    let again = boxed
        .service
        .apply(
            caller(),
            ConfigSyncApplyRequest {
                snapshot,
                domains: Vec::new(),
            },
        )
        .await
        .expect("apply again");
    for domain in [ConfigSyncDomain::Config, ConfigSyncDomain::Prompts] {
        assert_eq!(
            again.domains[&domain].status,
            DomainApplyStatus::SkippedUnchanged,
            "{domain}"
        );
    }
    assert!(again.restart_required.is_empty());
}

#[tokio::test]
async fn only_the_asked_domains_are_applied() {
    let mac = host(MAC, "[budget]\nuser_daily_usd = 25.0\n", "Mac prompt\n");
    let boxed = host(BOX, "[budget]\nuser_daily_usd = 5.0\n", "Box prompt\n");
    let snapshot = mac
        .service
        .export(caller(), ConfigSyncExportRequest::default())
        .await
        .expect("export");
    boxed
        .service
        .apply(
            caller(),
            ConfigSyncApplyRequest {
                snapshot,
                domains: vec![ConfigSyncDomain::Prompts],
            },
        )
        .await
        .expect("apply");
    assert_eq!(read(&boxed.prompt), "Mac prompt\n");
    assert!(read(&boxed.config).contains("user_daily_usd = 5.0"));
}

// ── store-backed domains: profiles and orchestrator settings ─────────────

use ironclaw_assistant::config_sync::orchestrator_settings::OrchestratorSettingsDomainHandler;
use ironclaw_assistant::config_sync::profiles::ProfilesDomainHandler;
use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::profiles::ProfileOwner;
use ironclaw_product_contracts::profiles::ProfileSettings;

struct StoreHost {
    _dir: tempfile::TempDir,
    home: String,
    engine: Arc<Engine>,
    service: ConfigSyncService,
}

async fn store_host(environment_id: &'static str) -> StoreHost {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("o.db"))
            .build()
            .await
            .expect("db"),
    );
    let runtime = Arc::new(ironclaw_libsql_runtime::LibSqlRuntime::new(db).expect("runtime"));
    let engine = Engine::open_libsql(runtime, "ironclaw")
        .await
        .expect("engine");
    let home = dir.path().join("home").display().to_string();
    let service = ConfigSyncService::new(
        Arc::new(Identity(environment_id)),
        dir.path().join("zlo").display().to_string(),
        home.clone(),
    )
    .with_handler(Arc::new(ProfilesDomainHandler::new(Arc::clone(&engine))))
    .with_handler(Arc::new(OrchestratorSettingsDomainHandler::new(
        Arc::clone(&engine),
    )));
    StoreHost {
        _dir: dir,
        home,
        engine,
        service,
    }
}

fn owner() -> ProfileOwner {
    ProfileOwner {
        tenant_id: TenantId::new("tenant-a").expect("tenant"),
        user_id: UserId::new("alice").expect("user"),
    }
}

fn profile(name: &str, soul: &str, folder: Option<String>) -> ProfileSettings {
    ProfileSettings {
        display_name: name.into(),
        soul: soul.into(),
        working_folder: folder,
        ..ProfileSettings::default()
    }
}

async fn profile_of(host: &StoreHost, id: &str) -> Option<ProfileSettings> {
    host.engine
        .store()
        .list_profiles_for_sync(&owner())
        .await
        .expect("profiles")
        .into_iter()
        .find(|p| p.profile_id.as_str() == id)
        .map(|p| p.settings)
}

#[tokio::test]
async fn profiles_and_settings_follow_the_source_and_keep_what_only_the_target_has() {
    let mac = store_host(MAC).await;
    let boxed = store_host(BOX).await;
    let id = |raw: &str| ironclaw_orchestrator::model::ProfileId::new(raw).expect("profile id");

    // The Mac: a profile working in a folder under its home, and a setting.
    mac.engine
        .store()
        .create_profile(
            &owner(),
            &id("research"),
            &profile(
                "Research",
                "Be thorough.",
                Some(format!("{}/src/research", mac.home)),
            ),
        )
        .await
        .expect("mac profile");
    mac.engine
        .store()
        .set_settings(&serde_json::json!({ "max_depth": 7 }))
        .await
        .expect("mac settings");
    // The box: a profile of its own, and the same id with other words.
    boxed
        .engine
        .store()
        .create_profile(&owner(), &id("box-only"), &profile("Box only", "", None))
        .await
        .expect("box profile");
    boxed
        .engine
        .store()
        .create_profile(
            &owner(),
            &id("research"),
            &profile("Old name", "Old.", None),
        )
        .await
        .expect("box research");

    let snapshot = mac
        .service
        .export(caller(), ConfigSyncExportRequest::default())
        .await
        .expect("export");
    // Nothing of the Mac's own home leaves it.
    let wire = serde_json::to_string(&snapshot).expect("json");
    assert!(
        !wire.contains(&mac.home),
        "home paths are tokens on the wire"
    );

    let report = boxed
        .service
        .apply(
            caller(),
            ConfigSyncApplyRequest {
                snapshot: snapshot.clone(),
                domains: Vec::new(),
            },
        )
        .await
        .expect("apply");
    assert_eq!(
        report.domains[&ConfigSyncDomain::Profiles].status,
        DomainApplyStatus::Applied
    );

    // The Mac's version won, with its folder under the box's home.
    let research = profile_of(&boxed, "research").await.expect("research");
    assert_eq!(research.display_name, "Research");
    assert_eq!(research.soul, "Be thorough.");
    assert_eq!(
        research.working_folder.as_deref(),
        Some(format!("{}/src/research", boxed.home).as_str())
    );
    // What only the box had is still there.
    assert!(profile_of(&boxed, "box-only").await.is_some());
    assert_eq!(
        boxed
            .engine
            .store()
            .settings()
            .await
            .expect("settings")
            .max_depth,
        7
    );

    // A second apply finds nothing to do.
    let again = boxed
        .service
        .apply(
            caller(),
            ConfigSyncApplyRequest {
                snapshot,
                domains: Vec::new(),
            },
        )
        .await
        .expect("apply again");
    for domain in [
        ConfigSyncDomain::Profiles,
        ConfigSyncDomain::OrchestratorSettings,
    ] {
        assert_eq!(
            again.domains[&domain].status,
            DomainApplyStatus::SkippedUnchanged,
            "{domain}"
        );
    }
}
