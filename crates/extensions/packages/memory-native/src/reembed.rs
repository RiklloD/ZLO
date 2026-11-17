//! The re-embed backfill: give every stored chunk a vector in the current
//! embedding space.
//!
//! Chunk vectors are keyed by the embedder's fingerprint, so a document
//! written before `[embeddings]` was configured, one whose embedding call
//! failed, or one embedded by a previous model has no vector the current
//! space's search can rank — it is reachable by full text only until its next
//! write. [`MemoryReembedJob`] closes that gap without waiting for writes:
//!
//! - **Scan.** Every memory scope under `/memory/tenants`, one query per
//!   scope; a chunk is *pending* when it has no vector under the current
//!   space's key and its text is embeddable (blank or over-long chunks are
//!   counted `ineligible` and stay text-only, as at write time).
//! - **Embed.** Pending chunks of one scope go to the provider in batches of
//!   `min(embedder max_batch, 256)` — the same cap and shape checks as
//!   indexing, through the same host-injected [`MemoryEmbedder`] (so the same
//!   mediated provider and network egress as `memory.write`; the key never
//!   reaches this crate). Calls are paced (`min_call_interval`), each bounded
//!   by `call_timeout`, and a transient failure (HTTP 429/5xx, transport,
//!   timeout) backs off exponentially, honoring the provider's `retry_after`.
//!   A run stops at its chunk budget, after `max_transient_retries`, or on a
//!   permanent failure; vectors already attached stay.
//! - **Attach.** Each vector is inserted into the chunk's *current* record
//!   (re-read, then CAS on its version) only while the document's content
//!   hash and the chunk's identity digest still match what was embedded;
//!   the CAS then cannot match a chunk a rewrite recreated meanwhile,
//!   since versions never repeat for a path (filesystem invariant 9). Nothing
//!   else on the chunk changes, vectors of other spaces stay (never deleted,
//!   just not queried), and a chunk a concurrent write replaced is left to
//!   that write's own indexing (`superseded`).
//! - **Resume.** Progress is a durable per-space cursor (CAS-written beside
//!   the tenant trees), advanced past each document once all its pending
//!   chunks are attached. A restarted run continues after it; a pass that
//!   reaches the end resets it. Re-running with no backlog writes nothing and
//!   calls no provider.

use std::sync::Arc;
use std::time::Duration;

use ironclaw_filesystem::{FilesystemError, RecordVersion, RootFilesystem};
use ironclaw_memory::MemoryDocumentScope;
use tokio::time::Instant;

use crate::embedding::{
    INDEX_EMBEDDING_BUDGET, MemoryEmbedder, MemoryEmbeddingError, MemoryEmbeddingFingerprint,
    MemoryEmbeddingRetry, effective_batch_size, embed_checked_batch,
};
use crate::repo::{
    FilesystemMemoryDocumentRepository, PendingChunk, ReembedCursor, ReembedPosition, scope_key,
};

/// How one backfill run is paced and bounded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryReembedOptions {
    /// Count the backlog only: no provider call, no write.
    pub dry_run: bool,
    /// Most chunk inputs one run sends to the provider, charged at dispatch:
    /// every input of every call, retries and inputs whose attach is later
    /// superseded included. `None` = until the backlog is done.
    pub max_chunks: Option<u64>,
    /// Least time between the starts of two provider calls.
    pub min_call_interval: Duration,
    /// First backoff after a transient failure; doubles per retry.
    pub initial_backoff: Duration,
    /// Longest single backoff, whatever the provider's `retry_after` says.
    pub max_backoff: Duration,
    /// Retries of one batch before the run stops as provider-unavailable.
    pub max_transient_retries: u32,
    /// Longest one provider call may take (a timeout is transient).
    pub call_timeout: Duration,
    /// Most provider calls one run makes, retries included; `None` = no cap.
    pub max_provider_calls: Option<u64>,
    /// Background only: wait this long after boot before the first scan.
    pub start_delay: Duration,
    /// Background only: pause between runs while a backlog remains.
    pub pause_between_runs: Duration,
}

impl MemoryReembedOptions {
    /// The boot-time background job: low priority — a delayed start, a
    /// bounded chunk budget per run, paced calls, long pauses.
    pub fn background() -> Self {
        Self {
            dry_run: false,
            max_chunks: Some(2_000),
            max_provider_calls: Some(64),
            min_call_interval: Duration::from_millis(500),
            initial_backoff: Duration::from_secs(2),
            max_backoff: Duration::from_secs(300),
            max_transient_retries: 5,
            call_timeout: INDEX_EMBEDDING_BUDGET,
            start_delay: Duration::from_secs(60),
            pause_between_runs: Duration::from_secs(600),
        }
    }

