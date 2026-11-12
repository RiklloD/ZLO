//! Production-shaped builds: hooks, readiness, trajectory observers, and the
//! user-sandbox process port.

use super::*;

/// Runtime-store unification (branch `unify-runtime-store-graph`): every build
/// now composes the single unified runtime store graph, so the hook framework
/// is wired for a production libsql build exactly as it is for standalone — the
/// old "hooks are not wired for production runtime launch" rejection premise no
/// longer holds (its `else if hooks_config.is_enabled()` branch in
/// `build_reborn_runtime` is now unreachable). This locks the new-but-correct
/// behavior: enabling hooks on a production runtime builds and validates
/// readiness instead of failing `MalformedConfig`.
#[tokio::test]
async fn production_runtime_wires_enabled_hooks_through_unified_runtime() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("reborn.db"))
            .build()
            .await
            .expect("libsql db"),
    );

    let input = RebornRuntimeInput::from_build_input(
        crate::test_support::libsql_host_bindings_for_test(
            crate::RebornCompositionProfile::Production,
            "runtime-production-hooks-owner",
            db,
            dir.path().join("reborn.db").to_string_lossy(),
            None,
            ironclaw_secrets::SecretMaterial::from("01234567890123456789012345678901"),
        )
        .expect("libSQL bindings")
        .with_production_trust_policy(Arc::new(
            crate::builtin_first_party_trust_policy().expect("trust policy"),
        ))
        .with_runtime_policy(EffectiveRuntimePolicy {
            deployment: DeploymentMode::HostedMultiTenant,
            requested_profile: RuntimeProfile::SecureDefault,
            resolved_profile: RuntimeProfile::SecureDefault,
            filesystem_backend: FilesystemBackendKind::ScopedVirtual,
            process_backend: ProcessBackendKind::UserSandbox,
            network_mode: NetworkMode::Deny,
            secret_mode: SecretMode::BrokeredHandles,
            approval_policy: ApprovalPolicy::AskAlways,
            audit_mode: AuditMode::Standard,
        })
        .with_runtime_process_binding(RebornRuntimeProcessBinding::user_sandbox(Arc::new(
            ironclaw_host_runtime::UserSandboxProcessPort::new(Arc::new(RecordingSandboxTransport)),
        ))),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-production-hooks-tenant".to_string(),
        agent_id: "runtime-production-hooks-agent".to_string(),
        source_binding_id: "runtime-production-hooks-source".to_string(),
        reply_target_binding_id: "runtime-production-hooks-reply".to_string(),
    })
    .with_hooks_config(HooksActivationConfig::enabled());

    let runtime = build_reborn_runtime(input)
        .await
        .expect("unified production runtime wires the hook framework when hooks are enabled");
    assert_eq!(
        runtime.readiness().state,
        RebornReadinessState::ProductionValidated
    );
    runtime.shutdown().await.expect("runtime shutdown");
}

