//! Provider switches inside one thread (round 3, item 4).
//!
//! A thread that changed provider runs as a chain of segments, each on one
//! runtime session. The thread row keeps pointing at the current segment,
//! so submit, Stop and the live stream keep routing by `runtime_id`. A
//! segment's marks are message counts in its runtime's own history, so the
//! stitched transcript is exact without timestamps: segment `k` shows
//! `history(runtime_k, session_k)[start_k..end_k]`.
//!
//! The first turn after a switch carries a context handoff (the whole
//! thread, or the delta since the target runtime last ran here, which then
//! resumes its own session) as next-message context. Threads that never
//! switched have no segment rows and read as before.

use std::collections::HashMap;

use ironclaw_host_api::ids::ThreadId;
use libsql::Value;
use serde::Serialize;
use serde_json::json;

use crate::engine::{Engine, Result};
use crate::handoff;
use crate::model::*;
use crate::runtime::{CreateThread, HistoryMessage, HistoryRole};
use crate::store::{Cols, Store, insert_transfer_sql, opt, s};

pub(crate) const SEGMENTS_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS orch_thread_segments (
  thread_id TEXT NOT NULL,
  ordinal INTEGER NOT NULL,
  runtime_id TEXT NOT NULL,
  provider TEXT,
  model TEXT,
  provider_name TEXT,
  native_session_id TEXT,
  start_mark INTEGER NOT NULL,
  end_mark INTEGER,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (thread_id, ordinal)
);
"#;

/// One stretch of a thread on one runtime session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Segment {
    pub runtime_id: RuntimeId,
    pub provider: Option<String>,
    pub model: Option<String>,
    /// The provider's display name when the switch happened.
    pub provider_name: Option<String>,
    pub native_session_id: Option<String>,
    /// Messages the runtime session already held when the segment opened.
    pub start: usize,
    /// Its message count when the thread switched away; `None` = current.
    pub end: Option<usize>,
    pub created_at: Millis,
}

impl Segment {
    /// The inline transcript marker that opens this segment.
    pub fn marker(&self) -> String {
        let name = self
            .provider_name
            .as_deref()
            .or(self.provider.as_deref())
            .unwrap_or("IronClaw");
        let model = self.model.as_deref().unwrap_or("default model");
        format!("Switched to {model} · {name}")
    }

    fn same_session(&self, runtime: &RuntimeId, session: Option<&str>) -> bool {
        &self.runtime_id == runtime && self.native_session_id.as_deref() == session
    }

    fn route(&self) -> String {
        route_label(
            self.provider_name.as_deref().or(self.provider.as_deref()),
            self.model.as_deref(),
        )
    }
}

fn route_label(provider: Option<&str>, model: Option<&str>) -> String {
    format!(
        "{} / {}",
        provider.unwrap_or("IronClaw"),
        model.unwrap_or("default model")
    )
}

fn segment_row(row: &libsql::Row) -> Result<Segment> {
    let mut c = Cols::new(row);
    Ok(Segment {
        runtime_id: RuntimeId::new(c.text()?)?,
        provider: c.otext()?,
        model: c.otext()?,
        provider_name: c.otext()?,
        native_session_id: c.otext()?,
        start: c.int()?.max(0) as usize,
        end: c.oint()?.map(|n| n.max(0) as usize),
        created_at: c.int()?,
    })
}

impl Store {
    /// The thread's segments, oldest first; empty for a thread that never switched.
    pub async fn segments(&self, thread: &ThreadId) -> Result<Vec<Segment>> {
        self.rows(
            "SELECT runtime_id, provider, model, provider_name, native_session_id, start_mark, \
             end_mark, created_at FROM orch_thread_segments WHERE thread_id = ?1 ORDER BY ordinal",
            vec![s(thread.as_str())],
            segment_row,
        )
        .await
    }