    /// An operator-started run (`zlo memory reembed`): no budget unless one
    /// is given, a lighter pace, the same backoff discipline.
    pub fn operator(dry_run: bool, max_chunks: Option<u64>) -> Self {
        Self {
            dry_run,
            max_chunks,
            max_provider_calls: None,
            min_call_interval: Duration::from_millis(100),
            initial_backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
            max_transient_retries: 6,
            ..Self::background()
        }
    }
}

/// A progress sample: emitted once after the scan and after every batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryReembedProgress {
    /// Chunks embedded since the previous sample.
    pub embedded: u64,
    /// Chunks embedded so far in this run.
    pub embedded_total: u64,
    /// Pending chunks this run has not dealt with yet.
    pub backlog: u64,
}

/// Why a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryReembedOutcome {
    /// The backlog is done (or there was none).
    Complete,
    /// `dry_run`: counted only.
    DryRun,
    /// The run's chunk budget ran out; the cursor holds the place.
    BudgetExhausted,
    /// The provider stayed overloaded or unreachable through every retry.
    ProviderUnavailable,
    /// The provider refused in a way retrying cannot fix.
    ProviderRejected,
}

impl MemoryReembedOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::DryRun => "dry_run",
            Self::BudgetExhausted => "budget_exhausted",
            Self::ProviderUnavailable => "provider_unavailable",
            Self::ProviderRejected => "provider_rejected",
        }
    }

    /// The run stopped on a provider failure, not by design.
    pub fn is_failure(self) -> bool {
        matches!(self, Self::ProviderUnavailable | Self::ProviderRejected)
    }
}

/// What one run found and did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryReembedReport {
    /// The space backfilled (`MemoryEmbeddingFingerprint`).
    pub fingerprint: String,
    /// Chunks with no vector in the space when the run started.
    pub backlog: u64,
    /// Chunks that can never be embedded (blank, or over the input limit).
    pub ineligible: u64,
    /// Chunks this run gave a vector.
    pub embedded: u64,
    /// Pending chunks a concurrent write replaced before their vector landed.
    pub superseded: u64,
    /// Pending chunks this run left (budget, failure, or dry run).
    pub remaining: u64,
    /// Embedding calls made, retries included.
    pub provider_calls: u64,
    /// Inputs sent to the provider, retries and later-superseded ones
    /// included: what the run's `max_chunks` budget is charged.
    pub dispatched: u64,
    /// The run continued an earlier, unfinished pass from its durable cursor.
    pub resumed: bool,
    pub outcome: MemoryReembedOutcome,
}

/// The backfill over one memory filesystem and one embedder. Cheap to clone.
#[derive(Clone)]
pub struct MemoryReembedJob {
    repository: Arc<FilesystemMemoryDocumentRepository<dyn RootFilesystem>>,
    embedder: Arc<dyn MemoryEmbedder>,
}

impl std::fmt::Debug for MemoryReembedJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryReembedJob")
            .field("fingerprint", &self.embedder.fingerprint().as_str())
            .finish()
    }
}

/// Per-run mutable state.
struct RunState<'a> {
    options: &'a MemoryReembedOptions,
    fingerprint: MemoryEmbeddingFingerprint,
    dimension: usize,
    batch: usize,
    last_call: Option<Instant>,
    provider_calls: u64,
    dispatched: u64,
    embedded: u64,
    superseded: u64,
    /// Pending chunks dealt with (embedded, superseded) this run.
    handled: u64,
    backlog: u64,
    cursor: ReembedCursor,
    cursor_version: Option<RecordVersion>,
}

impl RunState<'_> {
    /// Inputs the run may still dispatch.
    fn budget_left(&self) -> u64 {
        self.options
            .max_chunks
            .map_or(u64::MAX, |max| max.saturating_sub(self.dispatched))
    }

    /// Whether one more call of `inputs` inputs fits both budgets.
    fn can_dispatch(&self, inputs: usize) -> bool {
        let calls_ok = self
            .options
            .max_provider_calls
            .is_none_or(|max| self.provider_calls < max);
        calls_ok && self.budget_left() >= inputs as u64
    }

    fn progress(&self, embedded: u64) -> MemoryReembedProgress {
        MemoryReembedProgress {
            embedded,
            embedded_total: self.embedded,
            backlog: self.backlog.saturating_sub(self.handled),
        }
    }
}

