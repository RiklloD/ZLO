use super::*;

pub(crate) fn runtime() -> impl HostRuntime {
    runtime_with_filesystem(DiskFilesystem::new())
}

pub(crate) fn runtime_with_filesystem<F>(filesystem: F) -> impl HostRuntime
where
    F: RootFilesystem + 'static,
{
    runtime_with_filesystem_and_policy(filesystem, local_host_policy())
}

pub(crate) fn runtime_with_filesystem_and_policy<F>(
    filesystem: F,
    policy: EffectiveRuntimePolicy,
) -> impl HostRuntime
where
    F: RootFilesystem + 'static,
{
    runtime_with_filesystem_policy_and_trigger_repository(
        filesystem,
        policy,
        Arc::new(InMemoryTriggerRepository::default()),
    )
}

pub(crate) fn runtime_with_filesystem_without_http_egress<F>(filesystem: F) -> impl HostRuntime
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
    .with_first_party_capabilities(Arc::new(
        builtin_first_party_handlers(Arc::new(InMemoryTriggerRepository::default())).unwrap(),
    ))
    .with_runtime_policy(local_host_policy())
    .with_trust_policy(Arc::new(trust_policy()))
    .host_runtime_for_local_testing()
}

pub(crate) fn runtime_with_filesystem_and_http_egress<F, T>(
    filesystem: F,
    egress: Arc<T>,
) -> impl HostRuntime
where
    F: RootFilesystem + 'static,
    T: RuntimeHttpEgress + 'static,
{
    HostRuntimeServices::new(
        Arc::new(registry()),
        Arc::new(filesystem),
        Arc::new(InMemoryResourceGovernor::new()),
        Arc::new(GrantAuthorizer::new()),
        ironclaw_processes::ProcessServices::in_memory(),
        CapabilitySurfaceVersion::new("surface-v1").unwrap(),
    )
    .with_first_party_capabilities(Arc::new(
        builtin_first_party_handlers(Arc::new(InMemoryTriggerRepository::default())).unwrap(),
    ))
    .with_runtime_http_egress(egress)
    .with_runtime_policy(local_host_policy())
    .with_trust_policy(Arc::new(trust_policy()))
    .host_runtime_for_local_testing()
}

pub(crate) fn runtime_with_filesystem_and_audit_sink<F>(
    filesystem: F,
    audit_sink: Arc<InMemoryAuditSink>,
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
        let mut handlers =
            builtin_first_party_handlers(Arc::new(InMemoryTriggerRepository::default())).unwrap();
        register_native_memory_tools(&mut handlers).unwrap();
        handlers
    }))
    .with_runtime_http_egress(Arc::new(RecordingRuntimeHttpEgress::default()))
    .with_audit_sink(audit_sink)
    .with_runtime_policy(local_host_policy())
    .with_trust_policy(Arc::new(trust_policy()))
    .host_runtime_for_local_testing()
}

pub(crate) fn runtime_with_process_port<T>(process_port: Arc<T>) -> impl HostRuntime
where
    T: RuntimeProcessPort + 'static,
{
    runtime_with_process_port_and_policy(process_port, local_host_policy())
}

pub(crate) fn runtime_with_process_port_and_policy<T>(
    process_port: Arc<T>,
    policy: EffectiveRuntimePolicy,
) -> impl HostRuntime
where
    T: RuntimeProcessPort + 'static,
{
    HostRuntimeServices::new(
        Arc::new(registry()),
        Arc::new(DiskFilesystem::new()),
        Arc::new(InMemoryResourceGovernor::new()),
        Arc::new(GrantAuthorizer::new()),
        ironclaw_processes::ProcessServices::in_memory(),
        CapabilitySurfaceVersion::new("surface-v1").unwrap(),
    )
    .with_first_party_capabilities(Arc::new(
        builtin_first_party_handlers(Arc::new(InMemoryTriggerRepository::default())).unwrap(),
    ))
    .with_runtime_process_port(process_port)
    .with_runtime_http_egress(Arc::new(RecordingRuntimeHttpEgress::default()))
    .with_runtime_policy(policy)
    .with_trust_policy(Arc::new(trust_policy()))
    .host_runtime_for_local_testing()
}

