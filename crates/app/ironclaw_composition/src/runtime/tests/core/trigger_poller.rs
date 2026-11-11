//! Trigger poller worker: readiness, authorization, enablement, config
//! validation, and shutdown.

use super::*;

#[tokio::test]
async fn standalone_runtime_readiness_reports_trigger_poller_worker() {
    let root = tempfile::tempdir().expect("tempdir");
    let gateway = Arc::new(RecordingGateway {
        reply: "trigger readiness".to_string(),
        requests: Arc::new(StdMutex::new(Vec::new())),
    });

    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-trigger-readiness-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-trigger-readiness-tenant".to_string(),
        agent_id: "runtime-trigger-readiness-agent".to_string(),
        source_binding_id: "runtime-trigger-readiness-source".to_string(),
        reply_target_binding_id: "runtime-trigger-readiness-reply".to_string(),
    })
    .with_trigger_poller_settings(
        TriggerPollerSettings::enabled_with_tenant_scoped_authorizer_for_test(),
    )
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");

    assert!(runtime.readiness().workers.turn_runner);
    assert!(runtime.readiness().workers.trigger_poller);

    runtime.shutdown().await.expect("runtime shutdown");
}

#[tokio::test]
async fn standalone_runtime_rejects_trigger_poller_without_creator_authorization() {
    let root = tempfile::tempdir().expect("tempdir");
    let gateway = Arc::new(RecordingGateway {
        reply: "trigger auth required".to_string(),
        requests: Arc::new(StdMutex::new(Vec::new())),
    });

    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-trigger-auth-required-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-trigger-auth-required-tenant".to_string(),
        agent_id: "runtime-trigger-auth-required-agent".to_string(),
        source_binding_id: "runtime-trigger-auth-required-source".to_string(),
        reply_target_binding_id: "runtime-trigger-auth-required-reply".to_string(),
    })
    .with_trigger_poller_settings(TriggerPollerSettings::enabled())
    .with_model_gateway_override(gateway);

    let err = match build_reborn_runtime(input).await {
        Ok(runtime) => {
            runtime
                .shutdown()
                .await
                .expect("unexpected runtime shutdown");
            panic!(
                "creator-access-required setting must not enable trigger poller without an access checker"
            );
        }
        Err(err) => err,
    };

    assert!(
        matches!(err, crate::runtime::RebornRuntimeError::InvalidArgument { reason } if reason.contains("fire-time creator access checker"))
    );
}

#[tokio::test]
async fn standalone_runtime_accepts_trigger_poller_with_creator_access_checker() {
    let root = tempfile::tempdir().expect("tempdir");
    let gateway = Arc::new(RecordingGateway {
        reply: "trigger auth supplied".to_string(),
        requests: Arc::new(StdMutex::new(Vec::new())),
    });

    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-trigger-auth-supplied-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-trigger-auth-supplied-tenant".to_string(),
        agent_id: "runtime-trigger-auth-supplied-agent".to_string(),
        source_binding_id: "runtime-trigger-auth-supplied-source".to_string(),
        reply_target_binding_id: "runtime-trigger-auth-supplied-reply".to_string(),
    })
    .with_trigger_poller_settings(TriggerPollerSettings::enabled())
    .with_trigger_fire_access_checker(Arc::new(AllowingTriggerFireAccessChecker))
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input)
        .await
        .expect("runtime builds with creator access checker");

    assert!(runtime.readiness().workers.turn_runner);
    assert!(runtime.readiness().workers.trigger_poller);

    runtime.shutdown().await.expect("runtime shutdown");
}

#[tokio::test]
async fn standalone_runtime_disables_trigger_poller_worker_by_default() {
    let root = tempfile::tempdir().expect("tempdir");
    let gateway = Arc::new(RecordingGateway {
        reply: "trigger disabled".to_string(),
        requests: Arc::new(StdMutex::new(Vec::new())),
    });

    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-trigger-disabled-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-trigger-disabled-tenant".to_string(),
        agent_id: "runtime-trigger-disabled-agent".to_string(),
        source_binding_id: "runtime-trigger-disabled-source".to_string(),
        reply_target_binding_id: "runtime-trigger-disabled-reply".to_string(),
    })
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");

    assert!(runtime.readiness().workers.turn_runner);
    assert!(!runtime.readiness().workers.trigger_poller);

    runtime.shutdown().await.expect("runtime shutdown");
}

#[tokio::test]
async fn standalone_runtime_rejects_invalid_trigger_poller_worker_config() {
    let root = tempfile::tempdir().expect("tempdir");
    let gateway = Arc::new(RecordingGateway {
        reply: "trigger invalid config".to_string(),
        requests: Arc::new(StdMutex::new(Vec::new())),
    });
    let trigger_poller = TriggerPollerSettings::enabled()
        .with_worker_config(
            ironclaw_triggers::TriggerPollerWorkerConfig::default()
                .set_poll_interval(Duration::ZERO),
        )
        .with_tenant_scoped_authorizer_for_test();

    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-trigger-invalid-config-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-trigger-invalid-config-tenant".to_string(),
        agent_id: "runtime-trigger-invalid-config-agent".to_string(),
        source_binding_id: "runtime-trigger-invalid-config-source".to_string(),
        reply_target_binding_id: "runtime-trigger-invalid-config-reply".to_string(),
    })
    .with_trigger_poller_settings(trigger_poller)
    .with_model_gateway_override(gateway);

    let err = match build_reborn_runtime(input).await {
        Ok(runtime) => {
            runtime
                .shutdown()
                .await
                .expect("unexpected runtime shutdown");
            panic!("invalid trigger poller config must fail runtime build");
        }
        Err(err) => err,
    };

    assert!(
        matches!(err, crate::runtime::RebornRuntimeError::InvalidArgument { reason } if reason.contains("poll_interval must be non-zero"))
    );
}

#[tokio::test]
async fn standalone_runtime_shutdown_cancels_trigger_poller_worker() {
    let root = tempfile::tempdir().expect("tempdir");
    let gateway = Arc::new(RecordingGateway {
        reply: "trigger shutdown".to_string(),
        requests: Arc::new(StdMutex::new(Vec::new())),
    });

    let input = RebornRuntimeInput::from_build_input(
        crate::deployment::local_filesystem_build_input(
            "runtime-trigger-shutdown-owner",
            root.path().join("standalone"),
        )
        .with_runtime_policy(standalone_runtime_policy()),
    )
    .with_identity(RebornRuntimeIdentity {
        tenant_id: "runtime-trigger-shutdown-tenant".to_string(),
        agent_id: "runtime-trigger-shutdown-agent".to_string(),
        source_binding_id: "runtime-trigger-shutdown-source".to_string(),
        reply_target_binding_id: "runtime-trigger-shutdown-reply".to_string(),
    })
    .with_trigger_poller_settings(
        TriggerPollerSettings::enabled_with_tenant_scoped_authorizer_for_test(),
    )
    .with_model_gateway_override(gateway);

    let runtime = build_reborn_runtime(input).await.expect("runtime builds");
    assert!(runtime.readiness().workers.trigger_poller);

    tokio::time::timeout(std::time::Duration::from_secs(2), runtime.shutdown())
        .await
        .expect("shutdown returns before timeout")
        .expect("runtime shutdown");
}