/// Why a batch was not embedded.
enum BatchStop {
    /// The run's input or call budget is spent.
    Budget,
    Provider(MemoryEmbeddingError),
}

type ProgressSink<'a> = &'a mut (dyn FnMut(MemoryReembedProgress) + Send);

impl MemoryReembedJob {
    /// The job over the memory filesystem (`/memory/...`) with the embedder
    /// whose space is backfilled.
    pub fn new(filesystem: Arc<dyn RootFilesystem>, embedder: Arc<dyn MemoryEmbedder>) -> Self {
        Self {
            repository: Arc::new(FilesystemMemoryDocumentRepository::new(filesystem)),
            embedder,
        }
    }

    /// One run: scan, then embed pending chunks until done, out of budget, or
    /// the provider fails. Storage errors fail the run (nothing is skipped
    /// silently); provider failures end it with a reported outcome.
    pub async fn run(
        &self,
        options: &MemoryReembedOptions,
        on_progress: ProgressSink<'_>,
    ) -> Result<MemoryReembedReport, FilesystemError> {
        let fingerprint = self.embedder.fingerprint();
        let scopes = self.repository.memory_scopes().await?;
        // Count first (bounded: one scope's chunks in memory at a time).
        let mut per_scope = Vec::with_capacity(scopes.len());
        let (mut backlog, mut ineligible) = (0_u64, 0_u64);
        for scope in &scopes {
            let found = self
                .repository
                .scope_backlog(scope, self.embedder.as_ref())
                .await?;
            let pending: u64 = found
                .documents
                .iter()
                .map(|document| document.pending.len() as u64)
                .sum();
            backlog += pending;
            ineligible += found.ineligible;
            per_scope.push(pending);
        }
        let (cursor, cursor_version) =
            match self.repository.read_reembed_cursor(&fingerprint).await? {
                Some((cursor, version)) => (cursor, Some(version)),
                None => (
                    ReembedCursor {
                        fingerprint: fingerprint.as_str().to_string(),
                        ..ReembedCursor::default()
                    },
                    None,
                ),
            };
        let mut state = RunState {
            options,
            dimension: self.embedder.dimension() as usize,
            batch: effective_batch_size(self.embedder.as_ref()),
            fingerprint,
            last_call: None,
            provider_calls: 0,
            dispatched: 0,
            embedded: 0,
            superseded: 0,
            handled: 0,
            backlog,
            cursor,
            cursor_version,
        };
        let resumed = state.cursor.position.is_some() && !options.dry_run && backlog > 0;
        on_progress(state.progress(0));
        tracing::debug!(
            fingerprint = state.fingerprint.as_str(),
            backlog,
            ineligible,
            "memory re-embed scan"
        );
        let outcome = if options.dry_run {
            MemoryReembedOutcome::DryRun
        } else if backlog == 0 {
            // Nothing to do: only a stale mid-pass cursor is reset.
            if state.cursor.position.is_some() {
                self.finish_pass(&mut state).await?;
            }
            MemoryReembedOutcome::Complete
        } else {
            self.embed_backlog(&scopes, &per_scope, &mut state, on_progress)
                .await?
        };
        Ok(MemoryReembedReport {
            fingerprint: state.fingerprint.as_str().to_string(),
            backlog,
            ineligible,
            embedded: state.embedded,
            superseded: state.superseded,
            remaining: backlog.saturating_sub(state.handled),
            provider_calls: state.provider_calls,
            dispatched: state.dispatched,
            resumed,
            outcome,
        })
    }