pub(crate) fn runtime_with_local_and_sandbox_process_ports<L>(
    local_process: Arc<L>,
    sandbox_process: Arc<UserSandboxProcessPort>,
    policy: EffectiveRuntimePolicy,
) -> impl HostRuntime
where
    L: RuntimeProcessPort + 'static,
{
    HostRuntimeServices::new(
        Arc::new(registry()),
        Arc::new(DiskFilesystem::new()),
        Arc::new(InMemoryResourceGovernor::new()),
        Arc::new(GrantAuthorizer::new()),
        ironclaw_processes::ProcessServices::in_memory(),
        CapabilitySurfaceVersion::new("surface-v1").unwrap(),
    )
    .with_first_party_capabilities(Arc::new(
        builtin_first_party_handlers(Arc::new(InMemoryTriggerRepository::default())).unwrap(),
    ))
    .with_runtime_process_port(local_process)
    .with_user_sandbox_process_port(sandbox_process)
    .with_runtime_http_egress(Arc::new(RecordingRuntimeHttpEgress::default()))
    .with_runtime_policy(policy)
    .with_trust_policy(Arc::new(trust_policy()))
    .host_runtime_for_local_testing()
}

pub(crate) fn runtime_with_policy(policy: EffectiveRuntimePolicy) -> impl HostRuntime {
    HostRuntimeServices::new(
        Arc::new(registry()),
        Arc::new(DiskFilesystem::new()),
        Arc::new(InMemoryResourceGovernor::new()),
        Arc::new(GrantAuthorizer::new()),
        ironclaw_processes::ProcessServices::in_memory(),
        CapabilitySurfaceVersion::new("surface-v1").unwrap(),
    )
    .with_first_party_capabilities(Arc::new(
        builtin_first_party_handlers(Arc::new(InMemoryTriggerRepository::default())).unwrap(),
    ))
    .with_trust_policy(Arc::new(trust_policy()))
    .with_runtime_policy(policy)
    .host_runtime_for_local_testing()
}

pub(crate) fn local_host_policy() -> EffectiveRuntimePolicy {
    EffectiveRuntimePolicy {
        deployment: DeploymentMode::LocalSingleUser,
        requested_profile: RuntimeProfile::LocalHost,
        resolved_profile: RuntimeProfile::LocalHost,
        filesystem_backend: FilesystemBackendKind::HostWorkspace,
        process_backend: ProcessBackendKind::LocalHost,
        network_mode: NetworkMode::DirectLogged,
        secret_mode: SecretMode::ScrubbedEnv,
        approval_policy: ApprovalPolicy::AskDestructive,
        audit_mode: AuditMode::LocalMinimal,
    }
}

pub(crate) fn user_sandbox_process_policy() -> EffectiveRuntimePolicy {
    EffectiveRuntimePolicy {
        process_backend: ProcessBackendKind::UserSandbox,
        ..local_host_policy()
    }
}

pub(crate) fn hosted_dev_policy() -> EffectiveRuntimePolicy {
    EffectiveRuntimePolicy {
        deployment: DeploymentMode::HostedMultiTenant,
        requested_profile: RuntimeProfile::HostedDev,
        resolved_profile: RuntimeProfile::HostedDev,
        filesystem_backend: FilesystemBackendKind::TenantWorkspace,
        process_backend: ProcessBackendKind::UserSandbox,
        network_mode: NetworkMode::Allowlist,
        secret_mode: SecretMode::TenantBroker,
        approval_policy: ApprovalPolicy::AskDestructive,
        audit_mode: AuditMode::Standard,
    }
}

