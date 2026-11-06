//! History search for threads that run on a CLI runtime (Claude, Codex).
//!
//! Their transcripts live in the runtime's own store, so the thread service
//! cannot index them. This feeds it search projections instead: each thread's
//! external transcript sources (`Engine::external_transcript_sources`, read
//! whole) are cut into exactly the rows the thread's timeline renders
//! (`external::timeline_rows`, re-keyed per segment like the stitched
//! timeline), filtered by the shared visibility rule, and upserted from the
//! source's durable cursor. Idempotent and restart-safe; the runtime
//! transcripts are only read.
//!
//! Two triggers: every completed turn (`IngestOnCompletion`, an engine turn
//! listener) and a once-per-process sweep of the owner's CLI threads started
//! by the first history search (`spawn_owner_sweep`), which is how existing
//! threads — imported terminal sessions included, once they are threads
//! read by these runtimes — get indexed. Background work logs at `debug!`.

use std::collections::HashSet;
use std::sync::{Arc, LazyLock, Mutex, Weak};

use chrono::{DateTime, Utc};
use ironclaw_host_api::ids::{TenantId, ThreadId, UserId};
use ironclaw_orchestrator::engine::{Engine, TurnCompletedListener};
use ironclaw_orchestrator::model::ThreadRecord;
use ironclaw_orchestrator::profiles::ProfileOwner;
use ironclaw_threads::{
    ExternalThreadSummary, ExternalTranscriptCursorRequest, ExternalTranscriptMessage,
    ExternalTranscriptSource, SearchHitRole, SessionThreadService, ThreadMessageId, ThreadScope,
    UpsertExternalTranscriptRequest, visible_user_text,
};

use super::external::{TimelineRow, segment_message_id, timeline_rows};

/// Rows sent per upsert call.
const UPSERT_CHUNK: usize = 200;

/// The thread-service scope a CLI thread's projections live in: the owner's
/// own cell, the same one `history_search::caller_thread_scope` searches.
pub(crate) fn projection_scope(
    scope: &ironclaw_orchestrator::model::ThreadScope,
) -> Option<ThreadScope> {
    Some(ThreadScope {
        tenant_id: scope.tenant_id.clone(),
        agent_id: scope.agent_id.clone()?,
        project_id: scope.project_id.clone(),
        owner_user_id: Some(scope.user_id.clone()),
        mission_id: None,
    })
}

fn at(ms: Option<i64>) -> Option<DateTime<Utc>> {
    ms.and_then(DateTime::<Utc>::from_timestamp_millis)
}

/// A timeline row as a search projection, when it is visible conversation.
fn projection(
    row: &TimelineRow,
    ordinal: u64,
    id: uuid::Uuid,
) -> Option<ExternalTranscriptMessage> {
    let role = match row.kind {
        "user" => SearchHitRole::User,
        "assistant" => SearchHitRole::Assistant,
        _ => return None,
    };
    if role == SearchHitRole::User && visible_user_text(&row.text).is_none() {
        return None;
    }
    if row.text.trim().is_empty() {
        return None;
    }
    let navigation_message_id = ThreadMessageId::parse(&id.to_string()).ok()?; // silent-ok: a Uuid always round-trips its own text
    Some(ExternalTranscriptMessage {
        ordinal,
        navigation_message_id,
        role,
        text: row.text.clone(),
        created_at: at(row.at),
        run_id: row.run_id.clone(),
    })
}

/// Digest of the last settled row before `through` (the row at
/// `through - 2`; the newest row may still grow). Stable across processes.
fn anchor_of(rows: &[TimelineRow], through: u64) -> Option<String> {
    let at = usize::try_from(through).ok()?.checked_sub(2)?;
    let row = rows.get(at)?;
    Some(
        uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_OID,
            format!("{}\u{0}{}", row.kind, row.text).as_bytes(),
        )
        .to_string(),
    )
}

