//! Durable steer handoffs: messages a run will never read that must still be
//! delivered (or settled) by the thread's ONE dispatch decision.
//!
//! A run that ends with steers it never read — still queued, or consumed by
//! the loop but never carried by a model call — records a [`SteerHandoff`]
//! here at terminal settlement, before it releases its queue. So does a
//! message that arrives while its run is ending (it is staged instead of
//! making the sender wait). The handoff is its own durable document, apart
//! from the run's queue document: an older build that reclaims a settled
//! queue document cannot drop it, and it is retried independently of the
//! terminal observer's cursor (startup, a periodic sweep, and every new send
//! to the thread dispatch it first, so unread steers keep their place ahead
//! of newer messages).
//!
//! The store only holds the work. Who gets each message is decided by the
//! dispatcher through the transcript row's guarded delivery transition
//! (`SessionThreadService::change_message_delivery`), so replaying a
//! handoff never delivers a message twice.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ironclaw_filesystem::{
    CasExpectation, ContentType, Entry, FilesystemError, RecordVersion, RootFilesystem,
    ScopedFilesystem,
};
use ironclaw_host_api::ids::ThreadId;
use ironclaw_host_api::{path::ScopedPath, resource::ResourceScope};
use ironclaw_threads::{ThreadMessageId, ThreadScope};
use ironclaw_turns::{LoopMessageRef, TurnRunId, TurnScope};
use serde::{Deserialize, Serialize};

use crate::input_queue::HostInputQueueError;

/// How a handed-off message relates to the run it was meant for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffSteerKind {
    /// Queued for the run, never drained. Row: `Queued(run)`.
    Unread,
    /// Drained by the loop but no model call carried it before the run
    /// ended. Row: `Submitted(run)` (or still `Queued(run)` while its
    /// consumption flip is pending).
    ReadUnconfirmed,
    /// Sent while the run was ending: it was never meant for that run, so a
    /// Stop of the run does not hold it. Row: `Queued(run)`.
    AfterEnd,
}

/// One message to hand off, in thread order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffSteer {
    pub message_id: ThreadMessageId,
    pub message_ref: LoopMessageRef,
    pub kind: HandoffSteerKind,
}

/// The messages one ended (or ending) run leaves to its thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteerHandoff {
    pub ended_run_id: TurnRunId,
    pub thread_scope: ThreadScope,
    pub thread_id: ThreadId,
    /// The turn scope the messages were admitted under. `None` for entries
    /// queued by builds that did not record it: the dispatcher derives it
    /// from the thread scope and the ended run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_scope: Option<TurnScope>,
    /// The run was cancelled (a Stop): its unread steers are not delivered
    /// (the person decides); messages sent after the end still are.
    #[serde(default)]
    pub stopped: bool,
    /// Order among the thread's handoffs (milliseconds since the epoch at
    /// first record; ties break by run id).
    pub recorded_at_ms: i64,
    pub steers: Vec<HandoffSteer>,
}

impl SteerHandoff {
    /// Merge a later record of the same run: new messages append in order,
    /// a Stop sticks, a known turn scope fills a missing one.
    fn merge(&mut self, other: SteerHandoff) {
        for steer in other.steers {
            if !self.steers.iter().any(|s| s.message_id == steer.message_id) {
                self.steers.push(steer);
            }
        }
        self.stopped |= other.stopped;
        if self.turn_scope.is_none() {
            self.turn_scope = other.turn_scope;
        }
    }

    fn order_key(&self) -> (i64, uuid::Uuid) {
        (self.recorded_at_ms, self.ended_run_id.as_uuid())
    }
}

/// Durable home of [`SteerHandoff`]s.
#[async_trait]
pub trait SteerHandoffStore: Send + Sync {
    /// Record (or merge into) the run's handoff. Idempotent.
    async fn record(&self, handoff: SteerHandoff) -> Result<(), HostInputQueueError>;

    /// The run was cancelled: hold its unread steers (no-op without a
    /// handoff).
    async fn stop(&self, ended_run_id: TurnRunId) -> Result<(), HostInputQueueError>;

