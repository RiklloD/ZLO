//! The transcripts of threads the orchestrator runs off IronClaw's loop, as
//! history search ingests them: one source per runtime segment, read whole
//! (not capped at the timeline's `HISTORY_LIMIT`), carried context stripped.
//! Read-only: the runtimes' own transcripts are never touched.

use crate::engine::{Engine, Result};
use crate::handoff;
use crate::model::{RuntimeId, ThreadRecord};
use crate::profiles::ProfileOwner;
use crate::runtime::HistoryMessage;

/// One runtime transcript feeding a thread.
#[derive(Debug, Clone)]
pub struct TranscriptSource {
    /// The segment's position among the thread's segments (0 when the thread
    /// never switched engines); stitched timelines key message ids by it.
    pub segment: u32,
    pub runtime_id: RuntimeId,
    pub session_id: Option<String>,
    pub messages: Vec<HistoryMessage>,
}

impl Engine {
    /// Every external-runtime source of `thread`, oldest segment first.
    /// Segments on IronClaw's own loop are skipped (their messages are the
    /// canonical transcript, indexed by the thread service itself).
    pub async fn external_transcript_sources(
        &self,
        thread: &ThreadRecord,
    ) -> Result<Vec<TranscriptSource>> {
        let segments = self.store.segments(&thread.thread_id).await?;
        if segments.is_empty() {
            if self.is_default_runtime(&thread.runtime_id) {
                return Ok(Vec::new());
            }
            let messages = self
                .full_session_history(
                    thread,
                    &thread.runtime_id,
                    thread.native_session_id.as_deref(),
                )
                .await?;
            return Ok(vec![TranscriptSource {
                segment: 0,
                runtime_id: thread.runtime_id.clone(),
                session_id: thread.native_session_id.clone(),
                messages,
            }]);
        }
        let mut out = Vec::new();
        for (k, seg) in segments.iter().enumerate() {
            if self.is_default_runtime(&seg.runtime_id) {
                continue;
            }
            let all = self
                .full_session_history(thread, &seg.runtime_id, seg.native_session_id.as_deref())
                .await?;
            let end = seg.end.unwrap_or(all.len()).min(all.len());
            let mut messages = all[seg.start.min(end)..end].to_vec();
            handoff::strip_carried_context(&mut messages);
            out.push(TranscriptSource {
                segment: u32::try_from(k).unwrap_or(u32::MAX),
                runtime_id: seg.runtime_id.clone(),
                session_id: seg.native_session_id.clone(),
                messages,
            });
        }
        Ok(out)
    }

    /// The owner's threads that have at least one external-runtime source.
    pub async fn owner_external_threads(&self, owner: &ProfileOwner) -> Result<Vec<ThreadRecord>> {
        let mut out = Vec::new();
        for listed in self.store.inbox_threads(owner).await? {
            let Some(thread) = self.store.thread(&listed.thread_id).await? else {
                continue;
            };
            if !self.is_default_runtime(&thread.runtime_id)
                || !self.store.segments(&thread.thread_id).await?.is_empty()
            {
                out.push(thread);
            }
        }
        Ok(out)
    }

    async fn full_session_history(
        &self,
        thread: &ThreadRecord,
        runtime: &RuntimeId,
        session: Option<&str>,
    ) -> Result<Vec<HistoryMessage>> {
        let mut view = thread.clone();
        view.runtime_id = runtime.clone();
        view.native_session_id = session.map(str::to_string);
        let mut messages = self.runtime(runtime)?.history(&view, usize::MAX).await?;
        handoff::strip_carried_context(&mut messages);
        Ok(messages)
    }
}
