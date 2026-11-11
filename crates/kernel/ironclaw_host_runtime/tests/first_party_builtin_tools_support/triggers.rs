use super::*;

pub(crate) fn trigger_execution_contract(goal: impl Into<String>) -> Value {
    json!({
        "version": 1,
        "goal": goal.into(),
        "success_criteria": ["Complete the requested task"],
        "output_instructions": "Return a concise result",
        "no_result_text": "No result",
        "policy": { "result_delivery": "deliver" }
    })
}

pub(crate) fn runtime_with_trigger_repository(
    repository: Arc<dyn TriggerRepository>,
) -> impl HostRuntime {
    runtime_with_filesystem_policy_and_trigger_repository(
        DiskFilesystem::new(),
        local_host_policy(),
        repository,
    )
}

pub(crate) fn runtime_with_trigger_repository_and_manual_runner(
    trigger_repository: Arc<InMemoryTriggerRepository>,
    manual_fire_runner: Arc<dyn TriggerManualFireRunner>,
) -> impl HostRuntime {
    let trigger_create_hook = Arc::new(PersistedRecordTriggerCreateHook::new(Arc::clone(
        &trigger_repository,
    )));
    HostRuntimeServices::new(
        Arc::new(registry()),
        Arc::new(DiskFilesystem::new()),
        Arc::new(InMemoryResourceGovernor::new()),
        Arc::new(GrantAuthorizer::new()),
        ironclaw_processes::ProcessServices::in_memory(),
        CapabilitySurfaceVersion::new("surface-v1").unwrap(),
    )
    .with_first_party_capabilities(Arc::new(
        builtin_first_party_handlers_with_trigger_services(
            trigger_repository,
            trigger_create_hook,
            Arc::new(MissingTriggerActiveRunLookup),
            manual_fire_runner,
        )
        .unwrap(),
    ))
    .with_runtime_http_egress(Arc::new(RecordingRuntimeHttpEgress::default()))
    .with_audit_sink(Arc::new(InMemoryAuditSink::new()))
    .with_runtime_policy(local_host_policy())
    .with_trust_policy(Arc::new(trust_policy()))
    .host_runtime_for_local_testing()
}

pub(crate) fn runtime_with_trigger_repository_and_create_hook(
    trigger_repository: Arc<dyn TriggerRepository>,
    trigger_create_hook: Arc<dyn TriggerCreateHook>,
) -> impl HostRuntime {
    HostRuntimeServices::new(
        Arc::new(registry()),
        Arc::new(DiskFilesystem::new()),
        Arc::new(InMemoryResourceGovernor::new()),
        Arc::new(GrantAuthorizer::new()),
        ironclaw_processes::ProcessServices::in_memory(),
        CapabilitySurfaceVersion::new("surface-v1").unwrap(),
    )
    .with_first_party_capabilities(Arc::new(
        builtin_first_party_handlers_with_trigger_create_hook(
            trigger_repository,
            trigger_create_hook,
            // Test-only wiring (#5886): this helper isn't a production call
            // site, so it defaults the same `Missing`-resolving lookup the
            // bare `insert_handlers` wrapper uses.
            Arc::new(MissingTriggerActiveRunLookup),
        )
        .unwrap(),
    ))
    .with_runtime_http_egress(Arc::new(RecordingRuntimeHttpEgress::default()))
    .with_audit_sink(Arc::new(InMemoryAuditSink::new()))
    .with_runtime_policy(local_host_policy())
    .with_trust_policy(Arc::new(trust_policy()))
    .host_runtime_for_local_testing()
}

