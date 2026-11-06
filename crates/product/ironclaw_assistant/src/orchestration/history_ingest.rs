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
    let sources = engine
        .external_transcript_sources(thread)
        .await
        .map_err(|e| e.to_string())?;
    let summary = ExternalThreadSummary {
        thread_id: thread.thread_id.clone(),
        title: thread.title.clone(),
        created_at: at(Some(thread.created_at)),
        updated_at: at(Some(thread.updated_at)),
    };
    let mut written = 0;
    for source in sources {
        let key = ExternalTranscriptSource {
            segment: source.segment,
            runtime_id: source.runtime_id.as_str().to_string(),
            session_id: source.session_id.clone(),
        };
        let cursor = threads
            .external_transcript_cursor(ExternalTranscriptCursorRequest {
                scope: scope.clone(),
                thread_id: thread.thread_id.clone(),
                source: key.clone(),
            })
            .await
            .map_err(|e| e.to_string())?;
        // The last ingested row may have grown since (a run's later
        // assistant steps join it), so it is re-checked.
        let from = cursor.saturating_sub(1);
        let rows = timeline_rows(&thread.thread_id, &source.messages);
        let messages: Vec<ExternalTranscriptMessage> = rows
            .iter()
            .enumerate()
            .filter(|(ordinal, _)| *ordinal as u64 >= from)
            .filter_map(|(ordinal, row)| {
                let id = if segmented {
                    segment_message_id(&thread.thread_id, source.segment as usize, row.message_id)
                } else {
                    row.message_id
                };
                projection(row, ordinal as u64, id)
            })
            .collect();
        for chunk in messages.chunks(UPSERT_CHUNK) {
            let outcome = threads
                .upsert_external_transcript(UpsertExternalTranscriptRequest {
                    scope: scope.clone(),
                    thread: summary.clone(),
                    source: key.clone(),
                    messages: chunk.to_vec(),
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

async fn sweep(
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
    let mut complete = true;
    for thread in list {
        match ingest_thread(&engine, threads, &thread).await {
            Ok(written) => {
                tracing::debug!(thread = %thread.thread_id, written, "history sweep indexed a CLI thread")
            }
            Err(error) => {
                complete = false;
                tracing::debug!(thread = %thread.thread_id, %error, "history sweep skipped a CLI thread");
            }
        }
    }
    complete
}

/// Re-index a CLI thread after each of its completed turns.
pub(crate) struct IngestOnCompletion {
    pub(crate) engine: Weak<Engine>,
    pub(crate) threads: Arc<dyn SessionThreadService>,
}

impl TurnCompletedListener for IngestOnCompletion {
    fn turn_completed(&self, thread: &ThreadRecord) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let (engine, threads, thread) = (
            self.engine.clone(),
            Arc::clone(&self.threads),
            thread.clone(),
        );
        runtime.spawn(async move {
            let Some(engine) = engine.upgrade() else {
                return;
            };
            if let Err(error) = ingest_thread(&engine, threads.as_ref(), &thread).await {
                tracing::debug!(thread = %thread.thread_id, %error, "history indexing after a turn failed");
            }
        });
    }

    /// Indexing never holds a thread's title or badges back.
    fn busy(&self, _thread: &ThreadId) -> bool {
        false
    }
}