pub(crate) fn runtime_with_http_egress<T>(egress: Arc<T>) -> impl HostRuntime
where
    T: RuntimeHttpEgress + ToolCallHttpEgress + 'static,
{
    runtime_with_http_egress_and_governor(egress, Arc::new(InMemoryResourceGovernor::new()))
}

pub(crate) fn runtime_with_http_egress_and_policy<T>(
    egress: Arc<T>,
    policy: EffectiveRuntimePolicy,
) -> impl HostRuntime
where
    T: RuntimeHttpEgress + ToolCallHttpEgress + 'static,
{
    HostRuntimeServices::new(
        Arc::new(registry()),
        Arc::new(DiskFilesystem::new()),
        Arc::new(InMemoryResourceGovernor::new()),
        Arc::new(GrantAuthorizer::new()),
        ironclaw_processes::ProcessServices::in_memory(),
        CapabilitySurfaceVersion::new("surface-v1").unwrap(),
    )
    .with_first_party_capabilities(Arc::new(
        builtin_first_party_handlers(Arc::new(InMemoryTriggerRepository::default())).unwrap(),
    ))
    .with_first_party_http_egress(egress)
    .with_trust_policy(Arc::new(trust_policy()))
    .with_runtime_policy(policy)
    .host_runtime_for_local_testing()
}

pub(crate) fn runtime_with_http_egress_and_governor<T>(
    egress: Arc<T>,
    governor: Arc<InMemoryResourceGovernor>,
) -> impl HostRuntime
where
    T: RuntimeHttpEgress + ToolCallHttpEgress + 'static,
{
    HostRuntimeServices::new(
        Arc::new(registry()),
        Arc::new(DiskFilesystem::new()),
        governor,
        Arc::new(GrantAuthorizer::new()),
        ironclaw_processes::ProcessServices::in_memory(),
        CapabilitySurfaceVersion::new("surface-v1").unwrap(),
    )
    .with_first_party_capabilities(Arc::new(
        builtin_first_party_handlers(Arc::new(InMemoryTriggerRepository::default())).unwrap(),
    ))
    .with_first_party_http_egress(egress)
    .with_trust_policy(Arc::new(trust_policy()))
    .host_runtime_for_local_testing()
}

pub(crate) fn runtime_with_strict_http_egress_only<T>(egress: Arc<T>) -> impl HostRuntime
where
    T: RuntimeHttpEgress + 'static,
{
    HostRuntimeServices::new(
        Arc::new(registry()),
        Arc::new(DiskFilesystem::new()),
        Arc::new(InMemoryResourceGovernor::new()),
        Arc::new(GrantAuthorizer::new()),
        ironclaw_processes::ProcessServices::in_memory(),
        CapabilitySurfaceVersion::new("surface-v1").unwrap(),
    )
    .with_first_party_capabilities(Arc::new(
        builtin_first_party_handlers(Arc::new(InMemoryTriggerRepository::default())).unwrap(),
    ))
    .with_runtime_http_egress(egress)
    .with_trust_policy(Arc::new(trust_policy()))
    .host_runtime_for_local_testing()
}

pub(crate) fn runtime_with_host_http_egress<N>(network: N) -> impl HostRuntime
where
    N: NetworkHttpEgress + 'static,
{
    HostRuntimeServices::new(
        Arc::new(registry()),
        Arc::new(DiskFilesystem::new()),
        Arc::new(InMemoryResourceGovernor::new()),
        Arc::new(GrantAuthorizer::new()),
        ironclaw_processes::ProcessServices::in_memory(),
        CapabilitySurfaceVersion::new("surface-v1").unwrap(),
    )
    .with_secret_store(Arc::new(SecretStore::ephemeral()))
    .with_first_party_capabilities(Arc::new(
        builtin_first_party_handlers(Arc::new(InMemoryTriggerRepository::default())).unwrap(),
    ))
    .try_with_host_http_egress(network)
    .unwrap()
    .with_trust_policy(Arc::new(trust_policy()))
    .host_runtime_for_local_testing()
}
