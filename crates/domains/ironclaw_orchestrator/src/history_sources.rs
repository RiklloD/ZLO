//! The transcripts of threads the orchestrator runs off IronClaw's loop, as
//! history search ingests them: one source per runtime segment, read whole
//! (not capped at the timeline's `HISTORY_LIMIT`), carried context stripped.
//! Read-only: the runtimes' own transcripts are never touched.

use std::collections::HashSet;

use crate::engine::{Engine, Result};
use crate::handoff;
use ironclaw_host_api::ids::ThreadId;

use crate::model::{RuntimeId, ThreadRecord};
use crate::profiles::ProfileOwner;
use crate::runtime::{HistoryMessage, TurnOrigin};
use crate::store::{Cols, Store, s};

/// Runs the engine started with its own text (not the person's): the
/// provenance history search uses to leave that text out.
pub(crate) const RUN_ORIGINS_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS orch_run_origins (
  thread_id TEXT NOT NULL,
  run_id TEXT NOT NULL,
  origin TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (thread_id, run_id)
);
"#;

impl TurnOrigin {
    /// Text the engine writes on its own as a user turn: a delegation
    /// result, a context handoff, the restart continue prompt.
    pub fn is_engine_scaffolding(self) -> bool {
        matches!(
            self,
            TurnOrigin::ResultDelivery | TurnOrigin::ContextHandoff | TurnOrigin::AutoContinue
        )
    }

    fn tag(self) -> &'static str {
        match self {
            TurnOrigin::DelegatedBrief => "delegated_brief",
            TurnOrigin::QueueDrain => "queue_drain",
            TurnOrigin::ResultDelivery => "result_delivery",
            TurnOrigin::ContextHandoff => "context_handoff",
            TurnOrigin::AutoContinue => "auto_continue",
            TurnOrigin::AgentSend => "agent_send",
            TurnOrigin::UserPrompt => "user_prompt",
        }
    }
}

impl Store {
    /// Remember that `run` in `thread` was started with engine scaffolding.
    pub async fn record_scaffolding_run(
        &self,
        thread: &ThreadId,
        run: &str,
        origin: TurnOrigin,
    ) -> Result<()> {
        self.exec(
            "INSERT OR IGNORE INTO orch_run_origins (thread_id, run_id, origin, created_at) \
             VALUES (?1, ?2, ?3, ?4)",
            vec![
                s(thread.as_str()),
                s(run),
                s(origin.tag()),
                crate::model::now_ms().into(),
            ],
        )
        .await?;
        Ok(())
    }

    /// `(thread_id, run_id)` of every scaffolding run in the owner's threads.
    pub async fn scaffolding_runs(
        &self,
        owner: &ProfileOwner,
    ) -> Result<HashSet<(String, String)>> {
        let rows = self
            .rows(
                "SELECT o.thread_id, o.run_id FROM orch_run_origins o \
                 JOIN orch_threads t ON t.thread_id = o.thread_id \
                 WHERE t.tenant_id = ?1 AND t.user_id = ?2",
                vec![s(owner.tenant_id.as_str()), s(owner.user_id.as_str())],
                |row| {
                    let mut c = Cols::new(row);
                    Ok((c.text()?, c.text()?))
                },
            )
            .await?;
        Ok(rows.into_iter().collect())
    }
}

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

/// Where a source's messages come from, before reading it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptSourcePlan {
    pub segment: u32,
    pub runtime_id: RuntimeId,
    pub session_id: Option<String>,
    start: usize,
    end: Option<usize>,
}

impl TranscriptSourcePlan {
    /// A segment the thread switched away from: its slice never changes.
    pub fn closed(&self) -> bool {
        self.end.is_some()
    }
}

impl Engine {
    /// The thread's external-runtime sources, oldest segment first, without
    /// reading any transcript. Segments on IronClaw's own loop are skipped
    /// (their messages are the canonical transcript, indexed by the thread
    /// service itself).
    pub async fn external_source_plans(
        &self,
        thread: &ThreadRecord,
    ) -> Result<Vec<TranscriptSourcePlan>> {
        let segments = self.store.segments(&thread.thread_id).await?;
        if segments.is_empty() {
            if self.is_default_runtime(&thread.runtime_id) {
                return Ok(Vec::new());
            }
            return Ok(vec![TranscriptSourcePlan {
                segment: 0,
                runtime_id: thread.runtime_id.clone(),
                session_id: thread.native_session_id.clone(),
                start: 0,
                end: None,
            }]);
        }
        Ok(segments
            .iter()
            .enumerate()
            .filter(|(_, seg)| !self.is_default_runtime(&seg.runtime_id))
            .map(|(k, seg)| TranscriptSourcePlan {
                segment: u32::try_from(k).unwrap_or(u32::MAX),
                runtime_id: seg.runtime_id.clone(),
                session_id: seg.native_session_id.clone(),
                start: seg.start,
                end: seg.end,
            })
            .collect())
    }

    /// Read one source whole (not capped at `HISTORY_LIMIT`), carried
    /// context stripped.
    pub async fn read_external_source(
        &self,
        thread: &ThreadRecord,
        plan: &TranscriptSourcePlan,
    ) -> Result<Vec<HistoryMessage>> {
        let all = self
            .full_session_history(thread, &plan.runtime_id, plan.session_id.as_deref())
            .await?;
        let end = plan.end.unwrap_or(all.len()).min(all.len());
        let mut messages = all[plan.start.min(end)..end].to_vec();
        handoff::strip_carried_context(&mut messages);
        Ok(messages)
    }

    /// Every external-runtime source of `thread`, read.
    pub async fn external_transcript_sources(
        &self,
        thread: &ThreadRecord,
    ) -> Result<Vec<TranscriptSource>> {
        let mut out = Vec::new();
        for plan in self.external_source_plans(thread).await? {
            let messages = self.read_external_source(thread, &plan).await?;
            out.push(TranscriptSource {
                segment: plan.segment,
                runtime_id: plan.runtime_id,
                session_id: plan.session_id,
                messages,
            });
        }
        Ok(out)
    }

    /// The latest durable activity stamp of `thread` (creation, user
    /// message, turn end, any update): when it is unchanged, so is the
    /// thread's transcript as far as the engine can tell.
    pub fn thread_activity_version(thread: &ThreadRecord) -> i64 {
        [
            Some(thread.created_at),
            Some(thread.updated_at),
            thread.turn_ended_at,
            thread.last_user_at,
        ]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or(thread.updated_at)
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
