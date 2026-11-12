//! `RebornRuntime` turn settlement: waiting for a terminal state, run
//! cancellation (including descendant runs), and reading the reply. The
//! test-gated gate-aware variants live in `harness_accessors`.

use super::*;

const MAX_DESCENDANT_CANCEL_NODES: usize = 1_000;

const LOOP_RUN_CAPABILITY_ID: &str = "loop.run";

impl RebornRuntime {
    pub(super) async fn wait_for_terminal(
        &self,
        scope: &TurnScope,
        run_id: TurnRunId,
        cancellation: &CancellationToken,
    ) -> Result<TurnRunState, RebornRuntimeError> {
        let start = std::time::Instant::now();
        loop {
            if self.turn_scheduler.is_stopped() {
                return Err(RebornRuntimeError::WorkerStopped);
            }
            let state = self
                .turn_coordinator
                .get_run_state(GetRunStateRequest {
                    scope: scope.clone(),
                    run_id,
                })
                .await?;
            if state.status.is_terminal() {
                return Ok(state);
            }
            // TurnStatus::RecoveryRequired is now terminal (is_terminal() returns true)
            // so the branch above handles it; no special cancel-to-release-lock is needed.
            if start.elapsed() > self.poll_settings.max_total {
                if let Err(error) = self
                    .cancel_run(
                        scope,
                        run_id,
                        SanitizedCancelReason::Timeout,
                        "timeout-cancel",
                    )
                    .await
                {
                    tracing::debug!(
                        ?error,
                        %run_id,
                        "failed to cancel timed-out run while preserving timeout error"
                    );
                }
                return Err(RebornRuntimeError::RunTimeout {
                    timeout: self.poll_settings.max_total,
                });
            }
            tokio::select! {
                _ = cancellation.cancelled() => {
                    if let Err(error) = self
                        .cancel_run(
                            scope,
                            run_id,
                            SanitizedCancelReason::UserRequested,
                            "caller-cancel",
                        )
                        .await
                    {
                        tracing::debug!(
                            ?error,
                            %run_id,
                            "failed to cancel caller-cancelled run while preserving cancellation error"
                        );
                    }
                    return Err(RebornRuntimeError::OperationCancelled);
                }
                _ = tokio::time::sleep(self.poll_settings.interval) => {}
            }
        }
    }

    pub(super) async fn cancel_run(
        &self,
        scope: &TurnScope,
        run_id: TurnRunId,
        reason: SanitizedCancelReason,
        idempotency_suffix: &str,
    ) -> Result<CancelRunResponse, RebornRuntimeError> {
        let response = self
            .turn_coordinator
            .cancel_run(CancelRunRequest {
                scope: scope.clone(),
                actor: TurnActor::new(self.actor_user_id.clone()),
                run_id,
                reason,
                idempotency_key: IdempotencyKey::new(format!(
                    "{}-{}-{}",
                    self.source_binding_ref.as_str(),
                    idempotency_suffix,
                    run_id
                ))
                .map_err(|reason| RebornRuntimeError::InvalidArgument { reason })?,
            })
            .await?;
        let cancellation_accepted = matches!(
            response.status,
            TurnStatus::CancelRequested | TurnStatus::Cancelled
        );
        if cancellation_accepted {
            self.append_webui_loop_cancelled(scope, run_id);
        }
        self.turn_scheduler.notify(TurnRunWake {
            scope: scope.clone(),
            run_id: response.run_id,
            status: response.status,
            event_cursor: response.event_cursor,
        });
        if cancellation_accepted {
            self.cancel_descendant_runs(scope, run_id, reason, idempotency_suffix)
                .await?;
        }
        Ok(response)
    }