#[cfg(feature = "test-support")]
pub(crate) fn runtime_with_trigger_repository_and_clock(
    trigger_repository: Arc<dyn TriggerRepository>,
    trigger_clock: Arc<dyn TriggerManagementClock>,
) -> impl HostRuntime {
    HostRuntimeServices::new(
        Arc::new(registry()),
        Arc::new(DiskFilesystem::new()),
        Arc::new(InMemoryResourceGovernor::new()),
        Arc::new(GrantAuthorizer::new()),
        ironclaw_processes::ProcessServices::in_memory(),
        CapabilitySurfaceVersion::new("surface-v1").unwrap(),
    )
    .with_first_party_capabilities(Arc::new(
        builtin_first_party_handlers_with_trigger_clock(trigger_repository, trigger_clock).unwrap(),
    ))
    .with_runtime_http_egress(Arc::new(RecordingRuntimeHttpEgress::default()))
    .with_audit_sink(Arc::new(InMemoryAuditSink::new()))
    .with_runtime_policy(local_host_policy())
    .with_trust_policy(Arc::new(trust_policy()))
    .host_runtime_for_local_testing()
}

pub(crate) fn runtime_with_filesystem_policy_and_trigger_repository<F>(
    filesystem: F,
    policy: EffectiveRuntimePolicy,
    trigger_repository: Arc<dyn TriggerRepository>,
) -> impl HostRuntime
where
    F: RootFilesystem + 'static,
{
    HostRuntimeServices::new(
        Arc::new(registry()),
        Arc::new(filesystem),
        Arc::new(InMemoryResourceGovernor::new()),
        Arc::new(GrantAuthorizer::new()),
        ironclaw_processes::ProcessServices::in_memory(),
        CapabilitySurfaceVersion::new("surface-v1").unwrap(),
    )
    .with_first_party_capabilities(Arc::new({
        // Local-testing analog of the composition registration: builtin
        // handlers + the bound (native) memory provider's registry-routed
        // tools.
        let mut handlers = builtin_first_party_handlers(trigger_repository).unwrap();
        register_native_memory_tools(&mut handlers).unwrap();
        handlers
    }))
    .with_runtime_http_egress(Arc::new(RecordingRuntimeHttpEgress::default()))
    .with_audit_sink(Arc::new(InMemoryAuditSink::new()))
    .with_runtime_policy(policy)
    .with_trust_policy(Arc::new(trust_policy()))
    .host_runtime_for_local_testing()
}

pub(crate) struct PersistedRecordTriggerCreateHook {
    pub(crate) repository: Arc<InMemoryTriggerRepository>,
    pub(crate) records: std::sync::Mutex<Vec<TriggerRecord>>,
}