    /// The thread's handoffs, oldest first.
    async fn for_thread(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<SteerHandoff>, HostInputQueueError>;

    /// Every handoff still open, oldest first (startup and periodic sweep).
    async fn pending(&self) -> Result<Vec<SteerHandoff>, HostInputQueueError>;

    /// Drop the settled messages; the handoff goes once none remain. A
    /// concurrent record that added a message keeps it alive.
    async fn settle(
        &self,
        ended_run_id: TurnRunId,
        settled: &[ThreadMessageId],
    ) -> Result<(), HostInputQueueError>;
}

/// Told when a handoff was recorded, so it is dispatched now rather than at
/// the next sweep. Bound late by composition (the dispatcher needs the turn
/// coordinator, built after the queue).
#[async_trait]
pub trait SteerHandoffDispatch: Send + Sync {
    async fn handoff_recorded(&self, thread_scope: &ThreadScope, thread_id: &ThreadId);
}

pub(crate) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

// ---------------------------------------------------------------------------
// In-memory backend
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct InMemorySteerHandoffStore {
    handoffs: Mutex<BTreeMap<uuid::Uuid, SteerHandoff>>,
}

impl InMemorySteerHandoffStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, BTreeMap<uuid::Uuid, SteerHandoff>>, HostInputQueueError>
    {
        self.handoffs.lock().map_err(|error| {
            tracing::debug!(%error, "steer handoff store lock poisoned");
            HostInputQueueError::Internal
        })
    }
}

fn sorted(mut handoffs: Vec<SteerHandoff>) -> Vec<SteerHandoff> {
    handoffs.sort_by_key(SteerHandoff::order_key);
    handoffs
}

#[async_trait]
impl SteerHandoffStore for InMemorySteerHandoffStore {
    async fn record(&self, handoff: SteerHandoff) -> Result<(), HostInputQueueError> {
        let mut handoffs = self.lock()?;
        match handoffs.get_mut(&handoff.ended_run_id.as_uuid()) {
            Some(existing) => existing.merge(handoff),
            None => {
                handoffs.insert(handoff.ended_run_id.as_uuid(), handoff);
            }
        }
        Ok(())
    }

    async fn stop(&self, ended_run_id: TurnRunId) -> Result<(), HostInputQueueError> {
        if let Some(existing) = self.lock()?.get_mut(&ended_run_id.as_uuid()) {
            existing.stopped = true;
        }
        Ok(())
    }

