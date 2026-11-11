//! Live reload of `config.toml` for a running server.
//!
//! [`RebornConfigReloader`] is the one place a reload is decided: it re-reads
//! the operator's `config.toml`, validates it with the same parser boot uses,
//! diffs it against what is in effect (classification is owned by
//! `ironclaw_config`), applies the live-reloadable part through its owner
//! seam, and reports every boot-time key that changed as restart-required.
//!
//! Fail-closed contract:
//!
//! - A file that does not parse or validate applies nothing; the previous
//!   configuration stays fully in effect and the report carries a redacted
//!   reason.
//! - A live seam that refuses the new value (the LLM chain will not build for
//!   it) applies nothing either. The LLM seam builds the new provider chain
//!   before swapping, so a refusal never leaves a half-swapped provider.
//! - Restart-required keys are never applied. The in-effect baseline keeps
//!   their boot values, so they are reported on every reload until restart.
//!
//! Reloads are serialized: a SIGHUP racing an operator-surface save waits for
//! the first reload to finish rather than interleaving two baseline updates.

use std::path::PathBuf;
use std::sync::Arc;

use ironclaw_config::{
    ConfigReloadOutcome, ConfigReloadReport, RebornBootConfig, RebornConfigFile,
    RebornConfigFileError, diff_config_files,
};

use crate::LlmReloadTrigger;

/// The `config.toml` key the LLM seam owns.
const LLM_DEFAULT_KEY: &str = "llm.default";

pub struct RebornConfigReloader {
    config_path: PathBuf,
    llm: Option<Arc<dyn LlmReloadTrigger>>,
    /// The configuration currently in effect: the file as loaded at boot, with
    /// each successfully applied live key advanced to its new value.
    in_effect: tokio::sync::Mutex<Option<RebornConfigFile>>,
}

impl RebornConfigReloader {
    /// Capture the boot-time file as the in-effect baseline. `llm` is the live
    /// LLM seam; without it, an `[llm.default]` edit is restart-required.
    pub fn new(
        boot: &RebornBootConfig,
        llm: Option<Arc<dyn LlmReloadTrigger>>,
    ) -> Result<Self, RebornConfigFileError> {
        let config_path = boot.home().config_file_path();
        let in_effect = RebornConfigFile::load(&config_path)?;
        Ok(Self {
            config_path,
            llm,
            in_effect: tokio::sync::Mutex::new(in_effect),
        })
    }