impl PersistedRecordTriggerCreateHook {
    pub(crate) fn new(repository: Arc<InMemoryTriggerRepository>) -> Self {
        Self {
            repository,
            records: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn records(&self) -> Vec<TriggerRecord> {
        self.records.lock().unwrap().clone()
    }
}

#[async_trait]
impl TriggerCreateHook for PersistedRecordTriggerCreateHook {
    async fn validate_execution_policy(
        &self,
        _scope: &ResourceScope,
        _policy: &TurnExecutionPolicy,
    ) -> Result<(), TriggerError> {
        // Accept-all: this double pins the after-persist path only.
        Ok(())
    }

    async fn after_trigger_persisted(&self, record: &TriggerRecord) -> Result<(), TriggerError> {
        let persisted = self
            .repository
            .get_trigger(record.tenant_id.clone(), record.trigger_id)
            .await
            .expect("trigger lookup succeeds");
        assert_eq!(
            persisted.as_ref().map(|record| record.trigger_id),
            Some(record.trigger_id)
        );
        self.records.lock().unwrap().push(record.clone());
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct FailingTriggerCreateHook;

#[async_trait]
impl TriggerCreateHook for FailingTriggerCreateHook {
    async fn validate_execution_policy(
        &self,
        _scope: &ResourceScope,
        _policy: &TurnExecutionPolicy,
    ) -> Result<(), TriggerError> {
        // Accept-all: this double pins the after-persist failure path.
        Ok(())
    }

    async fn after_trigger_persisted(&self, _record: &TriggerRecord) -> Result<(), TriggerError> {
        Err(TriggerError::Backend {
            reason: "hook unavailable".to_string(),
        })
    }
}

#[derive(Debug)]
#[cfg(feature = "test-support")]
pub(crate) struct FixedTriggerClock(pub(crate) DateTime<Utc>);

#[cfg(feature = "test-support")]
impl TriggerManagementClock for FixedTriggerClock {
    fn now(&self) -> DateTime<Utc> {
        self.0
    }
}

pub(crate) struct FailingTriggerRepository;

#[derive(Default)]
pub(crate) struct RemoveFailingTriggerRepository {
    pub(crate) inner: InMemoryTriggerRepository,
    pub(crate) remove_attempts: std::sync::Mutex<usize>,
}

#[derive(Default)]
pub(crate) struct BatchRunHistoryFailingTriggerRepository {
    pub(crate) inner: InMemoryTriggerRepository,
}

impl RemoveFailingTriggerRepository {
    pub(crate) fn remove_attempts(&self) -> usize {
        *self.remove_attempts.lock().unwrap()
    }
}

pub(crate) fn trigger_backend_error() -> ironclaw_triggers::TriggerError {
    ironclaw_triggers::TriggerError::Backend {
        reason: "test backend failure".to_string(),
    }
}

#[async_trait]
impl TriggerRepository for RemoveFailingTriggerRepository {
    async fn find_trigger_run_by_thread_id(
        &self,
        _tenant_id: TenantId,
        _thread_id: &ThreadId,
    ) -> Result<Option<(TriggerRecord, TriggerRunRecord)>, TriggerError> {
        // Trigger-thread lookup is not exercised by this fake.
        Ok(None)
    }

    async fn upsert_trigger(
        &self,
        record: ironclaw_triggers::TriggerRecord,
    ) -> Result<(), ironclaw_triggers::TriggerError> {
        self.inner.upsert_trigger(record).await
    }

    async fn get_trigger(
        &self,
        tenant_id: TenantId,
        trigger_id: ironclaw_triggers::TriggerId,
    ) -> Result<Option<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        self.inner.get_trigger(tenant_id, trigger_id).await
    }

    async fn list_triggers(
        &self,
        tenant_id: TenantId,
    ) -> Result<Vec<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        self.inner.list_triggers(tenant_id).await
    }

    async fn list_scoped_triggers(
        &self,
        tenant_id: TenantId,
        creator_user_id: UserId,
        agent_id: Option<AgentId>,
        project_id: Option<ProjectId>,
        limit: usize,
        excluded_states: &[ironclaw_triggers::TriggerState],
    ) -> Result<Vec<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        self.inner
            .list_scoped_triggers(
                tenant_id,
                creator_user_id,
                agent_id,
                project_id,
                limit,
                excluded_states,
            )
            .await
    }

    async fn remove_trigger(
        &self,
        _tenant_id: TenantId,
        _trigger_id: ironclaw_triggers::TriggerId,
    ) -> Result<Option<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        *self.remove_attempts.lock().unwrap() += 1;
        Err(trigger_backend_error())
    }

