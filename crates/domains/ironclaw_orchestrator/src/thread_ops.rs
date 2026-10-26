//! Thread operations: the durable queue, send/steer, runs, fork and
//! merge-back, lineage.

use ironclaw_host_api::ids::ThreadId;
use serde_json::{Value, json};

use crate::delegation::Target;
use crate::engine::{CallContext, Engine, Result, continue_prompt, json_ok};
use crate::error::OrchestratorError;
use crate::handoff;
use crate::model::*;
use crate::runtime::*;

fn require_text(text: &str, field: &str) -> Result<String> {
    let text = text.trim();
    if text.is_empty() {
        return Err(OrchestratorError::invalid(format!("{field} is required")));
    }
    Ok(text.to_string())
}

impl Engine {
    // ── queue ───────────────────────────────────────────────────────────

    pub async fn queue_list(&self, thread: &ThreadId) -> Result<Value> {
        let hold = self.store.hold(thread).await?;
        // Clients poll this document; a thread off IronClaw's loop has no run
        // stream, so its runtime and turn state ride along.
        let record = self.store.thread(thread).await?;
        let items: Vec<Value> = self
            .store
            .queue(thread, false)
            .await?
            .iter()
            .enumerate()
            .map(|(i, q)| {
                json!({"id": q.id.as_str(), "text": q.text, "position": i + 1, "status": q.status.as_str(),
                       "createdAt": q.created_at, "updatedAt": q.updated_at, "createdBy": q.created_by.as_str()})
            })
            .collect();
        Ok(json!({"threadId": thread.as_str(), "held": hold.is_some(),
                  "holdReason": hold.map(HoldReason::as_str), "items": items,
                  "runtime": record.as_ref().map(|t| t.runtime_id.as_str()),
                  "running": record.as_ref().is_some_and(ThreadRecord::is_running),
                  "activeRunId": record.as_ref().and_then(|t| t.active_run_id.as_deref())}))
    }

    async fn pending_item(&self, thread: &ThreadId, id: &QueueItemId) -> Result<QueueItem> {
        let item = self.store.queue_item(thread, id).await?.ok_or_else(|| {
            OrchestratorError::not_found(
                "not_found",
                format!("No queued message {id} in this thread."),
            )
        })?;
        if item.status != QueueStatus::Pending {
            return Err(OrchestratorError::request(
                "not_queued",
                format!("Queued message {id} is already {}.", item.status),
            ));
        }
        Ok(item)
    }

    pub async fn queue_add(&self, thread: &ThreadId, text: &str, author: Author) -> Result<Value> {
        let text = require_text(text, "text")?;
        let id = self.store.queue_add(thread, &text, author).await?;
        let mut view = self.queue_list(thread).await?;
        view["queuedId"] = json!(id.as_str());
        Ok(json_ok(view))
    }

    pub async fn queue_edit(
        &self,
        thread: &ThreadId,
        id: &QueueItemId,
        text: &str,
    ) -> Result<Value> {
        self.pending_item(thread, id).await?;
        let text = require_text(text, "text")?;
        if !self.store.queue_edit(id, &text).await? {
            return Err(OrchestratorError::request(
                "not_queued",
                "The message was sent meanwhile.",
            ));
        }
        self.queue_list(thread).await.map(json_ok)
    }

    pub async fn queue_reorder(
        &self,
        thread: &ThreadId,
        id: &QueueItemId,
        before: Option<&QueueItemId>,
    ) -> Result<Value> {
        self.pending_item(thread, id).await?;
        self.store.queue_reorder(thread, id, before).await?;
        self.queue_list(thread).await.map(json_ok)
    }

    pub async fn queue_remove(&self, thread: &ThreadId, id: &QueueItemId) -> Result<Value> {
        self.pending_item(thread, id).await?;
        self.store
            .set_queue_status(id, QueueStatus::Pending, QueueStatus::Cancelled)
            .await?;
        self.queue_list(thread).await.map(json_ok)
    }

    pub async fn queue_clear(&self, thread: &ThreadId) -> Result<Value> {
        self.store.queue_clear(thread).await?;
        self.queue_list(thread).await.map(json_ok)
    }

    /// Releasing an `Interrupted` hold is the Continue action: the cut turn
    /// resumes first, then the queue drains behind it.
    pub async fn queue_hold(&self, thread: &ThreadId, held: bool) -> Result<Value> {
        let was_interrupted = self.store.hold(thread).await? == Some(HoldReason::Interrupted);
        self.store
            .set_hold(thread, held.then_some(HoldReason::User))
            .await?;
        if !held && was_interrupted {
            self.deliver(thread, continue_prompt(), TurnOrigin::AutoContinue, true)
                .await?;
        }
        self.queue_list(thread).await.map(json_ok)
    }