    /// Commit a switch in one transaction: the new segment chain, the
    /// thread's current route, and its handoff (superseding an unsent one).
    async fn commit_switch(
        &self,
        thread: &ThreadId,
        segments: &[Segment],
        handoff: Option<&TransferRecord>,
    ) -> Result<()> {
        let id = thread.as_str();
        let Some(current) = segments.last() else {
            return Err(crate::error::OrchestratorError::invalid(
                "a switch needs a segment",
            ));
        };
        let mut statements = vec![(
            "DELETE FROM orch_thread_segments WHERE thread_id = ?1".to_string(),
            vec![s(id)],
        )];
        for (ordinal, seg) in segments.iter().enumerate() {
            statements.push((
                "INSERT INTO orch_thread_segments (thread_id, ordinal, runtime_id, provider, model, \
                 provider_name, native_session_id, start_mark, end_mark, created_at) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)"
                    .to_string(),
                vec![
                    s(id),
                    (ordinal as i64).into(),
                    s(seg.runtime_id.as_str()),
                    opt(seg.provider.clone()),
                    opt(seg.model.clone()),
                    opt(seg.provider_name.clone()),
                    opt(seg.native_session_id.clone()),
                    (seg.start as i64).into(),
                    opt(seg.end.map(|n| n as i64)),
                    seg.created_at.into(),
                ],
            ));
        }
        statements.push((
            "UPDATE orch_threads SET runtime_id = ?2, provider = ?3, model = ?4, \
             native_session_id = ?5, updated_at = ?6 WHERE thread_id = ?1"
                .to_string(),
            vec![
                s(id),
                s(current.runtime_id.as_str()),
                opt(current.provider.clone()),
                opt(current.model.clone()),
                opt(current.native_session_id.clone()),
                now_ms().into(),
            ],
        ));
        statements.push((
            "UPDATE orch_transfers SET status = 'consumed', consumed_at = ?2 \
             WHERE kind = 'route_switch' AND target_thread = ?1 AND status = 'pending'"
                .to_string(),
            vec![s(id), Value::Integer(now_ms())],
        ));
        if let Some(x) = handoff {
            statements.push(insert_transfer_sql(x)?);
        }
        self.tx(statements).await
    }
}

impl Engine {
    pub async fn segments(&self, thread: &ThreadId) -> Result<Vec<Segment>> {
        self.store.segments(thread).await
    }

    /// Whether `runtime` is IronClaw's own loop (its history is the
    /// product transcript of the thread id).
    pub fn is_default_runtime(&self, runtime: &RuntimeId) -> bool {
        runtime == &self.default_runtime
    }

    /// One runtime session's whole history for `thread`.
    async fn session_history(
        &self,
        thread: &ThreadRecord,
        runtime: &RuntimeId,
        session: Option<&str>,
    ) -> Result<Vec<HistoryMessage>> {
        let mut view = thread.clone();
        view.runtime_id = runtime.clone();
        view.native_session_id = session.map(str::to_string);
        self.runtime(runtime)?
            .history(&view, crate::engine::HISTORY_LIMIT)
            .await
            .map_err(Into::into)
    }

    /// Each segment with its own messages, prepended context stripped from
    /// the user's text.
    pub async fn segment_slices(
        &self,
        thread: &ThreadRecord,
        segments: &[Segment],
    ) -> Result<Vec<Vec<HistoryMessage>>> {
        let mut cache: HashMap<(RuntimeId, Option<String>), Vec<HistoryMessage>> = HashMap::new();
        let mut out = Vec::with_capacity(segments.len());
        for seg in segments {
            let key = (seg.runtime_id.clone(), seg.native_session_id.clone());
            if !cache.contains_key(&key) {
                let all = self
                    .session_history(thread, &seg.runtime_id, seg.native_session_id.as_deref())
                    .await?;
                cache.insert(key.clone(), all);
            }
            let all = cache.get(&key).map(Vec::as_slice).unwrap_or_default();
            let end = seg.end.unwrap_or(all.len()).min(all.len());
            let mut slice = all[seg.start.min(end)..end].to_vec();
            for m in slice.iter_mut().filter(|m| m.role == HistoryRole::User) {
                if let Some(typed) = handoff::strip_context(&m.text).map(str::to_string) {
                    m.hidden = typed.is_empty();
                    m.text = typed;
                }
            }
            out.push(slice);
        }
        Ok(out)
    }

    /// The thread's whole transcript across switches.
    pub(crate) async fn stitched_history(
        &self,
        thread: &ThreadRecord,
        segments: &[Segment],
    ) -> Result<Vec<HistoryMessage>> {
        Ok(self
            .segment_slices(thread, segments)
            .await?
            .into_iter()
            .flatten()
            .collect())
    }

    async fn provider_name(&self, scope: &ThreadScope, provider: Option<&str>) -> Option<String> {
        let provider = provider?;
        self.catalog(scope)
            .await
            .into_iter()
            .find(|(_, p)| p.provider == provider)
            .map(|(_, p)| p.name)
    }

