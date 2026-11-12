//! Live reload of `config.toml` for a running server.
//!
//! [`RebornConfigReloader`] is the one place a reload is decided: it re-reads
//! the operator's `config.toml`, validates it with the same parser boot uses,
//! diffs it against what is in effect (classification is owned by
//! `ironclaw_config`), applies the live-reloadable part through its owner
//! seam, and reports every boot-time key that changed as restart-required.
//!
//! What "in effect" means:
//!
//! - Boot-time keys: the snapshot the server actually booted from, handed in
//!   by the caller (`zlo serve` passes the file it parsed at startup). It is
//!   never re-read, so an edit made while the server was starting is still
//!   reported instead of being mistaken for the boot value.
//! - `llm.default`: whatever [`LiveLlmDefault`] last applied — at boot, from
//!   Settings → Inference, or from an earlier reload. All of those share that
//!   one record, so a hand edit that reverts a Settings change is applied, and
//!   a reload after a Settings save does not rebuild the chain again.
//!
//! Fail-closed contract:
//!
//! - A file that does not parse or validate, or that carries a retired setup
//!   key boot would refuse, applies nothing; the previous configuration stays
//!   fully in effect and the report carries a redacted reason.
//! - A live seam that refuses the new value (selection does not resolve, or
//!   the LLM chain will not build) applies nothing either. The chain is built
//!   before the swap, so a refusal never leaves a half-swapped provider.
//! - Restart-required keys are never applied, and their baseline never
//!   advances, so they are reported on every reload until restart.
//!
//! Serialization: a reload holds the [`LiveLlmDefault`] lock from before it
//! reads the file until the swap is recorded. Settings saves, login flows and
//! the boot apply take the same lock around their own file read, resolution,
//! key read and swap, so the two entry points never interleave: whichever runs
//! second reads the file the first one left and diffs against what the first
//! one applied.

use std::path::PathBuf;
use std::sync::Arc;

use ironclaw_config::{
    ConfigReloadOutcome, ConfigReloadReport, LlmSlotSelection, RebornConfigFile, diff_config_files,
};

use crate::LiveLlmDefault;

/// The `config.toml` key the LLM seam owns.
const LLM_DEFAULT_KEY: &str = "llm.default";

pub struct RebornConfigReloader {
    config_path: PathBuf,
    llm: Option<Arc<LiveLlmDefault>>,
    /// The file the server booted from (`None` = no file). Immutable: only
    /// `llm.default` can change live, and that is tracked by `llm`.
    boot_file: Option<RebornConfigFile>,
}

impl RebornConfigReloader {
    /// `boot_file` is the snapshot the server's boot-time decisions were made
    /// from. `llm` is the live LLM seam; without it, an `[llm.default]` edit
    /// is restart-required.
    pub fn new(
        config_path: PathBuf,
        boot_file: Option<RebornConfigFile>,
        llm: Option<Arc<LiveLlmDefault>>,
    ) -> Self {
        Self {
            config_path,
            llm,
            boot_file,
        }
    }

    /// Re-read `config.toml` and apply what can be applied live.
    pub async fn reload(&self) -> ConfigReloadReport {
        // Locked before the read: see the module doc.
        let mut llm = match self.llm.as_ref() {
            Some(live) => Some(live.lock().await),
            None => None,
        };
        let next = match RebornConfigFile::load(&self.config_path) {
            Ok(next) => next,
            Err(error) => return ConfigReloadReport::rejected(error.redacted_reason(), Vec::new()),
        };
        // Boot refuses a retired setup key (`serve` fails closed on it), but
        // retired sections are invisible to the typed diff, so check here.
        if let Some(file) = next.as_ref()
            && let Err(error) = file.retired_section_migration(&self.config_path)
        {
            return ConfigReloadReport::rejected(error.to_string(), Vec::new());
        }
        let baseline = match llm.as_ref() {
            Some(guard) => Some(with_default_llm_slot(
                self.boot_file.as_ref(),
                guard.applied(),
            )),
            None => self.boot_file.clone(),
        };
        let changes = match diff_config_files(baseline.as_ref(), next.as_ref()) {
            Ok(changes) => changes,
            Err(error) => return ConfigReloadReport::rejected(error.to_string(), Vec::new()),
        };
        if changes.is_empty() {
            return ConfigReloadReport::unchanged();
        }

        let can_apply_llm = llm.is_some()
            && next
                .as_ref()
                .and_then(RebornConfigFile::default_llm_slot)
                .is_some();
        let mut restart_required = changes.restart_required;
        let mut live = Vec::new();
        for key in changes.live {
            // Clearing the selection has no live meaning: the swap seam can
            // only replace a provider, not unwire one, so the old provider
            // would keep serving while the report claimed otherwise.
            if key == LLM_DEFAULT_KEY && can_apply_llm {
                live.push(key);
            } else {
                restart_required.push(key);
            }
        }
        restart_required.sort();

        if live.is_empty() {
            return ConfigReloadReport {
                outcome: ConfigReloadOutcome::RestartRequired,
                applied: Vec::new(),
                restart_required,
            };
        }

        // Only `llm.default` is live today, and it was admitted above only
        // with the seam present.
        if let Some(guard) = llm.as_mut()
            && let Err(reason) = guard.apply(next.as_ref()).await
        {
            return ConfigReloadReport::rejected(
                format!("[{LLM_DEFAULT_KEY}] could not be applied: {reason}"),
                restart_required,
            );
        }
        ConfigReloadReport {
            outcome: ConfigReloadOutcome::Applied,
            applied: live,
            restart_required,
        }
    }
}