    /// Re-read `config.toml` and apply what can be applied live.
    pub async fn reload(&self) -> ConfigReloadReport {
        let mut in_effect = self.in_effect.lock().await;
        let next = match RebornConfigFile::load(&self.config_path) {
            Ok(next) => next,
            Err(error) => return ConfigReloadReport::rejected(error.redacted_reason(), Vec::new()),
        };
        let changes = match diff_config_files(in_effect.as_ref(), next.as_ref()) {
            Ok(changes) => changes,
            Err(error) => return ConfigReloadReport::rejected(error.to_string(), Vec::new()),
        };
        if changes.is_empty() {
            return ConfigReloadReport::unchanged();
        }

        let mut restart_required = changes.restart_required;
        let mut live = Vec::new();
        for key in changes.live {
            if self.can_apply_live(&key, next.as_ref()) {
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

        // Only `llm.default` is live today, and `can_apply_live` admitted it
        // only with the seam present.
        if let Some(llm) = self.llm.as_ref()
            && let Err(reason) = llm.reload_from_config(next.as_ref()).await
        {
            return ConfigReloadReport::rejected(
                format!("[{LLM_DEFAULT_KEY}] could not be applied: {reason}"),
                restart_required,
            );
        }

        let mut advanced = in_effect.take().unwrap_or_default();
        advance_default_llm_slot(&mut advanced, next.as_ref());
        *in_effect = Some(advanced);
        ConfigReloadReport {
            outcome: ConfigReloadOutcome::Applied,
            applied: live,
            restart_required,
        }
    }

    fn can_apply_live(&self, key: &str, next: Option<&RebornConfigFile>) -> bool {
        match key {
            // Clearing the selection has no live meaning: the swap seam can
            // only replace a provider, not unwire one, so the old provider
            // would keep serving while the report claimed otherwise.
            LLM_DEFAULT_KEY => {
                self.llm.is_some() && next.and_then(RebornConfigFile::default_llm_slot).is_some()
            }
            _ => false,
        }
    }
}

fn advance_default_llm_slot(in_effect: &mut RebornConfigFile, next: Option<&RebornConfigFile>) {
    let slot = next.and_then(RebornConfigFile::default_llm_slot).cloned();
    let slots = in_effect.llm.get_or_insert_with(Default::default);
    match slot {
        Some(slot) => {
            slots.insert("default".to_string(), slot);
        }
        None => {
            slots.remove("default");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use ironclaw_config::{RebornHome, RebornProfile};

    /// Records every config it was asked to apply; fails when told to.
    #[derive(Default)]
    struct RecordingLlm {
        applied_models: Mutex<Vec<Option<String>>>,
        fail: Mutex<bool>,
    }

    #[async_trait]
    impl LlmReloadTrigger for RecordingLlm {
        async fn reload(&self) -> Result<(), String> {
            Err("reload() must not be used; the reloader passes its validated file".to_string())
        }

        async fn reload_from_config(
            &self,
            config_file: Option<&RebornConfigFile>,
        ) -> Result<(), String> {
            if *self.fail.lock().expect("lock") {
                return Err("provider chain refused the selection".to_string());
            }
            let model = config_file
                .and_then(RebornConfigFile::default_llm_slot)
                .and_then(|slot| slot.model.clone());
            self.applied_models.lock().expect("lock").push(model);
            Ok(())
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        path: PathBuf,
        llm: Arc<RecordingLlm>,
        reloader: RebornConfigReloader,
    }

    fn fixture(initial: &str, with_llm: bool) -> Fixture {
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
        let llm = Arc::new(RecordingLlm::default());
        let seam = with_llm.then(|| Arc::clone(&llm) as Arc<dyn LlmReloadTrigger>);
        let reloader = RebornConfigReloader::new(&boot, seam).expect("reloader");
        Fixture {
            _dir: dir,
            path,
            llm,
            reloader,
        }
    }

    const MODEL_A: &str = "[llm.default]\nprovider_id = \"nearai\"\nmodel = \"a\"\n";
    const MODEL_B: &str = "[llm.default]\nprovider_id = \"nearai\"\nmodel = \"b\"\n";

    #[tokio::test]
    async fn untouched_file_is_unchanged() {
        let f = fixture(MODEL_A, true);
        assert_eq!(f.reloader.reload().await, ConfigReloadReport::unchanged());
        assert!(f.llm.applied_models.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn model_change_is_applied_once_then_unchanged() {
        let f = fixture(MODEL_A, true);
        std::fs::write(&f.path, MODEL_B).expect("write");
        let report = f.reloader.reload().await;
        assert_eq!(report.outcome, ConfigReloadOutcome::Applied);
        assert_eq!(report.applied, vec!["llm.default"]);
        assert_eq!(
            *f.llm.applied_models.lock().expect("lock"),
            vec![Some("b".to_string())]
        );
        assert_eq!(f.reloader.reload().await, ConfigReloadReport::unchanged());
    }

    #[tokio::test]
    async fn invalid_file_applies_nothing_and_keeps_baseline() {
        let f = fixture(MODEL_A, true);
        std::fs::write(
            &f.path,
            "[llm.default\nmodel = sk-live-0123456789abcdef0123456789\n",
        )
        .expect("write");
        let report = f.reloader.reload().await;
        let ConfigReloadOutcome::Rejected { reason } = &report.outcome else {
            panic!("expected rejection, got {report:?}");
        };
        assert!(
            !reason.contains("sk-live"),
            "reason leaked file text: {reason}"
        );
        assert!(f.llm.applied_models.lock().expect("lock").is_empty());

        // Fixing the file afterwards applies against the untouched baseline.
        std::fs::write(&f.path, MODEL_B).expect("write");
        assert_eq!(
            f.reloader.reload().await.outcome,
            ConfigReloadOutcome::Applied
        );
    }

    #[tokio::test]
    async fn refused_llm_swap_rejects_and_retries_next_time() {
        let f = fixture(MODEL_A, true);
        *f.llm.fail.lock().expect("lock") = true;
        std::fs::write(&f.path, MODEL_B).expect("write");
        let report = f.reloader.reload().await;
        assert!(
            matches!(report.outcome, ConfigReloadOutcome::Rejected { .. }),
            "{report:?}"
        );
        assert!(report.applied.is_empty());

        // The baseline did not advance, so the same edit is retried.
        *f.llm.fail.lock().expect("lock") = false;
        assert_eq!(
            f.reloader.reload().await.outcome,
            ConfigReloadOutcome::Applied
        );
    }

    #[tokio::test]
    async fn boot_time_keys_are_reported_every_time_and_never_applied() {
        let f = fixture(MODEL_A, true);
        std::fs::write(
            &f.path,
            format!("{MODEL_A}[webui]\nlisten_port = 4111\n[identity]\ntenant = \"t2\"\n"),
        )
        .expect("write");
        for _ in 0..2 {
            let report = f.reloader.reload().await;
            assert_eq!(report.outcome, ConfigReloadOutcome::RestartRequired);
            assert_eq!(report.restart_required, vec!["identity", "webui"]);
        }
        assert!(f.llm.applied_models.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn mixed_edit_applies_live_part_and_reports_the_rest() {
        let f = fixture(MODEL_A, true);
        std::fs::write(&f.path, format!("{MODEL_B}[runner]\nworker_count = 3\n")).expect("write");
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
        let f = fixture(MODEL_A, false);
        std::fs::write(&f.path, MODEL_B).expect("write");
        let report = f.reloader.reload().await;
        assert_eq!(report.outcome, ConfigReloadOutcome::RestartRequired);
        assert_eq!(report.restart_required, vec!["llm.default"]);
    }

    #[tokio::test]
    async fn clearing_the_default_slot_is_restart_required() {
        let f = fixture(MODEL_A, true);
        std::fs::write(&f.path, "").expect("write");
        let report = f.reloader.reload().await;
        assert_eq!(report.outcome, ConfigReloadOutcome::RestartRequired);
        assert_eq!(report.restart_required, vec!["llm.default"]);
        assert!(f.llm.applied_models.lock().expect("lock").is_empty());
    }
}
