//! The one owner of applying `[llm.default]` to the running provider.
//!
//! Three callers change the live LLM selection: the boot-time apply in
//! composition, the Settings → Inference save path (and the login flows that
//! share its trigger), and the `config.toml` reloader that `zlo serve` runs on
//! SIGHUP. Each of them reads `config.toml`, resolves the selection and the
//! stored key, and swaps the provider chain. [`LiveLlmDefault`] holds one lock
//! across that whole sequence — file read, resolution, key read, swap — and
//! records the `[llm.default]` slot that the last successful swap applied.
//!
//! Without the shared lock, a reload that read the file before a Settings
//! save could swap in its stale selection after the save, leaving disk and UI
//! on one model and requests on another. Without the shared record, the
//! reloader would diff a hand edit against its own boot snapshot and miss a
//! revert of a Settings change, or rebuild the chain for a change Settings
//! already applied.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_config::{LlmSlotSelection, RebornConfigFile};

use crate::llm_admin::llm_config_service::LlmReloadTrigger;

pub struct LiveLlmDefault {
    config_path: PathBuf,
    /// Unserialized resolve-and-swap (the runtime's `RebornLlmReloadAdapter`).
    /// Only ever called with `applied` locked.
    swap: Arc<dyn LlmReloadTrigger>,
    /// The `[llm.default]` slot the last successful swap applied; `None`
    /// until the first one (or when the applied file had no slot).
    applied: tokio::sync::Mutex<Option<LlmSlotSelection>>,
}

/// Exclusive access to the live LLM selection. While held, no other caller
/// can read the file, resolve, or swap.
pub struct LiveLlmDefaultGuard<'a> {
    swap: &'a Arc<dyn LlmReloadTrigger>,
    applied: tokio::sync::MutexGuard<'a, Option<LlmSlotSelection>>,
}

impl LiveLlmDefault {
    pub fn new(config_path: PathBuf, swap: Arc<dyn LlmReloadTrigger>) -> Self {
        Self {
            config_path,
            swap,
            applied: tokio::sync::Mutex::new(None),
        }
    }

    /// Take the lock. Read `config.toml` only after this returns, so the file
    /// a caller applies is never older than one another caller applied.
    pub async fn lock(&self) -> LiveLlmDefaultGuard<'_> {
        LiveLlmDefaultGuard {
            swap: &self.swap,
            applied: self.applied.lock().await,
        }
    }
}

impl LiveLlmDefaultGuard<'_> {
    /// The `[llm.default]` slot currently serving, as far as config is
    /// concerned.
    pub fn applied(&self) -> Option<&LlmSlotSelection> {
        self.applied.as_ref()
    }

    /// Resolve `config_file` and swap it in; on success record its slot as
    /// applied. On failure nothing is recorded and the old provider keeps
    /// serving (the chain is built before the swap).
    pub async fn apply(&mut self, config_file: Option<&RebornConfigFile>) -> Result<(), String> {
        self.swap.reload_from_config(config_file).await?;
        *self.applied = config_file
            .and_then(RebornConfigFile::default_llm_slot)
            .cloned();
        Ok(())
    }
}

#[async_trait]
impl LlmReloadTrigger for LiveLlmDefault {
    /// Read `config.toml` under the lock and apply it.
    async fn reload(&self) -> Result<(), String> {
        let mut guard = self.lock().await;
        let config_file =
            RebornConfigFile::load(&self.config_path).map_err(|error| error.redacted_reason())?;
        guard.apply(config_file.as_ref()).await
    }

    async fn reload_from_config(
        &self,
        config_file: Option<&RebornConfigFile>,
    ) -> Result<(), String> {
        self.lock().await.apply(config_file).await
    }
}