#[tokio::test]
async fn build_reborn_runtime_allows_validated_production_readiness() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("reborn.db"))
            .build()
            .await
            .expect("libsql db"),
    );
    let gateway = Arc::new(RecordingGateway {
        reply: "validated production runtime".to_string(),
        requests: Arc::new(StdMutex::new(Vec::new())),
    });

    let input = RebornRuntimeInput::from_build_input(
        crate::test_support::libsql_host_bindings_for_test(
            crate::RebornCompositionProfile::Production,
            "runtime-production-cutover-owner",
            db,
            dir.path().join("reborn.db").to_string_lossy(),
            None,
            ironclaw_secrets::SecretMaterial::from("01234567890123456789012345678901"),
        )
        .expect("libSQL bindings")
        .with_production_trust_policy(Arc::new(
            crate::builtin_first_party_trust_policy().expect("trust policy"),
        ))
        .with_runtime_policy(EffectiveRuntimePolicy {
            deployment: DeploymentMode::HostedMultiTenant,
            requested_profile: RuntimeProfile::SecureDefault,
            resolved_profile: RuntimeProfile::SecureDefault,
            filesystem_backend: FilesystemBackendKind::ScopedVirtual,
            process_backend: ProcessBackendKind::UserSandbox,
            network_mode: NetworkMode::Deny,
            secret_mode: SecretMode::BrokeredHandles,
            approval_policy: ApprovalPolicy::AskAlways,
            audit_mode: AuditMode::Standard,
        })
        .with_runtime_process_binding(RebornRuntimeProcessBinding::user_sandbox(Arc::new(
            ironclaw_host_runtime::UserSandboxProcessPort::new(Arc::new(RecordingSandboxTransport)),
        ))),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-production-cutover-tenant".to_string(),
        agent_id: "runtime-production-cutover-agent".to_string(),
        source_binding_id: "runtime-production-cutover-source".to_string(),
        reply_target_binding_id: "runtime-production-cutover-reply".to_string(),
    })
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input)
        .await
        .expect("validated production readiness should start runtime");

    assert_eq!(
        runtime.readiness().state,
        RebornReadinessState::ProductionValidated
    );
    assert!(runtime.readiness().diagnostics.is_empty());
    assert!(runtime.readiness().workers.turn_runner);

    runtime.shutdown().await.expect("runtime shutdown");
}

/// Runtime-store unification (branch `unify-runtime-store-graph`): the
/// trajectory observer is wired through the (now single, always-present)
/// capability path, so a production runtime observes turns exactly as standalone
/// does. The old rejection guard (Firat's review) existed because a
/// non-standalone runtime had no capability hook and would silently produce an
/// empty trajectory — that premise no longer holds (the `else` reject branch in
/// `build_reborn_runtime` is now unreachable), so supplying an observer is
/// accepted and wired rather than rejected.
#[tokio::test]
async fn build_reborn_runtime_wires_trajectory_observer_through_unified_runtime() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("reborn.db"))
            .build()
            .await
            .expect("libsql db"),
    );
    let gateway = Arc::new(ToolCallingGateway::default());
    let gateway_for_runtime: Arc<dyn HostManagedModelGateway> = gateway.clone();
    let observer = Arc::new(RecordingTrajectoryObserver::default());

    let input = RebornRuntimeInput::from_build_input(
        crate::test_support::libsql_host_bindings_for_test(
            crate::RebornCompositionProfile::Production,
            "runtime-observer-reject-owner",
            db,
            dir.path().join("reborn.db").to_string_lossy(),
            None,
            ironclaw_secrets::SecretMaterial::from("01234567890123456789012345678901"),
        )
        .expect("libSQL bindings")
        .with_production_trust_policy(Arc::new(
            crate::builtin_first_party_trust_policy().expect("trust policy"),
        ))
        .with_runtime_policy(EffectiveRuntimePolicy {
            deployment: DeploymentMode::HostedMultiTenant,
            requested_profile: RuntimeProfile::SecureDefault,
            resolved_profile: RuntimeProfile::SecureDefault,
            filesystem_backend: FilesystemBackendKind::ScopedVirtual,
            process_backend: ProcessBackendKind::UserSandbox,
            network_mode: NetworkMode::Deny,
            secret_mode: SecretMode::BrokeredHandles,
            approval_policy: ApprovalPolicy::AskAlways,
            audit_mode: AuditMode::Standard,
        })
        .with_runtime_process_binding(RebornRuntimeProcessBinding::user_sandbox(Arc::new(
            ironclaw_host_runtime::UserSandboxProcessPort::new(Arc::new(RecordingSandboxTransport)),
        ))),
    )
    .with_tool_disclosure(ToolDisclosureMode::Off)
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-observer-reject-tenant".to_string(),
        agent_id: "runtime-observer-reject-agent".to_string(),
        source_binding_id: "runtime-observer-reject-source".to_string(),
        reply_target_binding_id: "runtime-observer-reject-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: RUNTIME_SEND_TIMEOUT,
    })
    .with_raw_trajectory_observer(observer.clone())
    .with_model_gateway_override(gateway_for_runtime);

    let runtime = build_reborn_runtime(input)
        .await
        .expect("unified production runtime accepts and wires a trajectory observer");
    assert_eq!(
        runtime.readiness().state,
        RebornReadinessState::ProductionValidated
    );
    let conversation = runtime.new_conversation().await.expect("conversation");
    runtime
        .enable_global_auto_approve_for_test(&conversation)
        .await;
    let reply = tokio::time::timeout(
        RUNTIME_SEND_TIMEOUT,
        runtime.send_user_message(&conversation, "use echo tool"),
    )
    .await
    .expect("runtime send should finish")
    .expect("runtime send should succeed");
    assert_eq!(reply.status, TurnStatus::Completed, "reply: {reply:?}");
    assert_eq!(reply.text.as_deref(), Some("tool ok"));
    runtime.shutdown().await.expect("runtime shutdown");

    let inputs = observer.inputs.lock().expect("inputs lock");
    assert_eq!(inputs.len(), 1, "exactly one capability input observed");
    let (input_call_id, input_capability, arguments) = &inputs[0];
    assert!(!input_call_id.is_empty(), "input call_id should be present");
    assert_eq!(input_capability, "builtin.echo");
    assert_eq!(
        arguments,
        &serde_json::json!({"message": "hello from tool"}),
        "observer should receive the raw model-emitted tool arguments"
    );

    let results = observer.results.lock().expect("results lock");
    assert_eq!(results.len(), 1, "exactly one capability result observed");
    let (result_call_id, result_capability, output) = &results[0];
    assert_eq!(result_capability, "builtin.echo");
    assert_eq!(
        result_call_id, input_call_id,
        "result and input callbacks correlate by call_id"
    );
    assert!(
        output.to_string().contains("hello from tool"),
        "observer should receive the staged capability output, got {output}"
    );
}

