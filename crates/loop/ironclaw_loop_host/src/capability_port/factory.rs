use super::*;

#[async_trait]
pub trait LoopCapabilityPortFactory: Send + Sync {
    async fn create_capability_port(
        &self,
        run_context: &LoopRunContext,
    ) -> Result<Arc<dyn LoopCapabilityPort>, AgentLoopHostError>;

    /// Build a port from an already-resolved host-owned model-surface policy.
    ///
    /// The default preserves compatibility for factories whose inner surface
    /// has no host-runtime visibility request. Production host-backed
    /// factories override this method so disclosure, outer filtering, and the
    /// host's visible snapshot all consume the same resolved value.
    async fn create_capability_port_with_surface_policy(
        &self,
        run_context: &LoopRunContext,
        _surface_policy: Arc<CapabilitySurfacePolicy>,
    ) -> Result<Arc<dyn LoopCapabilityPort>, AgentLoopHostError> {
        self.create_capability_port(run_context).await
    }
}

pub trait LoopCapabilityPortDecorator: Send + Sync {
    fn decorate(
        &self,
        run_context: &LoopRunContext,
        inner: Arc<dyn LoopCapabilityPort>,
    ) -> Arc<dyn LoopCapabilityPort>;
}

pub struct DecoratingLoopCapabilityPortFactory {
    inner: Arc<dyn LoopCapabilityPortFactory>,
    decorators: Vec<Arc<dyn LoopCapabilityPortDecorator>>,
}

impl DecoratingLoopCapabilityPortFactory {
    pub fn new(inner: Arc<dyn LoopCapabilityPortFactory>) -> Self {
        Self {
            inner,
            decorators: Vec::new(),
        }
    }

    pub fn with_decorator(mut self, decorator: Arc<dyn LoopCapabilityPortDecorator>) -> Self {
        self.decorators.push(decorator);
        self
    }
}

#[async_trait]
impl LoopCapabilityPortFactory for DecoratingLoopCapabilityPortFactory {
    async fn create_capability_port(
        &self,
        run_context: &LoopRunContext,
    ) -> Result<Arc<dyn LoopCapabilityPort>, AgentLoopHostError> {
        let mut port = self.inner.create_capability_port(run_context).await?;
        for decorator in &self.decorators {
            port = decorator.decorate(run_context, port);
        }
        Ok(port)
    }

    async fn create_capability_port_with_surface_policy(
        &self,
        run_context: &LoopRunContext,
        surface_policy: Arc<CapabilitySurfacePolicy>,
    ) -> Result<Arc<dyn LoopCapabilityPort>, AgentLoopHostError> {
        let mut port = self
            .inner
            .create_capability_port_with_surface_policy(run_context, surface_policy)
            .await?;
        for decorator in &self.decorators {
            port = decorator.decorate(run_context, port);
        }
        Ok(port)
    }
}

#[derive(Clone)]
pub struct HostRuntimeLoopCapabilityPortFactory {
    runtime: Arc<dyn HostRuntime>,
    visible_request: ironclaw_host_runtime::VisibleCapabilityRequest,
    input_resolver: Arc<dyn LoopCapabilityInputResolver>,
    result_writer: Arc<dyn LoopCapabilityResultWriter>,
    milestone_sink: Arc<dyn LoopHostMilestoneSink>,
    execution_mounts: MountView,
    capability_execution_mounts: HashMap<CapabilityId, MountView>,
    trajectory_observer: Option<Arc<dyn CapabilityTrajectoryObserver>>,
    gate_record_store: Arc<dyn GateRecordStorePort>,
    replay_payload_store: Arc<dyn ReplayPayloadStorePort>,
}