    async fn remove_scoped_trigger(
        &self,
        tenant_id: TenantId,
        creator_user_id: UserId,
        agent_id: Option<AgentId>,
        project_id: Option<ProjectId>,
        trigger_id: ironclaw_triggers::TriggerId,
    ) -> Result<Option<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        self.inner
            .remove_scoped_trigger(tenant_id, creator_user_id, agent_id, project_id, trigger_id)
            .await
    }

    async fn set_scoped_trigger_state(
        &self,
        _tenant_id: TenantId,
        _creator_user_id: UserId,
        _agent_id: Option<AgentId>,
        _project_id: Option<ProjectId>,
        _trigger_id: ironclaw_triggers::TriggerId,
        _state: ironclaw_triggers::TriggerState,
    ) -> Result<Option<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        Err(trigger_backend_error())
    }

    async fn list_due_triggers(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> Result<Vec<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        self.inner.list_due_triggers(now, limit).await
    }

    async fn list_active_triggers(
        &self,
        limit: usize,
    ) -> Result<Vec<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        self.inner.list_active_triggers(limit).await
    }

    async fn list_active_triggers_after(
        &self,
        after: Option<ironclaw_triggers::ActiveTriggerScanCursor>,
        limit: usize,
    ) -> Result<Vec<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        self.inner.list_active_triggers_after(after, limit).await
    }

    async fn claim_due_fire(
        &self,
        request: ironclaw_triggers::ClaimDueFireRequest,
    ) -> Result<ironclaw_triggers::ClaimDueFireOutcome, ironclaw_triggers::TriggerError> {
        self.inner.claim_due_fire(request).await
    }

    async fn mark_fire_accepted(
        &self,
        request: ironclaw_triggers::FireAcceptedRequest,
    ) -> Result<Option<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        self.inner.mark_fire_accepted(request).await
    }

    async fn mark_fire_replayed(
        &self,
        request: ironclaw_triggers::FireReplayedRequest,
    ) -> Result<Option<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        self.inner.mark_fire_replayed(request).await
    }

    async fn mark_fire_retryable_failed(
        &self,
        request: ironclaw_triggers::FireRetryableFailedRequest,
    ) -> Result<Option<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        self.inner.mark_fire_retryable_failed(request).await
    }

    async fn mark_fire_permanently_failed(
        &self,
        request: ironclaw_triggers::FirePermanentFailedRequest,
    ) -> Result<Option<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        self.inner.mark_fire_permanently_failed(request).await
    }

    async fn mark_fire_terminally_failed(
        &self,
        request: ironclaw_triggers::FireTerminalFailedRequest,
    ) -> Result<Option<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        self.inner.mark_fire_terminally_failed(request).await
    }

    async fn clear_active_fire(
        &self,
        request: ironclaw_triggers::ClearActiveFireRequest,
    ) -> Result<Option<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        self.inner.clear_active_fire(request).await
    }
}

#[async_trait]
impl TriggerRepository for BatchRunHistoryFailingTriggerRepository {
    async fn find_trigger_run_by_thread_id(
        &self,
        _tenant_id: TenantId,
        _thread_id: &ThreadId,
    ) -> Result<Option<(TriggerRecord, TriggerRunRecord)>, TriggerError> {
        // Trigger-thread lookup is not exercised by this fake.
        Ok(None)
    }

    async fn upsert_trigger(
        &self,
        record: ironclaw_triggers::TriggerRecord,
    ) -> Result<(), ironclaw_triggers::TriggerError> {
        self.inner.upsert_trigger(record).await
    }

