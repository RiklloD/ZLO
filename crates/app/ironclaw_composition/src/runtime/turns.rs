//! `RebornRuntime` conversation and turn submission: `new_conversation`,
//! `send_user_message*`, skill-message execution, and per-thread send locks.

use super::*;

impl RebornRuntime {
    /// Create a fresh conversation. Returns the opaque conversation id used
    /// in subsequent `send_user_message` calls.
    ///
    /// The thread is materialized inside the session thread service so
    /// `accept_inbound_message` does not error on the first send.
    pub async fn new_conversation(&self) -> Result<ConversationId, RebornRuntimeError> {
        let thread_id =
            ThreadId::new(format!("reborn-conv-{}", Uuid::new_v4())).map_err(|reason| {
                RebornRuntimeError::InvalidArgument {
                    reason: reason.to_string(),
                }
            })?;
        self.thread_service
            .ensure_thread(EnsureThreadRequest {
                scope: self.thread_scope.clone(),
                thread_id: Some(thread_id.clone()),
                created_by_actor_id: self.actor_user_id.as_str().to_string(),
                title: None,
                metadata_json: None,
            })
            .await
            .map_err(|error| RebornRuntimeError::ThreadService(error.to_string()))?;
        Ok(ConversationId(thread_id))
    }

    /// Submit a user message into the conversation, wait for the run to
    /// reach a terminal state, and return the assistant reply read back
    /// from the session thread service.
    ///
    /// Without an LLM provider configured, the run will fail and the
    /// returned reply will surface that failure via `status = Failed`
    /// and `text = None`.
    ///
    /// **CLI origin contract**: this task-level send path resolves
    /// the turn's product-context source channel as CLI chat (`resolve_cli`).
    /// A non-WebUI ingress (e.g. a future channel adapter) must not reuse
    /// this method for its submissions; it must resolve its own origin at
    /// that ingress instead.
    pub async fn send_user_message(
        &self,
        conversation: &ConversationId,
        text: &str,
    ) -> Result<AssistantReply, RebornRuntimeError> {
        self.send_user_message_with_cancellation(conversation, text, CancellationToken::new())
            .await
    }

    /// Submit a user message with a cooperative cancellation token. If the
    /// token fires while waiting for completion, the runtime cancels the run
    /// before returning.
    pub async fn send_user_message_with_cancellation(
        &self,
        conversation: &ConversationId,
        text: &str,
        cancellation: CancellationToken,
    ) -> Result<AssistantReply, RebornRuntimeError> {
        self.send_user_message_internal(conversation, text, cancellation, false)
            .await
    }

    async fn send_user_message_internal(
        &self,
        conversation: &ConversationId,
        text: &str,
        cancellation: CancellationToken,
        capture_skill_execution_plan: bool,
    ) -> Result<AssistantReply, RebornRuntimeError> {
        let total_started_at = live_latency_started_at();
        let submit_started_at = total_started_at;
        let submitted = match self
            .submit_user_turn(
                conversation,
                text,
                &cancellation,
                capture_skill_execution_plan,
            )
            .await
        {
            Ok(submitted) => {
                trace_runtime_latency_ok(
                    "submit_user_turn",
                    &conversation.0,
                    Some(submitted.run_id),
                    submit_started_at,
                );
                submitted
            }
            Err(error) => {
                trace_runtime_latency_error(
                    "submit_user_turn",
                    &conversation.0,
                    None,
                    submit_started_at,
                    &error,
                );
                trace_runtime_latency_error(
                    "send_user_message",
                    &conversation.0,
                    None,
                    total_started_at,
                    &error,
                );
                return Err(error);
            }
        };

        let wait_started_at = live_latency_started_at();
        let reply = async {
            let terminal_state = self
                .wait_for_terminal(&submitted.scope, submitted.run_id, &cancellation)
                .await?;
            let assistant_text = self
                .read_latest_assistant_text(&conversation.0, submitted.run_id)
                .await?;

            Ok(AssistantReply {
                conversation: conversation.clone(),
                run_id: submitted.run_id,
                status: terminal_state.status,
                failure_category: terminal_state
                    .failure
                    .as_ref()
                    .map(|failure| failure.category().to_string()),
                text: assistant_text,
            })
        }
        .await;
        match &reply {
            Ok(_) => trace_runtime_latency_ok(
                "wait_for_terminal_and_read_reply",
                &conversation.0,
                Some(submitted.run_id),
                wait_started_at,
            ),
            Err(error) => trace_runtime_latency_error(
                "wait_for_terminal_and_read_reply",
                &conversation.0,
                Some(submitted.run_id),
                wait_started_at,
                error,
            ),
        }

        if let Some(skill_activation_source) = &self.skill_activation_source
            && let Err(clear_error) = skill_activation_source
                .clear_accepted_message(&submitted.scope, &submitted.accepted_message_ref)
        {
            if reply.is_ok() {
                // Primary turn succeeded, so the cleanup failure is the only
                // error to surface.
                trace_runtime_latency_error(
                    "send_user_message",
                    &conversation.0,
                    Some(submitted.run_id),
                    total_started_at,
                    &clear_error,
                );
                return Err(RebornRuntimeError::TurnSubmission(clear_error.to_string()));
            }
            // Primary turn already failed: don't mask it with the cleanup
            // error — log the secondary (sanitized id only) and return the
            // primary. See error-handling.md.
            tracing::debug!(
                accepted_message_ref = submitted.accepted_message_ref.as_str(),
                "failed to clear accepted message after primary turn failure"
            );
        }

        match &reply {
            Ok(_) => trace_runtime_latency_ok(
                "send_user_message",
                &conversation.0,
                Some(submitted.run_id),
                total_started_at,
            ),
            Err(error) => trace_runtime_latency_error(
                "send_user_message",
                &conversation.0,
                Some(submitted.run_id),
                total_started_at,
                error,
            ),
        }
        reply
    }