#[derive(Debug)]
struct RecordingSandboxTransport;

#[async_trait]
impl ironclaw_host_api::process::SandboxCommandTransport for RecordingSandboxTransport {
    async fn run_command(
        &self,
        _request: ironclaw_host_api::process::CommandExecutionRequest,
    ) -> Result<
        ironclaw_host_api::process::CommandExecutionOutput,
        ironclaw_host_api::process::RuntimeProcessError,
    > {
        Ok(ironclaw_host_api::process::CommandExecutionOutput {
            output: String::new(),
            saved_output: None,
            exit_code: 0,
            sandboxed: true,
            duration: Duration::ZERO,
        })
    }
}

#[derive(Debug, Default)]
struct ShellRecordingSandboxTransport {
    requests: StdMutex<Vec<ironclaw_host_api::process::CommandExecutionRequest>>,
    shutdown_calls: AtomicUsize,
}

#[test]
fn user_sandbox_shutdown_error_preserves_runtime_process_source() {
    use std::error::Error as _;

    let source = ironclaw_host_api::process::RuntimeProcessError::ExecutionFailed(
        "sanitized checkpoint failure".to_string(),
    );
    let error = crate::runtime::RebornRuntimeError::UserSandboxShutdown(source.clone());

    assert_eq!(
        error.source().map(ToString::to_string),
        Some(source.to_string())
    );
}

#[async_trait]
impl ironclaw_host_api::process::SandboxCommandTransport for ShellRecordingSandboxTransport {
    async fn run_command(
        &self,
        request: ironclaw_host_api::process::CommandExecutionRequest,
    ) -> Result<
        ironclaw_host_api::process::CommandExecutionOutput,
        ironclaw_host_api::process::RuntimeProcessError,
    > {
        self.requests
            .lock()
            .expect("sandbox request lock poisoned")
            .push(request);
        Ok(ironclaw_host_api::process::CommandExecutionOutput {
            output: "railway-sandbox-marker".to_string(),
            saved_output: None,
            exit_code: 0,
            // The trusted process adapter, rather than a provider transport,
            // owns this provenance bit and must normalize it to true.
            sandboxed: false,
            duration: Duration::ZERO,
        })
    }