    /// Deliver a queued item into the running turn (steer) or redirect it.
    pub async fn queue_promote(
        &self,
        thread: &ThreadRecord,
        id: &QueueItemId,
        how: SteerHow,
    ) -> Result<Value> {
        let item = self.pending_item(&thread.thread_id, id).await?;
        if !thread.is_running() {
            return Err(OrchestratorError::request(
                "no_active_run",
                "The thread has no active turn to steer; it will run from the queue.",
            ));
        }
        let outcome = self.steer(thread, &item.text, how).await?;
        if outcome == SteerOutcome::Rejected {
            return Err(OrchestratorError::request(
                "steer_rejected",
                "The active turn can't take it mid-turn; it stays queued and runs when the turn ends.",
            ));
        }
        self.store
            .set_queue_status(id, QueueStatus::Pending, QueueStatus::Sent)
            .await?;
        let mut view = self.queue_list(&thread.thread_id).await?;
        view["steerStatus"] = json!(steer_label(&outcome));
        Ok(json_ok(view))
    }

    /// A steer never kills the turn: on a runtime without in-turn steering
    /// it is `Rejected`, and callers leave the text in the queue. Only an
    /// explicit `Restart` stops and restarts the turn.
    pub(crate) async fn steer(
        &self,
        thread: &ThreadRecord,
        text: &str,
        how: SteerHow,
    ) -> Result<SteerOutcome> {
        let runtime = self.runtime(&thread.runtime_id)?;
        if how == SteerHow::Steer && !runtime.capabilities().steer_in_turn {
            return Ok(SteerOutcome::Rejected);
        }
        Ok(runtime.steer(thread, text, how).await?)
    }

    // ── send ────────────────────────────────────────────────────────────

    /// T3's `t3_thread send`: `auto` starts an idle thread or steers an
    /// active one; `queue`, `steer` and `restart` are explicit.
    pub async fn send(
        &self,
        ctx: &CallContext,
        thread: &ThreadId,
        text: &str,
        mode: SendMode,
    ) -> Result<Value> {
        let text = require_text(text, "message")?;
        if thread == &ctx.thread_id && mode != SendMode::Queue {
            return Err(OrchestratorError::invalid(
                "Use mode='queue' to send a follow-up to this same thread.",
            ));
        }
        let record = self.owned_thread(thread, &ctx.scope).await?;
        match mode {
            SendMode::Queue => self.queue_add(thread, &text, Author::Agent).await,
            SendMode::Steer | SendMode::Restart => {
                if !record.is_running() {
                    return Err(OrchestratorError::request(
                        "no_active_run",
                        format!("mode={mode} needs an active turn in the target thread."),
                    ));
                }
                let how = if mode == SendMode::Steer {
                    SteerHow::Steer
                } else {
                    SteerHow::Restart
                };
                let outcome = self.steer(&record, &text, how).await?;
                Ok(
                    json!({"ok": outcome != SteerOutcome::Rejected, "status": steer_label(&outcome)}),
                )
            }
            SendMode::Auto => {
                if record.is_running() {
                    let outcome = self.steer(&record, &text, SteerHow::Steer).await?;
                    if outcome != SteerOutcome::Rejected {
                        return Ok(
                            json!({"ok": true, "status": format!("steered ({})", steer_label(&outcome))}),
                        );
                    }
                    return self.queue_add(thread, &text, Author::Agent).await;
                }
                // An explicit agent send is not an automatic wake: no hold check.
                if self
                    .deliver(thread, text.clone(), TurnOrigin::AgentSend, false)
                    .await?
                {
                    Ok(json!({"ok": true, "status": "started"}))
                } else {
                    self.queue_add(thread, &text, Author::Agent).await
                }
            }
        }
    }

    // ── runs, fork, merge-back ──────────────────────────────────────────

    pub async fn runs(&self, thread: &ThreadRecord) -> Result<Value> {
        let messages = self.history(thread).await?;
        let usage = self.store.turn_log(&thread.thread_id, 200).await?;
        Ok(
            json!({"threadId": thread.thread_id.as_str(), "runs": handoff::runs(&messages), "turns": usage}),
        )
    }

