use ironclaw_loop_contracts::{
    AgentLoopDriverHost, LoopDriverNoteKind, LoopExit, LoopProgressEvent, ParentLoopOutput,
};
use tracing::debug;

use crate::{family::LoopFamily, state::LoopExecutionState, strategies::TurnEndKind};

use super::streamed_reads::SpeculativeReads;

use super::{
    AgentLoopExecutorError, AssistantReplyInput, BudgetInput, BudgetStep, CancelCheck,
    CapabilityInput, CheckpointStage, DefaultExecutorPipeline, DrainInput, ExecutorStage,
    ExitInput, InputStep, ModelInput, ModelStep, PromptInput, PromptStep, ReplyAdmissionInput,
    ReplyAdmissionStep, StageContext, StopInput, StopObservationInput, StopObservationStep,
    StopStep, TurnCompletedStep, UserFacingInputDrainMode, latency,
};

impl DefaultExecutorPipeline {
    pub(super) async fn execute(
        &self,
        family: &LoopFamily,
        host: &(dyn AgentLoopDriverHost + Send + Sync),
        mut state: LoopExecutionState,
    ) -> Result<LoopExit, AgentLoopExecutorError> {
        let planner = family.planner();
        let ctx = StageContext { planner, host };

        loop {
            state = match latency::stage!(
                "cancel_check",
                host.run_context(),
                state.iteration,
                CheckpointStage.cancel_if_requested(ctx, state),
            )? {
                CancelCheck::Continue(state) => *state,
                CancelCheck::Exit(exit) => return Ok(exit),
            };

            match latency::stage!(
                "budget",
                host.run_context(),
                state.iteration,
                self.budget.process(ctx, BudgetInput { state },),
            )? {
                BudgetStep::Continue { state: next } => {
                    state = *next;
                }
                BudgetStep::Exit(exit) => return Ok(exit),
            }

            let progress_started_at = latency::started_at();
            CheckpointStage
                .emit_progress(
                    ctx,
                    LoopProgressEvent::IterationStarted {
                        iteration: state.iteration,
                    },
                )
                .await;
            latency::operation_ok(
                "emit_iteration_started",
                host.run_context(),
                state.iteration,
                progress_started_at,
            );

            match latency::stage!(
                "input_drain_steering",
                host.run_context(),
                state.iteration,
                self.input.process(
                    ctx,
                    DrainInput {
                        state,
                        mode: UserFacingInputDrainMode::Steering,
                    },
                ),
            )? {
                InputStep::Continue { state: next, .. } => {
                    state = *next;
                }
                InputStep::Exit(exit) => return Ok(exit),
            }

            match latency::stage!(
                "prompt",
                host.run_context(),
                state.iteration,
                self.prompt.process(ctx, PromptInput { state },),
            )? {
                PromptStep::Exit(exit) => return Ok(exit),

                PromptStep::Prepared(prompt) => {
                    let prompt = *prompt;
                    state = prompt.state;

                    state = latency::stage!(
                        "checkpoint_before_model",
                        host.run_context(),
                        state.iteration,
                        CheckpointStage.write_before_model_batched(ctx, state),
                    )?;
                    if prompt.rendered_repeated_call_warning {
                        state.stop_state.mark_repeated_call_warning_rendered();
                        let note_started_at = latency::started_at();
                        CheckpointStage
                            .emit_progress(
                                ctx,
                                LoopProgressEvent::driver_note(
                                    LoopDriverNoteKind::Planning,
                                    "repeated capability call warning rendered",
                                )
                                .map_err(|_| AgentLoopExecutorError::PlannerContract {
                                    detail: "repeated-call warning progress summary was invalid",
                                })?,
                            )
                            .await;
                        latency::operation_ok(
                            "emit_repeated_call_warning",
                            host.run_context(),
                            state.iteration,
                            note_started_at,
                        );
                    }
                    let (model_response, speculative_reads) = match latency::stage!(
                        "model",
                        host.run_context(),
                        state.iteration,
                        self.model.process(
                            ctx,
                            ModelInput {
                                state,
                                messages: prompt.messages,
                                inline_messages: prompt.inline_messages,
                                surface_version: prompt.surface.version.clone(),
                                capability_view: prompt.capability_view,
                            },
                        ),
                    )? {
                        ModelStep::Response(next, response, speculative_reads) => {
                            state = *next;
                            // The model call carried everything drained so
                            // far: the agent has read it.
                            if !state.unread_input_acks.is_empty() {
                                let read = std::mem::take(&mut state.unread_input_acks);
                                host.confirm_inputs_read(read).await.map_err(|error| {
                                    debug!(%error, "confirming drained inputs read failed");
                                    AgentLoopExecutorError::HostUnavailable {
                                        stage: super::HostStage::Input,
                                    }
                                })?;
                            }
                            (response, speculative_reads)
                        }
                        ModelStep::RetryIteration(next) => {
                            state = *next;
                            continue;
                        }
                        ModelStep::Exit(exit) => return Ok(exit),
                    };

                    let turn_iteration = state.iteration;
                    let completed = match model_response.output {
                        ParentLoopOutput::AssistantReply(reply) => {
                            match latency::stage!(
                                "reply_admission",
                                host.run_context(),
                                turn_iteration,
                                self.reply_admission
                                    .process(ctx, ReplyAdmissionInput { state, reply }),
                            )? {
                                ReplyAdmissionStep::Accept { state, reply } => latency::stage!(
                                    "assistant_reply",
                                    host.run_context(),
                                    turn_iteration,
                                    self.assistant_reply.process(
                                        ctx,
                                        AssistantReplyInput {
                                            state: *state,
                                            reply,
                                        },
                                    ),
                                )?,
                                ReplyAdmissionStep::Reject { state } => {
                                    TurnCompletedStep::Continue {
                                        state,
                                        summary: crate::strategies::TurnSummary::reply_rejected(),
                                    }
                                }
                            }
                        }
                        ParentLoopOutput::CapabilityCalls(calls) => latency::stage!(
                            "capabilities",
                            host.run_context(),
                            turn_iteration,
                            self.capabilities.process(
                                ctx,
                                CapabilityInput {
                                    state,
                                    surface: prompt.surface,
                                    calls,
                                    speculative_reads,
                                },
                            ),
                        )?,
                    };

                    let completed = latency::stage!(
                        "post_capability",
                        host.run_context(),
                        completed.iteration_or(turn_iteration),
                        self.post_capability.process(ctx, completed),
                    )?;

                    let (next_state, summary) = match completed {
                        TurnCompletedStep::Continue { state, summary } => (*state, summary),
                        TurnCompletedStep::Exit(exit) => return Ok(exit),
                    };
                    let completed_kind = summary.kind;

                    let (mut next_state, summary) = match latency::stage!(
                        "stop_observe",
                        host.run_context(),
                        next_state.iteration,
                        self.stop.observe(
                            ctx,
                            StopObservationInput {
                                state: next_state,
                                summary,
                            },
                        ),
                    )? {
                        StopObservationStep::Continue { state, summary } => (*state, summary),
                        StopObservationStep::Exit(exit) => return Ok(exit),
                    };

                    if completed_kind == TurnEndKind::ReplyOnly {
                        debug!(
                            iteration = next_state.iteration,
                            "agent loop checking follow-up input after reply-only turn end"
                        );
                        match latency::stage!(
                            "input_drain_follow_up",
                            host.run_context(),
                            next_state.iteration,
                            self.input.process(
                                ctx,
                                DrainInput {
                                    state: next_state,
                                    mode: UserFacingInputDrainMode::FollowUp,
                                },
                            ),
                        )? {
                            InputStep::Continue {
                                state: next,
                                drained,
                            } => {
                                next_state = *next;
                                if drained {
                                    debug!(
                                        iteration = next_state.iteration,
                                        "agent loop continuing after queued follow-up input"
                                    );
                                    next_state.iteration = next_state.iteration.saturating_add(1);
                                    state = next_state;
                                    continue;
                                }
                            }
                            InputStep::Exit(exit) => return Ok(exit),
                        }
                    }

                    let stop_step = latency::stage!(
                        "stop_decide",
                        host.run_context(),
                        next_state.iteration,
                        self.stop.decide(
                            ctx,
                            StopInput {
                                state: next_state,
                                summary,
                            },
                        ),
                    )?;
                    let stop_step = self.stop.apply_fresh_completion_nudge(host, stop_step);
                    let stop_step = match self
                        .drain_before_stop(ctx, stop_step, completed_kind == TurnEndKind::ReplyOnly)
                        .await?
                    {
                        BeforeStop::Stop(step) => step,
                        BeforeStop::Continue(next) => {
                            state = next;
                            continue;
                        }
                        BeforeStop::Exit(exit) => return Ok(exit),
                    };
                    match stop_step {
                        StopStep::Stop {
                            state: stop_state,
                            kind,
                        } => {
                            let exit_iteration = stop_state.iteration;
                            let exit = latency::stage!(
                                "exit",
                                host.run_context(),
                                exit_iteration,
                                self.exit.process(
                                    ctx,
                                    ExitInput {
                                        state: stop_state,
                                        kind,
                                    },
                                ),
                            )?;
                            let _ = exit_iteration;
                            return Ok(exit);
                        }
                        StopStep::Continue { state: next } => {
                            state = next;
                        }
                        StopStep::Exit(exit) => return Ok(exit),
                    }

                    state.iteration = state.iteration.saturating_add(1);
                }

                PromptStep::ResumeApproval(resume)
                | PromptStep::ResumeAuth(resume)
                | PromptStep::ResumeExternalTool(resume) => {
                    let resume = *resume;
                    let resume_iteration = resume.state.iteration;
                    let completed = latency::stage!(
                        "capabilities_resume",
                        host.run_context(),
                        resume_iteration,
                        self.capabilities.process(
                            ctx,
                            CapabilityInput {
                                state: resume.state,
                                surface: resume.surface,
                                calls: vec![resume.call],
                                speculative_reads: SpeculativeReads::default(),
                            },
                        ),
                    )?;

                    let completed = latency::stage!(
                        "post_capability_resume",
                        host.run_context(),
                        completed.iteration_or(resume_iteration),
                        self.post_capability.process(ctx, completed),
                    )?;

                    let (next_state, summary) = match completed {
                        TurnCompletedStep::Continue { state, summary } => (*state, summary),
                        TurnCompletedStep::Exit(exit) => return Ok(exit),
                    };

                    let (next_state, summary) = match latency::stage!(
                        "stop_observe_resume",
                        host.run_context(),
                        next_state.iteration,
                        self.stop.observe(
                            ctx,
                            StopObservationInput {
                                state: next_state,
                                summary,
                            },
                        ),
                    )? {
                        StopObservationStep::Continue { state, summary } => (*state, summary),
                        StopObservationStep::Exit(exit) => return Ok(exit),
                    };

                    let stop_step = latency::stage!(
                        "stop_decide_resume",
                        host.run_context(),
                        next_state.iteration,
                        self.stop.decide(
                            ctx,
                            StopInput {
                                state: next_state,
                                summary,
                            },
                        ),
                    )?;
                    let stop_step = match self.drain_before_stop(ctx, stop_step, false).await? {
                        BeforeStop::Stop(step) => step,
                        BeforeStop::Continue(next) => {
                            state = next;
                            continue;
                        }
                        BeforeStop::Exit(exit) => return Ok(exit),
                    };
                    match stop_step {
                        StopStep::Stop {
                            state: stopped_state,
                            kind,
                        } => {
                            let exit_iteration = stopped_state.iteration;
                            let exit = latency::stage!(
                                "exit_resume",
                                host.run_context(),
                                exit_iteration,
                                self.exit.process(
                                    ctx,
                                    ExitInput {
                                        state: stopped_state,
                                        kind,
                                    },
                                ),
                            )?;
                            return Ok(exit);
                        }
                        StopStep::Continue { state: next } => {
                            state = next;
                        }
                        StopStep::Exit(exit) => return Ok(exit),
                    }

                    state.iteration = state.iteration.saturating_add(1);
                }

                PromptStep::SkipModel(skipped_state) => {
                    // Compaction-only turn: the prompt stage ran compaction and
                    // set skip_model_this_iteration. Bypass CheckpointStage
                    // (BeforeModel), ModelStage, reply/capability, and
                    // PostCapabilityStage entirely. Route the synthetic summary
                    // through stop.observe + stop.decide so noprogress detection
                    // sees the turn (without counting it as AfterCapabilityBatch
                    // evidence).
                    let skipped_state = *skipped_state;
                    let summary = crate::strategies::TurnSummary::compaction_only();

                    let (mut next_state, summary) = match latency::stage!(
                        "stop_observe_skip_model",
                        host.run_context(),
                        skipped_state.iteration,
                        self.stop.observe(
                            ctx,
                            StopObservationInput {
                                state: skipped_state,
                                summary,
                            },
                        ),
                    )? {
                        StopObservationStep::Continue { state, summary } => (*state, summary),
                        StopObservationStep::Exit(exit) => return Ok(exit),
                    };

                    // The reply-only follow-up drain does not apply here;
                    // `drain_before_stop` below reads steers before a stop.

                    let stop_step = latency::stage!(
                        "stop_decide_skip_model",
                        host.run_context(),
                        next_state.iteration,
                        self.stop.decide(
                            ctx,
                            StopInput {
                                state: next_state,
                                summary,
                            },
                        ),
                    )?;
                    let stop_step = match self.drain_before_stop(ctx, stop_step, false).await? {
                        BeforeStop::Stop(step) => step,
                        BeforeStop::Continue(next) => {
                            state = next;
                            continue;
                        }
                        BeforeStop::Exit(exit) => return Ok(exit),
                    };
                    match stop_step {
                        StopStep::Stop {
                            state: stopped_state,
                            kind,
                        } => {
                            let exit_iteration = stopped_state.iteration;
                            let exit = latency::stage!(
                                "exit_skip_model",
                                host.run_context(),
                                exit_iteration,
                                self.exit.process(
                                    ctx,
                                    ExitInput {
                                        state: stopped_state,
                                        kind,
                                    },
                                ),
                            )?;
                            return Ok(exit);
                        }
                        StopStep::Continue { state: next } => {
                            next_state = next;
                        }
                        StopStep::Exit(exit) => return Ok(exit),
                    }

                    next_state.iteration = next_state.iteration.saturating_add(1);
                    state = next_state;
                    continue;
                }
            }
        }
    }
    /// The ONE final-drain decision every graceful stop goes through (the
    /// main path, a resumed gate, a compaction-only turn): pending steers are
    /// read first and the run continues with them, so a steer sent before
    /// the end is answered by this run. `already_drained`: the reply-only
    /// follow-up drain just ran for this turn.
    async fn drain_before_stop(
        &self,
        ctx: StageContext<'_>,
        stop_step: StopStep,
        already_drained: bool,
    ) -> Result<BeforeStop, AgentLoopExecutorError> {
        let StopStep::Stop {
            state,
            kind: crate::strategies::StopKind::GracefulStop,
        } = stop_step
        else {
            return Ok(BeforeStop::Stop(stop_step));
        };
        if already_drained {
            return Ok(BeforeStop::Stop(StopStep::Stop {
                state,
                kind: crate::strategies::StopKind::GracefulStop,
            }));
        }
        match self
            .input
            .process(
                ctx,
                DrainInput {
                    state,
                    mode: UserFacingInputDrainMode::FollowUp,
                },
            )
            .await?
        {
            InputStep::Continue {
                state: next,
                drained: true,
            } => {
                debug!(
                    iteration = next.iteration,
                    "agent loop continuing with steering read before a graceful stop"
                );
                let mut next = *next;
                next.iteration = next.iteration.saturating_add(1);
                Ok(BeforeStop::Continue(next))
            }
            InputStep::Continue {
                state: next,
                drained: false,
            } => Ok(BeforeStop::Stop(StopStep::Stop {
                state: *next,
                kind: crate::strategies::StopKind::GracefulStop,
            })),
            InputStep::Exit(exit) => Ok(BeforeStop::Exit(exit)),
        }
    }
}

/// What [`DefaultExecutorPipeline::drain_before_stop`] decided.
enum BeforeStop {
    Stop(StopStep),
    Continue(LoopExecutionState),
    Exit(LoopExit),
}