/// Ingest `thread`'s external transcripts. Returns rows written.
pub(crate) async fn ingest_thread(
    engine: &Engine,
    threads: &dyn SessionThreadService,
    thread: &ThreadRecord,
) -> Result<usize, String> {
    let Some(scope) = projection_scope(&thread.scope) else {
        // No agent binding: the thread is outside every searchable cell.
        return Ok(0);
    };
    let segmented = !engine
        .segments(&thread.thread_id)
        .await
        .map_err(|e| e.to_string())?
        .is_empty();
    let plans = engine
        .external_source_plans(thread)
        .await
        .map_err(|e| e.to_string())?;
    let summary = ExternalThreadSummary {
        thread_id: thread.thread_id.clone(),
        title: thread.title.clone(),
        created_at: at(Some(thread.created_at)),
        updated_at: at(Some(thread.updated_at)),
        source_version: Some(Engine::thread_activity_version(thread)),
    };
    let mut written = 0;
    for plan in plans {
        let key = ExternalTranscriptSource {
            segment: plan.segment,
            runtime_id: plan.runtime_id.as_str().to_string(),
            session_id: plan.session_id.clone(),
        };
        let cursor = threads
            .external_transcript_cursor(ExternalTranscriptCursorRequest {
                scope: scope.clone(),
                thread_id: thread.thread_id.clone(),
                source: key.clone(),
            })
            .await
            .map_err(|e| e.to_string())?;
        // A closed segment never changes: once fully ingested it is not
        // read again.
        if plan.closed() && cursor.complete {
            continue;
        }
        let source = engine
            .read_external_source(thread, &plan)
            .await
            .map_err(|e| e.to_string())?;
        let rows = timeline_rows(&thread.thread_id, &source);
        // A source that shrank, or whose last settled row is no longer the
        // one ingested, was rewritten: re-ingest it whole as a new generation.
        let rewritten = cursor.ingested_through > rows.len() as u64
            || cursor
                .anchor
                .as_deref()
                .is_some_and(|a| anchor_of(&rows, cursor.ingested_through).as_deref() != Some(a));
        // Otherwise only new rows, plus the last ingested one, which may
        // have grown since (a run's later assistant steps join it).
        let from = if rewritten {
            0
        } else {
            cursor.ingested_through.saturating_sub(1)
        };
        let messages: Vec<ExternalTranscriptMessage> = rows
            .iter()
            .enumerate()
            .filter(|(ordinal, _)| *ordinal as u64 >= from)
            .filter_map(|(ordinal, row)| {
                let id = if segmented {
                    segment_message_id(&thread.thread_id, plan.segment as usize, row.message_id)
                } else {
                    row.message_id
                };
                projection(row, ordinal as u64, id)
            })
            .collect();
        // At least one call per source, so the summary (with its activity
        // version) and a closed source's completion are recorded even when
        // no row changed.
        let chunks: Vec<&[ExternalTranscriptMessage]> = if messages.is_empty() {
            vec![&[][..]]
        } else {
            messages.chunks(UPSERT_CHUNK).collect()
        };
        let last = chunks.len().saturating_sub(1);
        for (n, chunk) in chunks.into_iter().enumerate() {
            let through = chunk
                .iter()
                .map(|m| m.ordinal.saturating_add(1))
                .max()
                .unwrap_or(0);
            let outcome = threads
                .upsert_external_transcript(UpsertExternalTranscriptRequest {
                    scope: scope.clone(),
                    thread: summary.clone(),
                    source: key.clone(),
                    messages: chunk.to_vec(),
                    // The new generation starts with the batch that ends it.
                    replace: rewritten && n == last,
                    anchor: anchor_of(&rows, through),
                    source_complete: plan.closed() && n == last,
                })
                .await
                .map_err(|e| e.to_string())?;
            written += outcome.written;
        }
    }
    Ok(written)
}

/// An owner, per engine instance (an engine is one deployment's store).
type OwnerKey = (usize, TenantId, UserId);

fn owner_key(engine: &Engine, owner: &ProfileOwner) -> OwnerKey {
    (
        std::ptr::from_ref(engine) as usize,
        owner.tenant_id.clone(),
        owner.user_id.clone(),
    )
}

/// Owners whose CLI threads were swept in this process, and sweeps running.
/// Single-flight bookkeeping, not a cache of any stored value.
static SWEPT: LazyLock<Mutex<HashSet<OwnerKey>>> = LazyLock::new(Default::default);
static SWEEPING: LazyLock<Mutex<HashSet<OwnerKey>>> = LazyLock::new(Default::default);

/// Whether this process already indexed every CLI thread of `owner`
/// (completed turns keep it current afterwards).
pub(crate) fn owner_swept(engine: &Engine, owner: &ProfileOwner) -> bool {
    SWEPT
        .lock()
        .is_ok_and(|s| s.contains(&owner_key(engine, owner)))
}

/// Index every CLI thread of `owner` in the background, once per process.
pub(crate) fn spawn_owner_sweep(
    engine: &Arc<Engine>,
    threads: &Arc<dyn SessionThreadService>,
    owner: ProfileOwner,
) {
    if owner_swept(engine, &owner) {
        return;
    }
    let key = owner_key(engine, &owner);
    let Ok(mut running) = SWEEPING.lock() else {
        return;
    };
    if !running.insert(key.clone()) {
        return;
    }
    drop(running);
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        if let Ok(mut running) = SWEEPING.lock() {
            running.remove(&key);
        }
        return;
    };
    let engine = Arc::downgrade(engine);
    let threads = Arc::clone(threads);
    runtime.spawn(async move {
        let complete = sweep(&engine, threads.as_ref(), &owner).await;
        if complete && let Ok(mut swept) = SWEPT.lock() {
            swept.insert(key.clone());
        }
        if let Ok(mut running) = SWEEPING.lock() {
            running.remove(&key);
        }
    });
}

