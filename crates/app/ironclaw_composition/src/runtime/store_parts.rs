//! Destructures the configured substrate stores into the handles the runtime
//! assembly consumes (`RuntimeStoreParts`).

use super::*;

pub(super) struct RuntimeStoreParts {
    pub(super) scoped_filesystem: Arc<ScopedFilesystem<CompositeRootFilesystem>>,
    pub(super) turn_projection: Arc<AgentTurnProcessRuntime>,
    pub(super) processes: ProcessRuntimeSystem,
    pub(super) loop_checkpoint_store: Arc<dyn ironclaw_turns::LoopCheckpointStore>,
    pub(super) thread_service: Arc<dyn SessionThreadService>,
    pub(super) event_log: Arc<dyn DurableEventLog>,
    pub(super) runtime_event_sink: Arc<dyn NonBlockingEventSink>,
    pub(super) audit_log: Arc<dyn DurableAuditLog>,
    pub(super) resource_governor: Arc<dyn ironclaw_resources::ResourceGovernor>,
    pub(super) budget_gate_store: Arc<dyn ironclaw_resources::BudgetGateStorePort>,
    pub(super) broadcast_budget_event_sink: Arc<ironclaw_resources::BroadcastBudgetEventSink>,
    /// §3 replacement for `subagent_gate_store`: built here (not later, once
    /// `capability_result_writer` becomes available) because `F` (the
    /// filesystem backend generic) is only nameable while the configured graph
    /// is being destructured. By the time the shared caller consumes
    /// `RuntimeStoreParts`, everything is type-erased. The resolver's result
    /// writer isn't ready yet at this point either, so it's bound later via
    /// `AwaitEdgeSettler::bind_result_writer` (a deferred-binding trait method
    /// mirroring `bind_coordinator`).
    pub(super) subagent_await_edge_writer: Arc<dyn AwaitEdgeWriter>,
    pub(super) subagent_await_edge_resolver: Arc<AwaitEdgeResolver<dyn SessionThreadService>>,
    pub(super) subagent_await_edge_store: Arc<AwaitEdgeStore>,
    pub(super) subagent_await_edge_evidence: Arc<dyn AwaitDependentRunEvidenceStore>,
    pub(super) trigger_repository: Arc<dyn ironclaw_triggers::TriggerRepository>,
    /// Process lifecycle source for trigger active-run lookup. Every substrate
    /// now provides the same typed process-journal projection.
    pub(super) admin_secret_provisioner: Arc<dyn ironclaw_assistant::AdminSecretProvisioner>,
    pub(super) project_service:
        Arc<dyn ironclaw_product_contracts::project_service::ProjectService>,
    pub(super) trigger_conversation_services: Option<RebornFilesystemConversationServices>,
}

pub(super) fn runtime_store_parts(services: &RebornRuntimeStores) -> RuntimeStoreParts {
    let scoped_filesystem = Arc::clone(&services.scoped_filesystem);
    let thread_service = Arc::clone(&services.thread_service);
    let resource_governor = Arc::clone(&services.resource_governor);
    let budget_gate_store = Arc::clone(&services.budget_gate_store);
    let broadcast_budget_event_sink = Arc::clone(&services.broadcast_budget_event_sink);
    let event_log = Arc::clone(&services.event_log);
    let runtime_event_sink = Arc::clone(&services.runtime_event_sink);
    let audit_log = Arc::clone(&services.audit_log);
    let admin_secret_provisioner = Arc::clone(&services.admin_secret_provisioner);
    let project_service = Arc::clone(&services.project_service);

    let processes = services.processes.clone();
    let turn_projection = Arc::new(processes.agent_turn_runtime());
    let loop_checkpoint_store = Arc::new(ironclaw_turns::ProcessLoopCheckpointStore::new(
        processes.checkpoints(),
    )) as Arc<dyn ironclaw_turns::LoopCheckpointStore>;

    let (
        subagent_await_edge_writer,
        subagent_await_edge_resolver,
        subagent_await_edge_evidence,
        subagent_await_edge_store,
    ) = {
        let store = Arc::new(AwaitEdgeStore::new(processes.dependencies()));
        let resolver = Arc::new(AwaitEdgeResolver::new_unbound_deferred_result_writer(
            Arc::clone(&store),
            Arc::clone(&turn_projection) as Arc<dyn ironclaw_turns::AgentTurnSpawnTreeRuntimePort>,
            Arc::clone(&thread_service),
        ));
        let driver = Arc::new(ScopeRecoveryDriver::new(
            Arc::clone(&resolver),
            Arc::clone(&store),
        ));
        (
            driver as Arc<dyn AwaitEdgeWriter>,
            resolver,
            Arc::clone(&store) as Arc<dyn AwaitDependentRunEvidenceStore>,
            store,
        )
    };

    RuntimeStoreParts {
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
        trigger_repository: Arc::clone(&services.trigger_repository),
        admin_secret_provisioner,
        project_service,
        trigger_conversation_services: Some(services.trigger_conversation_services.clone()),
    }
}