    /// Switch a thread that already has history to `runtime_id` without
    /// leaving it. The target resumes the session it had in this thread,
    /// or gets a new one; either way its next turn carries what it has not
    /// seen yet.
    pub(crate) async fn switch_in_place(
        &self,
        thread: &ThreadRecord,
        runtime_id: RuntimeId,
        provider: Option<String>,
        model: Option<String>,
    ) -> Result<serde_json::Value> {
        let id = &thread.thread_id;
        let mut segments = self.store.segments(id).await?;
        if segments.is_empty() {
            segments.push(Segment {
                runtime_id: thread.runtime_id.clone(),
                provider: thread.provider.clone(),
                model: thread.model.clone(),
                provider_name: self
                    .provider_name(&thread.scope, thread.provider.as_deref())
                    .await,
                native_session_id: thread.native_session_id.clone(),
                start: 0,
                end: None,
                created_at: thread.created_at,
            });
        }
        let before = self.stitched_history(thread, &segments).await?;
        let same_runtime = runtime_id == thread.runtime_id;
        // Close the current segment at its runtime's message count.
        let current_len = self
            .session_history(
                thread,
                &thread.runtime_id,
                thread.native_session_id.as_deref(),
            )
            .await?
            .len();
        let open = segments.len() - 1;
        segments[open].end = Some(current_len.max(segments[open].start));
        // A segment that never ran a turn leaves no marker behind.
        if segments.len() > 1 && segments[open].end == Some(segments[open].start) {
            segments.pop();
        }

        let previous = segments
            .iter()
            .rposition(|seg| seg.runtime_id == runtime_id);
        let (provider, model, session, start) = if same_runtime {
            (
                provider,
                model,
                thread.native_session_id.clone(),
                current_len,
            )
        } else if let Some(at) = previous {
            let session = segments[at].native_session_id.clone();
            let len = self
                .session_history(thread, &runtime_id, session.as_deref())
                .await?
                .len();
            (provider, model, session, len)
        } else if runtime_id == self.default_runtime {
            let len = self.session_history(thread, &runtime_id, None).await?.len();
            (provider, model, None, len)
        } else {
            let created = self
                .runtime(&runtime_id)?
                .create_thread(CreateThread {
                    scope: thread.scope.clone(),
                    title: thread.title.clone(),
                    provider: provider.clone(),
                    model: model.clone(),
                    idempotency_key: format!("switch:{id}:{runtime_id}:{}", segments.len()),
                })
                .await?;
            (
                created.provider.or(provider),
                created.model.or(model),
                created.native_session_id,
                0,
            )
        };

        // What the target has not seen: everything after its last segment.
        let handoff_text = if same_runtime {
            None
        } else {
            let seen_until = match previous {
                Some(at) => {
                    let slices = self.segment_slices(thread, &segments[..=at]).await?;
                    slices.iter().map(Vec::len).sum()
                }
                None => 0,
            };
            let unseen = &before[seen_until.min(before.len())..];
            (handoff::visible_count(unseen) > 0).then(|| {
                let from = segments.last().map(Segment::route).unwrap_or_default();
                (unseen.to_vec(), from, previous.is_some())
            })
        };

        let provider_name = self.provider_name(&thread.scope, provider.as_deref()).await;
        let next = Segment {
            runtime_id: runtime_id.clone(),
            provider_name,
            provider,
            model,
            native_session_id: session,
            start,
            end: None,
            created_at: now_ms(),
        };
        // Back where it was before an unused detour: reopen that segment.
        match segments.last_mut() {
            Some(last)
                if last.same_session(&next.runtime_id, next.native_session_id.as_deref())
                    && last.provider == next.provider
                    && last.model == next.model
                    && last.end == Some(next.start) =>
            {
                last.end = None;
            }
            _ => segments.push(next.clone()),
        }

        let budget = self.store.settings().await?.handoff_bytes;
        let transfer = handoff_text.map(|(unseen, from, delta)| {
            let text = handoff::switch_context(&unseen, &from, &next.route(), delta, budget);
            TransferRecord {
                id: TransferId::generate(),
                kind: TransferKind::RouteSwitch,
                source_thread: id.clone(),
                target_thread: Some(id.clone()),
                source_point: Some(json!({ "messages": before.len() })),
                base_point: Some(json!({ "messages": before.len() - unseen.len() })),
                strategy: if delta {
                    TransferStrategy::DeltaContext
                } else {
                    TransferStrategy::PortableContext
                },
                status: TransferStatus::Pending,
                delivery: TransferDelivery::NextMessage,
                handoff: Some(text),
                created_by: Author::User,
                created_at: now_ms(),
                consumed_at: None,
            }
        });
        self.store
            .commit_switch(id, &segments, transfer.as_ref())
            .await?;
        Ok(json!({
            "ok": true, "threadId": id.as_str(),
            "moved": if same_runtime { "in_place" } else { "switched" },
            "forkThreadId": null, "provider": next.provider, "model": next.model,
            "handoffBytes": transfer.as_ref().and_then(|x| x.handoff.as_ref()).map(String::len),
            "resumed": previous.is_some() && !same_runtime,
        }))
    }
}
