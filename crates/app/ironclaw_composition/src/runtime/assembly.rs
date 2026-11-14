//! `assemble_runtime`: the one function that builds the started
//! [`RebornRuntime`] from a [`RebornRuntimeInput`]. Reached only through
//! `build_runtime_with_resource_governor`. The NEAR AI MCP bootstrap helpers it
//! calls at startup live here too.

// arch-exempt: large_file, `assemble_runtime` is one ~1,960-line function moved verbatim out of runtime.rs; shrinking it needs helper extraction (a logic refactor, not a move), plan #4471
use super::*;

use super::assembly_support::{
    AllowAllCapabilitySurfaceResolver, append_trusted_laptop_access_audit, bool_env_flag,
    build_generic_channel_connection_facade, build_webui_auth_interaction_service,
    optional_nonzero_u32_env, unattended_denied_capability_ids,
};
use super::skill_context::{filesystem_skill_context_runtime, filesystem_skill_context_source};
use super::startup_guards::{
    check_production_scheduler_wake_wiring, enforce_runtime_cutover_gate, validate_runtime_identity,
};
use super::store_parts::{RuntimeStoreParts, runtime_store_parts};

pub(super) async fn assemble_runtime(
    input: RebornRuntimeInput,
) -> Result<BuiltRuntime, RebornRuntimeError> {
    let RebornRuntimeInput {
        services: services_input,
        llm,
        boot,
        ironhub_agent_shared_key,
        ironhub_manifest_url,
        runner,
        tool_disclosure,
        trigger_poller,
        credential_refresh,
        trigger_fire_access_checker,
        trigger_fire_access,
        poll,
        identity,
        default_project_id,
        regex_skill_activation_enabled,
        skill_context_source: configured_skill_context_source,
        hooks: hooks_config,
        budget_defaults,
        budget_event_observer,
        trajectory_observer,
        admin_api_token_minter,
        #[cfg(any(test, feature = "test-support"))]
        model_gateway_override,
        #[cfg(any(test, feature = "test-support"))]
        model_cost_table_override,
        #[cfg(any(test, feature = "test-support"))]
        model_availability_retry_attempts_override,
    } = input;

    let mut services_input = services_input.ok_or(RebornRuntimeError::InvalidArgument {
        reason: "RebornRuntimeInput.services is required".to_string(),
    })?;

    let profile = services_input.profile();
    // The deployment this build assembles, as data (§4.4/§5.6). Every axis
    // below — live-traffic admission, the cutover gate, substrate selection —
    // reads a field on this value instead of re-matching the profile.
    let deployment = services_input.deployment().clone();
    // Read before `build_runtime_substrate` consumes the input (§ the runtime
    // policy capture below, same reason).
    let memory_curation_interval_turns = services_input.memory_curation_interval_turns();
    if let Some(reason) = deployment.traffic().live_traffic_refusal(profile) {
        return Err(RebornRuntimeError::InvalidArgument { reason });
    }
    // Capture the resolved policy before `build_runtime_substrate` consumes the
    // input. Downstream wiring selects enforcement behaviour from resolved
    // policy *values* (§4.4) rather than re-branching on the deployment
    // profile, so the policy has to outlive the services input.
    let runtime_policy =
        services_input
            .runtime_policy()
            .cloned()
            .ok_or(RebornRuntimeError::InvalidArgument {
                reason: "RebornRuntimeInput.services must include a resolved runtime policy"
                    .to_string(),
            })?;

    let validated_identity = validate_runtime_identity(identity)?;
    services_input = services_input.with_local_runtime_identity(
        validated_identity.tenant_id.clone(),
        validated_identity.agent_id.clone(),
    );
    let mut has_nearai_mcp_bootstrap_config = services_input.has_nearai_mcp_bootstrap_config();
    if !has_nearai_mcp_bootstrap_config
        && let Some(llm) = llm.as_ref()
        && let Some(config) =
            ironclaw_operator::llm_admin::nearai_mcp::nearai_mcp_bootstrap_config_from_llm_config(
                llm.config(),
            )
            .await
            .map_err(|error| RebornRuntimeError::InvalidArgument {
                reason: format!("NEAR AI MCP bootstrap config: {error}"),
            })?
    {
        services_input = services_input.with_nearai_mcp_bootstrap_config(config);
        has_nearai_mcp_bootstrap_config = true;
    }
    let trusted_laptop_access = services_input.grants_trusted_laptop_access();
    let owner_id = services_input.owner_id().to_string();
    let mut max_running_by_class = BTreeMap::new();
    if let Some(limit) = runner.max_concurrent_trigger_runs {
        max_running_by_class.insert(
            ProcessConcurrencyClass::from_trusted("scheduled_trigger"),
            limit.get(),
        );
    }
    if let Some(limit) = runner.max_concurrent_conversation_runs {
        max_running_by_class.insert(
            ProcessConcurrencyClass::from_trusted("conversation"),
            limit.get(),
        );
    }
    if let Some(limit) = runner.max_concurrent_unbound_runs {
        max_running_by_class.insert(
            ProcessConcurrencyClass::from_trusted("unbound"),
            limit.get(),
        );
    }
    services_input = services_input.with_process_concurrency_limits(ProcessConcurrencyLimits {
        max_running_per_owner: runner
            .max_concurrent_runs_per_user
            .map(std::num::NonZeroU32::get),
        max_running_by_class,
    });
    services_input = services_input.with_ironhub_manifest_url(ironhub_manifest_url.clone());
    let actor_user_id =
        UserId::new(owner_id.clone()).map_err(|reason| RebornRuntimeError::InvalidArgument {
            reason: format!("user id: {reason}"),
        })?;
    let nearai_mcp_owner_scope = ResourceScope {
        tenant_id: validated_identity.tenant_id.clone(),
        user_id: actor_user_id.clone(),
        agent_id: Some(validated_identity.agent_id.clone()),
        project_id: default_project_id.clone(),
        mission_id: None,
        thread_id: None,
        invocation_id: InvocationId::new(),
    };
    let mut services = build_runtime_substrate(services_input).await?;
    // The stored key no longer feeds the model gateway here (see the
    // post-construction reload below); the NEAR AI MCP bootstrap check is a
    // separate consumer that inspects `llm.config.nearai.api_key` directly,
    // so it still needs the key overlaid onto a local clone.
    if !has_nearai_mcp_bootstrap_config {
        let llm_for_mcp_bootstrap =
            overlay_stored_llm_key_for_nearai_mcp_bootstrap(llm.clone(), &services).await?;
        bootstrap_nearai_mcp_from_effective_llm(
            &services,
            llm_for_mcp_bootstrap.as_ref(),
            nearai_mcp_owner_scope,
        )
        .await?;
    }
    enforce_runtime_cutover_gate(&deployment, &services.readiness)?;

    // Extract the pre-minted scheduler wake wiring from the production composition path
    // (minted in `build_production_shaped`) so it can be handed to
    // `DefaultPlannedRuntimeParts.scheduler_wake_wiring` below. The standalone path
    // leaves this `None` and `build_default_planned_runtime` mints its own wiring.
    let production_scheduler_wake = {
        let wiring = services.production_scheduler_wake.take();
        // Production and migration-dry-run mint this in `build_production_shaped` so the
        // `HostRuntimeServices` notifier and the scheduler wake loop share one channel.
        // Fail closed if it is missing rather than let `build_default_planned_runtime`
        // mint a divergent scheduler-local channel (silent contract break).
        check_production_scheduler_wake_wiring(profile, &wiring)?;
        wiring
    };

    let runtime_parts = runtime_store_parts(&services);
    let RuntimeStoreParts {
        scoped_filesystem,
        turn_projection,
        processes,
        loop_checkpoint_store,
        thread_service,
        event_log,
        runtime_event_sink,
        audit_log,
        resource_governor,
        budget_gate_store,
        broadcast_budget_event_sink,
        subagent_await_edge_writer,
        subagent_await_edge_resolver,
        subagent_await_edge_store,
        subagent_await_edge_evidence,
        trigger_repository,
        admin_secret_provisioner,
        project_service,
        trigger_conversation_services,
    } = runtime_parts;
    let process_journal_source = processes.journal();
    let process_lifecycle_lookup_source = processes.lifecycle();
    let process_gate_query_source = processes.gates();
    // Suggestion generation is an ordinary durable AgentTurn.  Register its
    // materializer before the planned runtime starts so replay covers both
    // prior terminal commits and runs submitted immediately after startup.
    let suggestions_store: Arc<dyn ironclaw_assistant::SuggestionsStore> = Arc::new(
        ironclaw_assistant::FilesystemSuggestionsStore::new(Arc::clone(&scoped_filesystem)),
    );
    processes
        .subscribe_process_observer(Arc::new(SuggestionsProcessCommitObserver::new(
            Arc::clone(&suggestions_store),
            Arc::clone(&thread_service),
        )))
        .map_err(|error| RebornRuntimeError::MalformedConfig {
            reason: format!("suggestion generation observer wiring failed: {error}"),
        })?;
    // Computer use is on when the engine is bundled or configured; a thread's Stop ends its session.
    let computer_use = ironclaw_computer_use::ComputerUse::from_env().map_err(|error| {
        RebornRuntimeError::MalformedConfig {
            reason: error.to_string(),
        }
    })?;
    if let Some(computer_use) = &computer_use {
        let observer = ironclaw_assistant::computer_use::stop_observer(computer_use);
        processes
            .subscribe_process_observer(observer)
            .map_err(|error| RebornRuntimeError::MalformedConfig {
                reason: format!("computer use stop observer wiring failed: {error}"),
            })?;
    }
    if let Some(engine) = &services.orchestrator {
        // Turn lifecycle for the orchestrator, replayed from the durable cursor.
        processes
            .subscribe_process_observer(Arc::new(
                ironclaw_assistant::orchestration::OrchestratorProcessObserver::new(Arc::clone(
                    engine,
                )),
            ))
            .map_err(|error| RebornRuntimeError::MalformedConfig {
                reason: format!("orchestrator turn observer wiring failed: {error}"),
            })?;
        // Goal checks run where the shell runs, under the shell's approvals.
        engine.set_goal_check_runner(Arc::new(
            crate::goal_check_runner::HostGoalCheckRunner::new(
                services.shell_process_port.clone(),
                capability_host::approval_settings(&services),
            ),
        ));
    }
    processes
        .subscribe_process_observer(Arc::new(
            RunOutcomeProcessCommitObserver::new(
                Arc::clone(&services.notification_inbox),
                Arc::clone(&thread_service),
            )
            .with_process_journal_source(Arc::clone(&process_journal_source)),
        ))
        .map_err(|error| RebornRuntimeError::MalformedConfig {
            reason: format!("run outcome notification observer wiring failed: {error}"),
        })?;
    processes
        .subscribe_process_observer(Arc::new(
            AuthNotificationBackfillProcessCommitObserver::new(
                Arc::clone(&services.notification_inbox),
                Arc::clone(&process_journal_source),
            ),
        ))
        .map_err(|error| RebornRuntimeError::MalformedConfig {
            reason: format!("auth notification backfill observer wiring failed: {error}"),
        })?;
    processes
        .subscribe_process_observer(Arc::new(
            ApprovalNotificationBackfillProcessCommitObserver::new(
                Arc::clone(&services.notification_inbox),
                Arc::clone(&thread_service),
            ),
        ))
        .map_err(|error| RebornRuntimeError::MalformedConfig {
            reason: format!("approval notification backfill observer wiring failed: {error}"),
        })?;
    processes
        .subscribe_process_observer(Arc::new(ResourceBlockBackfillProcessCommitObserver::new(
            Arc::clone(&services.notification_inbox),
            Arc::clone(&thread_service),
            Arc::clone(&process_journal_source),
        )))
        .map_err(|error| RebornRuntimeError::MalformedConfig {
            reason: format!("resource-block notification backfill wiring failed: {error}"),
        })?;
    let filesystem_skill_context_runtime = filesystem_skill_context_runtime(&services);
    let (skill_context_source, skill_activation_source, skill_execution_adapter) = match (
        configured_skill_context_source,
        filesystem_skill_context_runtime,
    ) {
        (Some(source), _) => (Some(source), None, None),
        (None, Some(runtime)) => {
            let filesystem_skills = filesystem_skill_context_source(
                runtime,
                &validated_identity.tenant_id,
                regex_skill_activation_enabled,
            )?;
            let skill_warm_scope = ResourceScope {
                tenant_id: validated_identity.tenant_id.clone(),
                user_id: actor_user_id.clone(),
                agent_id: Some(validated_identity.agent_id.clone()),
                project_id: default_project_id.clone(),
                mission_id: None,
                thread_id: None,
                invocation_id: InvocationId::new(),
            };
            filesystem_skills
                .bundle_source
                .warm_system_root_descriptor_cache(&skill_warm_scope)
                .await
                .map_err(|error| RebornRuntimeError::InvalidArgument {
                    reason: format!("first-party skills warmup: {error}"),
                })?;
            // Profiles: hidden skills leave IronClaw's loop, and CLI threads get
            // their profile's soul, folder and skills.
            if let Some(engine) = &services.orchestrator {
                use ironclaw_assistant::orchestration::{
                    CliProfileContext, ProfileHome, ProfileSkillFilter,
                };
                filesystem_skills
                    .bundle_source
                    .set_run_filter(Arc::new(ProfileSkillFilter::new(Arc::clone(engine))));
                if let Some(home) = ProfileHome::from_env() {
                    let skills = cli_skills::StoreUserSkillFiles::new(
                        Arc::clone(&runtime.skill_filesystem),
                        skill_warm_scope.clone(),
                    );
                    engine.set_profile_context(Arc::new(CliProfileContext::new(
                        home,
                        Arc::new(skills),
                    )));
                }
            }
            (
                Some(filesystem_skills.source),
                Some(filesystem_skills.activation_source),
                Some(filesystem_skills.execution_adapter),
            )
        }
        (None, None) => (None, None, None),
    };
    let local_runtime = Some(&services);

    let tenant_id = validated_identity.tenant_id.clone();
    let agent_id = validated_identity.agent_id.clone();
    let thread_scope = ThreadScope {
        tenant_id,
        agent_id,
        project_id: default_project_id,
        // Keep standalone runtime threads aligned with WebUI's owner-scoped
        // facade so both entrypoints drive the same runner/evidence path.
        owner_user_id: Some(actor_user_id.clone()),
        mission_id: None,
    };

    // A test gateway override short-circuits the production build entirely:
    //    building a real gateway only to discard it wastes startup work (and, on
    //    the cold-boot path, an LLM session manager), which made
    //    timeout-sensitive tests flaky. When no override is set, build normally.
    // Build the (optional) skill-learning provider from the resolved LLM config.
    // Distillation/refinement runs against a stronger model
    // (IRONCLAW_SKILL_LEARNING_MODEL), reusing the run's NEAR AI credentials
    // with only the model overridden. `llm` no longer feeds the model gateway
    // build below (see `build_production_model_gateway`).
    let skill_learning_provider = match llm.as_ref() {
        Some(resolved) => build_skill_learning_provider(resolved.config()).await,
        None => None,
    };
    // Caller instrumentation seam (e.g. a benchmark harness layering
    // token/reasoning capture): carry the resolved LLM's provider factory into
    // the cold-boot gateway so the wrapper wraps the swappable and stays in the
    // call path across the boot-time reload. `llm` is held by shared reference
    // here (already read above for the NEAR AI MCP bootstrap), so clone the
    // cheap Arc handle rather than move the factory out of the borrow.
    let boot_provider_factory = llm
        .as_ref()
        .and_then(|resolved| resolved.provider_factory());
    // Per-thread provider routing needs the boot catalog and the operator keys.
    let provider_routes =
        boot.as_ref()
            .map(|boot| crate::model_gateway_assembly::ProviderRoutesInput {
                boot: boot.clone(),
                keys: ironclaw_operator::LlmKeyStore::new(
                    crate::RuntimeOperatorSecretValueStore::shared(Arc::clone(
                        &services.secret_store,
                    )),
                ),
            });
    #[cfg(any(test, feature = "test-support"))]
    let (model_gateway, llm_cost_table, llm_reload) = match model_gateway_override {
        Some(override_gateway) => (override_gateway, None, None),
        None => build_production_model_gateway(boot_provider_factory, provider_routes).await?,
    };
    #[cfg(not(any(test, feature = "test-support")))]
    let (model_gateway, llm_cost_table, llm_reload) =
        build_production_model_gateway(boot_provider_factory, provider_routes).await?;
    let live_llm_default = crate::model_gateway_assembly::live_llm_default(
        boot.as_ref(),
        llm_reload.as_ref(),
        &services.secret_store,
    );

    // Resolved cost table is either: the LLM-policy-derived table (real
    // LLM wired), a test override (so tests can drive deterministic
    // prices through stub gateways), or None — in which case the
    // accountant doesn't get built (no spend, no cascade). The test
    // override (when set) wins over the LLM-derived table — the test is
    // being explicit about the prices it wants.
    let llm_cost_table_arc: Option<Arc<dyn ironclaw_loop_host::ModelCostTable>> =
        llm_cost_table.map(|table| Arc::new(table) as Arc<dyn ironclaw_loop_host::ModelCostTable>);
    #[cfg(any(test, feature = "test-support"))]
    let resolved_cost_table = model_cost_table_override.or(llm_cost_table_arc);
    #[cfg(not(any(test, feature = "test-support")))]
    let resolved_cost_table = llm_cost_table_arc;

    // Build the model budget accountant from the resolved cost table plus
    // the standalone governor. `BudgetEnforcement::Unenforced` — the resolved
    // trusted-laptop boundary — is the explicit exception: it inherits host
    // trust and must not pause on budget gates. Reading the resolved value
    // rather than the deployment profile means a tenant/org ceiling that
    // narrows yolo away also restores enforcement (§4.4).
    // When neither an LLM policy nor a test override supplies a cost table
    // we deliberately skip the accountant — there's no spend to track and
    // the cascade would never fire.
    //
    // The accountant is wired with a seeding policy derived from the
    // caller-supplied `BudgetDefaults` (or `compiled_defaults().with_env()`
    // as the composition-root fallback when no caller pre-resolves them)
    // so a fresh user / project account picks up the default daily cap on
    // the first model call. Without this seeding step the standalone
    // governor starts empty and `reserve_with_outcome_in_state` skips
    // accounts that have no configured limit — model calls would record
    // usage but never enforce a cap (review feedback High #2 + Thermo-
    // Nuclear #1: defaults resolve once at the composition root with
    // explicit precedence and a `validate()` call instead of being
    // re-read by the wiring helper).
    let model_budget_accountant: Option<
        Arc<dyn ironclaw_loop_contracts::LoopModelBudgetAccountant>,
    > = match (
        ironclaw_runtime_policy::budget_enforcement(&runtime_policy),
        resolved_cost_table,
    ) {
        (ironclaw_runtime_policy::BudgetEnforcement::Unenforced, _) => None,
        (_, Some(cost_table)) => {
            let resolved_budget_defaults = match budget_defaults {
                Some(defaults) => {
                    defaults
                        .validate()
                        .map_err(|error| RebornRuntimeError::InvalidArgument {
                            reason: format!("supplied budget defaults invalid: {error}"),
                        })?;
                    defaults
                }
                None => {
                    let defaults = ironclaw_config::BudgetDefaults::compiled_defaults()
                        .with_env()
                        .map_err(|error| RebornRuntimeError::InvalidArgument {
                            reason: format!("budget defaults env-override invalid: {error}"),
                        })?;
                    defaults
                        .validate()
                        .map_err(|error| RebornRuntimeError::InvalidArgument {
                            reason: format!("resolved budget defaults invalid: {error}"),
                        })?;
                    defaults
                }
            };
            // Shared helper — same wiring shape used by any production
            // loop composer that wants the accountant.
            // The accountant uses the same broadcast-backed sink that
            // the governor writes to, so `BudgetEvent::GateOpened`
            // (emitted by the accountant) lands on the same downstream
            // projection as the governor's `Warned` / `Denied` events.
            let event_sink: Arc<dyn ironclaw_resources::BudgetEventSink> =
                Arc::clone(&broadcast_budget_event_sink)
                    as Arc<dyn ironclaw_resources::BudgetEventSink>;
            let accountant = crate::build_default_budget_accountant(
                Arc::clone(&resource_governor),
                cost_table,
                Arc::clone(&budget_gate_store),
                event_sink,
                &resolved_budget_defaults,
            );
            Some(accountant)
        }
        (_, None) => None,
    };

    let await_dependent_run_evidence: Arc<dyn AwaitDependentRunEvidenceStore> =
        Arc::clone(&subagent_await_edge_evidence);
    let mut loop_exit_evidence = ThreadCheckpointLoopExitEvidencePort::new_with_thread_scope(
        Arc::clone(&thread_service),
        Arc::clone(&turn_projection) as Arc<dyn ironclaw_turns::AgentTurnRuntimePort>,
        Arc::clone(&loop_checkpoint_store) as Arc<dyn ironclaw_turns::LoopCheckpointStore>,
        await_dependent_run_evidence,
        thread_scope.clone(),
    );
    if let Some(local_runtime) = local_runtime {
        let approval_requests = &local_runtime.approval_requests;
        loop_exit_evidence =
            loop_exit_evidence.with_approval_gate_evidence(Arc::new(ApprovalRequestGateEvidence {
                approval_requests: Arc::clone(approval_requests)
                    as Arc<dyn ironclaw_approvals::ApprovalRequestStorePort>,
            }));
        loop_exit_evidence = loop_exit_evidence.with_resource_gate_evidence(
            crate::observability::budget_evidence::budget_gate_evidence(Arc::clone(
                &local_runtime.budget_gate_store,
            )),
        );
    }
    let loop_exit_evidence = Arc::new(loop_exit_evidence);
    let milestone_thread_scope = ThreadScope {
        owner_user_id: Some(actor_user_id.clone()),
        ..thread_scope.clone()
    };
    let milestone_scope = DurableLoopHostMilestoneScope::from_thread_scope(&milestone_thread_scope)
        .map_err(|error| RebornRuntimeError::InvalidArgument {
            reason: error.to_string(),
        })?;
    let durable_milestone_sink: Arc<dyn LoopHostMilestoneSink> = Arc::new(
        DurableLoopHostMilestoneSink::new(Arc::clone(&runtime_event_sink), milestone_scope),
    );
    // One safe reply projection per runtime: the milestone sink composes
    // every run's document into it, reply publication reads from it.
    let reply_projection = Arc::new(ironclaw_assistant::projection::reply::ReplyProjection::new());
    if trusted_laptop_access {
        append_trusted_laptop_access_audit(&audit_log, &thread_scope, &actor_user_id).await?;
    }
    let mut projection_services = build_reborn_projection_services(
        Arc::clone(&event_log),
        validated_identity.reply_target_binding_ref.clone(),
    )
    .with_thread_service(Arc::clone(&thread_service))
    // A run parked on the native `ask_user` call projects as a form.
    .with_external_tool_catalog(Arc::clone(&services.external_tool_catalog));
    if let Some(local_runtime) = local_runtime {
        let approval_requests = &local_runtime.approval_requests;
        projection_services = projection_services
            .with_approval_requests(Arc::clone(approval_requests)
                as Arc<dyn ironclaw_approvals::ApprovalRequestStorePort>);
    }
    let live_projection_publisher =
        projection_services.live_projection_publisher(actor_user_id.clone());
    // The authenticated-session channel's reply sink publishes reconciled
    // reply revisions through the same live source the SSE/WebSocket
    // transports tail. Bound here because the binary assembled the binding
    // before this projection graph existed.
    if let Some(sink) = services.projection_reply_sink.as_ref() {
        _ = sink.bind_publisher(Arc::clone(&live_projection_publisher)); // first bind wins
    }
    if let Some(skill_activation_source) = &skill_activation_source {
        skill_activation_source
            .set_activation_observer(
                projection_services
                    .skill_activation_observer(Arc::clone(&live_projection_publisher)),
            )
            .map_err(|error| RebornRuntimeError::SkillExecution(error.to_string()))?;
    }
    // The registry is created with the local-runtime services (one instance
    // per runtime) so the trigger-create hook validates per-trigger delivery
    // targets against the same registry product hosts register into.
    let outbound_delivery_target_registry =
        local_runtime.map(|local_runtime| Arc::clone(&local_runtime.outbound_delivery_targets));
    let outbound_preferences_facade: Option<Arc<dyn OutboundPreferencesProductService>> =
        match (local_runtime, &outbound_delivery_target_registry) {
            (Some(local_runtime), Some(registry)) => {
                let registry = Arc::clone(registry);
                let provider: Arc<dyn OutboundDeliveryTargetProvider> = registry;
                let outbound_preferences = &local_runtime.outbound_preferences;
                Some(Arc::new(RebornOutboundPreferencesService::new(
                    Arc::clone(outbound_preferences),
                    provider,
                ))
                    as Arc<dyn OutboundPreferencesProductService>)
            }
            _ => None,
        };
    let skill_learning_publisher = live_projection_publisher;
    // Live progress reaches the browser through reply publication: the
    // milestone sink composes the safe reply document, and the
    // authenticated-session channel's projection reply sink renders each
    // revision as live projection items — the same path every channel uses.
    let milestone_sink: Arc<dyn LoopHostMilestoneSink> = Arc::new(
        ironclaw_assistant::projection::reply::ReplyProjectionMilestoneSink::new(
            durable_milestone_sink,
            Arc::clone(&reply_projection),
        ),
    );
    let diagnostic_store_impl =
        Arc::new(ironclaw_assistant::inspector_store::InMemoryDiagnosticStore::default());
    let diagnostic_store: Arc<dyn ironclaw_assistant::inspector_store::DiagnosticStorePort> =
        diagnostic_store_impl.clone();
    // Supermemory replaces local memory for callers with a key and binding;
    // a thread's profile picks its containers, the env binding is the default.
    let profile_engine = services.orchestrator.clone();
    let supermemory = ironclaw_supermemory::Supermemory::from_env_with_bindings(
        crate::RuntimeOperatorSecretValueStore::shared(Arc::clone(&services.secret_store)),
        |env| match profile_engine {
            Some(engine) => Arc::new(
                ironclaw_assistant::orchestration::ProfileSupermemoryBindings::new(engine, env),
            ),
            None => env,
        },
    )
    .map_err(|error| RebornRuntimeError::MalformedConfig {
        reason: error.to_string(),
    })?;
    let (
        capability_factory,
        capability_input_resolver,
        capability_result_writer,
        capability_surface_resolver,
        model_gateway,
        builtin_capability_policy,
        display_previews,
    ) = if local_runtime.is_some() {
        let builtin_capability_policy = Arc::clone(&services.capability_policy);
        let tool_diagnostic_sink = Arc::new(
            ironclaw_loop_host::BufferedPromptDiagnosticSink::new(
                diagnostic_store_impl.clone()
                    as Arc<dyn ironclaw_loop_host::HostManagedPromptDiagnosticSink>,
                ironclaw_loop_host::DEFAULT_TOOL_DIAGNOSTIC_QUEUE_CAPACITY,
            )
            .map_err(|reason| RebornRuntimeError::MalformedConfig { reason })?,
        )
            as Arc<dyn ironclaw_loop_host::HostManagedPromptDiagnosticSink>;
        let capability_host = capability_host::capability_wiring(
            &services,
            Arc::clone(&thread_service) as Arc<dyn SessionThreadService>,
            actor_user_id.clone(),
            Arc::clone(&builtin_capability_policy),
            model_gateway,
            milestone_sink.clone(),
            skill_activation_source.clone(),
            outbound_preferences_facade.clone(),
            trajectory_observer,
            Some(tool_diagnostic_sink),
            trigger_poller.enabled,
            Some(Arc::clone(&supermemory)),
            computer_use.clone(),
        )?;
        _ = reply_projection.bind_display_previews(Arc::clone(&capability_host.display_previews));
        (
            capability_host.capability_factory,
            capability_host.capability_input_resolver,
            capability_host.capability_result_writer,
            Arc::new(AllowAllCapabilitySurfaceResolver)
                as Arc<dyn CapabilitySurfaceProfileResolver>,
            capability_host.model_gateway,
            Some(builtin_capability_policy),
            Some(capability_host.display_previews),
        )
    } else {
        // The trajectory observer is wired only through the capability-host capability
        // path; runtimes without a capability host have no capability/result hook to forward
        // to. Accepting one here would silently produce an empty trajectory, so
        // fail fast — the seam is capability-host/bench-only (see
        // `RebornRuntimeInput::with_trajectory_observer`).
        if trajectory_observer.is_some() {
            return Err(RebornRuntimeError::InvalidArgument {
                reason: "a trajectory observer was supplied, but it is only supported on \
                         runtimes with a capability host; this profile has no local runtime to observe"
                    .to_string(),
            });
        }
        let capability_io = Arc::new(UnavailableCapabilityIo);
        let capability_input_resolver: Arc<dyn LoopCapabilityInputResolver> = capability_io.clone();
        let capability_result_writer: Arc<dyn LoopCapabilityResultWriter> = capability_io;
        let capability_factory: Arc<dyn LoopCapabilityPortFactory> =
            Arc::new(UnavailableCapabilityPortFactory);
        (
            capability_factory,
            capability_input_resolver,
            capability_result_writer,
            Arc::new(EmptyCapabilitySurfaceResolver) as Arc<dyn CapabilitySurfaceProfileResolver>,
            model_gateway,
            None,
            None,
        )
    };
    // Hook framework activation (#3934 + third-party projection), gated behind
    // the typed `HooksActivationConfig` carried in `RebornRuntimeInput` (master
    // flag default OFF; third-party sub-flag also default OFF). The env vars
    // (`HOOKS_ENABLED`, `HOOKS_THIRD_PARTY_ENABLED`) are resolved ONCE by the
    // CLI where it builds the runtime input; this composition root consumes
    // the typed config and never reads the environment itself.
    //
    // Hook-only projection containment: third-party `[[hooks]]` are discovered
    // and projected into a `HookProjectionRegistry` that carries ONLY hook
    // metadata (no `ExtensionRegistry`, no `ExtensionPackage`) and reaches ONLY
    // this hook factory, not the capability catalog or surface resolver.
    // Kept for the Settings hook inventory (built below, once memory upkeep
    // wiring is known).
    let mut hook_projection_registry = None;
    let hook_dispatcher_builder_factory = if let Some(local_runtime) = local_runtime {
        let extension_filesystem = local_runtime.extension_filesystem.as_ref();
        let third_party_input = crate::observability::hooks::ThirdPartyDiscoveryInput {
            filesystem: extension_filesystem,
            tenant_id: &validated_identity.tenant_id,
        };
        let projection_registry = crate::observability::hooks::build_hook_projection_registry(
            builtin_extension_registry()?,
            Some(third_party_input),
            hooks_config,
        )
        .await
        .map_err(|error| RebornRuntimeError::InvalidArgument {
            reason: format!("hook projection registry assembly failed: {error}"),
        })?;
        let factory =
            crate::observability::hooks::build_hook_dispatcher_builder_factory_for_tenant(
                hooks_config,
                &projection_registry,
                &validated_identity.tenant_id,
            )
            .map_err(|error| RebornRuntimeError::InvalidArgument {
                reason: format!("hook framework activation failed: {error}"),
            })?;
        hook_projection_registry = Some(projection_registry);
        factory
    } else if hooks_config.is_enabled() {
        return Err(RebornRuntimeError::MalformedConfig {
            reason: "hook framework is not supported or wired for production runtime launch"
                .to_string(),
        });
    } else {
        None
    };

    // Autonomous Trace Commons capture: a best-effort lifecycle sink mirrors
    // the v1 binary's turn-end capture. Policy-gated per user scope — the
    // sink is inert (one policy-file read per turn) until a scope enrolls
    // via `builtin.trace_commons.onboard` or `traces opt-in`.
    // Seed with the runtime owner's TENANT-SCOPED key (matching how capture
    // keys state), so startup pending-queue discovery finds the owner's queued
    // traces — a bare owner id would miss the `trace_scope_key(tenant, owner)`
    // queue dir.
    let runtime_owner_trace_scope = ironclaw_trace_commons::contribution::trace_scope_key(
        thread_scope.tenant_id.as_str(),
        actor_user_id.as_str(),
    );
    let trace_capture_scopes: ironclaw_trace_commons::capture::ObservedTraceScopes =
        Arc::new(std::sync::Mutex::new(std::collections::BTreeSet::from([
            runtime_owner_trace_scope,
        ])));
    let trace_capture_sink: Arc<dyn ironclaw_turns::TurnEventSink> = Arc::new(
        ironclaw_turn_runner::trace_capture::TraceCaptureTurnEventSink::new(
            Arc::clone(&thread_service),
            Arc::clone(&trace_capture_scopes),
        ),
    );
    let projection_turn_event_wake_sink = projection_services.turn_event_wake_sink();
    // Skill learning shares the turn-end seam with trace capture (composed
    // additively, so the trace-capture path is unchanged). It is active only
    // when a learning model is configured (a stronger model than the run's, via
    // IRONCLAW_SKILL_LEARNING_MODEL); otherwise only trace capture runs.
    // Operator metrics fold committed turn transitions (runs by outcome and
    // duration, approval gates opened) into the process-wide registry the
    // `/api/operator/metrics` scrape renders.
    let operator_metrics_sink: Arc<dyn ironclaw_turns::TurnEventSink> = Arc::new(
        crate::observability::operator_metrics::OperatorMetricsTurnEventSink::new(
            ironclaw_operator::operator_metrics(),
        ),
    );
    let mut turn_event_sinks: Vec<Arc<dyn ironclaw_turns::TurnEventSink>> = vec![
        trace_capture_sink,
        projection_turn_event_wake_sink,
        operator_metrics_sink,
    ];
    let mut skill_learning_extraction_tasks: Option<
        Arc<ironclaw_extension_host::skill_learning::SkillLearningExtractionTasks>,
    > = None;
    if let (Some((learning_provider, learning_model)), Some(local_runtime)) =
        (skill_learning_provider, local_runtime)
    {
        let inference: Arc<dyn ironclaw_skills::learning::SkillInferencePort> = Arc::new(
            ironclaw_extension_host::skill_learning::SkillLearningInferenceAdapter::new(
                learning_provider,
                learning_model,
            ),
        );
        // Reuse the runtime's already-built scoped skill-management port so the
        // learned skill lands exactly where the WebUI lists it and the next run
        // loads it. The writer evolves an existing learned skill in place when a
        // recurring task is re-learned, using the same learning model to refine
        // it (accumulated gotchas, bumped version) instead of accreting siblings.
        let skill_refiner: Arc<dyn ironclaw_extension_host::skill_learning::SkillRefiner> =
            Arc::new(
                ironclaw_extension_host::skill_learning::LlmSkillRefiner::new(Arc::clone(
                    &inference,
                )),
            );
        let skill_writer: Arc<dyn ironclaw_extension_host::skill_learning::SkillWriter> = Arc::new(
            ironclaw_extension_host::skill_learning::PortSkillWriter::new(
                Arc::clone(&local_runtime.skill_management),
                skill_refiner,
            ),
        );
        // Live "learned a skill" bubble on the run's thread stream (reuses the
        // SkillActivation projection -> existing chat bubble).
        let skill_learned_notifier: Arc<
            dyn ironclaw_extension_host::skill_learning::SkillLearnedNotifier,
        > = Arc::new(
            crate::model_gateway_assembly::LiveSkillLearnedNotifier::new(skill_learning_publisher),
        );
        let extraction_tasks =
            Arc::new(ironclaw_extension_host::skill_learning::SkillLearningExtractionTasks::new());
        skill_learning_extraction_tasks = Some(Arc::clone(&extraction_tasks));
        turn_event_sinks.push(Arc::new(
            ironclaw_extension_host::skill_learning::SkillLearningTurnEventSink::new(
                Arc::clone(&thread_service),
                inference,
                skill_writer,
                skill_learned_notifier,
                extraction_tasks,
            ),
        ));
    }
    let turn_event_sink: Arc<dyn ironclaw_turns::TurnEventSink> = Arc::new(
        ironclaw_extension_host::skill_learning::CompositeTurnEventSink::new(turn_event_sinks),
    );

    let communication_context_provider: Option<
        Arc<dyn ironclaw_loop_contracts::CommunicationContextProvider>,
    > = match (local_runtime, outbound_preferences_facade.clone()) {
        (Some(local_runtime), Some(outbound_preferences_facade)) => {
            let lifecycle_service =
                ironclaw_extension_manager::ExtensionHostLifecycleProductService::new(Arc::clone(
                    &local_runtime.skill_management,
                ))
                .with_extension_management(Arc::clone(&local_runtime.extension_management))
                .with_channel_config(Arc::clone(&local_runtime.channel_config_service));
            // Per-caller truth ports (#7247): the same scope-gated credential
            // status the extensions card and the runtime auth gate resolve
            // through, plus the same channel-connection facade the product
            // surface uses. Without them the provider must not — and does not
            // — claim any credentialed extension or personal-connection
            // channel is authenticated for the caller.
            let extension_credentials = Arc::new(
                ironclaw_extension_manager::webui_extension_credentials::ProductAuthExtensionCredentialSetup::new(
                    Arc::clone(&local_runtime.product_auth),
                ),
            );
            let channel_connections = build_generic_channel_connection_facade(
                validated_identity.tenant_id.clone(),
                &local_runtime.extension_management,
                &local_runtime.channel_identity_store,
                &local_runtime.product_auth,
                &local_runtime.channel_dm_target_store,
                local_runtime.channel_pairing.clone(),
            );
            Some(Arc::new(
                ironclaw_assistant::RuntimeCommunicationContextProvider::new(
                    outbound_preferences_facade,
                )
                .with_lifecycle_service(Arc::new(lifecycle_service))
                .with_extension_credentials(extension_credentials)
                .with_channel_connections(channel_connections),
            )
                as Arc<
                    dyn ironclaw_loop_contracts::CommunicationContextProvider,
                >)
        }
        _ => None,
    };

    // Resolve the disclosure mode once so the runtime config and the system-prompt
    // disclosure-protocol injection agree on a single value.
    let resolved_tool_disclosure = tool_disclosure.unwrap_or_else(ToolDisclosureMode::from_env);
    let default_runtime_config = DefaultPlannedRuntimeConfig::try_from_env()?;
    let tool_mentions = Arc::clone(&default_runtime_config.tool_mentions);
    // Resolve the bound memory provider once (issue #3537): the
    // profile source, prompt-context lane, and after-turn writer all fan out from
    // this single resolution, so they agree on the bound provider (native, or
    // `None` for a disabled/third-party-without-a-provider binding). The bound
    // provider's DECLARED lifecycle set gates each consumer below: a hook the
    // manifest does not declare is never wired, so it is never called.
    let resolved_memory_provider = local_runtime.and_then(|local_runtime| {
        local_runtime.memory_service_resolver.resolve_provider(
            Arc::clone(&local_runtime.extension_filesystem)
                as Arc<dyn ironclaw_filesystem::RootFilesystem>,
            None,
        )
    });
    let memory_lifecycle = local_runtime
        .map(|local_runtime| local_runtime.memory_lifecycle.clone())
        .unwrap_or_default();
    // Scheduled memory upkeep (#7276 / #7664): the bound provider's
    // DECLARATION arms it — validated shape, resolved prompt, recommended
    // cadence — and `[memory].curation_interval_turns` ENABLES it (owner
    // decision, 2026-08-22: background model spend stays opt-in; a manifest
    // must not switch on token cost for a deployment that never asked).
    // Config set while the bound provider declares nothing is a startup
    // error, not a degraded mode — that deployment would believe memory is
    // being tidied while nothing runs
    // (`scheduled_after_turn_interval_override` carries the reasoning).
    let memory_scheduled_op_interval_override =
        crate::memory_provider_factory::scheduled_after_turn_interval_override(
            memory_curation_interval_turns,
            &memory_lifecycle,
            local_runtime
                .map(|local_runtime| local_runtime.memory_service_resolver.resolved_binding())
                .as_ref(),
        )
        .map_err(|reason| RebornRuntimeError::MalformedConfig { reason })?;
    // Which hook, at which phase, under which trust class is decided by the
    // crate that owns the dispatcher, so this is one call into it.
    let memory_scheduled_op_hook_wiring = match memory_lifecycle
        .scheduled_op(ironclaw_extension_contracts::memory::MemoryScheduledTrigger::AfterTurn)
        .filter(|_| resolved_memory_provider.is_some())
        // Opt-in: an armed declaration with no configured interval stays
        // dormant. The declared `interval_turns` is the provider's
        // RECOMMENDED cadence for the config key, not an activation.
        .filter(|_| memory_scheduled_op_interval_override.is_some())
    {
        None => None,
        Some(declared) => {
            // The prompt was resolved fail-closed at bundle construction, so a
            // declared op always has one here. Treat its absence as the
            // configuration error it would be rather than scheduling a pass
            // with an empty instruction.
            let prompt = local_runtime
                .and_then(|local_runtime| {
                    local_runtime
                        .memory_scheduled_pass_prompts
                        .iter()
                        .find(|(trigger, _)| *trigger == declared.trigger)
                        .map(|(_, prompt)| prompt.clone())
                })
                .ok_or_else(|| RebornRuntimeError::MalformedConfig {
                    reason: "the bound memory provider declares an after_turn scheduled op whose \
                             prompt asset did not resolve"
                        .to_string(),
                })?;
            let declared = declared.clone();
            Some(Box::new(
                move |deps: ironclaw_turn_runner::runtime::AfterTurnHookDeps| {
                    let submitter = Arc::new(ironclaw_assistant::UnboundTurnService::new(
                        deps.thread_service,
                        deps.coordinator,
                        deps.thread_scope.agent_id,
                        deps.thread_scope.project_id,
                    ));
                    // Fail closed: the bound provider declared this work. If the
                    // hook cannot install, swallowing it would leave a
                    // deployment that believes memory is being tidied while
                    // nothing ever runs — a difference nothing surfaces later.
                    // The build carries this out as a startup error instead.
                    ironclaw_assistant::memory_scheduled_ops::after_turn_scheduled_op_dispatcher_factory(
                        submitter,
                        &declared,
                        &prompt,
                        memory_scheduled_op_interval_override,
                    )
                },
            )
                as ironclaw_turn_runner::runtime::AfterTurnHookWiring)
        }
    };
    let memory_upkeep_hook_wired = memory_scheduled_op_hook_wiring.is_some();
    let memory_page = crate::memory_page_parts::MemoryPageParts {
        documents: local_runtime
            .filter(|local_runtime| {
                local_runtime.memory_service_resolver.resolved_binding()
                    == ironclaw_host_runtime::memory_binding::MemoryProviderBinding::Native
            })
            .map(|local_runtime| {
                Arc::new(crate::memory_page_parts::NativeMemoryDocuments::new(
                    Arc::clone(&local_runtime.extension_filesystem)
                        as Arc<dyn ironclaw_filesystem::RootFilesystem>,
                    local_runtime.memory_prompt_safety_sink.clone(),
                    local_runtime.memory_service_resolver.native_embedder(),
                ))
                    as Arc<dyn ironclaw_product_contracts::memory_page::MemoryDocumentsSource>
            }),
        supermemory: Arc::clone(&supermemory),
        hooks: Arc::new(crate::memory_page_parts::StaticHookCatalog(
            crate::observability::hooks::catalog::hook_catalog(
                hooks_config,
                hook_projection_registry.as_ref(),
                memory_upkeep_hook_wired,
            ),
        )),
    };
    let crate::memory_provider_factory::MemoryLifecycleConsumers {
        memory_context_service: wired_memory_context_service,
        after_turn_memory_writer: wired_after_turn_memory_writer,
        user_profile_source: wired_memory_user_profile_source,
    } = crate::memory_provider_factory::memory_lifecycle_consumers(
        resolved_memory_provider,
        &memory_lifecycle,
    );
    // The bound provider's own memory guidance for the model, if it ships any
    // (#7185). The text is the provider's — it names that provider's tools and
    // describes that provider's recall behavior — resolved generically at
    // bundle-construction time against the BOUND provider's own asset table
    // (`memory_provider_factory::resolve_memory_provider`), never by a
    // host-side match on a specific provider's constants. Two conditions, both
    // necessary: a provider must actually be resolved (a `Disabled` binding
    // registers no package, so the model sees no `ironclaw.memory.*` tools and
    // must not be told they exist), and that provider must declare a
    // `guidance_doc`. Either missing ⇒ nothing is appended.
    let memory_guidance = wired_memory_context_service
        .is_some()
        .then(|| {
            local_runtime
                .map(|local_runtime| local_runtime.memory_guidance.clone())
                .unwrap_or_default()
        })
        .flatten();

    // Deferred bind (§ await-edge resolver ordering note above,
    // `RuntimeStoreParts`'s doc comment): the resolver was assembled inside
    // `runtime_store_parts` before `capability_result_writer` existed. Bind it
    // now, exactly once, before the resolver's settler ever runs.
    subagent_await_edge_resolver
        .bind_result_writer(Arc::clone(&capability_result_writer))
        .map_err(|error| RebornRuntimeError::MalformedConfig {
            reason: format!("await-edge resolver result writer bind failed: {error}"),
        })?;
    // Steering/followup input queue: a message queued while a run is busy is
    // persisted per-run through the composed scoped filesystem
    // (`FilesystemHostInputQueue`), so it survives a daemon restart — the
    // scheduler re-claims the run from its checkpoint and drains the persisted
    // input. The same instance serves as the loop's drain reader
    // (`parts.input_queue`) and every inbound surface's enqueue port.
    let host_input_queue = {
        let owner_scope = ResourceScope {
            tenant_id: thread_scope.tenant_id.clone(),
            user_id: actor_user_id.clone(),
            agent_id: Some(thread_scope.agent_id.clone()),
            project_id: thread_scope.project_id.clone(),
            mission_id: None,
            thread_id: None,
            invocation_id: InvocationId::new(),
        };
        Arc::new(ironclaw_loop_host::FilesystemHostInputQueue::new(
            Arc::clone(&services.scoped_filesystem),
            owner_scope,
            Arc::clone(&thread_service),
        ))
    };
    host_input_queue
        .bind_ack_effect_handler(Arc::clone(&subagent_await_edge_resolver) as Arc<_>)
        .map_err(|error| RebornRuntimeError::MalformedConfig {
            reason: format!("await-edge resolver ack effect handler bind failed: {error}"),
        })?;
    let host_input_queue_reader: Arc<dyn ironclaw_loop_host::HostInputQueue> =
        host_input_queue.clone();
    let host_input_queue_for_cancel_reconcile: Arc<
        dyn ironclaw_loop_host::HostInputQueueReconcile,
    > = host_input_queue.clone();
    let host_input_queue_for_terminal_reconcile: Arc<
        dyn ironclaw_loop_host::HostInputQueueReconcile,
    > = host_input_queue.clone();
    let host_input_enqueue: Arc<dyn ironclaw_loop_host::HostInputEnqueuePort> =
        host_input_queue.clone();
    // Deferred bind: background delivery enqueues a settled child's result as
    // steering input for the parent's live run through this queue.
    subagent_await_edge_resolver
        .bind_input_enqueue(Arc::clone(&host_input_enqueue))
        .map_err(|error| RebornRuntimeError::MalformedConfig {
            reason: format!("await-edge resolver input enqueue port bind failed: {error}"),
        })?;

    #[cfg(feature = "test-support")]
    let runtime_skill_context_source = skill_context_source.clone();
    let diagnostic_inner =
        diagnostic_store_impl as Arc<dyn ironclaw_loop_host::HostManagedPromptDiagnosticSink>;
    // Model-call telemetry (context, speed, cache) feeds the orchestrator.
    let diagnostic_inner = match &services.orchestrator {
        Some(engine) => Arc::new(
            ironclaw_assistant::orchestration::OrchestratorTelemetrySink::new(
                diagnostic_inner,
                Arc::clone(engine),
            ),
        ) as Arc<dyn ironclaw_loop_host::HostManagedPromptDiagnosticSink>,
        None => diagnostic_inner,
    };
    let prompt_diagnostic_sink = Arc::new(
        ironclaw_loop_host::BufferedPromptDiagnosticSink::new(
            diagnostic_inner,
            ironclaw_loop_host::DEFAULT_PROMPT_DIAGNOSTIC_QUEUE_CAPACITY,
        )
        .map_err(|reason| RebornRuntimeError::MalformedConfig { reason })?,
    )
        as Arc<dyn ironclaw_loop_host::HostManagedPromptDiagnosticSink>;
    let mut planned_runtime_parts = DefaultPlannedRuntimeParts {
        process_system: processes.clone(),
        thread_service: Arc::clone(&thread_service),
        thread_scope: thread_scope.clone(),
        // Read landed attachment bytes back through the project workspace
        // filesystem so the model port can build multimodal image parts for
        // vision-capable models. Only available when a local runtime (and thus a
        // workspace filesystem) is composed.
        //
        // Lander and reader share ONE handle so an inbound attachment is read
        // back from the subtree it landed in. Under a per-caller workspace
        // policy (`serve` sets it unconditionally) the lander writes to
        // `/projects/workspace/tenants/{tenant}/users/{user}`, and the shared
        // read-only `workspace_filesystem` would address the root instead —
        // the exact regression that dropped every image from the model payload
        // after #7062 scoped the write lanes. Mirrors the channel-host wiring
        // in `channel_host_source`.
        attachment_read_port: crate::runtime_mounts::read_write_workspace_filesystem(
            &services.extension_filesystem,
            &services.workspace_mounts,
        )
        .map(|filesystem| {
            Arc::new(ironclaw_assistant::ProjectScopedAttachmentReader::new(
                filesystem,
            )) as Arc<dyn ironclaw_loop_host::LoopAttachmentReadPort>
        }),
        tool_image_port: computer_use.clone().map(|computer_use| {
            Arc::new(computer_use_images::ComputerUseToolImages(computer_use))
                as Arc<dyn ironclaw_loop_host::LoopToolImagePort>
        }),
        prompt_diagnostic_sink: Some(prompt_diagnostic_sink),
        reply_attachment_intent_port: Some(Arc::clone(&services.reply_attachment_intents)),
        // §5.2.9 render-from-record: a `GateRecordStore` over the SAME
        // shared `extension_filesystem` + per-user mount view the standalone
        // capability port persists `GateRecord::Auth` into (see
        // `runtime/capability_host.rs`'s capability wiring, which builds
        // its store the same way and passes it via `with_gate_record_store`).
        // Both are stateless views over one durable Arc, so the turn executor
        // reads back exactly the record the capability port saved under the
        // matching owner scope. The two constructions MUST stay over the same
        // filesystem/scope.
        gate_record_store: Some(Arc::new(ironclaw_approvals::GateRecordStore::new(
            crate::wrap_scoped(Arc::clone(&services.extension_filesystem)),
        ))
            as Arc<dyn ironclaw_approvals::GateRecordStorePort>),
        model_gateway: Arc::clone(&model_gateway),
        loop_checkpoint_store: Arc::clone(&loop_checkpoint_store)
            as Arc<dyn ironclaw_turns::LoopCheckpointStore>,
        milestone_sink,
        capability_factory,
        capability_surface_resolver,
        capability_result_writer,
        subagent_await_edge_writer,
        subagent_await_edge_settler: Arc::clone(&subagent_await_edge_resolver)
            as Arc<dyn AwaitEdgeSettler>,
        subagent_await_edge_evidence,
        subagent_definition_resolver: Arc::new(StaticSubagentDefinitionResolver),
        subagent_spawn_input_codec: Arc::new(JsonSpawnSubagentInputCodec::new(
            capability_input_resolver,
        )),
        subagent_spawn_limits: ironclaw_loop_host::SubagentSpawnLimits::default(),
        loop_exit_evidence,
        config: DefaultPlannedRuntimeConfig {
            heartbeat_interval: runner.heartbeat_interval,
            poll_interval: runner.poll_interval,
            lease_recovery_interval: default_runtime_config.lease_recovery_interval,
            worker_count: runner.worker_count,
            disabled_capability_ids: default_runtime_config.disabled_capability_ids,
            unattended_denied_capability_ids: unattended_denied_capability_ids()?,
            text_only_driver: Default::default(),
            host: Default::default(),
            tool_disclosure: resolved_tool_disclosure,
            tool_disclosure_profile_pins: default_runtime_config.tool_disclosure_profile_pins,
            tool_mentions: Arc::clone(&tool_mentions),
            planned_default_iteration_limit: optional_nonzero_u32_env(
                "IRONCLAW_REBORN_PLANNED_DEFAULT_ITERATION_LIMIT",
            )?,
            planned_model_availability_retry_attempts: {
                #[cfg(any(test, feature = "test-support"))]
                let resolved = match model_availability_retry_attempts_override {
                    Some(attempts) => Some(attempts),
                    None => optional_nonzero_u32_env(
                        "IRONCLAW_REBORN_MODEL_AVAILABILITY_RETRY_ATTEMPTS",
                    )?,
                };
                #[cfg(not(any(test, feature = "test-support")))]
                let resolved =
                    optional_nonzero_u32_env("IRONCLAW_REBORN_MODEL_AVAILABILITY_RETRY_ATTEMPTS")?;
                resolved
            },
        },
        model_route_resolver: None,
        cancellation_factory: None,
        skill_context_source,
        input_queue: Some(host_input_queue_reader),
        input_queue_reconcile: Some(host_input_queue_for_terminal_reconcile),
        identity_context_source: match (
            services.standalone_storage_root.clone(),
            services.default_system_prompt_path.clone(),
        ) {
            (Some(standalone_storage_root), Some(default_system_prompt_path)) => {
                Arc::new(
                    // Standalone seeding validates the prompt path first, so non-file prompt paths fail
                    // as build errors before this runtime-level identity-source guard is reached.
                    DefaultSystemPromptIdentitySource::try_new(
                        standalone_storage_root,
                        default_system_prompt_path,
                        SystemPromptProtocols {
                            // `is_enabled()` (not `is_bridged()`): #7410 widened
                            // the disclosure protocol to every enabled mode.
                            disclosure: resolved_tool_disclosure.is_enabled(),
                            benchmarking_mode: bool_env_flag("BENCHMARKING_MODE"),
                            // Provider-shipped, not host-owned: whatever the
                            // bound memory extension declares as its guidance,
                            // or nothing.
                            memory_guidance,
                            // The computer-use server's own instructions,
                            // only while its tools are wired.
                            computer_use_guidance: computer_use
                                .as_ref()
                                .map(|computer_use| computer_use.instructions()),
                        },
                    )
                    .map_err(|error| RebornRuntimeError::InvalidArgument {
                        reason: error.to_string(),
                    })?,
                ) as Arc<dyn HostIdentityContextSource>
            }
            (None, None) => {
                Arc::new(EmptyIdentityContextSource) as Arc<dyn HostIdentityContextSource>
            }
            _ => {
                return Err(RebornRuntimeError::InvalidArgument {
                    reason: "assembled runtime must provide local storage root and default system prompt path together"
                        .to_string(),
                });
            }
        },
        // Resolve the per-user agent-context profile (timezone/locale/location) from
        // `context/profile.json` via the workspace filesystem. When a standalone workspace
        // filesystem is available, the `MemoryBackedUserProfileSource` adapter reads it;
        // otherwise `EmptyUserProfileSource` degrades gracefully to `None` (profile unknown).
        // `extension_filesystem` is the raw `Arc<CompositeRootFilesystem>` (=
        // `CompositeRootFilesystem`) — the underlying RootFilesystem the workspace
        // mounts are built from. `MemoryBackedUserProfileSource` constructs its own
        // full virtual paths via `profile_scope_and_path` and does not use the
        // `ScopedFilesystem` mount view, so the raw `RootFilesystem` is correct here.
        //
        // NOTE: this `Some(local_runtime) => real / None => Empty` guard intentionally
        // mirrors `identity_context_source` directly above. Runtime builds without the
        // auxiliary local substrate currently wire NEITHER the identity source NOR this
        // profile source — both degrade to Empty there today. Wiring configured
        // production equivalents for these optional context sources is a single
        // deferred follow-up (identity + profile together, to keep them paired); do not
        // wire only one of them here, or they will diverge. See issue #5013.
        //
        // Profile reads go through the same memory provider resolver as the
        // memory tools (issue #3537), AND only when the bound provider's
        // manifest declares the `profile_read` lifecycle hook. A disabled /
        // unconstructible binding, or an undeclared hook, degrades to `Empty`
        // (profile unknown) rather than silently reading native — keeping
        // profile reads and tools consistent, from one construction point.
        user_profile_source: wired_memory_user_profile_source
            .unwrap_or_else(|| Arc::new(EmptyUserProfileSource) as Arc<dyn HostUserProfileSource>),
        // Proactive memory (#3537 / mem0 flow): fan out from the SAME resolved
        // provider the profile source and after-turn writer use, wrap it in
        // the host's prompt-context adapter with the provider's DECLARED
        // lifecycle (each retrieval lane is queried only if declared), and let
        // the loop surface the declared lanes into the prompt once per run. A
        // disabled or third-party-without-a-provider binding resolves to
        // `None` — degrading to no memory rather than silently reading native
        // (issue #5013).
        memory_context_service: ironclaw_supermemory::SupermemoryPromptContext::wrap(
            &supermemory,
            wired_memory_context_service,
        ),
        // After-turn memory recording (#3537 / mem0 `add`): the RAW bound
        // provider — the SAME resolved provider the profile source and prompt-context
        // lane use, NOT wrapped in `ProductionMemoryPromptContextService`. The
        // executor forwards each Completed run's transcript to `record_interaction`
        // ONLY when the provider's manifest declares that hook; `None` degrades
        // to no after-turn recording (issue #5013).
        after_turn_memory_writer: ironclaw_supermemory::SupermemoryInteractionRecorder::wrap(
            &supermemory,
            wired_after_turn_memory_writer,
        ),
        model_policy_guard: None,
        model_budget_accountant,
        safety_context: None,
        hook_security_audit_sink: Some(Arc::new(ironclaw_event_log::TracingSecurityAuditSink)),
        turn_event_sink: Some(turn_event_sink),
        hook_dispatcher_builder_factory,
        after_turn_hook_wiring: memory_scheduled_op_hook_wiring,
        communication_context_provider,
        // For the production composition path, use the pre-minted wiring from
        // `build_production_shaped` so the `HostRuntimeServices` notifier (used by
        // `turn_coordinator_for_production`) and the scheduler's wake loop share the
        // exact same channel. For standalone, `None` causes `build_default_planned_runtime`
        // to mint its own wiring internally (existing behavior).
        scheduler_wake_wiring: production_scheduler_wake,
    };
    // Stage 3 profiles: SOUL.md joins SYSTEM.md; the allowlist narrows the surface.
    if let Some(engine) = &services.orchestrator {
        use ironclaw_assistant::orchestration::{
            profile_identity_source, profile_surface_resolver,
        };
        let parts = &mut planned_runtime_parts;
        parts.identity_context_source =
            profile_identity_source(Arc::clone(&parts.identity_context_source), engine);
        let registry = Arc::clone(&services.shared_extension_registry);
        let mcp_capabilities: ironclaw_assistant::orchestration::McpCapabilityLister =
            Arc::new(move || {
                registry
                    .snapshot()
                    .capabilities()
                    .filter(|c| c.runtime == ironclaw_host_api::runtime::RuntimeKind::Mcp)
                    .map(|c| (c.provider.clone(), c.id.clone()))
                    .collect()
            });
        parts.capability_surface_resolver = profile_surface_resolver(
            Arc::clone(&parts.capability_surface_resolver),
            engine,
            mcp_capabilities,
        );
    }
    let composition = build_default_planned_runtime(planned_runtime_parts)?;
    let default_resolved_run_profile = composition
        .run_profile_resolver
        .resolve_run_profile(RunProfileResolutionRequest::interactive_default())
        .await
        .map_err(|error| RebornRuntimeError::InvalidArgument {
            reason: format!("could not resolve default run profile: {error}"),
        })?;
    services
        .trigger_create_hook
        .bind_execution_preflight(Arc::new(StructuredTriggerExecutionPreflight::new(
            Arc::clone(&services.shared_extension_registry),
            skill_activation_source.clone(),
            default_resolved_run_profile.clone(),
            // Mirrors the runner's decorator-attach condition: bridge ids only
            // exist on fired runs when disclosure is enabled.
            resolved_tool_disclosure.is_enabled(),
        )))
        .map_err(|error| RebornRuntimeError::InvalidArgument {
            reason: format!("structured trigger preflight could not be bound: {error}"),
        })?;
    let default_run_profile_id = default_resolved_run_profile.profile_id.as_str().to_string();
    let failure_explanation_thread_id =
        ThreadId::new("failure-explanation-system").map_err(|reason| {
            RebornRuntimeError::InvalidArgument {
                reason: format!("failure explanation thread id: {reason}"),
            }
        })?;
    let failure_explanation_scope = TurnScope::new(
        thread_scope.tenant_id.clone(),
        Some(thread_scope.agent_id.clone()),
        thread_scope.project_id.clone(),
        failure_explanation_thread_id,
    );
    let failure_explanation_profile = default_resolved_run_profile.clone();
    let failure_explanation_model_gateway = Arc::clone(&model_gateway);
    let failure_explanation_inference = Arc::new(move || {
        Arc::new(ModelGatewayBackedSystemInferencePort::new(
            Arc::clone(&failure_explanation_model_gateway),
            LoopRunContext::new(
                failure_explanation_scope.clone(),
                TurnId::new(),
                TurnRunId::new(),
                failure_explanation_profile.clone(),
            ),
        )) as Arc<dyn ironclaw_loop_contracts::SystemInferencePort>
    });
    // Terminal reconciliation of stranded steering inputs: every cancel caller
    // goes through this ONE decorated coordinator, so a run cancelled before
    // its next drain flips its queued messages to `RejectedBusy` (resend
    // affordance) instead of leaving them `Queued` forever.
    let planned_turn_coordinator: Arc<dyn TurnCoordinator> = Arc::new(
        ironclaw_turn_runner::steering_reconcile::CancelReconcilingTurnCoordinator::new(
            composition.coordinator.clone(),
            host_input_queue_for_cancel_reconcile,
        ),
    );
    let approval_interaction_service: Arc<dyn ApprovalInteractionService> =
        if let (Some(local_runtime), Some(builtin_capability_policy)) =
            (local_runtime, builtin_capability_policy.as_ref())
        {
            build_approval_interaction_service(
                local_runtime,
                Arc::clone(builtin_capability_policy),
                Arc::clone(&planned_turn_coordinator),
                None,
            )?
        } else {
            Arc::new(UnavailableApprovalInteractionService)
        };
    let auth_interaction_service = if let Some(local_runtime) = local_runtime {
        build_webui_auth_interaction_service(
            services.product_auth.as_ref(),
            Arc::clone(&local_runtime.process_gate_query_source),
            Arc::clone(&planned_turn_coordinator),
            Arc::clone(&services.notification_inbox),
        )
    } else {
        Arc::new(auth_interaction::UnavailableAuthInteractionService)
    };
    let turn_event_source: Arc<dyn TurnEventProjectionSource> =
        Arc::new(ironclaw_turns::TurnEventProjectionFromProcessJournal::new(
            Arc::clone(&process_journal_source),
        ));
    let mut projection_services = projection_services
        .with_turn_events(turn_event_source, Arc::clone(&planned_turn_coordinator))
        .with_model_failure_explainer_factory(failure_explanation_inference);
    if let Some(display_previews) = display_previews {
        projection_services = projection_services.with_display_previews(display_previews);
    }
    // One recipe-driven challenge provider feeds both WebUI projections and
    // external-channel delivery. OAuth/manual challenges delegate to
    // product-auth; host-issued pairing delegates to the canonical pairing
    // service and reuses its live code/deep-link/expiry presentation.
    let auth_challenges =
        ironclaw_extension_host::run_delivery_ports::RecipeAuthChallengeProvider::compose(
            product_auth_challenge_provider(&services.product_auth),
            services.channel_pairing.clone(),
        );
    let projection_services = if let Some(provider) = auth_challenges.clone() {
        projection_services.with_auth_challenges(provider)
    } else {
        projection_services
    };
    // Durable idempotency ledger for the authenticated-session inbound lane
    // (browser + API transports riding `submit_turn`): the session half of the
    // same durable-admission discipline the per-extension channel ledgers
    // provide, on the same filesystem substrate.
    let session_inbound_ledger = ironclaw_assistant::build_session_inbound_ledger(
        &(services.extension_filesystem.clone() as Arc<dyn ironclaw_filesystem::RootFilesystem>),
        &validated_identity.tenant_id,
        ironclaw_host_api::resource::ResourceScope {
            tenant_id: validated_identity.tenant_id.clone(),
            user_id: actor_user_id.clone(),
            agent_id: Some(validated_identity.agent_id.clone()),
            project_id: None,
            mission_id: None,
            thread_id: None,
            invocation_id: ironclaw_host_api::ids::InvocationId::new(),
        },
    )
    .map_err(|reason| RebornRuntimeError::InvalidArgument { reason })?;
    let session_channel_directory: Arc<
        dyn ironclaw_product_contracts::session_ingress::SessionChannelDirectory,
    > = Arc::new(
        ironclaw_extension_host::session_ingress::DeploymentSessionChannelDirectory::new(
            services.deployment_channels.clone(),
        ),
    );
    // The deployment's session channel, advertised to the SPA on
    // `GET /session`. Exactly one session channel resolves; zero or several
    // resolve to none — fail closed, never guess.
    let session_channel_extension_id = {
        let session_ids: Vec<String> = services
            .deployment_channels
            .extension_ids()
            .into_iter()
            .filter(|extension_id| session_channel_directory.is_session_channel(extension_id))
            .collect();
        match session_ids.as_slice() {
            [only] => Some(only.clone()),
            [] => None,
            // Ambiguous: fail closed rather than pick one. The built-in
            // surface is not a safe answer here either, because a channel
            // that believes it owns the session would silently stop
            // receiving browser turns.
            several => {
                tracing::debug!(
                    count = several.len(),
                    "multiple session channels declared; advertising none"
                );
                None
            }
        }
    };

    let llm_config_service = crate::product_surface::compose_llm_config_service(
        boot.as_ref(),
        ironclaw_operator::LlmKeyStore::new(crate::RuntimeOperatorSecretValueStore::shared(
            Arc::clone(&services.secret_store),
        )),
        Arc::clone(&scoped_filesystem),
        llm_reload.as_ref(),
        live_llm_default.as_ref(),
    );
    let started_channel_host = crate::extension_host_assembly::build_runtime_channel_host(
        &services,
        crate::extension_host_assembly::RuntimeExtensionHostAssemblyWiring {
            thread_service: Arc::clone(&thread_service),
            turn_coordinator: Arc::clone(&planned_turn_coordinator),
            input_enqueue: Arc::clone(&host_input_enqueue),
            llm_config: llm_config_service.clone(),
            approval_interaction: Arc::clone(&approval_interaction_service),
            auth_interaction: Arc::clone(&auth_interaction_service),
            thread_scope: &thread_scope,
            actor_user_id: actor_user_id.clone(),
            auth_challenges,
            outbound_delivery_targets: outbound_delivery_target_registry.as_ref(),
            local_runtime,
            reply_projection: Arc::clone(&reply_projection),
        },
    )
    .await;
    let channel_workflow_factory = started_channel_host
        .as_ref()
        .map(|started| Arc::clone(&started.workflow_factory));
    // A run's terminal commit resumes its publications: on this node a fast
    // path to the durable terminal facts, on any other node the way an
    // orphaned publication gets a worker again. The coordinator acknowledges
    // the commit only after recovery ran, and the boot sweep below covers a
    // crash that happened after an acknowledgement.
    let mut reply_publication_recovery: Option<tokio::task::JoinHandle<()>> = None;
    if let Some(coordinator) = services.delivery_coordinator.clone() {
        processes
            .subscribe_process_observer(Arc::clone(&coordinator)
                as Arc<dyn ironclaw_processes::ProcessJournalCommitObserver>)
            .map_err(|error| RebornRuntimeError::MalformedConfig {
                reason: format!("reply publication observer wiring failed: {error}"),
            })?;
        let recovery_thread_id = ThreadId::new("reply-publication-recovery").map_err(|reason| {
            RebornRuntimeError::InvalidArgument {
                reason: format!("reply publication recovery thread id: {reason}"),
            }
        })?;
        // `/outbound` is a per-user mount: the sweep must carry the deployment
        // actor as owner; runs owned by other users resume via the journal.
        let recovery_scope = TurnScope::new_with_owner(
            thread_scope.tenant_id.clone(),
            Some(thread_scope.agent_id.clone()),
            thread_scope.project_id.clone(),
            recovery_thread_id,
            thread_scope.owner_user_id.clone(),
        );
        // One-shot attempts a crash left `Sending` become `Unknown` (never
        // resent). That needs sole ownership of the outbound store, which only
        // a deployment on its own local storage root has; shared stores leave
        // them `Sending`.
        if deployment.uses_local_filesystem_storage() {
            match coordinator
                .recover_interrupted_deliveries(recovery_scope.clone())
                .await
            {
                Ok(recovered) if recovered > 0 => {
                    tracing::debug!(recovered, "marked interrupted deliveries unknown at boot")
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::debug!(%error, "interrupted delivery recovery failed at boot; attempts stay sending")
                }
            }
        }
        reply_publication_recovery = Some(tokio::spawn(async move {
            match coordinator.resume_reply_publications(&recovery_scope).await {
                Ok(resumed) if resumed > 0 => {
                    tracing::debug!(resumed, "resumed open reply publications at boot")
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::debug!(%error, "reply publication boot resume failed; open publications wait for the next terminal signal")
                }
            }
        }));
    }
    let channel_host_assembly = started_channel_host.map(|started| started.assembly);

    // Forward-migrate pre-removal routines that still carry a stored delivery
    // target: rewrite the route into the routine's prompt (the only place a
    // fire can still act on it) and clear the field. Run immediately before an
    // enabled poller starts. Target metadata enriches the prompt when its
    // registry is available; the durable target id remains actionable without
    // it. Idempotent, so every later enabled boot is a no-op.
    if trigger_poller.enabled {
        crate::automation::trigger_delivery_migration::migrate_trigger_delivery_targets_at_boot(
            trigger_repository.as_ref(),
            outbound_delivery_target_registry.as_deref(),
            &thread_scope.tenant_id,
        )
        .await
        .map_err(|error| RebornRuntimeError::MalformedConfig {
            reason: format!("stored trigger delivery-target migration failed: {error}"),
        })?;
    }

    // `trigger_poller_handle` and the test-support
    // `trigger_conversation_pairing_value` are produced from the same
    // `trigger_poller.enabled` branch. Avoid a `let mut … = None` sentinel
    // pattern flagged by code review (review f-ptr-3).
    #[cfg(any(test, feature = "test-support"))]
    let trigger_conversation_pairing_value: Option<
        Arc<dyn ironclaw_conversations::ConversationActorPairingService>,
    >;
    let trigger_poller_handle: Option<TriggerPollerRuntimeHandle> = if trigger_poller.enabled {
        // Fire-time authorizer: an explicit override wins (tests/advanced),
        // otherwise build one from the deployment's `TriggerFireAccessPolicy`
        // (arch-simplification §4.4 — the former `local_trigger_access` store is
        // now a config value, not a per-deployment store type). Grants are
        // OR-combined, preserving the union the single store expressed.
        let mut grant_checkers: Vec<Arc<dyn TriggerFireAccessChecker>> = Vec::new();
        for grant in trigger_fire_access.grants() {
            match grant {
                TriggerFireAccessGrant::StaticOwner {
                    owner,
                    agent,
                    project,
                } => {
                    let checker: Arc<dyn TriggerFireAccessChecker> =
                        Arc::new(StaticOwnerTriggerFireChecker::new(
                            thread_scope.tenant_id.clone(),
                            owner.clone(),
                            agent.clone(),
                            project.clone(),
                        ));
                    grant_checkers.push(checker);
                }
                TriggerFireAccessGrant::TenantMembership { agent, project } => {
                    // Membership is resolved against the canonical identity
                    // directory the SSO login path populates — the same store
                    // `reborn_user_directory` opens, built here from the same
                    // configured scoped filesystem that backs the final runtime.
                    let directory = poller_user_directory(
                        Arc::clone(&scoped_filesystem),
                        &thread_scope.tenant_id,
                        &actor_user_id,
                        &thread_scope.agent_id,
                        thread_scope.project_id.as_ref(),
                    );
                    let checker: Arc<dyn TriggerFireAccessChecker> =
                        Arc::new(IdentityMembershipTriggerFireChecker::new(
                            directory,
                            thread_scope.tenant_id.clone(),
                            agent.clone(),
                            project.clone(),
                        ));
                    grant_checkers.push(checker);
                }
            }
        }
        let policy_checker: Option<Arc<dyn TriggerFireAccessChecker>> = if grant_checkers.len() <= 1
        {
            grant_checkers.into_iter().next()
        } else {
            let composite: Arc<dyn TriggerFireAccessChecker> =
                Arc::new(CompositeTriggerFireChecker::new(grant_checkers));
            Some(composite)
        };
        let effective_trigger_fire_access_checker =
            trigger_fire_access_checker.clone().or(policy_checker);
        validate_trigger_poller_authorization(
            &trigger_poller,
            effective_trigger_fire_access_checker.as_ref(),
        )?;
        let conversation_services =
            if let Some(conversation_services) = trigger_conversation_services.clone() {
                conversation_services
            } else {
                RebornFilesystemConversationServices::new(Arc::clone(&scoped_filesystem))
                    .await
                    .map_err(|error| RebornRuntimeError::InvalidArgument {
                        reason: format!("trigger conversation services unavailable: {error}"),
                    })?
            };
        let trigger_poller_services = build_trigger_poller_services(
            conversation_services,
            Arc::clone(&planned_turn_coordinator),
            Arc::clone(&thread_service),
            trigger_poller.authorizer,
            effective_trigger_fire_access_checker.clone(),
            thread_scope.tenant_id.clone(),
            validated_identity.agent_id.clone(),
        )?;
        let active_run_lookup =
            build_trigger_active_run_lookup(Arc::clone(&process_lifecycle_lookup_source));
        let trigger_repository = trigger_repository.clone();
        #[cfg(any(test, feature = "test-support"))]
        {
            trigger_conversation_pairing_value =
                Some(Arc::clone(&trigger_poller_services.pairing_service));
        }
        let hook_slot = Arc::clone(&trigger_poller_services.post_submit_hook_slot);
        // Install the settlement hook before the poller task can observe a due
        // fire. `tokio::spawn` may schedule on another runtime worker
        // immediately, so late binding after `spawn_trigger_poller` creates a
        // startup race. The observer still waits defensively for test/custom
        // assembly seams, but production never needs an in-memory handoff.
        // ONE background-run notifier for every channel extension, built by
        // the SAME product-side workflow factory the channel host graphs are
        // built by (§12.11 D-A). It decodes each stored notification target
        // through the assembly's LIVE codec view, so a channel activated after
        // boot still decodes its own targets. Channel egress stays optional.
        let notifier = match (
            channel_host_assembly.as_ref(),
            channel_workflow_factory.as_ref(),
        ) {
            (Some(assembly), Some(workflow_factory)) => workflow_factory.background_run_notifier(
                Arc::new(
                ironclaw_extension_host::channel_triggered_delivery::AssemblyPreferenceTargetCodecs::new(
                    Arc::clone(assembly),
                ),
                ),
            ),
            _ => None,
        };
        // Inbox publication must not depend on channel-host assembly. The
        // durable store is a core runtime service, so install this publisher
        // even in web-only/custom deployments with no channel egress.
        let pre_submit_failure_notifier = Some(Arc::new(
            ironclaw_assistant::PreSubmitFailureInboxNotifier::new(Arc::clone(
                &services.notification_inbox,
            )),
        )
            as Arc<dyn ironclaw_outbound::TriggeredRunDelivery>);
        let generic_trigger_hook: Arc<
            dyn crate::automation::trigger_poller::PostSubmitDeliveryHook,
        > = Arc::new(
            ironclaw_extension_host::channel_triggered_delivery::GenericTriggeredRunDeliveryHook::new(
                notifier,
                pre_submit_failure_notifier,
                Arc::clone(&services.triggered_run_delivery),
            ),
        );
        if hook_slot.set(generic_trigger_hook).is_err() {
            tracing::debug!(
                "generic triggered-run delivery hook slot was already occupied; keeping the first hook"
            );
        }
        spawn_trigger_poller(
            trigger_poller,
            TriggerPollerCompositionDeps {
                repository: trigger_repository,
                materializer: trigger_poller_services.materializer,
                trusted_submitter: trigger_poller_services.trusted_submitter,
                active_run_lookup,
                manual_fire_runner: Arc::clone(&services.trigger_manual_fire_runner),
                post_submit_hook_slot: hook_slot,
            },
        )
        .map_err(|error| RebornRuntimeError::InvalidArgument {
            reason: format!("trigger poller could not be started: {error}"),
        })?
    } else {
        #[cfg(any(test, feature = "test-support"))]
        {
            trigger_conversation_pairing_value = None;
        }
        None
    };

    let scheduler_notifier = composition.scheduler_handle.wake_notifier();

    #[cfg(any(test, feature = "test-support"))]
    let interaction_service_test_parts = local_runtime.zip(builtin_capability_policy.as_ref()).map(
        |(local_runtime, builtin_capability_policy)| InteractionServiceTestParts {
            approval_requests: Arc::clone(&local_runtime.approval_requests),
            capability_leases: Arc::clone(&local_runtime.capability_leases),
            extension_registry: Arc::clone(&local_runtime.extension_registry),
            workspace_mounts: local_runtime.workspace_mounts.clone(),
            memory_mounts: local_runtime.memory_mounts.clone(),
            system_extensions_lifecycle_mounts: local_runtime
                .system_extensions_lifecycle_mounts
                .clone(),
            persistent_approval_policies: Arc::clone(&local_runtime.persistent_approval_policies),
            tool_permission_overrides: Arc::clone(&local_runtime.tool_permission_overrides),
            extension_management: Arc::clone(&local_runtime.extension_management),
            skill_management: Arc::clone(&local_runtime.skill_management),
            admin_configuration_resolver: Arc::clone(&local_runtime.channel_config_service),
            product_auth: Arc::clone(&local_runtime.product_auth),
            builtin_capability_policy: Arc::clone(builtin_capability_policy),
        },
    );

    // Spawn the engine-owned credential keepalive sweep (B4;
    // `ironclaw_auth::keepalive`). The factory reports whether the durable
    // candidate source, recipe data, leader lock, and refresh port are ready
    // together. Standalone and override paths report `Absent`; the `enabled`
    // policy flag still gates the actual spawn inside `spawn_keepalive_sweep`.
    let credential_refresh_worker_handle = match std::mem::replace(
        &mut services.credential_refresh_worker,
        crate::factory::CredentialRefreshWorkerReady::Absent,
    ) {
        crate::factory::CredentialRefreshWorkerReady::Ready {
            candidate_source,
            recipes,
            leader_lock,
            refresh_port,
        } => ironclaw_auth::spawn_keepalive_sweep(
            credential_refresh,
            ironclaw_auth::KeepaliveSweepDeps {
                candidates: candidate_source,
                recipes,
                refresh: refresh_port as std::sync::Arc<dyn ironclaw_auth::KeepaliveRefreshPort>,
                leader_lock: std::sync::Arc::new(leader_lock),
            },
        ),
        crate::factory::CredentialRefreshWorkerReady::Absent => None,
    };
    let trace_flush_worker =
        ironclaw_trace_commons::capture::spawn_trace_queue_flush_worker(trace_capture_scopes);
    // Scheduler is running (started inside build_default_planned_runtime); mark readiness.
    services.readiness.workers.turn_runner = true;
    services.readiness.workers.trigger_poller = trigger_poller_handle.is_some();
    let turn_coordinator = planned_turn_coordinator;

    // Spawn the budget-event projection task as the production owner
    // of the broadcast sink — review feedback Thermo-Nuclear #3
    // (#3841 follow-up A2). The runtime's `broadcast_budget_event_sink`
    // accessor used to expose a sink that no one subscribed to; with
    // this projection the runtime always has at least the tracing
    // observer attached, and callers can install a richer observer
    // (SSE projection, telemetry export) through
    // `RebornRuntimeInput::with_budget_event_observer`.
    let budget_event_projection = Some({
        let observer = budget_event_observer.unwrap_or_else(|| {
            Arc::new(crate::observability::budget_events::TracingBudgetEventObserver)
                as Arc<dyn crate::BudgetEventObserver>
        });
        crate::observability::budget_events::BudgetEventProjection::spawn(
            broadcast_budget_event_sink.as_ref(),
            observer,
        )
    });

    // Apply the effective LLM config (config.toml/env selection + any stored
    // key) to the placeholder gateway exactly once, via the same live-reload
    // path the settings UI uses (the shared `live_llm_default`). Failure
    // degrades like a boot with no LLM configured: placeholder stays wired,
    // operator retries through Settings -> Inference without a restart.
    // Through the shared live seam, so the slot it applies is the baseline a
    // later `config.toml` reload diffs against.
    if let Some(live) = live_llm_default.as_ref()
        && let Err(error) = ironclaw_operator::LlmReloadTrigger::reload(live.as_ref()).await
    {
        tracing::warn!(
            %error,
            "boot-time LLM reload failed; the placeholder provider stays active until the \
             next successful reload (e.g. through Settings -> Inference)"
        );
    }

    let ironhub_link_state = Arc::clone(&services.ironhub_link_state);
    let ironhub_link_service = match ironhub_agent_shared_key {
        Some(shared_key) => {
            let egress = services.runtime_http_egress.clone().ok_or_else(|| {
                RebornRuntimeError::MalformedConfig {
                    reason:
                        "IronHub gateway key was configured but mediated HTTP egress is unavailable"
                            .to_string(),
                }
            })?;
            let service = ironclaw_extension_manager::ironhub::RebornIronhubLinkService::new(
                services.skill_management.clone(),
                services.extension_management.clone(),
                egress,
                Arc::clone(&ironhub_link_state),
                shared_key,
            )
            .map_err(|error| RebornRuntimeError::MalformedConfig {
                reason: error.to_string(),
            })?
            .with_manifest_url(ironhub_manifest_url.clone());
            Some(Arc::new(service)
                as Arc<
                    dyn ironclaw_product_contracts::ironhub::IronhubLinkService,
                >)
        }
        None => None,
    };

    let runtime = RebornRuntime {
        memory_page,
        host_runtime: services.host_runtime.clone(),
        user_sandbox_process_port: services.user_sandbox_process_port.clone(),
        product_auth: services.product_auth.clone(),
        readiness: services.readiness.clone(),
        skill_management: services.skill_management.clone(),
        extension_lifecycle_surface_context: services.extension_lifecycle_surface_context.clone(),
        secret_store: Arc::clone(&services.secret_store),
        scoped_filesystem,
        suggestions_store,
        llm_config_service,
        admin_secret_provisioner,
        project_service,
        diagnostic_store,
        trigger_repository: trigger_repository.clone(),
        orchestrator: services.orchestrator.clone(),
        cli_bridge: services.cli_bridge.clone(),
        workspace_host_root: services.workspace_host_root.clone(),
        computer_use,
        trigger_manual_fire_runner: services.trigger_manual_fire_runner.clone(),
        #[cfg(any(test, feature = "test-support"))]
        trigger_process_lifecycle_source: Arc::clone(&services.trigger_process_lifecycle_source),
        broadcast_budget_event_sink,
        external_tool_catalog: services.external_tool_catalog.clone(),
        embedding_provider: services.embedding_provider.clone(),
        persistent_approval_policies: Arc::clone(&services.persistent_approval_policies),
        tool_permission_overrides: services.tool_permission_overrides.clone(),
        auto_approve_settings: services.auto_approve_settings.clone(),
        extension_registry: services.extension_registry.clone(),
        shared_extension_registry: services.shared_extension_registry.clone(),
        skill_auto_activate_learned: Arc::clone(&services.skill_auto_activate_learned),
        extension_management: services.extension_management.clone(),
        runtime_http_egress: services.runtime_http_egress.as_ref().map(Arc::clone),
        ironhub_link_state,
        ironhub_manifest_url,
        ironhub_link_service,
        owner_user_id: services.owner_user_id.clone(),
        extension_filesystem: services.extension_filesystem.clone(),
        session_inbound_ledger,
        session_channel_directory,
        session_channel_extension_id,
        reply_publication_recovery,
        workspace_mount_policy: services.workspace_mounts.clone(),
        system_extensions_lifecycle_mounts: services.system_extensions_lifecycle_mounts.clone(),
        outbound_preferences: services.outbound_preferences.clone(),
        notification_inbox: services.notification_inbox.clone(),
        #[cfg(any(test, feature = "test-support"))]
        outbound_state: OutboundTestStores {
            state: services.outbound_state.clone(),
            reply_attachment_intents: services.reply_attachment_intents.clone(),
        },
        #[cfg(any(test, feature = "test-support"))]
        triggered_run_delivery: services.triggered_run_delivery.clone(),
        #[cfg(any(test, feature = "test-support"))]
        delivered_gate_routes: services.delivered_gate_routes.clone(),
        delivery_coordinator: services.delivery_coordinator.clone(),
        channel_facade_slot: services.channel_disconnect_slot.clone(),
        channel_config_service: services.channel_config_service.clone(),
        admin_configuration: services.admin_configuration.clone(),
        admin_configuration_uses: services.admin_configuration_uses.clone(),
        channel_identity_store: services.channel_identity_store.clone(),
        channel_dm_target_store: services.channel_dm_target_store.clone(),
        extension_ingress: services.extension_ingress.clone(),
        #[cfg(any(test, feature = "test-support"))]
        deployment_channels: services.deployment_channels.clone(),
        channel_pairing: services.channel_pairing.clone(),
        channel_delivery_resolver: services.channel_delivery_resolver.clone(),
        delivery_registrations: services.delivery_registrations.clone(),
        delivery_client_bootstrap: services.delivery_client_bootstrap.clone(),
        #[cfg(feature = "test-support")]
        channel_egress_credential_bridges: services.channel_egress_credential_bridges.clone(),
        turn_coordinator,
        _channel_host_assembly: channel_host_assembly,
        channel_workflow_factory: channel_workflow_factory.clone(),
        _process_gate_query_source: process_gate_query_source,
        turn_tree_store: turn_projection,
        thread_service,
        subagent_delivery: SubagentDeliveryRuntime {
            input_queue: host_input_queue,
            _resolver: subagent_await_edge_resolver,
            _store: subagent_await_edge_store,
        },
        thread_scope,
        turn_scheduler: RuntimeTurnScheduler::new(composition.scheduler_handle, scheduler_notifier),
        trigger_poller_handle,
        credential_refresh_worker_handle,
        trace_flush_worker,
        skill_learning_extraction_tasks,
        #[cfg(any(test, feature = "test-support"))]
        trigger_conversation_pairing: trigger_conversation_pairing_value,
        outbound_delivery_target_registry,
        budget_event_projection,
        poll_settings: poll,
        admin_api_token_minter,
        process_lifecycle_lookup_source,
        actor_user_id,
        source_binding_ref: validated_identity.source_binding_ref,
        reply_target_binding_ref: validated_identity.reply_target_binding_ref,
        projection_services,
        approval_interaction_service,
        auth_interaction_service,
        #[cfg(any(test, feature = "test-support"))]
        interaction_service_test_parts,
        runtime_event_sink,
        default_run_profile_id,
        send_locks: Mutex::new(HashMap::new()),
        #[cfg(feature = "test-support")]
        skill_context_source: runtime_skill_context_source,
        skill_activation_source,
        skill_execution_adapter,
        tool_mentions,
        boot,
        llm_reload,
        live_llm_default,
    };
    // Channel graphs begin reconciling before the canonical product surface
    // can exist. Fill their first-write-wins command handle only after the
    // runtime is complete, using the same surface exposed to WebUI and other
    // product callers.
    if let Some(assembly) = runtime._channel_host_assembly.as_ref() {
        let command_surface = runtime.product_surface(None)?;
        let _ = assembly.set_product_command_surface(command_surface);
    }
    // Fill the composition's late-bound channel-connection facade slot (§6.4)
    // now the runtime's serving tenant is known: extension removal
    // (`ExtensionManagementPort::remove`) disconnects the caller's
    // channel identity through this facade, and the identity-binding write
    // hook is only reachable from runtime-backed compositions — so filling
    // here keeps "wherever a binding can be written, removal disconnects it".
    // First write wins by `OnceLock` contract: a test bundle that filled the
    // slot before the runtime was built keeps its facade (same stores, same
    // durable state), so the discarded `set` result is deliberate.
    if let Some(channel_connection) = runtime.generic_channel_connection_facade() {
        #[expect(
            clippy::let_underscore_must_use,
            reason = "OnceLock first write wins; a pre-filled test facade is kept"
        )]
        let _ = runtime.channel_facade_slot.set(channel_connection);
    }
    Ok((runtime, resource_governor))
}