    /// Passes from the cursor to the end of the tree; a pass that resumed
    /// mid-tree and skipped pending chunks before its start is followed by
    /// one full pass, so `Complete` means the whole backlog was dealt with.
    async fn embed_backlog(
        &self,
        scopes: &[MemoryDocumentScope],
        per_scope: &[u64],
        state: &mut RunState<'_>,
        on_progress: ProgressSink<'_>,
    ) -> Result<MemoryReembedOutcome, FilesystemError> {
        loop {
            let start = state.cursor.position.clone();
            let mut skipped = 0_u64;
            for (scope, &pending) in scopes.iter().zip(per_scope) {
                let key = scope_key(scope);
                if let Some(start) = &start
                    && key < start.scope
                {
                    skipped += pending;
                    continue;
                }
                let found = self
                    .repository
                    .scope_backlog(scope, self.embedder.as_ref())
                    .await?;
                let mut queue: Vec<(PendingChunk, Option<String>)> = Vec::new();
                for document in found.documents {
                    if let Some(start) = &start
                        && key == start.scope
                        && document.relative_path <= start.document
                    {
                        skipped += document.pending.len() as u64;
                        continue;
                    }
                    let last = document.pending.len().saturating_sub(1);
                    for (index, chunk) in document.pending.into_iter().enumerate() {
                        let done = (index == last).then(|| document.relative_path.clone());
                        queue.push((chunk, done));
                    }
                }
                if let Some(outcome) = self
                    .embed_scope(scope, key, queue, state, on_progress)
                    .await?
                {
                    return Ok(outcome);
                }
            }
            self.finish_pass(state).await?;
            if start.is_none() || skipped == 0 {
                return Ok(MemoryReembedOutcome::Complete);
            }
        }
    }

    /// Embed one scope's queue batch by batch. `Some(outcome)` stops the run.
    async fn embed_scope(
        &self,
        scope: &MemoryDocumentScope,
        key: [String; 4],
        queue: Vec<(PendingChunk, Option<String>)>,
        state: &mut RunState<'_>,
        on_progress: ProgressSink<'_>,
    ) -> Result<Option<MemoryReembedOutcome>, FilesystemError> {
        let mut rest = queue.as_slice();
        while !rest.is_empty() {
            let budget = state.budget_left();
            if budget == 0 {
                return Ok(Some(MemoryReembedOutcome::BudgetExhausted));
            }
            let take = rest
                .len()
                .min(state.batch)
                .min(usize::try_from(budget).unwrap_or(usize::MAX));
            let (group, tail) = rest.split_at(take);
            rest = tail;
            let inputs = group
                .iter()
                .map(|(chunk, _)| chunk.content.clone())
                .collect();
            let vectors = match self.embed_with_backoff(scope, inputs, state).await {
                Ok(vectors) => vectors,
                Err(BatchStop::Budget) => return Ok(Some(MemoryReembedOutcome::BudgetExhausted)),
                Err(BatchStop::Provider(error)) => {
                    tracing::debug!(%error, "memory re-embed stopped on a provider failure");
                    return Ok(Some(if error.is_transient() {
                        MemoryReembedOutcome::ProviderUnavailable
                    } else {
                        MemoryReembedOutcome::ProviderRejected
                    }));
                }
            };
            let mut embedded = 0_u64;
            let mut done_document = None;
            for ((chunk, done), vector) in group.iter().zip(vectors) {
                if self
                    .repository
                    .attach_chunk_vector(chunk, &state.fingerprint, &vector)
                    .await?
                {
                    embedded += 1;
                } else {
                    state.superseded += 1;
                }
                state.handled += 1;
                if done.is_some() {
                    done_document = done.clone();
                }
            }
            state.embedded += embedded;
            if let Some(document) = done_document {
                state.cursor.position = Some(ReembedPosition {
                    scope: key.clone(),
                    document,
                });
                self.save_cursor(state).await?;
            }
            on_progress(state.progress(embedded));
        }
        Ok(None)
    }

    /// One shape-checked provider call with pacing, a timeout, and
    /// exponential backoff on transient failures. Each attempt (retries
    /// included) is charged to the run's input and call budgets before it is
    /// sent; when they are spent the batch stops — as `Budget` on its first
    /// attempt, as the provider failure it was retrying otherwise.
    async fn embed_with_backoff(
        &self,
        scope: &MemoryDocumentScope,
        inputs: Vec<String>,
        state: &mut RunState<'_>,
    ) -> Result<Vec<Vec<f32>>, BatchStop> {
        let mut backoff = state.options.initial_backoff;
        let mut retries = 0_u32;
        let mut last_error: Option<MemoryEmbeddingError> = None;
        loop {
            if !state.can_dispatch(inputs.len()) {
                return Err(last_error.map_or(BatchStop::Budget, BatchStop::Provider));
            }
            if let Some(last) = state.last_call {
                tokio::time::sleep_until(last + state.options.min_call_interval).await;
            }
            state.last_call = Some(Instant::now());
            state.provider_calls += 1;
            state.dispatched += inputs.len() as u64;
            let call = embed_checked_batch(
                self.embedder.as_ref(),
                scope,
                inputs.clone(),
                state.dimension,
            );
            let error = match tokio::time::timeout(state.options.call_timeout, call).await {
                Ok(Ok(vectors)) => return Ok(vectors),
                Ok(Err(error)) => error,
                Err(_) => MemoryEmbeddingError::transient("re-embed call timed out", None),
            };
            let MemoryEmbeddingRetry::Transient { retry_after } = error.retry else {
                return Err(BatchStop::Provider(error));
            };
            if retries >= state.options.max_transient_retries {
                return Err(BatchStop::Provider(error));
            }
            retries += 1;
            let wait = retry_after
                .unwrap_or_default()
                .max(backoff)
                .min(state.options.max_backoff);
            tracing::debug!(%error, retries, ?wait, "memory re-embed backing off");
            tokio::time::sleep(wait).await;
            backoff = backoff.saturating_mul(2).min(state.options.max_backoff);
            last_error = Some(error);
        }
    }