/// Threads one sweep pass ingests before stopping; the next search starts
/// the next pass, which skips what this one finished.
const SWEEP_THREADS_PER_PASS: usize = 25;
/// Rows one sweep pass writes before stopping.
const SWEEP_ROWS_PER_PASS: usize = 5_000;

/// One bounded pass over the owner's CLI threads. A thread whose activity
/// version matches its stored summary is skipped without reading its
/// transcript, so a pass resumes where the last one (or the last process)
/// stopped. `true` when every thread is current.
pub(super) async fn sweep(
    engine: &Weak<Engine>,
    threads: &dyn SessionThreadService,
    owner: &ProfileOwner,
) -> bool {
    let Some(engine) = engine.upgrade() else {
        return false;
    };
    let list = match engine.owner_external_threads(owner).await {
        Ok(list) => list,
        Err(error) => {
            tracing::debug!(%error, "history sweep could not list CLI threads");
            return false;
        }
    };
    let (mut ingested, mut rows) = (0usize, 0usize);
    let mut complete = true;
    for thread in list {
        if ingested >= SWEEP_THREADS_PER_PASS || rows >= SWEEP_ROWS_PER_PASS {
            tracing::debug!(ingested, rows, "history sweep pass reached its budget");
            return false;
        }
        if thread_is_current(threads, &thread).await {
            continue;
        }
        match ingest_thread(&engine, threads, &thread).await {
            Ok(written) => {
                ingested += 1;
                rows += written;
                tracing::debug!(thread = %thread.thread_id, written, "history sweep indexed a CLI thread");
            }
            Err(error) => {
                complete = false;
                tracing::debug!(thread = %thread.thread_id, %error, "history sweep skipped a CLI thread");
            }
        }
        tokio::task::yield_now().await;
    }
    complete
}

/// The thread's stored summary carries its current activity version.
async fn thread_is_current(threads: &dyn SessionThreadService, thread: &ThreadRecord) -> bool {
    let Some(scope) = projection_scope(&thread.scope) else {
        return true;
    };
    match threads
        .external_thread_summary(&scope, &thread.thread_id)
        .await
    {
        Ok(Some(summary)) => {
            summary.source_version == Some(Engine::thread_activity_version(thread))
        }
        Ok(None) => false,
        Err(error) => {
            tracing::debug!(%error, "history sweep could not read a thread summary");
            false
        }
    }
}

/// Re-index a CLI thread after each of its completed turns. Jobs coalesce
/// per thread: one ingestion runs at a time, and completions that land
/// while it runs fold into a single follow-up run.
pub(crate) struct IngestOnCompletion {
    pub(crate) engine: Weak<Engine>,
    pub(crate) threads: Arc<dyn SessionThreadService>,
    /// Threads with an ingestion running, and whether another is due.
    pub(crate) running: Arc<Mutex<std::collections::HashMap<ThreadId, bool>>>,
}

impl IngestOnCompletion {
    pub(crate) fn new(engine: Weak<Engine>, threads: Arc<dyn SessionThreadService>) -> Self {
        Self {
            engine,
            threads,
            running: Arc::default(),
        }
    }
}

impl TurnCompletedListener for IngestOnCompletion {
    fn turn_completed(&self, thread: &ThreadRecord) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        {
            let Ok(mut running) = self.running.lock() else {
                return;
            };
            if let Some(again) = running.get_mut(&thread.thread_id) {
                *again = true;
                return;
            }
            running.insert(thread.thread_id.clone(), false);
        }
        let (engine, threads, running, id) = (
            self.engine.clone(),
            Arc::clone(&self.threads),
            Arc::clone(&self.running),
            thread.thread_id.clone(),
        );
        runtime.spawn(async move {
            loop {
                let Some(engine) = engine.upgrade() else {
                    break;
                };
                // Re-read: the completion's record may be stale by now.
                match engine.store().thread(&id).await {
                    Ok(Some(thread)) => {
                        if let Err(error) = ingest_thread(&engine, threads.as_ref(), &thread).await
                        {
                            tracing::debug!(thread = %id, %error, "history indexing after a turn failed");
                        }
                    }
                    Ok(None) => {}
                    Err(error) => tracing::debug!(thread = %id, %error, "history indexing could not read the thread"),
                }
                let Ok(mut state) = running.lock() else {
                    break;
                };
                match state.get_mut(&id) {
                    Some(again) if *again => *again = false,
                    _ => {
                        state.remove(&id);
                        break;
                    }
                }
            }
        });
    }

    /// Indexing never holds a thread's title or badges back.
    fn busy(&self, _thread: &ThreadId) -> bool {
        false
    }
}