    /// Submit a user message turn and return once the run is accepted, holding
    /// the per-conversation send lock for the returned `SubmittedTurn`'s
    /// lifetime. Shared by [`Self::send_user_message_internal`] and the
    /// test-support [`Self::send_user_message_until_gate`] so both drive an
    /// identical accept/submit path and differ only in how they wait for the
    /// run to settle.
    pub(super) async fn submit_user_turn(
        &self,
        conversation: &ConversationId,
        text: &str,
        cancellation: &CancellationToken,
        capture_skill_execution_plan: bool,
    ) -> Result<SubmittedTurn, RebornRuntimeError> {
        let send_lock = self.send_lock_for(conversation).await;
        let send_lock_started_at = live_latency_started_at();
        let _send_guard = send_lock.lock_owned().await;
        trace_runtime_latency_ok(
            "send_lock_wait",
            &conversation.0,
            None,
            send_lock_started_at,
        );
        // Stopped only when every worker has exited; a single crashed worker must not
        // reject submissions while others run.
        if self.turn_scheduler.is_stopped() {
            let error = RebornRuntimeError::WorkerStopped;
            trace_runtime_latency_error(
                "submit_user_turn_preflight",
                &conversation.0,
                None,
                send_lock_started_at,
                &error,
            );
            return Err(error);
        }
        let scope = self.turn_scope_for(&conversation.0);
        let resolved_model = match self.llm_config_service.as_ref() {
            Some(service) => service
                .resolve_user_model(
                    ProductSurfaceCaller::new(
                        self.thread_scope.tenant_id.clone(),
                        self.actor_user_id.clone(),
                        Some(self.thread_scope.agent_id.clone()),
                        self.thread_scope.project_id.clone(),
                    ),
                    None,
                )
                .await
                .map_err(cli_model_resolution_error)?,
            None => None,
        };
        let accept_started_at = live_latency_started_at();
        let accept_request = AcceptInboundMessageRequest {
            scope: self.thread_scope.clone(),
            thread_id: conversation.0.clone(),
            actor_id: self.actor_user_id.as_str().to_string(),
            source_binding_id: Some(self.source_binding_ref.as_str().to_string()),
            reply_target_binding_id: Some(self.reply_target_binding_ref.as_str().to_string()),
            // This task-level API does not receive an upstream stable
            // event id, so mint a best-effort unique id scoped to the
            // caller-provided source binding.
            external_event_id: Some(format!(
                "{}:{}",
                self.source_binding_ref.as_str(),
                Uuid::new_v4()
            )),
            content: MessageContent::text(text.to_string()),
        };
        let accepted = match self
            .thread_service
            .accept_inbound_message_with_replay_metadata(
                accept_request,
                InboundMessageReplayMetadata { resolved_model },
            )
            .await
        {
            Ok(accepted) => {
                trace_runtime_latency_ok(
                    "accept_inbound_message",
                    &conversation.0,
                    None,
                    accept_started_at,
                );
                accepted
            }
            Err(error) => {
                trace_runtime_latency_error(
                    "accept_inbound_message",
                    &conversation.0,
                    None,
                    accept_started_at,
                    &error,
                );
                return Err(RebornRuntimeError::ThreadService(error.to_string()));
            }
        };

        let accepted_message_ref = AcceptedMessageRef::new(format!("msg:{}", accepted.message_id))
            .map_err(|reason| RebornRuntimeError::InvalidArgument { reason })?;
        self.tool_mentions
            .record(&scope, &accepted_message_ref, text);
        let idempotency_key = IdempotencyKey::new(format!(
            "{}-{}",
            self.source_binding_ref.as_str(),
            Uuid::new_v4()
        ))
        .map_err(|reason| RebornRuntimeError::InvalidArgument { reason })?;

        if capture_skill_execution_plan {
            let adapter = self
                .skill_execution_adapter
                .as_ref()
                .ok_or(RebornRuntimeError::SkillExecutionUnavailable)?;
            let skill_record_started_at = live_latency_started_at();
            if let Err(error) = adapter.record_user_message_for_execution(
                scope.clone(),
                accepted_message_ref.clone(),
                text,
            ) {
                trace_runtime_latency_error(
                    "record_skill_execution_message",
                    &conversation.0,
                    None,
                    skill_record_started_at,
                    &error,
                );
                return Err(RebornRuntimeError::TurnSubmission(error.to_string()));
            }
            trace_runtime_latency_ok(
                "record_skill_execution_message",
                &conversation.0,
                None,
                skill_record_started_at,
            );
        } else if let Some(skill_activation_source) = &self.skill_activation_source {
            let skill_record_started_at = live_latency_started_at();
            if let Err(error) = skill_activation_source.record_user_message(
                scope.clone(),
                accepted_message_ref.clone(),
                text,
            ) {
                trace_runtime_latency_error(
                    "record_skill_activation_message",
                    &conversation.0,
                    None,
                    skill_record_started_at,
                    &error,
                );
                return Err(RebornRuntimeError::TurnSubmission(error.to_string()));
            }
            trace_runtime_latency_ok(
                "record_skill_activation_message",
                &conversation.0,
                None,
                skill_record_started_at,
            );
        }

        let turn_submit_started_at = live_latency_started_at();
        let response = match self
            .turn_coordinator
            .submit_turn(SubmitTurnRequest {
                subagent_activation_provenance: None,
                requested_model: accepted.replay_metadata.resolved_model.clone(),
                scope: scope.clone(),
                actor: TurnActor::new(self.actor_user_id.clone()),
                accepted_message_ref: accepted_message_ref.clone(),
                requested_run_profile: None,
                output_contract: None,
                idempotency_key,
                received_at: Utc::now(),
                requested_run_id: None,
                parent_run_id: None,
                subagent_depth: 0,
                spawn_tree_root_run_id: None,
                product_context: Some(ironclaw_turns::product_context::resolve_cli(
                    scope.product_owner(&TurnActor::new(self.actor_user_id.clone())),
                )),
            })
            .await
        {
            Ok(response) => {
                let SubmitTurnResponse::Accepted { run_id, .. } = &response;
                trace_runtime_latency_ok(
                    "turn_coordinator_submit_turn",
                    &conversation.0,
                    Some(*run_id),
                    turn_submit_started_at,
                );
                response
            }
            Err(error) => {
                trace_runtime_latency_error(
                    "turn_coordinator_submit_turn",
                    &conversation.0,
                    None,
                    turn_submit_started_at,
                    &error,
                );
                if let Some(skill_activation_source) = &self.skill_activation_source {
                    skill_activation_source
                        .clear_accepted_message(&scope, &accepted_message_ref)
                        .map_err(|clear_error| {
                            RebornRuntimeError::TurnSubmission(clear_error.to_string())
                        })?;
                }
                return Err(error.into());
            }
        };

        let SubmitTurnResponse::Accepted {
            run_id,
            status: submit_status,
            event_cursor: submit_cursor,
            ..
        } = response;
        if cancellation.is_cancelled() {
            if let Some(skill_activation_source) = &self.skill_activation_source {
                skill_activation_source
                    .clear_accepted_message(&scope, &accepted_message_ref)
                    .map_err(|error| RebornRuntimeError::TurnSubmission(error.to_string()))?;
            }
            self.cancel_run(
                &scope,
                run_id,
                SanitizedCancelReason::UserRequested,
                "caller-cancel",
            )
            .await?;
            return Err(RebornRuntimeError::OperationCancelled);
        }
        let notify_started_at = live_latency_started_at();
        self.turn_scheduler.notify(TurnRunWake {
            scope: scope.clone(),
            run_id,
            status: submit_status,
            event_cursor: submit_cursor,
        });
        trace_runtime_latency_ok(
            "turn_scheduler_notify",
            &conversation.0,
            Some(run_id),
            notify_started_at,
        );

        Ok(SubmittedTurn {
            _send_guard,
            scope,
            run_id,
            accepted_message_ref,
        })
    }