    async fn for_thread(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<SteerHandoff>, HostInputQueueError> {
        Ok(sorted(
            self.lock()?
                .values()
                .filter(|handoff| &handoff.thread_id == thread_id)
                .cloned()
                .collect(),
        ))
    }

    async fn pending(&self) -> Result<Vec<SteerHandoff>, HostInputQueueError> {
        Ok(sorted(self.lock()?.values().cloned().collect()))
    }

    async fn settle(
        &self,
        ended_run_id: TurnRunId,
        settled: &[ThreadMessageId],
    ) -> Result<(), HostInputQueueError> {
        let mut handoffs = self.lock()?;
        let key = ended_run_id.as_uuid();
        if let Some(existing) = handoffs.get_mut(&key) {
            existing
                .steers
                .retain(|steer| !settled.contains(&steer.message_id));
            if existing.steers.is_empty() {
                handoffs.remove(&key);
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Filesystem backend
// ---------------------------------------------------------------------------

const MAX_CAS_RETRIES: usize = 8;

/// Filesystem-backed [`SteerHandoffStore`]: one CAS-guarded document per run
/// under `/turns/steer-handoff/runs/`, plus an empty per-thread marker under
/// `/turns/steer-handoff/threads/<thread key>/` so a thread's handoffs are
/// found without a scan. The marker is written after the document and
/// removed before it, so a stray document (a crash between the two) is
/// still found by the sweep.
pub struct FilesystemSteerHandoffStore<F>
where
    F: RootFilesystem + ?Sized,
{
    filesystem: Arc<ScopedFilesystem<F>>,
    owner_scope: ResourceScope,
}

impl<F> std::fmt::Debug for FilesystemSteerHandoffStore<F>
where
    F: RootFilesystem + ?Sized,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FilesystemSteerHandoffStore")
            .field("owner_scope", &self.owner_scope)
            .finish_non_exhaustive()
    }
}

fn fs_error(error: FilesystemError) -> HostInputQueueError {
    HostInputQueueError::Unavailable {
        reason: error.to_string(),
    }
}

fn path(raw: String) -> Result<ScopedPath, HostInputQueueError> {
    ScopedPath::new(raw).map_err(|error| HostInputQueueError::Unavailable {
        reason: format!("invalid steer handoff path: {error}"),
    })
}

fn thread_key(thread_id: &ThreadId) -> String {
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, thread_id.as_str().as_bytes()).to_string()
}

fn run_doc_path(run: TurnRunId) -> Result<ScopedPath, HostInputQueueError> {
    path(format!("/turns/steer-handoff/runs/{}.json", run.as_uuid()))
}

fn thread_dir(thread_id: &ThreadId) -> Result<ScopedPath, HostInputQueueError> {
    path(format!(
        "/turns/steer-handoff/threads/{}",
        thread_key(thread_id)
    ))
}

fn thread_marker_path(
    thread_id: &ThreadId,
    run: TurnRunId,
) -> Result<ScopedPath, HostInputQueueError> {
    path(format!(
        "/turns/steer-handoff/threads/{}/{}.json",
        thread_key(thread_id),
        run.as_uuid()
    ))
}

fn run_from_file_name(name: &str) -> Option<TurnRunId> {
    let stem = name.strip_suffix(".json").unwrap_or(name);
    uuid::Uuid::parse_str(stem).ok().map(TurnRunId::from_uuid)
}

impl<F> FilesystemSteerHandoffStore<F>
where
    F: RootFilesystem + ?Sized + 'static,
{
    pub fn new(filesystem: Arc<ScopedFilesystem<F>>, owner_scope: ResourceScope) -> Self {
        Self {
            filesystem,
            owner_scope,
        }
    }

    async fn load(
        &self,
        run: TurnRunId,
    ) -> Result<Option<(SteerHandoff, RecordVersion)>, HostInputQueueError> {
        match self
            .filesystem
            .get(&self.owner_scope, &run_doc_path(run)?)
            .await
        {
            Ok(Some(versioned)) => {
                let handoff = serde_json::from_slice(&versioned.entry.body).map_err(|error| {
                    HostInputQueueError::Unavailable {
                        reason: format!("steer handoff document is corrupt: {error}"),
                    }
                })?;
                Ok(Some((handoff, versioned.version)))
            }
            Ok(None) => Ok(None),
            Err(error) => Err(fs_error(error)),
        }
    }

    /// CAS-write the run's document. `Ok(false)` on a lost race.
    async fn store(
        &self,
        handoff: &SteerHandoff,
        version: Option<RecordVersion>,
    ) -> Result<bool, HostInputQueueError> {
        let body =
            serde_json::to_vec(handoff).map_err(|error| HostInputQueueError::Unavailable {
                reason: format!("steer handoff serialization failed: {error}"),
            })?;
        let cas = match version {
            Some(version) => CasExpectation::Version(version),
            None => CasExpectation::Absent,
        };
        match self
            .filesystem
            .put(
                &self.owner_scope,
                &run_doc_path(handoff.ended_run_id)?,
                Entry::bytes(body).with_content_type(ContentType::json()),
                cas,
            )
            .await
        {
            Ok(_) => Ok(true),
            Err(FilesystemError::VersionMismatch { .. }) => Ok(false),
            Err(error) => Err(fs_error(error)),
        }
    }

    async fn write_marker(&self, handoff: &SteerHandoff) -> Result<(), HostInputQueueError> {
        let marker = thread_marker_path(&handoff.thread_id, handoff.ended_run_id)?;
        match self
            .filesystem
            .put(
                &self.owner_scope,
                &marker,
                Entry::bytes(b"{}".to_vec()).with_content_type(ContentType::json()),
                CasExpectation::Absent,
            )
            .await
        {
            Ok(_) | Err(FilesystemError::VersionMismatch { .. }) => Ok(()),
            Err(error) => Err(fs_error(error)),
        }
    }

    async fn delete_marker(
        &self,
        thread_id: &ThreadId,
        run: TurnRunId,
    ) -> Result<(), HostInputQueueError> {
        let marker = thread_marker_path(thread_id, run)?;
        let current = match self.filesystem.get(&self.owner_scope, &marker).await {
            Ok(Some(versioned)) => versioned.version,
            Ok(None) => return Ok(()),
            Err(error) => return Err(fs_error(error)),
        };
        match self
            .filesystem
            .delete_if_version(&self.owner_scope, &marker, current)
            .await
        {
            Ok(()) | Err(FilesystemError::VersionMismatch { .. }) => Ok(()),
            Err(FilesystemError::NotFound { .. }) => Ok(()),
            Err(error) => Err(fs_error(error)),
        }
    }

    async fn runs_in(&self, dir: &ScopedPath) -> Result<Vec<TurnRunId>, HostInputQueueError> {
        let entries = self
            .filesystem
            .list_dir(&self.owner_scope, dir)
            .await
            .or_else(|error| match error {
                FilesystemError::NotFound { .. } => Ok(Vec::new()),
                other => Err(fs_error(other)),
            })?;
        Ok(entries
            .iter()
            .filter_map(|entry| run_from_file_name(&entry.name))
            .collect())
    }

    async fn load_all(
        &self,
        runs: Vec<TurnRunId>,
    ) -> Result<Vec<SteerHandoff>, HostInputQueueError> {
        let mut handoffs = Vec::new();
        for run in runs {
            if let Some((handoff, _)) = self.load(run).await? {
                handoffs.push(handoff);
            }
        }
        Ok(sorted(handoffs))
    }
}

fn cas_exhausted(operation: &'static str) -> HostInputQueueError {
    HostInputQueueError::Unavailable {
        reason: format!("steer handoff {operation} exhausted CAS retries"),
    }
}

#[async_trait]
impl<F> SteerHandoffStore for FilesystemSteerHandoffStore<F>
where
    F: RootFilesystem + ?Sized + 'static,
{
    async fn record(&self, handoff: SteerHandoff) -> Result<(), HostInputQueueError> {
        for _ in 0..MAX_CAS_RETRIES {
            let (merged, version) = match self.load(handoff.ended_run_id).await? {
                Some((mut existing, version)) => {
                    existing.merge(handoff.clone());
                    (existing, Some(version))
                }
                None => (handoff.clone(), None),
            };
            if self.store(&merged, version).await? {
                return self.write_marker(&merged).await;
            }
        }
        Err(cas_exhausted("record"))
    }

    async fn stop(&self, ended_run_id: TurnRunId) -> Result<(), HostInputQueueError> {
        for _ in 0..MAX_CAS_RETRIES {
            let Some((mut existing, version)) = self.load(ended_run_id).await? else {
                return Ok(());
            };
            if existing.stopped {
                return Ok(());
            }
            existing.stopped = true;
            if self.store(&existing, Some(version)).await? {
                return Ok(());
            }
        }
        Err(cas_exhausted("stop"))
    }

    async fn for_thread(
        &self,
        thread_id: &ThreadId,
    ) -> Result<Vec<SteerHandoff>, HostInputQueueError> {
        let runs = self.runs_in(&thread_dir(thread_id)?).await?;
        self.load_all(runs).await
    }

    async fn pending(&self) -> Result<Vec<SteerHandoff>, HostInputQueueError> {
        let runs = self
            .runs_in(&path("/turns/steer-handoff/runs".to_string())?)
            .await?;
        self.load_all(runs).await
    }

    async fn settle(
        &self,
        ended_run_id: TurnRunId,
        settled: &[ThreadMessageId],
    ) -> Result<(), HostInputQueueError> {
        for _ in 0..MAX_CAS_RETRIES {
            let Some((mut existing, version)) = self.load(ended_run_id).await? else {
                return Ok(());
            };
            existing
                .steers
                .retain(|steer| !settled.contains(&steer.message_id));
            if !existing.steers.is_empty() {
                if self.store(&existing, Some(version)).await? {
                    return Ok(());
                }
                continue;
            }
            // Marker first: a document without one is still swept.
            self.delete_marker(&existing.thread_id, ended_run_id)
                .await?;
            match self
                .filesystem
                .delete_if_version(&self.owner_scope, &run_doc_path(ended_run_id)?, version)
                .await
            {
                Ok(()) => return Ok(()),
                Err(FilesystemError::VersionMismatch { .. }) => {
                    // A record raced in: restore its marker and retry.
                    if let Some((current, _)) = self.load(ended_run_id).await? {
                        self.write_marker(&current).await?;
                    }
                    continue;
                }
                Err(error) => return Err(fs_error(error)),
            }
        }
        Err(cas_exhausted("settle"))
    }
}