    async fn get_trigger(
        &self,
        tenant_id: TenantId,
        trigger_id: ironclaw_triggers::TriggerId,
    ) -> Result<Option<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        self.inner.get_trigger(tenant_id, trigger_id).await
    }

    async fn list_triggers(
        &self,
        tenant_id: TenantId,
    ) -> Result<Vec<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        self.inner.list_triggers(tenant_id).await
    }

    async fn list_scoped_triggers(
        &self,
        tenant_id: TenantId,
        creator_user_id: UserId,
        agent_id: Option<AgentId>,
        project_id: Option<ProjectId>,
        limit: usize,
        excluded_states: &[ironclaw_triggers::TriggerState],
    ) -> Result<Vec<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        self.inner
            .list_scoped_triggers(
                tenant_id,
                creator_user_id,
                agent_id,
                project_id,
                limit,
                excluded_states,
            )
            .await
    }

    async fn remove_trigger(
        &self,
        tenant_id: TenantId,
        trigger_id: ironclaw_triggers::TriggerId,
    ) -> Result<Option<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        self.inner.remove_trigger(tenant_id, trigger_id).await
    }

    async fn remove_scoped_trigger(
        &self,
        tenant_id: TenantId,
        creator_user_id: UserId,
        agent_id: Option<AgentId>,
        project_id: Option<ProjectId>,
        trigger_id: ironclaw_triggers::TriggerId,
    ) -> Result<Option<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        self.inner
            .remove_scoped_trigger(tenant_id, creator_user_id, agent_id, project_id, trigger_id)
            .await
    }

    async fn set_scoped_trigger_state(
        &self,
        _tenant_id: TenantId,
        _creator_user_id: UserId,
        _agent_id: Option<AgentId>,
        _project_id: Option<ProjectId>,
        _trigger_id: ironclaw_triggers::TriggerId,
        _state: ironclaw_triggers::TriggerState,
    ) -> Result<Option<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        Err(trigger_backend_error())
    }

    async fn list_due_triggers(
        &self,
        now: Timestamp,
        limit: usize,
    ) -> Result<Vec<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        self.inner.list_due_triggers(now, limit).await
    }

    async fn list_active_triggers(
        &self,
        limit: usize,
    ) -> Result<Vec<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        self.inner.list_active_triggers(limit).await
    }

    async fn list_active_triggers_after(
        &self,
        after: Option<ironclaw_triggers::ActiveTriggerScanCursor>,
        limit: usize,
    ) -> Result<Vec<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        self.inner.list_active_triggers_after(after, limit).await
    }

    async fn claim_due_fire(
        &self,
        request: ironclaw_triggers::ClaimDueFireRequest,
    ) -> Result<ironclaw_triggers::ClaimDueFireOutcome, ironclaw_triggers::TriggerError> {
        self.inner.claim_due_fire(request).await
    }

    async fn mark_fire_accepted(
        &self,
        request: ironclaw_triggers::FireAcceptedRequest,
    ) -> Result<Option<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        self.inner.mark_fire_accepted(request).await
    }

    async fn mark_fire_replayed(
        &self,
        request: ironclaw_triggers::FireReplayedRequest,
    ) -> Result<Option<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        self.inner.mark_fire_replayed(request).await
    }

    async fn mark_fire_retryable_failed(
        &self,
        request: ironclaw_triggers::FireRetryableFailedRequest,
    ) -> Result<Option<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        self.inner.mark_fire_retryable_failed(request).await
    }

    async fn mark_fire_permanently_failed(
        &self,
        request: ironclaw_triggers::FirePermanentFailedRequest,
    ) -> Result<Option<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        self.inner.mark_fire_permanently_failed(request).await
    }

    async fn mark_fire_terminally_failed(
        &self,
        request: ironclaw_triggers::FireTerminalFailedRequest,
    ) -> Result<Option<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        self.inner.mark_fire_terminally_failed(request).await
    }

    async fn clear_active_fire(
        &self,
        request: ironclaw_triggers::ClearActiveFireRequest,
    ) -> Result<Option<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        self.inner.clear_active_fire(request).await
    }

    async fn list_trigger_run_history_batch(
        &self,
        _tenant_id: TenantId,
        _trigger_ids: &[ironclaw_triggers::TriggerId],
        _limit: usize,
    ) -> Result<HashMap<ironclaw_triggers::TriggerId, Vec<TriggerRunRecord>>, TriggerError> {
        Err(trigger_backend_error())
    }
}

#[async_trait]
impl TriggerRepository for FailingTriggerRepository {
    async fn find_trigger_run_by_thread_id(
        &self,
        _tenant_id: TenantId,
        _thread_id: &ThreadId,
    ) -> Result<Option<(TriggerRecord, TriggerRunRecord)>, TriggerError> {
        // Trigger-thread lookup is not exercised by this fake.
        Ok(None)
    }

    async fn upsert_trigger(
        &self,
        _record: ironclaw_triggers::TriggerRecord,
    ) -> Result<(), ironclaw_triggers::TriggerError> {
        Err(trigger_backend_error())
    }