    /// Submit a skill-aware message through the normal Reborn loop and return
    /// the structured activation plan produced during prompt construction.
    pub async fn execute_skill_message(
        &self,
        conversation: &ConversationId,
        text: &str,
    ) -> Result<RebornSkillExecutionResult, RebornRuntimeError> {
        let adapter = self
            .skill_execution_adapter
            .as_ref()
            .ok_or(RebornRuntimeError::SkillExecutionUnavailable)?;
        let scope = self.turn_scope_for(&conversation.0);
        let reply = self
            .send_user_message_internal(conversation, text, CancellationToken::new(), true)
            .await?;
        let plan = self.skill_execution_plan_for_run(adapter, &scope, reply.run_id)?;
        Ok(RebornSkillExecutionResult { plan, reply })
    }

    /// Read a bundle-relative asset from a skill activated by
    /// [`Self::execute_skill_message`].
    pub async fn read_skill_execution_asset(
        &self,
        conversation: &ConversationId,
        plan: &RebornSkillExecutionPlan,
        activation: &RebornSkillActivation,
        path: impl AsRef<str>,
    ) -> Result<RebornSkillAsset, RebornRuntimeError> {
        if plan.run_context().thread_id != conversation.0 {
            return Err(RebornRuntimeError::SkillExecution(
                "skill execution plan does not belong to this conversation".to_string(),
            ));
        }
        let adapter = self
            .skill_execution_adapter
            .as_ref()
            .ok_or(RebornRuntimeError::SkillExecutionUnavailable)?;
        adapter
            .read_file_for_activation(
                plan.run_context(),
                plan.first_party_plan(),
                &activation.to_first_party_request(),
                path,
            )
            .await
            .map(RebornSkillAsset::from)
            .map_err(skill_asset_error)
    }

