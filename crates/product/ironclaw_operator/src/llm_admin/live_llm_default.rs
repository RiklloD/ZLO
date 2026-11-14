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
//! Every writer of the running server's selection — Settings saves, the
//! Codex / ChatGPT-plan / NEAR AI login completions — goes through
//! [`LiveLlmDefault::write_and_apply`], so its write, apply and rollback are
//! one step under this lock (lock order: this lock, then the config file
//! update lock inside it). The guarantee is **per server process**: the CLI
//! (`zlo models`, `zlo onboard`, `zlo config set`) runs in its own process
//! and writes `config.toml` under the file lock only, so its edits cannot
//! tear a write but are applied to a running server only by the next SIGHUP
//! reload or Settings apply, which reconciles disk and the live provider.
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
use ironclaw_product_contracts::operator_llm::LlmSelectionConfigState;

use crate::llm_admin::llm_config_service::LlmReloadTrigger;

pub struct LiveLlmDefault {
    config_path: PathBuf,
    /// Unserialized resolve-and-swap (the runtime's `RebornLlmReloadAdapter`).
    /// Only ever called with `applied` locked.
    swap: Arc<dyn LlmReloadTrigger>,
    /// The `[llm.default]` slot the last successful swap applied; `None`
    /// until the first one (or when the applied file had no slot).
    applied: tokio::sync::Mutex<Option<LlmSlotSelection>>,
    /// Callers currently waiting for the lock ([`Self::queued_callers`]).
    waiting: std::sync::atomic::AtomicUsize,
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
            waiting: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Take the lock. Read `config.toml` only after this returns, so the file
    /// a caller applies is never older than one another caller applied.
    pub async fn lock(&self) -> LiveLlmDefaultGuard<'_> {
        use std::sync::atomic::Ordering;
        self.waiting.fetch_add(1, Ordering::SeqCst);
        let applied = self.applied.lock().await;
        self.waiting.fetch_sub(1, Ordering::SeqCst);
        LiveLlmDefaultGuard {
            swap: &self.swap,
            applied,
        }
    }

    /// How many callers are parked on [`Self::lock`] right now: selection
    /// writers or reloads queued behind the one applying. A diagnostic read;
    /// ordering tests use it as their explicit "the racing caller is queued"
    /// signal instead of sleeping.
    pub fn queued_callers(&self) -> usize {
        self.waiting.load(std::sync::atomic::Ordering::SeqCst)
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

/// Why [`LiveLlmDefault::write_and_apply`] did not leave its selection
/// serving.
#[derive(Debug)]
pub(crate) enum SelectionWriteError<E> {
    /// The write itself failed; nothing was applied.
    Write(E),
    /// The write task did not run to completion (panicked or was cancelled);
    /// nothing was applied.
    Interrupted,
    /// The write landed but the running provider refused it; `config` says
    /// what became of the written file. `reason` is for logs only.
    NotApplied {
        reason: String,
        config: LlmSelectionConfigState,
    },
}

impl LiveLlmDefault {
    /// The Settings → Inference transaction, under the one lock every apply
    /// takes: run `write` (which records the text it replaced and wrote,
    /// under the config update lock), apply exactly what it wrote, and when
    /// the apply is refused put the file back before the lock is released.
    /// A concurrent Settings save, SIGHUP reload or boot apply therefore
    /// never sees the refused selection on disk, and a rollback never
    /// replaces a change another writer made (see
    /// [`ironclaw_config::ConfigTextWrite::restore_if_unchanged`]).
    pub(crate) async fn write_and_apply<T, E>(
        &self,
        write: impl FnOnce() -> Result<(T, ironclaw_config::ConfigTextWrite), E> + Send + 'static,
    ) -> Result<T, SelectionWriteError<E>>
    where
        T: Send + 'static,
        E: Send + 'static,
    {
        let mut guard = self.lock().await;
        let (value, record) = tokio::task::spawn_blocking(write)
            .await
            .map_err(|_| SelectionWriteError::Interrupted)?
            .map_err(SelectionWriteError::Write)?;
        let applied = match RebornConfigFile::parse_text(&record.written, &record.path) {
            Ok(file) => guard.apply(Some(&file)).await,
            Err(error) => Err(error.redacted_reason()),
        };
        let Err(reason) = applied else {
            return Ok(value);
        };
        let config = match tokio::task::spawn_blocking(move || record.restore_if_unchanged()).await
        {
            Ok(Ok(true)) => LlmSelectionConfigState::RolledBack,
            Ok(Ok(false)) => LlmSelectionConfigState::KeptConcurrentEdit,
            Ok(Err(error)) => {
                tracing::debug!(%error, "restoring config.toml after a refused LLM selection failed");
                LlmSelectionConfigState::Diverged
            }
            Err(error) => {
                tracing::debug!(%error, "restoring config.toml after a refused LLM selection did not run");
                LlmSelectionConfigState::Diverged
            }
        };
        drop(guard);
        Err(SelectionWriteError::NotApplied { reason, config })
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