    async fn shutdown(&self) -> Result<(), ironclaw_host_api::process::RuntimeProcessError> {
        self.shutdown_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn railway_sandbox_profile_routes_model_shell_call_to_user_sandbox_process_port() {
    let root = tempfile::tempdir().expect("tempdir");
    let gateway = Arc::new(SandboxShellCallingGateway::default());
    let sandbox_transport = Arc::new(ShellRecordingSandboxTransport::default());
    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input_with_profile(
            RebornCompositionProfile::HostedSingleTenantVolumeSandboxedRailway,
            "runtime-railway-shell-owner",
            root.path().join("sandboxed"),
        )
        .with_runtime_policy(
            crate::hosted_single_tenant_volume_sandboxed_runtime_policy()
                .expect("hosted sandbox policy resolves"),
        )
        .with_runtime_process_binding(RebornRuntimeProcessBinding::user_sandbox(Arc::new(
            ironclaw_host_runtime::UserSandboxProcessPort::new(sandbox_transport.clone()),
        ))),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-railway-shell-tenant".to_string(),
        agent_id: "runtime-railway-shell-agent".to_string(),
        source_binding_id: "runtime-railway-shell-source".to_string(),
        reply_target_binding_id: "runtime-railway-shell-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: RUNTIME_SEND_TIMEOUT,
    })
    .with_model_gateway_override(gateway);

    let runtime =
        tokio::time::timeout(PRODUCTION_SHAPED_BUILD_TIMEOUT, build_reborn_runtime(input))
            .await
            .expect("sandboxed Railway runtime build should finish")
            .expect("sandboxed Railway runtime builds");
    let conversation = runtime.new_conversation().await.expect("conversation");
    runtime
        .enable_global_auto_approve_for_test(&conversation)
        .await;
    let reply = tokio::time::timeout(
        RUNTIME_SEND_TIMEOUT,
        runtime.send_user_message(&conversation, "run the shell marker"),
    )
    .await
    .expect("sandbox shell turn should finish")
    .expect("sandbox shell turn succeeds");

    assert_eq!(reply.status, TurnStatus::Completed, "reply: {reply:?}");
    assert_eq!(reply.text.as_deref(), Some("sandbox shell ok"));
    {
        let requests = sandbox_transport
            .requests
            .lock()
            .expect("sandbox request lock poisoned");
        assert_eq!(
            requests.len(),
            1,
            "shell must use the sandbox transport once"
        );
        assert_eq!(requests[0].command, "printf railway-sandbox-marker");
    }
    tokio::time::timeout(RUNTIME_SEND_TIMEOUT, runtime.shutdown())
        .await
        .expect("runtime shutdown should finish")
        .expect("runtime shutdown");
    assert_eq!(sandbox_transport.shutdown_calls.load(Ordering::SeqCst), 1);
}

/// Reads one sample from the process-wide operator registry. The pins below
/// assert the registry *moved by at least* what their own turn produced:
/// other tests in this binary only add to it, and a deleted production wrap
/// site leaves no code path that increments it at all.
fn global_operator_sample(series: &str) -> u64 {
    let prefix = format!("{series} ");
    ironclaw_operator::operator_metrics()
        .render_prometheus()
        .lines()
        .find_map(|line| line.strip_prefix(&prefix)?.parse().ok())
        .unwrap_or(0)
}