    pub(super) fn turn_scope_for(&self, thread_id: &ThreadId) -> TurnScope {
        // RebornRuntime is bound to a single actor user, so its turns are
        // owned by that user (not the shared agent).  Passing the explicit
        // owner here makes `TurnScope::product_owner` resolve to
        // `TurnOwner::Personal` instead of `TurnOwner::SharedAgent`.
        TurnScope::new_with_owner(
            self.thread_scope.tenant_id.clone(),
            Some(self.thread_scope.agent_id.clone()),
            self.thread_scope.project_id.clone(),
            thread_id.clone(),
            Some(self.actor_user_id.clone()),
        )
    }

    fn skill_execution_plan_for_run(
        &self,
        adapter: &SkillExecutionAdapter<FilesystemSkillBundleSource<CompositeRootFilesystem>>,
        scope: &TurnScope,
        run_id: TurnRunId,
    ) -> Result<RebornSkillExecutionPlan, RebornRuntimeError> {
        adapter
            .take_execution_plan_for_run(scope, run_id)
            .map_err(|error| RebornRuntimeError::SkillExecution(error.to_string()))?
            .map(RebornSkillExecutionPlan::from_first_party)
            .ok_or_else(|| {
                RebornRuntimeError::SkillExecution("skill activation plan unavailable".to_string())
            })
    }

    async fn send_lock_for(&self, conversation: &ConversationId) -> Arc<Mutex<()>> {
        let mut locks = self.send_locks.lock().await;
        Arc::clone(
            locks
                .entry(conversation.clone())
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }
}