    pub async fn fork(
        &self,
        ctx: &CallContext,
        source: &ThreadRecord,
        run: Option<usize>,
        title: Option<&str>,
        target: Option<&Target>,
        created_by: Author,
    ) -> Result<Value> {
        let messages = self.history(source).await?;
        if messages.is_empty() {
            return Err(OrchestratorError::invalid(
                "Nothing to fork: the thread has no messages yet.",
            ));
        }
        let all_runs = handoff::runs(&messages);
        let cut = handoff::cut_for_run(&messages, run).map_err(OrchestratorError::invalid)?;
        let ordinal = run.unwrap_or(all_runs.len());
        let latest = run.is_none() || run == Some(all_runs.len());
        let source_ctx = CallContext {
            thread_id: source.thread_id.clone(),
            scope: source.scope.clone(),
            provider: source.provider.clone(),
            model: source.model.clone(),
        };
        if target.is_some_and(|t| t.profile.as_deref().is_some_and(|p| !p.trim().is_empty())) {
            return Err(OrchestratorError::request(
                "profile_unavailable",
                "A fork keeps its source thread's profile; delegate to run as another profile.",
            ));
        }
        let (runtime_id, provider, model) = self
            .resolve_target(
                &target.cloned().unwrap_or_default(),
                &source_ctx,
                &source.runtime_id,
            )
            .await?;
        let runtime = self.runtime(&runtime_id)?;
        let source_title = source.title.clone().unwrap_or_else(|| "thread".into());
        let name = match title.map(str::trim).filter(|t| !t.is_empty()) {
            Some(t) => t.to_string(),
            None if run.is_some() => format!("{source_title} (fork @ run {ordinal})"),
            None => format!("{source_title} (fork)"),
        };
        let transfer_id = TransferId::generate();
        let request = CreateThread {
            scope: ctx.scope.clone(),
            title: Some(name.clone()),
            provider: provider.clone(),
            model: model.clone(),
            idempotency_key: format!("t3o:{transfer_id}"),
        };
        // A thread that switched runtime has no single native session to branch.
        let native = runtime.capabilities().native_fork
            && latest
            && runtime_id == source.runtime_id
            && self.store.segments(&source.thread_id).await?.is_empty();
        let (created, strategy, handoff_text) = if native {
            (
                runtime.fork_native(source, request).await?,
                TransferStrategy::NativeFork,
                None,
            )
        } else {
            let route = format!(
                "{} / {}",
                source.provider.as_deref().unwrap_or("default provider"),
                source.model.as_deref().unwrap_or("default model")
            );
            let mut text = handoff::portable_context(
                &messages[..cut],
                &source_title,
                source.thread_id.as_str(),
                &route,
                "fork",
                self.store.settings().await?.handoff_bytes,
            );
            // A fork of a fork: its history no longer holds the context it was
            // seeded with, so carry that along ahead of its own conversation.
            if let Some(seed) = self
                .store
                .fork_origin(&source.thread_id)
                .await?
                .and_then(|o| o.handoff)
            {
                text = format!("{seed}\n\n{text}");
            }
            (
                runtime.create_thread(request).await?,
                TransferStrategy::PortableContext,
                Some(text),
            )
        };
        let mut fork = ThreadRecord::new(created.thread_id.clone(), ctx.scope.clone(), runtime_id);
        fork.profile_id = source.profile_id.clone();
        fork.provider = created.provider.or(provider);
        fork.model = created.model.or(model);
        fork.title = Some(name.clone());
        fork.native_session_id = created.native_session_id;
        self.store.ensure_thread(&fork).await?;
        let pending = handoff_text.is_some();
        // A portable fork rides its context on the first message sent in it,
        // so it runs nothing until then and merge-back has no run to skip.
        let point = json!({
            "run": ordinal, "messages": cut,
            "runStatus": all_runs.get(ordinal.saturating_sub(1)).map(|r| r.status),
            "copiedVisible": if native { handoff::visible_count(&messages[..cut]) } else { 0 },
            "handoffRuns": 0,
        });
        let now = now_ms();
        self.store
            .insert_transfer(&TransferRecord {
                id: transfer_id.clone(),
                kind: TransferKind::Fork,
                source_thread: source.thread_id.clone(),
                target_thread: Some(fork.thread_id.clone()),
                source_point: Some(point.clone()),
                base_point: None,
                strategy,
                status: if pending {
                    TransferStatus::Pending
                } else {
                    TransferStatus::Consumed
                },
                // The fork cannot adopt a transcript: the handoff waits for the
                // person's first message there instead of starting a turn.
                delivery: TransferDelivery::NextMessage,
                handoff: handoff_text,
                created_by,
                created_at: now,
                consumed_at: (!pending).then_some(now),
            })
            .await?;
        Ok(json!({
            "ok": true, "forkThreadId": fork.thread_id.as_str(), "title": name,
            "strategy": strategy.as_str(), "sourcePoint": point,
            "provider": fork.provider, "model": fork.model, "transferId": transfer_id.as_str(),
        }))
    }