/// Pins the production operator-metrics wrap sites a full production runtime
/// goes through: the turn-event fan-out (`assemble_runtime`) and the runtime
/// event sink (`build_backend_production` → `metered_runtime_event_sink`).
#[tokio::test]
async fn production_runtime_records_turns_and_tool_calls_into_operator_metrics() {
    const RUNS: &str = "ironclaw_turn_runs_total{outcome=\"completed\"}";
    const ECHO: &str = "ironclaw_capability_invocations_total{capability_id=\"builtin.echo\",outcome=\"succeeded\"}";
    const MODEL_CALLS: &str = "ironclaw_model_calls_total{outcome=\"completed\"}";
    let (runs_before, echo_before, model_calls_before) = (
        global_operator_sample(RUNS),
        global_operator_sample(ECHO),
        global_operator_sample(MODEL_CALLS),
    );

    let dir = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(
        libsql::Builder::new_local(dir.path().join("reborn.db"))
            .build()
            .await
            .expect("libsql db"),
    );
    let gateway: Arc<dyn HostManagedModelGateway> = Arc::new(ToolCallingGateway::default());
    let input = RebornRuntimeInput::from_build_input(
        crate::test_support::libsql_host_bindings_for_test(
            crate::RebornCompositionProfile::Production,
            "runtime-operator-metrics-owner",
            db,
            dir.path().join("reborn.db").to_string_lossy(),
            None,
            ironclaw_secrets::SecretMaterial::from("01234567890123456789012345678901"),
        )
        .expect("libSQL bindings")
        .with_production_trust_policy(Arc::new(
            crate::builtin_first_party_trust_policy().expect("trust policy"),
        ))
        .with_runtime_policy(EffectiveRuntimePolicy {
            deployment: DeploymentMode::HostedMultiTenant,
            requested_profile: RuntimeProfile::SecureDefault,
            resolved_profile: RuntimeProfile::SecureDefault,
            filesystem_backend: FilesystemBackendKind::ScopedVirtual,
            process_backend: ProcessBackendKind::UserSandbox,
            network_mode: NetworkMode::Deny,
            secret_mode: SecretMode::BrokeredHandles,
            approval_policy: ApprovalPolicy::AskAlways,
            audit_mode: AuditMode::Standard,
        })
        .with_runtime_process_binding(RebornRuntimeProcessBinding::user_sandbox(Arc::new(
            ironclaw_host_runtime::UserSandboxProcessPort::new(Arc::new(RecordingSandboxTransport)),
        ))),
    )
    .with_tool_disclosure(ToolDisclosureMode::Off)
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-operator-metrics-tenant".to_string(),
        agent_id: "runtime-operator-metrics-agent".to_string(),
        source_binding_id: "runtime-operator-metrics-source".to_string(),
        reply_target_binding_id: "runtime-operator-metrics-reply".to_string(),
    })
    .with_poll_settings(PollSettings {
        interval: Duration::from_millis(10),
        max_total: RUNTIME_SEND_TIMEOUT,
    })
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    let conversation = runtime.new_conversation().await.expect("conversation");
    runtime
        .enable_global_auto_approve_for_test(&conversation)
        .await;
    let reply = tokio::time::timeout(
        RUNTIME_SEND_TIMEOUT,
        runtime.send_user_message(&conversation, "use echo tool"),
    )
    .await
    .expect("runtime send should finish")
    .expect("runtime send should succeed");
    assert_eq!(reply.status, TurnStatus::Completed, "reply: {reply:?}");
    runtime
        .runtime_event_sink
        .flush()
        .await
        .expect("runtime events flush");
    // Turn lifecycle sinks are best-effort after the terminal commit.
    for _ in 0..100 {
        if global_operator_sample(RUNS) > runs_before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    runtime.shutdown().await.expect("runtime shutdown");

    assert!(
        global_operator_sample(RUNS) > runs_before,
        "the turn-event fan-out records completed runs"
    );
    assert!(
        global_operator_sample(ECHO) > echo_before,
        "the runtime event sink records committed tool calls"
    );
    assert!(
        global_operator_sample(MODEL_CALLS) >= model_calls_before + 2,
        "the runtime event sink records committed model calls"
    );
}