    async fn get_trigger(
        &self,
        _tenant_id: TenantId,
        _trigger_id: ironclaw_triggers::TriggerId,
    ) -> Result<Option<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        Err(trigger_backend_error())
    }

    async fn list_triggers(
        &self,
        _tenant_id: TenantId,
    ) -> Result<Vec<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        Err(trigger_backend_error())
    }

    async fn list_scoped_triggers(
        &self,
        _tenant_id: TenantId,
        _creator_user_id: UserId,
        _agent_id: Option<AgentId>,
        _project_id: Option<ProjectId>,
        _limit: usize,
        _excluded_states: &[ironclaw_triggers::TriggerState],
    ) -> Result<Vec<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        Err(trigger_backend_error())
    }

    async fn remove_trigger(
        &self,
        _tenant_id: TenantId,
        _trigger_id: ironclaw_triggers::TriggerId,
    ) -> Result<Option<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        Err(trigger_backend_error())
    }

    async fn remove_scoped_trigger(
        &self,
        _tenant_id: TenantId,
        _creator_user_id: UserId,
        _agent_id: Option<AgentId>,
        _project_id: Option<ProjectId>,
        _trigger_id: ironclaw_triggers::TriggerId,
    ) -> Result<Option<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        Err(trigger_backend_error())
    }

    async fn set_scoped_trigger_state(
        &self,
        _tenant_id: TenantId,
        _creator_user_id: UserId,
        _agent_id: Option<AgentId>,
        _project_id: Option<ProjectId>,
        _trigger_id: ironclaw_triggers::TriggerId,
        _state: ironclaw_triggers::TriggerState,
    ) -> Result<Option<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        Err(trigger_backend_error())
    }

    async fn list_due_triggers(
        &self,
        _now: Timestamp,
        _limit: usize,
    ) -> Result<Vec<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        unreachable!("failing test repository does not list due triggers")
    }

    async fn list_active_triggers(
        &self,
        _limit: usize,
    ) -> Result<Vec<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        unreachable!("failing test repository does not list active triggers")
    }

    async fn list_active_triggers_after(
        &self,
        _after: Option<ironclaw_triggers::ActiveTriggerScanCursor>,
        _limit: usize,
    ) -> Result<Vec<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        unreachable!("failing test repository does not list active triggers")
    }

    async fn claim_due_fire(
        &self,
        _request: ironclaw_triggers::ClaimDueFireRequest,
    ) -> Result<ironclaw_triggers::ClaimDueFireOutcome, ironclaw_triggers::TriggerError> {
        unreachable!("failing test repository does not claim due fires")
    }

    async fn mark_fire_accepted(
        &self,
        _request: ironclaw_triggers::FireAcceptedRequest,
    ) -> Result<Option<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        unreachable!("failing test repository does not update fire results")
    }

    async fn mark_fire_replayed(
        &self,
        _request: ironclaw_triggers::FireReplayedRequest,
    ) -> Result<Option<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        unreachable!("failing test repository does not update fire results")
    }

    async fn mark_fire_retryable_failed(
        &self,
        _request: ironclaw_triggers::FireRetryableFailedRequest,
    ) -> Result<Option<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        unreachable!("failing test repository does not update fire results")
    }

    async fn mark_fire_permanently_failed(
        &self,
        _request: ironclaw_triggers::FirePermanentFailedRequest,
    ) -> Result<Option<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        unreachable!("failing test repository does not update fire results")
    }

    async fn mark_fire_terminally_failed(
        &self,
        _request: ironclaw_triggers::FireTerminalFailedRequest,
    ) -> Result<Option<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        unreachable!("failing test repository does not update fire results")
    }

    async fn clear_active_fire(
        &self,
        _request: ironclaw_triggers::ClearActiveFireRequest,
    ) -> Result<Option<ironclaw_triggers::TriggerRecord>, ironclaw_triggers::TriggerError> {
        unreachable!("failing test repository does not clear active fires")
    }
}