    pub async fn merge_back(
        &self,
        ctx: &CallContext,
        fork: &ThreadRecord,
        target: Option<&ThreadId>,
        run: Option<usize>,
        delivery: Option<TransferDelivery>,
        created_by: Author,
    ) -> Result<Value> {
        let origin = self.store.fork_origin(&fork.thread_id).await?;
        let target_id = match (target, &origin) {
            (Some(t), _) => t.clone(),
            (None, Some(o)) => o.source_thread.clone(),
            (None, None) => {
                return Err(OrchestratorError::invalid(
                    "This thread is not a fork; pass target_thread explicitly.",
                ));
            }
        };
        let target_thread = self.owned_thread(&target_id, &ctx.scope).await?;
        let messages = self.history(fork).await?;
        let cut = handoff::cut_for_run(&messages, run).map_err(OrchestratorError::invalid)?;
        let base = match &origin {
            Some(o) if o.source_thread == target_id => {
                let point = o.source_point.clone().unwrap_or(Value::Null);
                let copied = point["copiedVisible"].as_u64().unwrap_or(0) as usize;
                let handoff_runs = point["handoffRuns"].as_u64().unwrap_or(0) as usize;
                let after_copy = handoff::index_after_visible(&messages, copied);
                let rs = handoff::runs(&messages);
                match handoff_runs {
                    0 => after_copy,
                    n => rs
                        .get(n - 1)
                        .map_or(after_copy, |r| r.end_index.max(after_copy)),
                }
            }
            _ => 0,
        }
        .min(cut);
        let fork_title = fork.title.clone().unwrap_or_else(|| "fork".into());
        let text = handoff::delta_context(
            &messages[..cut],
            &fork_title,
            fork.thread_id.as_str(),
            base,
            self.store.settings().await?.handoff_bytes,
        );
        // Runtimes here cannot inject hidden context, so `auto` means a context turn.
        let delivery = delivery.unwrap_or(TransferDelivery::Now);
        let transfer_id = TransferId::generate();
        self.store
            .insert_transfer(&TransferRecord {
                id: transfer_id.clone(),
                kind: TransferKind::MergeBack,
                source_thread: fork.thread_id.clone(),
                target_thread: Some(target_thread.thread_id.clone()),
                source_point: Some(json!({"run": run, "messages": cut})),
                base_point: Some(json!({"messages": base})),
                strategy: TransferStrategy::DeltaContext,
                status: TransferStatus::Pending,
                delivery,
                handoff: Some(text.clone()),
                created_by,
                created_at: now_ms(),
                consumed_at: None,
            })
            .await?;
        Ok(json!({
            "ok": true, "transferId": transfer_id.as_str(), "targetThreadId": target_id.as_str(),
            "delivery": delivery.as_str(), "deltaMessages": cut.saturating_sub(base),
            "handoffBytes": text.len(),
            "note": match delivery {
                TransferDelivery::NextMessage => "Delivered with the next message sent in the target thread.",
                TransferDelivery::Now => "Delivered as a context turn as soon as the target thread is idle.",
            },
        }))
    }

    pub async fn lineage(&self, thread: &ThreadId) -> Result<Value> {
        let rows = self.store.lineage_rows(thread).await?;
        let pick = |kind: TransferKind, outgoing: bool| -> Vec<&TransferRecord> {
            rows.iter()
                .filter(|x| x.kind == kind)
                .filter(|x| {
                    if outgoing {
                        &x.source_thread == thread
                    } else {
                        x.target_thread.as_ref() == Some(thread)
                    }
                })
                .collect()
        };
        Ok(json!({
            "threadId": thread.as_str(),
            "forkedFrom": pick(TransferKind::Fork, false).last(),
            "forks": pick(TransferKind::Fork, true),
            "mergesIn": pick(TransferKind::MergeBack, false),
            "mergesOut": pick(TransferKind::MergeBack, true),
        }))
    }
}

fn steer_label(outcome: &SteerOutcome) -> &'static str {
    match outcome {
        SteerOutcome::Steered => "steered",
        SteerOutcome::Restarted => "restarted",
        SteerOutcome::Rejected => "rejected",
    }
}