/// Overlay the stored LLM key (if any) onto a clone of `llm`, scoped to
/// feeding [`bootstrap_nearai_mcp_from_effective_llm`]'s `api_key` presence
/// check (it inspects the config directly, not the live provider). NOT the
/// general "stored key -> live provider" mechanism — that's
/// the boot apply through `ironclaw_operator::LiveLlmDefault::reload` (which
/// drives `RebornLlmReloadAdapter`), invoked once after boot construction.
async fn overlay_stored_llm_key_for_nearai_mcp_bootstrap(
    llm: Option<ResolvedRebornLlm>,
    services: &RebornRuntimeStores,
) -> Result<Option<ResolvedRebornLlm>, RebornRuntimeError> {
    let Some(mut llm) = llm else {
        return Ok(None);
    };

    let keys = ironclaw_operator::LlmKeyStore::new(crate::RuntimeOperatorSecretValueStore::shared(
        Arc::clone(&services.secret_store),
    ));
    if let Some(stored) = keys
        .read(llm.provider_id())
        .await
        .map_err(|error| RebornRuntimeError::LlmProvider(error.to_string()))?
    {
        ironclaw_operator::apply_stored_api_key(llm.config_mut(), stored);
    }

    Ok(Some(llm))
}

async fn bootstrap_nearai_mcp_from_effective_llm(
    services: &RebornRuntimeStores,
    llm: Option<&ResolvedRebornLlm>,
    owner_scope: ResourceScope,
) -> Result<(), RebornRuntimeError> {
    let Some(llm) = llm else {
        return Ok(());
    };
    let Some(config) =
        ironclaw_operator::llm_admin::nearai_mcp::nearai_mcp_bootstrap_config_from_llm_config(
            llm.config(),
        )
        .await
        .map_err(|error| RebornRuntimeError::InvalidArgument {
            reason: format!("NEAR AI MCP bootstrap config: {error}"),
        })?
    else {
        return Ok(());
    };
    if let Err(error) = config.endpoint() {
        tracing::debug!(
            %error,
            "NEAR AI MCP auto-bootstrap skipped because the resolved LLM endpoint is not MCP-compatible"
        );
        return Ok(());
    }
    let extension_management = &services.extension_management;
    let outcome = crate::llm_admin::nearai_mcp::bootstrap_nearai_mcp(
        Some(config),
        &services.product_auth,
        extension_management,
        owner_scope,
    )
    .await
    .map_err(|error| RebornRuntimeError::InvalidArgument {
        reason: format!("NEAR AI MCP bootstrap from LLM config failed: {error}"),
    })?;
    outcome.log_completion();
    Ok(())
}