/// `base` with its `[llm.default]` replaced by `slot` (removed when `None`).
fn with_default_llm_slot(
    base: Option<&RebornConfigFile>,
    slot: Option<&LlmSlotSelection>,
) -> RebornConfigFile {
    let mut file = base.cloned().unwrap_or_default();
    let slots = file.llm.get_or_insert_with(Default::default);
    match slot {
        Some(slot) => {
            slots.insert("default".to_string(), slot.clone());
        }
        None => {
            slots.remove("default");
        }
    }
    file
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use ironclaw_config::{RebornBootConfig, RebornHome, RebornProfile};
    use ironclaw_host_api::ids::{AgentId, ProjectId, TenantId, UserId};
    use ironclaw_product_contracts::operator_llm::{LlmConfigService, SetActiveLlmRequest};
    use ironclaw_product_contracts::surface::ProductSurfaceCaller;
    use ironclaw_product_contracts::test_support::fakes::FakeOperatorSecretValueStore;
    use tokio::sync::Notify;

    use crate::{LlmKeyStore, LlmReloadTrigger, RebornLlmConfigService};

    /// One armed pause: the next swap signals `entered`, then waits for
    /// `release` before it records — standing in for the stored-key read and
    /// chain build that sit between resolution and the swap.
    struct Pause {
        entered: Arc<Notify>,
        release: Arc<Notify>,
    }

    /// The unserialized resolve-and-swap seam. Records the model of every
    /// swap that completed, in completion order; fails when told to.
    #[derive(Default)]
    struct RecordingSwap {
        swapped_models: Mutex<Vec<Option<String>>>,
        fail: Mutex<bool>,
        pause: Mutex<Option<Pause>>,
    }

    impl RecordingSwap {
        fn swapped(&self) -> Vec<Option<String>> {
            self.swapped_models.lock().expect("lock").clone()
        }

        fn arm_pause(&self) -> (Arc<Notify>, Arc<Notify>) {
            let entered = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            *self.pause.lock().expect("lock") = Some(Pause {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
            });
            (entered, release)
        }
    }

    #[async_trait]
    impl LlmReloadTrigger for RecordingSwap {
        async fn reload(&self) -> Result<(), String> {
            Err("the swap seam is only driven through LiveLlmDefault".to_string())
        }

        async fn reload_from_config(
            &self,
            config_file: Option<&RebornConfigFile>,
        ) -> Result<(), String> {
            let model = config_file
                .and_then(RebornConfigFile::default_llm_slot)
                .and_then(|slot| slot.model.clone());
            let pause = self.pause.lock().expect("lock").take();
            if let Some(pause) = pause {
                pause.entered.notify_one();
                pause.release.notified().await;
            }
            if *self.fail.lock().expect("lock") {
                return Err("provider chain refused the selection".to_string());
            }
            self.swapped_models.lock().expect("lock").push(model);
            Ok(())
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        path: PathBuf,
        boot: RebornBootConfig,
        swap: Arc<RecordingSwap>,
        live: Arc<LiveLlmDefault>,
        reloader: Arc<RebornConfigReloader>,
    }

    impl Fixture {
        fn write(&self, text: &str) {
            std::fs::write(&self.path, text).expect("write config");
        }

        /// The Settings → Inference service wired to the same live seam the
        /// runtime gives it.
        fn settings(&self) -> RebornLlmConfigService {
            RebornLlmConfigService::new(
                self.boot.clone(),
                LlmKeyStore::new(Arc::new(FakeOperatorSecretValueStore::new())),
            )
            .with_reload_trigger(Arc::clone(&self.live) as Arc<dyn LlmReloadTrigger>)
        }
    }

    async fn settings_pick(service: &RebornLlmConfigService, model: &str) {
        let caller = ProductSurfaceCaller::new(
            TenantId::new("tenant-alpha").expect("tenant"),
            UserId::new("user-alpha").expect("user"),
            Some(AgentId::new("agent-alpha").expect("agent")),
            Some(ProjectId::new("project-alpha").expect("project")),
        )
        .with_operator_config(true);
        service
            .set_active(
                caller,
                SetActiveLlmRequest {
                    provider_id: "nearai".to_string(),
                    model: Some(model.to_string()),
                },
            )
            .await
            .expect("settings save");
    }

    /// Boot the way the runtime does: parse the file once for boot-time
    /// decisions, apply `[llm.default]` through the live seam, and hand the
    /// parsed snapshot to the reloader.
    async fn fixture(initial: &str, with_llm: bool) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = RebornHome::resolve_from_env_parts(
            Some(dir.path().as_os_str().to_os_string()),
            None,
            None,
        )
        .expect("home");
        let path = home.config_file_path();
        std::fs::write(&path, initial).expect("write config");
        let boot = RebornBootConfig::new(home, RebornProfile::Standalone);
        let boot_file = RebornConfigFile::load(&path).expect("boot config parses");
        let swap = Arc::new(RecordingSwap::default());
        let live = Arc::new(LiveLlmDefault::new(
            path.clone(),
            Arc::clone(&swap) as Arc<dyn LlmReloadTrigger>,
        ));
        live.reload().await.expect("boot apply");
        swap.swapped_models.lock().expect("lock").clear();
        let reloader = Arc::new(RebornConfigReloader::new(
            path.clone(),
            boot_file,
            with_llm.then(|| Arc::clone(&live)),
        ));
        Fixture {
            _dir: dir,
            path,
            boot,
            swap,
            live,
            reloader,
        }
    }

    const MODEL_A: &str = "[llm.default]\nprovider_id = \"nearai\"\nmodel = \"a\"\n";
    const MODEL_B: &str = "[llm.default]\nprovider_id = \"nearai\"\nmodel = \"b\"\n";
    const MODEL_D: &str = "[llm.default]\nprovider_id = \"nearai\"\nmodel = \"d\"\n";

    fn some(model: &str) -> Option<String> {
        Some(model.to_string())
    }

    #[tokio::test]
    async fn untouched_file_is_unchanged() {
        let f = fixture(MODEL_A, true).await;
        assert_eq!(f.reloader.reload().await, ConfigReloadReport::unchanged());
        assert!(f.swap.swapped().is_empty());
    }

    #[tokio::test]
    async fn model_change_is_applied_once_then_unchanged() {
        let f = fixture(MODEL_A, true).await;
        f.write(MODEL_B);
        let report = f.reloader.reload().await;
        assert_eq!(report.outcome, ConfigReloadOutcome::Applied);
        assert_eq!(report.applied, vec!["llm.default"]);
        assert_eq!(f.swap.swapped(), vec![some("b")]);
        assert_eq!(f.reloader.reload().await, ConfigReloadReport::unchanged());
    }

    /// Finding: Settings swapped B without the reloader knowing, so a hand
    /// revert to the boot value A diffed as "unchanged" and B kept serving.
    #[tokio::test]
    async fn hand_revert_after_a_settings_save_is_applied() {
        let f = fixture(MODEL_A, true).await;
        settings_pick(&f.settings(), "b").await;
        assert_eq!(f.swap.swapped(), vec![some("b")]);

        f.write(MODEL_A);
        let report = f.reloader.reload().await;
        assert_eq!(report.outcome, ConfigReloadOutcome::Applied, "{report}");
        assert_eq!(f.swap.swapped(), vec![some("b"), some("a")]);
    }

    /// The reverse: an untouched file after a Settings save is already in
    /// effect and must not rebuild the chain.
    #[tokio::test]
    async fn unedited_reload_after_a_settings_save_is_unchanged() {
        let f = fixture(MODEL_A, true).await;
        settings_pick(&f.settings(), "b").await;
        assert_eq!(f.reloader.reload().await, ConfigReloadReport::unchanged());
        assert_eq!(f.swap.swapped(), vec![some("b")]);
    }

    /// Lets a racing task run as far as it can; with correct serialization it
    /// parks on the lock, so this only bounds how hard the race is driven.
    async fn let_race_run() {
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    /// SIGHUP resolved B and is mid-swap when Settings saves C. Settings must
    /// wait and then apply C; disk, UI and requests all end on C.
    #[tokio::test]
    async fn settings_save_during_a_reload_waits_and_wins() {
        let f = fixture(MODEL_A, true).await;
        f.write(MODEL_B);
        let (entered, release) = f.swap.arm_pause();
        let reloader = Arc::clone(&f.reloader);
        let sighup = tokio::spawn(async move { reloader.reload().await });
        entered.notified().await;

        let settings = f.settings();
        let save = tokio::spawn(async move { settings_pick(&settings, "c").await });
        let_race_run().await;
        assert!(
            f.swap.swapped().is_empty(),
            "Settings swapped while the reload held the selection"
        );
        release.notify_one();
        let report = sighup.await.expect("sighup task");
        save.await.expect("settings task");

        assert_eq!(report.outcome, ConfigReloadOutcome::Applied, "{report}");
        assert_eq!(f.swap.swapped(), vec![some("b"), some("c")]);
        let on_disk = RebornConfigFile::load(&f.path)
            .expect("parse")
            .and_then(|file| file.default_llm_slot().and_then(|slot| slot.model.clone()));
        assert_eq!(on_disk, some("c"));
        assert_eq!(f.reloader.reload().await, ConfigReloadReport::unchanged());
    }

    /// Settings is mid-swap on C when the operator hand-edits D and sends
    /// SIGHUP. The reload must wait, then read D and diff it against C.
    #[tokio::test]
    async fn reload_during_a_settings_save_waits_and_diffs_against_it() {
        let f = fixture(MODEL_A, true).await;
        let (entered, release) = f.swap.arm_pause();
        let settings = f.settings();
        let save = tokio::spawn(async move { settings_pick(&settings, "c").await });
        entered.notified().await;

        f.write(MODEL_D);
        let reloader = Arc::clone(&f.reloader);
        let sighup = tokio::spawn(async move { reloader.reload().await });
        let_race_run().await;
        assert!(
            f.swap.swapped().is_empty(),
            "the reload swapped while Settings held the selection"
        );
        release.notify_one();
        save.await.expect("settings task");
        let report = sighup.await.expect("sighup task");

        assert_eq!(report.outcome, ConfigReloadOutcome::Applied, "{report}");
        assert_eq!(f.swap.swapped(), vec![some("c"), some("d")]);
        assert_eq!(f.reloader.reload().await, ConfigReloadReport::unchanged());
    }

    /// Settings is mid-swap on C and SIGHUP arrives with the file untouched
    /// since: nothing to do once Settings finishes.
    #[tokio::test]
    async fn reload_during_a_settings_save_of_the_same_file_is_unchanged() {
        let f = fixture(MODEL_A, true).await;
        let (entered, release) = f.swap.arm_pause();
        let settings = f.settings();
        let save = tokio::spawn(async move { settings_pick(&settings, "c").await });
        entered.notified().await;

        let reloader = Arc::clone(&f.reloader);
        let sighup = tokio::spawn(async move { reloader.reload().await });
        let_race_run().await;
        release.notify_one();
        save.await.expect("settings task");
        let report = sighup.await.expect("sighup task");

        assert_eq!(report, ConfigReloadReport::unchanged());
        assert_eq!(f.swap.swapped(), vec![some("c")]);
    }

    /// The boot snapshot, not a later re-read, is the restart-required
    /// baseline: a boot-time key edited after boot parsed the file is still
    /// reported.
    #[tokio::test]
    async fn edit_made_after_the_boot_parse_is_reported() {
        let f = fixture(MODEL_A, true).await;
        // Written after boot parsed the file, before any reload.
        f.write(&format!("{MODEL_A}[webui]\nlisten_port = 4111\n"));
        let report = f.reloader.reload().await;
        assert_eq!(report.outcome, ConfigReloadOutcome::RestartRequired);
        assert_eq!(report.restart_required, vec!["webui"]);
    }

    #[tokio::test]
    async fn invalid_file_applies_nothing_and_keeps_baseline() {
        let f = fixture(MODEL_A, true).await;
        f.write("[llm.default\nmodel = sk-live-0123456789abcdef0123456789\n");
        let report = f.reloader.reload().await;
        let ConfigReloadOutcome::Rejected { reason } = &report.outcome else {
            panic!("expected rejection, got {report:?}");
        };
        assert!(
            !reason.contains("sk-live"),
            "reason leaked file text: {reason}"
        );
        assert!(f.swap.swapped().is_empty());

        // Fixing the file afterwards applies against the untouched baseline.
        f.write(MODEL_B);
        assert_eq!(
            f.reloader.reload().await.outcome,
            ConfigReloadOutcome::Applied
        );
    }

    /// Boot refuses a retired setup key; a reload must not report the same
    /// file as "unchanged" just because retired sections skip the typed diff.
    #[tokio::test]
    async fn retired_setup_key_is_rejected_like_boot() {
        let f = fixture(MODEL_A, true).await;
        f.write(&format!("{MODEL_A}[slack]\nteam_id = \"T123\"\n"));
        let report = f.reloader.reload().await;
        let ConfigReloadOutcome::Rejected { reason } = &report.outcome else {
            panic!("expected rejection, got {report:?}");
        };
        assert!(reason.contains("[slack]"), "{reason}");
        assert!(f.swap.swapped().is_empty());
    }

    #[tokio::test]
    async fn refused_llm_swap_rejects_and_retries_next_time() {
        let f = fixture(MODEL_A, true).await;
        *f.swap.fail.lock().expect("lock") = true;
        f.write(MODEL_B);
        let report = f.reloader.reload().await;
        assert!(
            matches!(report.outcome, ConfigReloadOutcome::Rejected { .. }),
            "{report:?}"
        );
        assert!(report.applied.is_empty());

        // The baseline did not advance, so the same edit is retried.
        *f.swap.fail.lock().expect("lock") = false;
        assert_eq!(
            f.reloader.reload().await.outcome,
            ConfigReloadOutcome::Applied
        );
    }

    #[tokio::test]
    async fn boot_time_keys_are_reported_every_time_and_never_applied() {
        let f = fixture(MODEL_A, true).await;
        f.write(&format!(
            "{MODEL_A}[webui]\nlisten_port = 4111\n[identity]\ntenant = \"t2\"\n"
        ));
        for _ in 0..2 {
            let report = f.reloader.reload().await;
            assert_eq!(report.outcome, ConfigReloadOutcome::RestartRequired);
            assert_eq!(report.restart_required, vec!["identity", "webui"]);
        }
        assert!(f.swap.swapped().is_empty());
    }

    #[tokio::test]
    async fn mixed_edit_applies_live_part_and_reports_the_rest() {
        let f = fixture(MODEL_A, true).await;
        f.write(&format!("{MODEL_B}[runner]\nworker_count = 3\n"));
        let report = f.reloader.reload().await;
        assert_eq!(report.outcome, ConfigReloadOutcome::Applied);
        assert_eq!(report.applied, vec!["llm.default"]);
        assert_eq!(report.restart_required, vec!["runner"]);
        // The applied key advanced; the boot-time one did not.
        let again = f.reloader.reload().await;
        assert_eq!(again.outcome, ConfigReloadOutcome::RestartRequired);
        assert_eq!(again.restart_required, vec!["runner"]);
    }

    #[tokio::test]
    async fn llm_edit_without_a_live_seam_is_restart_required() {
        let f = fixture(MODEL_A, false).await;
        f.write(MODEL_B);
        let report = f.reloader.reload().await;
        assert_eq!(report.outcome, ConfigReloadOutcome::RestartRequired);
        assert_eq!(report.restart_required, vec!["llm.default"]);
    }

    #[tokio::test]
    async fn clearing_the_default_slot_is_restart_required() {
        let f = fixture(MODEL_A, true).await;
        f.write("");
        let report = f.reloader.reload().await;
        assert_eq!(report.outcome, ConfigReloadOutcome::RestartRequired);
        assert_eq!(report.restart_required, vec!["llm.default"]);
        assert!(f.swap.swapped().is_empty());
    }
}