impl HostRuntimeLoopCapabilityPortFactory {
    pub fn new(
        runtime: Arc<dyn HostRuntime>,
        visible_request: ironclaw_host_runtime::VisibleCapabilityRequest,
        input_resolver: Arc<dyn LoopCapabilityInputResolver>,
        result_writer: Arc<dyn LoopCapabilityResultWriter>,
        milestone_sink: Arc<dyn LoopHostMilestoneSink>,
    ) -> Self {
        Self {
            runtime,
            visible_request,
            input_resolver,
            result_writer,
            milestone_sink,
            execution_mounts: MountView::default(),
            capability_execution_mounts: HashMap::new(),
            trajectory_observer: None,
            // Transitional no-op default until composition wires the durable
            // store via `with_gate_record_store` (the record has no reader until
            // the resume-read follow-up, so skipping the write is behavior-
            // preserving). See `NoopGateRecordStore`.
            gate_record_store: Arc::new(NoopGateRecordStore),
            // Transitional fail-closed default until composition wires the durable
            // replay-payload store via `with_replay_payload_store`. See
            // `NoopReplayPayloadStore`: an unwired factory persists nothing, so a
            // gate/auth resume that must reconstitute its replay input fails closed
            // (sanitized terminal failure) rather than dispatching empty input.
            replay_payload_store: Arc::new(NoopReplayPayloadStore),
        }
    }

    /// Wire the durable [`GateRecordStorePort`] every port built by this factory
    /// persists pending-gate records into (§5.2.9). Production composition always
    /// calls this; the fail-closed default only guards an unwired factory.
    pub fn with_gate_record_store(mut self, store: Arc<dyn GateRecordStorePort>) -> Self {
        self.gate_record_store = store;
        self
    }

    /// Wire the durable host-private [`ReplayPayloadStorePort`] every port built by
    /// this factory persists gate/auth replay payloads into and reconstitutes
    /// them from on resume (arch-simplification §5.3 Stage 2a-i). Production
    /// composition always calls this; the fail-closed default only guards an
    /// unwired factory.
    pub fn with_replay_payload_store(mut self, store: Arc<dyn ReplayPayloadStorePort>) -> Self {
        self.replay_payload_store = store;
        self
    }

    /// Attach a [`CapabilityTrajectoryObserver`] that every port built by this
    /// factory forwards capability inputs to. No-op when unset.
    pub fn with_trajectory_observer(
        mut self,
        observer: Option<Arc<dyn CapabilityTrajectoryObserver>>,
    ) -> Self {
        self.trajectory_observer = observer;
        self
    }

    pub fn with_execution_mounts(mut self, mounts: MountView) -> Self {
        self.execution_mounts = mounts;
        self
    }

    pub fn with_capability_execution_mount(
        mut self,
        capability_id: CapabilityId,
        mounts: MountView,
    ) -> Self {
        self.capability_execution_mounts
            .insert(capability_id, mounts);
        self
    }

    pub fn for_run_context(&self, run_context: LoopRunContext) -> Arc<dyn LoopCapabilityPort> {
        Arc::new(self.port_for_run_context(run_context))
    }

    pub(super) fn port_for_run_context(
        &self,
        run_context: LoopRunContext,
    ) -> HostRuntimeLoopCapabilityPort {
        HostRuntimeLoopCapabilityPort::new(
            Arc::clone(&self.runtime),
            run_context,
            self.visible_request.clone(),
            Arc::clone(&self.input_resolver),
            Arc::clone(&self.result_writer),
            Arc::clone(&self.milestone_sink),
        )
        .with_gate_record_store(Arc::clone(&self.gate_record_store))
        .with_replay_payload_store(Arc::clone(&self.replay_payload_store))
        .with_execution_mounts(self.execution_mounts.clone())
        .with_capability_execution_mounts(self.capability_execution_mounts.clone())
        .with_trajectory_observer(self.trajectory_observer.clone())
    }
}

#[async_trait]
impl LoopCapabilityPortFactory for HostRuntimeLoopCapabilityPortFactory {
    async fn create_capability_port(
        &self,
        run_context: &LoopRunContext,
    ) -> Result<Arc<dyn LoopCapabilityPort>, AgentLoopHostError> {
        Ok(self.for_run_context(run_context.clone()))
    }

    async fn create_capability_port_with_surface_policy(
        &self,
        run_context: &LoopRunContext,
        surface_policy: Arc<CapabilitySurfacePolicy>,
    ) -> Result<Arc<dyn LoopCapabilityPort>, AgentLoopHostError> {
        let mut factory = self.clone();
        factory.visible_request.policy = surface_policy.as_ref().clone();
        factory.create_capability_port(run_context).await
    }
}