    async fn cancel_descendant_runs(
        &self,
        scope: &TurnScope,
        run_id: TurnRunId,
        reason: SanitizedCancelReason,
        idempotency_suffix: &str,
    ) -> Result<(), RebornRuntimeError> {
        let mut stack = self.turn_tree_store.children_of(scope, run_id).await?;
        let mut visited = HashSet::new();
        let mut visited_count = 0_usize;
        while let Some(child) = stack.pop() {
            if !visited.insert(child.run_id) {
                continue;
            }
            visited_count += 1;
            if visited_count > MAX_DESCENDANT_CANCEL_NODES {
                tracing::warn!(
                    scope = ?scope,
                    run_id = %run_id,
                    max_nodes = MAX_DESCENDANT_CANCEL_NODES,
                    "stopped descendant cancellation traversal after node budget was reached"
                );
                break;
            }
            if child.status.is_terminal() {
                continue;
            }
            let grandchildren = self
                .turn_tree_store
                .children_of(&child.scope, child.run_id)
                .await?;
            stack.extend(grandchildren);
            let idempotency_key = IdempotencyKey::new(format!(
                "{}-{}-descendant-{}",
                self.source_binding_ref.as_str(),
                idempotency_suffix,
                child.run_id
            ))
            .map_err(|reason| RebornRuntimeError::InvalidArgument { reason })?;
            let child_scope = child.scope.clone();
            let child_run_id = child.run_id;
            let response = self
                .turn_coordinator
                .cancel_run(CancelRunRequest {
                    scope: child_scope.clone(),
                    actor: TurnActor::new(self.actor_user_id.clone()),
                    run_id: child_run_id,
                    reason,
                    idempotency_key,
                })
                .await;
            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    let state = self
                        .turn_coordinator
                        .get_run_state(GetRunStateRequest {
                            scope: child_scope.clone(),
                            run_id: child_run_id,
                        })
                        .await?;
                    if matches!(
                        state.status,
                        TurnStatus::CancelRequested | TurnStatus::Cancelled
                    ) {
                        self.turn_scheduler.notify(TurnRunWake {
                            scope: child_scope,
                            run_id: child_run_id,
                            status: state.status,
                            event_cursor: EventCursor(0),
                        });
                        continue;
                    }
                    return Err(error.into());
                }
            };
            if matches!(
                response.status,
                TurnStatus::CancelRequested | TurnStatus::Cancelled
            ) {
                self.append_webui_loop_cancelled(&child.scope, child_run_id);
            }
            self.turn_scheduler.notify(TurnRunWake {
                scope: child_scope,
                run_id: response.run_id,
                status: response.status,
                event_cursor: response.event_cursor,
            });
        }
        Ok(())
    }

    fn append_webui_loop_cancelled(&self, scope: &TurnScope, run_id: TurnRunId) {
        let capability_id = match CapabilityId::new(LOOP_RUN_CAPABILITY_ID) {
            Ok(capability_id) => capability_id,
            Err(error) => {
                tracing::debug!(error = %error, "loop cancellation runtime event was not built");
                return;
            }
        };
        if let Err(error) = self
            .runtime_event_sink
            .try_emit(RuntimeEvent::loop_cancelled(
                ResourceScope {
                    tenant_id: scope.tenant_id.clone(),
                    user_id: self.actor_user_id.clone(),
                    agent_id: scope.agent_id.clone(),
                    project_id: scope.project_id.clone(),
                    mission_id: None,
                    thread_id: Some(scope.thread_id.clone()),
                    invocation_id: InvocationId::from_uuid(run_id.as_uuid()),
                },
                capability_id,
            ))
        {
            tracing::debug!(error = %error, "loop cancellation runtime event was not emitted");
        }
    }

    pub(super) async fn read_latest_assistant_text(
        &self,
        thread_id: &ThreadId,
        run_id: TurnRunId,
    ) -> Result<Option<String>, RebornRuntimeError> {
        let history = self
            .thread_service
            .list_thread_history(ThreadHistoryRequest {
                scope: self.thread_scope.clone(),
                thread_id: thread_id.clone(),
            })
            .await
            .map_err(|error| RebornRuntimeError::ThreadService(error.to_string()))?;
        let run_id_str = run_id.to_string();
        let reply = history
            .messages
            .into_iter()
            .rev()
            .find(|message| {
                matches!(message.kind, MessageKind::Assistant)
                    && matches!(message.status, MessageStatus::Finalized)
                    && message.turn_run_id.as_deref() == Some(run_id_str.as_str())
            })
            .and_then(|message| message.content);
        Ok(reply)
    }
}
