use super::*;

pub(crate) fn execution_context<I>(grants: I) -> ExecutionContext
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    let capability_set = CapabilitySet {
        grants: grants
            .into_iter()
            .map(|grant| dispatch_grant(grant.as_ref()))
            .collect(),
    };
    let mut context = ExecutionContext::local_default(
        UserId::new("user").unwrap(),
        ExtensionId::new("caller").unwrap(),
        RuntimeKind::FirstParty,
        TrustClass::FirstParty,
        capability_set,
        MountView::default(),
    )
    .unwrap();
    context.run_id = Some(RunId::new());
    context
}

pub(crate) fn execution_context_with_mounts<I>(grants: I, mounts: MountView) -> ExecutionContext
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    let capability_set = CapabilitySet {
        grants: grants
            .into_iter()
            .map(|grant| dispatch_grant_with_mounts(grant.as_ref(), mounts.clone()))
            .collect(),
    };
    let mut context = ExecutionContext::local_default(
        UserId::new("user").unwrap(),
        ExtensionId::new("caller").unwrap(),
        RuntimeKind::FirstParty,
        TrustClass::FirstParty,
        capability_set,
        mounts,
    )
    .unwrap();
    context.run_id = Some(RunId::new());
    context
}

pub(crate) fn execution_context_with_network<I>(
    grants: I,
    network: NetworkPolicy,
) -> ExecutionContext
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    execution_context_with_mounts_and_network(grants, MountView::default(), network)
}

pub(crate) fn execution_context_with_mounts_and_network<I>(
    grants: I,
    mounts: MountView,
    network: NetworkPolicy,
) -> ExecutionContext
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    let capability_set = CapabilitySet {
        grants: grants
            .into_iter()
            .map(|grant| {
                dispatch_grant_with_mounts_and_network(
                    grant.as_ref(),
                    mounts.clone(),
                    network.clone(),
                )
            })
            .collect(),
    };
    let mut context = ExecutionContext::local_default(
        UserId::new("user").unwrap(),
        ExtensionId::new("caller").unwrap(),
        RuntimeKind::FirstParty,
        TrustClass::FirstParty,
        capability_set,
        mounts,
    )
    .unwrap();
    context.run_id = Some(RunId::new());
    context
}

pub(crate) fn set_context_scope(
    context: &mut ExecutionContext,
    tenant_id: TenantId,
    user_id: UserId,
    agent_id: Option<AgentId>,
    project_id: Option<ProjectId>,
) {
    context.tenant_id = tenant_id.clone();
    context.user_id = user_id.clone();
    context.agent_id = agent_id.clone();
    context.project_id = project_id.clone();
    context.resource_scope.tenant_id = tenant_id;
    context.resource_scope.user_id = user_id;
    context.resource_scope.agent_id = agent_id;
    context.resource_scope.project_id = project_id;
}

pub(crate) fn dispatch_grant(capability: &str) -> CapabilityGrant {
    dispatch_grant_with_mounts(capability, MountView::default())
}

pub(crate) fn dispatch_grant_with_mounts(capability: &str, mounts: MountView) -> CapabilityGrant {
    dispatch_grant_with_mounts_and_network(capability, mounts, NetworkPolicy::default())
}

pub(crate) fn dispatch_grant_with_mounts_and_network(
    capability: &str,
    mounts: MountView,
    network: NetworkPolicy,
) -> CapabilityGrant {
    CapabilityGrant {
        id: CapabilityGrantId::new(),
        capability: capability_id(capability),
        grantee: Principal::Extension(ExtensionId::new("caller").unwrap()),
        issued_by: Principal::HostRuntime,
        constraints: GrantConstraints {
            allowed_effects: builtin_effects(),
            mounts,
            network,
            secrets: Vec::new(),
            resource_ceiling: None,
            expires_at: None,
            max_invocations: None,
        },
    }
}

pub(crate) fn builtin_effects() -> Vec<EffectKind> {
    vec![
        EffectKind::DispatchCapability,
        EffectKind::ReadFilesystem,
        EffectKind::WriteFilesystem,
        EffectKind::DeleteFilesystem,
        EffectKind::Network,
        EffectKind::SpawnProcess,
        EffectKind::ExecuteCode,
        // Required by builtin.trace_commons.onboard.
        EffectKind::ExternalWrite,
    ]
}

pub(crate) fn network_denied_policy() -> EffectiveRuntimePolicy {
    EffectiveRuntimePolicy {
        deployment: DeploymentMode::LocalSingleUser,
        requested_profile: RuntimeProfile::SecureDefault,
        resolved_profile: RuntimeProfile::SecureDefault,
        filesystem_backend: FilesystemBackendKind::ScopedVirtual,
        process_backend: ProcessBackendKind::None,
        network_mode: NetworkMode::Deny,
        secret_mode: SecretMode::BrokeredHandles,
        approval_policy: ApprovalPolicy::AskAlways,
        audit_mode: AuditMode::LocalMinimal,
    }
}

pub(crate) fn http_test_policy() -> NetworkPolicy {
    NetworkPolicy {
        allowed_targets: vec![NetworkTargetPattern {
            scheme: Some(NetworkScheme::Https),
            host_pattern: "api.example.test".to_string(),
            port: None,
        }],
        deny_private_ip_ranges: true,
        max_egress_bytes: Some(10_000),
    }
}

pub(crate) fn shell_test_policy() -> NetworkPolicy {
    NetworkPolicy {
        allowed_targets: vec![NetworkTargetPattern {
            scheme: None,
            host_pattern: "*".to_string(),
            port: None,
        }],
        deny_private_ip_ranges: false,
        max_egress_bytes: None,
    }
}

pub(crate) fn trust_policy() -> HostTrustPolicy {
    HostTrustPolicy::new(vec![Box::new(AdminConfig::with_entries(vec![
        AdminEntry::for_local_manifest(
            PackageId::new("builtin").unwrap(),
            "/system/extensions/builtin/manifest.toml".to_string(),
            None,
            HostTrustAssignment::first_party(),
            builtin_effects(),
            None,
        ),
        // Native memory rides the always-on lane alongside builtin and carries
        // its own first-party trust entry (mirrors the composition trust policy).
        AdminEntry::for_local_manifest(
            PackageId::new(NATIVE_MEMORY_FIRST_PARTY_PROVIDER).unwrap(),
            "/system/extensions/ironclaw.memory/manifest.toml".to_string(),
            None,
            HostTrustAssignment::first_party(),
            builtin_effects(),
            None,
        ),
    ]))])
    .unwrap()
}

pub(crate) fn provider_trust() -> BTreeMap<ExtensionId, TrustDecision> {
    BTreeMap::from([
        (provider_id(), trust_decision()),
        (
            ExtensionId::new(NATIVE_MEMORY_FIRST_PARTY_PROVIDER).unwrap(),
            trust_decision(),
        ),
    ])
}

pub(crate) fn trust_decision() -> TrustDecision {
    TrustDecision {
        effective_trust: EffectiveTrustClass::user_trusted(),
        authority_ceiling: AuthorityCeiling {
            allowed_effects: builtin_effects(),
            max_resource_ceiling: None,
        },
        provenance: TrustProvenance::Default,
        evaluated_at: chrono::Utc::now(),
    }
}