    /// End of a pass: the next run starts from the top.
    async fn finish_pass(&self, state: &mut RunState<'_>) -> Result<(), FilesystemError> {
        state.cursor.position = None;
        state.cursor.completed_passes = state.cursor.completed_passes.saturating_add(1);
        self.save_cursor(state).await
    }

    /// CAS the cursor. On a conflict (another run moved it) re-read once and
    /// write this run's position over the fresh version: both runs only ever
    /// record work that is durably done, so either position is safe.
    async fn save_cursor(&self, state: &mut RunState<'_>) -> Result<(), FilesystemError> {
        for _ in 0..2 {
            if let Some(version) = self
                .repository
                .write_reembed_cursor(&state.fingerprint, &state.cursor, state.cursor_version)
                .await?
            {
                state.cursor_version = Some(version);
                return Ok(());
            }
            state.cursor_version = self
                .repository
                .read_reembed_cursor(&state.fingerprint)
                .await?
                .map(|(_, version)| version);
        }
        tracing::debug!("memory re-embed cursor kept moving under this run; not saved");
        Ok(())
    }

    /// Start the low-priority boot-time backfill: after `start_delay`, runs
    /// with `options` until the backlog is done, pausing between budgeted
    /// runs. It gives up after a permanent provider failure, three runs in a
    /// row that found the provider unavailable, or a storage error — the
    /// next boot (or `zlo memory reembed`) tries again. Logs at `debug!`
    /// only. Dropping the returned task stops it; the cursor holds the place.
    pub fn spawn_background(
        self,
        options: MemoryReembedOptions,
        on_progress: impl Fn(MemoryReembedProgress) + Send + Sync + 'static,
    ) -> MemoryReembedTask {
        let handle = tokio::spawn(async move {
            tokio::time::sleep(options.start_delay).await;
            let mut unavailable = 0_u32;
            loop {
                let mut sink = |progress| on_progress(progress);
                let report = match self.run(&options, &mut sink).await {
                    Ok(report) => report,
                    Err(error) => {
                        tracing::debug!(%error, "memory re-embed background run failed on storage");
                        return;
                    }
                };
                tracing::debug!(
                    outcome = report.outcome.as_str(),
                    embedded = report.embedded,
                    remaining = report.remaining,
                    "memory re-embed background run"
                );
                match report.outcome {
                    MemoryReembedOutcome::BudgetExhausted => unavailable = 0,
                    MemoryReembedOutcome::ProviderUnavailable if unavailable < 2 => {
                        unavailable += 1;
                    }
                    _ => return,
                }
                tokio::time::sleep(options.pause_between_runs).await;
            }
        });
        MemoryReembedTask {
            handle: Some(handle),
        }
    }
}

/// The running background backfill. Dropping it aborts the task (safe at any
/// point: every write is a CAS, and the cursor only records finished work).
#[derive(Debug)]
pub struct MemoryReembedTask {
    handle: Option<tokio::task::JoinHandle<()>>,
}

impl MemoryReembedTask {
    /// Wait for the task to finish on its own.
    pub async fn wait(mut self) {
        if let Some(handle) = self.handle.take() {
            #[expect(
                clippy::let_underscore_must_use,
                reason = "a panicked or aborted backfill task has nothing left to report"
            )]
            let _ = handle.await;
        }
    }
}

impl Drop for MemoryReembedTask {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}
