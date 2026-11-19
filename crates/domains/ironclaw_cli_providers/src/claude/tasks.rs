//! The session's task store (`<config>/tasks/<session>/<id>.json`, written by
//! the CLI's task tools) read into the thread's plan after each task call.
//!
//! The store is the CLI's, so it is read defensively: the folder itself must
//! be a real directory (not a link), each entry is opened without following
//! a link and non-blocking (a FIFO never hangs the read), checked to be the
//! regular file that was listed, and the read is bounded per file, in entry
//! count and in total bytes. A missing store is simply "no tasks yet"; any
//! other failure is logged at `debug` and leaves the plan as it was.
//!
//! Refreshes run off the read loop and may overlap; each carries a
//! generation, and only the newest one publishes, so an older, slower read
//! can never replace a newer plan.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ironclaw_host_api::agent_plan::AgentPlan;
use serde_json::Value;
use tokio::io::AsyncReadExt;

use crate::interaction;
use crate::session_import::files;

/// Directory entries looked at per read; the rest are ignored.
pub(super) const MAX_ENTRIES: usize = 512;
/// One task file; a larger one is skipped.
pub(super) const MAX_FILE_BYTES: u64 = 64 * 1024;
/// All task files of one read together.
pub(super) const MAX_TOTAL_BYTES: u64 = 1024 * 1024;

/// The store read into a plan; `None` when there is none (or it could not
/// be read).
pub(super) async fn read_plan(store: &Path) -> Option<AgentPlan> {
    match tokio::fs::symlink_metadata(store).await {
        Ok(meta) if meta.file_type().is_dir() => {}
        Ok(_) => {
            tracing::debug!("claude task store: not a real directory; ignored");
            return None;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::debug!(%error, "claude task store: unreadable");
            return None;
        }
    }
    let mut dir = match tokio::fs::read_dir(store).await {
        Ok(dir) => dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::debug!(%error, "claude task store: listing failed");
            return None;
        }
    };
    let mut tasks = Vec::new();
    let (mut seen, mut total) = (0usize, 0u64);
    loop {
        let entry = match dir.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(error) => {
                tracing::debug!(%error, "claude task store: listing cut short");
                break;
            }
        };
        seen += 1;
        if seen > MAX_ENTRIES {
            tracing::debug!("claude task store: entry limit reached; the rest is ignored");
            break;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match read_entry(&path, MAX_TOTAL_BYTES - total).await {
            Ok(Some((len, task))) => {
                total += len;
                tasks.push(task);
            }
            Ok(None) => {}
            // A half-written or vanished entry is read again next time.
            Err(error) => tracing::debug!(%error, "claude task store: entry skipped"),
        }
        if total >= MAX_TOTAL_BYTES {
            tracing::debug!("claude task store: byte limit reached; the rest is ignored");
            break;
        }
    }
    interaction::claude_task_plan(tasks)
}

/// One entry, if it is a regular file within `budget` bytes holding JSON.
async fn read_entry(path: &Path, budget: u64) -> std::io::Result<Option<(u64, Value)>> {
    // `symlink_metadata`: a link is reported as a link, never followed.
    let meta = tokio::fs::symlink_metadata(path).await?;
    if !meta.file_type().is_file() {
        tracing::debug!("claude task store: not a regular file; skipped");
        return Ok(None);
    }
    let limit = MAX_FILE_BYTES.min(budget);
    if meta.len() > limit {
        tracing::debug!(
            size = meta.len(),
            "claude task store: entry too large; skipped"
        );
        return Ok(None);
    }
    let file = files::open_regular(path, &files::info(&meta).id).await?;
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes).await?;
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX); // silent-ok: usize always fits u64 here
    if len > limit {
        tracing::debug!("claude task store: entry grew past the limit; skipped");
        return Ok(None);
    }
    match serde_json::from_slice(&bytes) {
        Ok(task) => Ok(Some((len, task))),
        Err(error) => {
            tracing::debug!(%error, "claude task store: entry not JSON yet; skipped");
            Ok(None)
        }
    }
}

type PlanFuture = Pin<Box<dyn Future<Output = Option<AgentPlan>> + Send>>;
/// How a refresh reads the store ([`read_plan`]; tests slow it down).
pub(super) type PlanReader = Arc<dyn Fn(PathBuf) -> PlanFuture + Send + Sync>;

/// Re-reads one session's task store into its plan, newest read wins.
pub(super) struct TaskRefresher {
    store: PathBuf,
    reader: PlanReader,
    /// The newest refresh asked for.
    latest: Arc<AtomicU64>,
    /// Held while a refresh checks it is still the newest and publishes.
    publish: Arc<Mutex<()>>,
}

impl TaskRefresher {
    pub(super) fn new(store: PathBuf) -> Self {
        Self::with_reader(
            store,
            Arc::new(|store| Box::pin(async move { read_plan(&store).await })),
        )
    }

    pub(super) fn with_reader(store: PathBuf, reader: PlanReader) -> Self {
        Self {
            store,
            reader,
            latest: Arc::default(),
            publish: Arc::default(),
        }
    }

    /// Read the store off the caller's task and hand the plan to `publish`,
    /// unless a newer refresh was asked for meanwhile (that one publishes).
    pub(super) fn refresh(&self, publish: impl FnOnce(AgentPlan) + Send + 'static) {
        let generation = self.latest.fetch_add(1, Ordering::SeqCst) + 1;
        let (latest, lock) = (Arc::clone(&self.latest), Arc::clone(&self.publish));
        let (reader, store) = (Arc::clone(&self.reader), self.store.clone());
        tokio::spawn(async move {
            if latest.load(Ordering::SeqCst) != generation {
                return; // superseded before it began
            }
            let Some(plan) = reader(store).await else {
                return;
            };
            // Checked and published under one lock: a newer refresh either
            // was asked for already (this one stands down) or publishes after.
            let _guard = lock
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if latest.load(Ordering::SeqCst) == generation {
                publish(plan);
            }
        });
    }
}
