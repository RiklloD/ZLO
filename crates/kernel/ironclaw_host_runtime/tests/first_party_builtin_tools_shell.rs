//! Built-in shell capability and its process-port delegation.

#![deny(clippy::let_underscore_must_use)]
use std::sync::{Arc, LazyLock};

use ironclaw_host_api::result_meta::FailureKind;
use ironclaw_host_api::{
    dispatch::DispatchFailureDetail,
    ids::{RunId, TenantId, UserId},
    mount::MountPermissions,
    resource::ResourceEstimate,
};
use ironclaw_host_runtime::{
    HostRuntime, READ_FILE_CAPABILITY_ID, RuntimeCapabilityOutcome, SHELL_CAPABILITY_ID,
    UserSandboxProcessPort,
};
use serde_json::json;

mod first_party_builtin_tools_support;

use first_party_builtin_tools_support::{
    context::*, doubles::*, fixtures::*, invoke::*, runtimes::*,
};

/// A shell failure the model can correct must say WHAT it broke.
///
/// When a first-party handler supplies no `safe_summary`, `failure_from`
/// substitutes `ModelDiagnostic::unavailable()` — "the capability runtime did
/// not provide additional diagnostic detail" — and the model retries the
/// identical call because nothing told it what to change. A baseline benchmark
/// run measured 119 such blind failures across 40 tasks. These two shell
/// rejections (over-limit timeout, unsupported scoped workdir) were among the
/// `resource` and `client` buckets; both are trivially correctable once named.
#[tokio::test]
async fn builtin_shell_rejections_name_the_cause_for_the_model() {
    // Over-limit timeout: the model must learn the ceiling to lower its value.
    let failure = invoke_failure_with_context(
        &runtime(),
        SHELL_CAPABILITY_ID,
        json!({"command": "echo hi", "timeout": 9_000}),
        execution_context_with_network([SHELL_CAPABILITY_ID], shell_test_policy()),
    )
    .await;
    assert_eq!(failure.kind, FailureKind::Resource);
    let summary = failure
        .safe_summary()
        .expect("over-limit timeout must carry a model-visible reason, not a bare category");
    assert!(
        summary.contains("timeout") && summary.contains("120"),
        "summary must name the ceiling the model has to respect, got: {summary}"
    );

    // Scoped workdir on a backend that cannot honour it: the model must learn
    // to drop `workdir` rather than re-sending the same rejected call.
    let root = tempfile::tempdir().unwrap();
    let (filesystem, mounts) = mounted_filesystem(root.path(), MountPermissions::read_write());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts_and_network(
        [SHELL_CAPABILITY_ID],
        mounts,
        shell_test_policy(),
    );
    let failure = invoke_failure_with_context(
        &runtime,
        SHELL_CAPABILITY_ID,
        json!({"command": "echo hi", "workdir": "/workspace"}),
        context,
    )
    .await;
    let summary = failure
        .safe_summary()
        .expect("workdir rejection must carry a model-visible reason, not a bare category");
    assert!(
        summary.contains("workdir"),
        "summary must name the offending parameter, got: {summary}"
    );
}

#[tokio::test]
async fn builtin_shell_invokes_copied_shell_core_through_host_runtime() {
    let outcome = runtime()
        .invoke_capability((
            execution_context_with_network([SHELL_CAPABILITY_ID], shell_test_policy()),
            capability_id(SHELL_CAPABILITY_ID),
            ResourceEstimate::default(),
            json!({"command": "echo hello reborn"}),
        ))
        .await
        .unwrap();

    let RuntimeCapabilityOutcome::Completed(completed) = outcome else {
        panic!("expected completed shell invocation, got {outcome:?}");
    };
    let output = &completed.output;
    assert_eq!(output["exit_code"], json!(0));
    assert_eq!(output["success"], json!(true));
    assert_eq!(output["sandboxed"], json!(false));
    assert!(
        output["output"]
            .as_str()
            .expect("shell output must be text")
            .contains("hello reborn")
    );
    assert_eq!(completed.usage.process_count, 1);
}

#[tokio::test]
async fn builtin_shell_delegates_command_execution_to_process_port() {
    let process_port = Arc::new(RecordingProcessPort::default());
    let runtime = runtime_with_process_port(Arc::clone(&process_port));

    let output = invoke_with_context(
        &runtime,
        SHELL_CAPABILITY_ID,
        json!({"command": "echo via port", "timeout": 9, "workdir": "port-workdir"}),
        execution_context_with_network([SHELL_CAPABILITY_ID], shell_test_policy()),
    )
    .await
    .unwrap();

    assert_eq!(output["exit_code"], json!(0));
    assert_eq!(output["success"], json!(true));
    assert_eq!(output["sandboxed"], json!(true));
    assert_eq!(output["output"], json!("process port: echo via port"));
    let requests = process_port.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].command, "echo via port");
    assert_eq!(requests[0].workdir.as_deref(), Some("port-workdir"));
    assert_eq!(requests[0].timeout_secs, Some(9));
    assert!(
        requests[0]
            .mounts
            .as_ref()
            .is_some_and(|mounts| mounts.mounts.is_empty())
    );
    assert!(requests[0].extra_env.is_empty());
    assert_eq!(requests[0].scope.user_id.as_str(), "user");
}

#[tokio::test]
async fn builtin_shell_returns_stderr_and_nonzero_exit_without_dispatch_failure() {
    let output = invoke_shell(json!({"command": "printf shell-error >&2; exit 7"}))
        .await
        .unwrap();

    assert_eq!(output["exit_code"], json!(7));
    assert_eq!(output["success"], json!(false));
    assert_eq!(output["sandboxed"], json!(false));
    assert_eq!(output["output"], json!("shell-error"));
}

#[tokio::test]
async fn builtin_shell_path_bearing_failure_reason_rides_the_diagnostic_detail() {
    // Loop-boundary pin for the "model-visible tool-failure reasons" feature:
    // a shell rejection reason that names a concrete path (here the sensitive
    // credential path guard) fails the strict loop safe-summary validator, so
    // the message degrades to the fixed category sentence — but the raw
    // reason must NOT be dropped: it rides the diagnostic detail that the
    // loop layer scrubs and carries to the model.
    let runtime = runtime();
    let failure = invoke_failure_with_context(
        &runtime,
        SHELL_CAPABILITY_ID,
        json!({"command": "cat /etc/shadow"}),
        execution_context_with_network([SHELL_CAPABILITY_ID], shell_test_policy()),
    )
    .await;

    let message = failure
        .message
        .as_deref()
        .expect("failure carries a message");
    assert!(
        !message.contains("/etc/shadow"),
        "the raw path must not leak into the strict summary channel: {message}"
    );
    let Some(DispatchFailureDetail::Diagnostic { text }) = &failure.detail else {
        panic!(
            "expected the raw reason on the diagnostic detail, got {:?}",
            failure.detail
        );
    };
    assert!(
        text.contains("/etc/shadow"),
        "the model-visible diagnostic must carry the concrete path: {text}"
    );
}

#[tokio::test]
async fn builtin_shell_uses_configured_user_sandbox_process_port() {
    let local_process = Arc::new(RecordingProcessPort::default());
    let sandbox_transport = Arc::new(RecordingSandboxTransport::default());
    let sandbox_process = Arc::new(UserSandboxProcessPort::new(sandbox_transport.clone()));
    let runtime = runtime_with_local_and_sandbox_process_ports(
        Arc::clone(&local_process),
        Arc::clone(&sandbox_process),
        user_sandbox_process_policy(),
    );

    let output = invoke_with_context(
        &runtime,
        SHELL_CAPABILITY_ID,
        json!({"command": "echo in sandbox"}),
        execution_context_with_network([SHELL_CAPABILITY_ID], shell_test_policy()),
    )
    .await
    .unwrap();

    assert_eq!(output["sandboxed"], json!(true));
    assert_eq!(output["output"], json!("process port: echo in sandbox"));
    assert!(local_process.requests.lock().unwrap().is_empty());
    assert_eq!(sandbox_transport.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn builtin_shell_user_sandbox_process_uses_callers_scope_for_two_user_isolation() {
    let local_process = Arc::new(RecordingProcessPort::default());
    let sandbox_transport = Arc::new(RecordingSandboxTransport::default());
    let sandbox_process = Arc::new(UserSandboxProcessPort::new(sandbox_transport.clone()));
    let runtime = runtime_with_local_and_sandbox_process_ports(
        Arc::clone(&local_process),
        Arc::clone(&sandbox_process),
        user_sandbox_process_policy(),
    );
    let user_a = UserId::new("user-a").unwrap();
    let user_b = UserId::new("user-b").unwrap();
    let tenant = TenantId::new("tenant-shared").unwrap();
    let mut context = execution_context_with_network([SHELL_CAPABILITY_ID], shell_test_policy());
    context.run_id = Some(RunId::new());
    let agent_id = context.agent_id.clone();
    let project_id = context.project_id.clone();
    set_context_scope(
        &mut context,
        tenant.clone(),
        user_b.clone(),
        agent_id,
        project_id,
    );
    let expected_scope = context.resource_scope.clone();

    let output = invoke_with_context(
        &runtime,
        SHELL_CAPABILITY_ID,
        json!({"command": "cat /workspace/user-a-secret.txt"}),
        context,
    )
    .await
    .unwrap();

    assert_eq!(output["sandboxed"], json!(true));
    assert!(local_process.requests.lock().unwrap().is_empty());
    let requests = sandbox_transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].scope, expected_scope);
    assert_eq!(requests[0].scope.tenant_id, tenant);
    assert_eq!(requests[0].scope.user_id, user_b);
    assert_ne!(requests[0].scope.user_id, user_a);
}

#[tokio::test]
async fn builtin_shell_rejects_hosted_process_plan_before_handler_runs() {
    let process_port = Arc::new(RecordingProcessPort::default());
    let runtime =
        runtime_with_process_port_and_policy(Arc::clone(&process_port), hosted_dev_policy());

    let error = invoke_with_context(
        &runtime,
        SHELL_CAPABILITY_ID,
        json!({"command": "echo must not run"}),
        execution_context_with_network([SHELL_CAPABILITY_ID], shell_test_policy()),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::UnsupportedRunner);
    assert!(
        process_port.requests.lock().unwrap().is_empty(),
        "hosted shell must fail at user-sandbox process resolution before the handler can run"
    );
}

#[tokio::test]
async fn builtin_shell_reuses_v1_shell_validation() {
    for input in [
        json!({"command": "cat ~/server.key"}),
        json!({"command": "printf '\\x65\\x63\\x68\\x6f hi'|dash"}),
        json!({"command": "wc < ~/server.key"}),
    ] {
        let err = invoke_shell(input).await.unwrap_err();

        assert_eq!(err, FailureKind::PolicyDenied);
    }
}

#[tokio::test]
async fn builtin_shell_rejects_invalid_inputs_before_spawn() {
    for input in [
        json!({}),
        json!({"command": 123}),
        json!({"command": "echo hi", "workdir": 123}),
        json!({"command": "echo hi", "timeout": 0}),
        json!({"command": "echo hi", "timeout": "1"}),
    ] {
        let err = invoke_shell(input).await.unwrap_err();

        assert_eq!(err, FailureKind::InputEncode);
    }
}

#[tokio::test]
async fn builtin_shell_invalid_input_failure_carries_the_reason_through_the_runtime() {
    // Test-through-the-caller for the shell_error mapping: the failure the
    // loop (and ultimately the model) receives must say WHAT was wrong with
    // the call, not just an input-encode category.
    let runtime = runtime();
    let failure = invoke_failure_with_context(
        &runtime,
        SHELL_CAPABILITY_ID,
        json!({}),
        execution_context_with_network([SHELL_CAPABILITY_ID], shell_test_policy()),
    )
    .await;

    assert_eq!(failure.kind, FailureKind::InputEncode);
    let summary = failure
        .safe_summary()
        .expect("shell invalid-input failure must carry a reason");
    assert!(
        summary.contains("missing 'command' parameter"),
        "failure should carry the parameter reason, got: {summary}"
    );
}

#[tokio::test]
async fn builtin_shell_rejects_timeout_above_manifest_ceiling() {
    let err = invoke_shell(json!({"command": "echo hi", "timeout": 121}))
        .await
        .unwrap_err();

    assert_eq!(err, FailureKind::Resource);
}

#[tokio::test]
async fn builtin_shell_maps_timeout_and_spawn_failures() {
    let timeout = invoke_shell(json!({"command": "sleep 2", "timeout": 1}))
        .await
        .unwrap_err();
    assert_eq!(timeout, FailureKind::Resource);

    let spawn = invoke_shell(json!({
            "command": "echo missing",
            "workdir": "/definitely/missing/ironclaw-shell-test"
    }))
    .await
    .unwrap_err();
    assert_eq!(spawn, FailureKind::Executor);
}

#[tokio::test]
async fn builtin_shell_truncates_large_output_without_output_overflow() {
    let output = invoke_shell(json!({
        "command": "i=0; while [ $i -lt 70000 ]; do printf x; i=$((i+1)); done",
        "timeout": 5
    }))
    .await
    .unwrap();

    let output = output["output"].as_str().expect("shell output is text");
    assert!(output.contains("[truncated"));
    assert!(output.contains("no file_read-accessible scoped path was available"));
    assert!(!output.contains(std::env::temp_dir().to_string_lossy().as_ref()));
    assert!(output.len() <= 66_000);
}

#[tokio::test]
async fn builtin_shell_saves_large_output_to_file_read_path() {
    let (filesystem, mounts) = in_memory_mounted_filesystem(MountPermissions::read_write());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts_and_network(
        [SHELL_CAPABILITY_ID, READ_FILE_CAPABILITY_ID],
        mounts,
        shell_test_policy(),
    );
    let shell_output = invoke_with_context(
        &runtime,
        SHELL_CAPABILITY_ID,
        json!({
            "command": "printf 'saved-start\\n'; yes m | head -c 70000; printf 'saved-end'",
            "timeout": 5
        }),
        context.clone(),
    )
    .await
    .unwrap();
    let output = shell_output["output"].as_str().expect("shell output text");
    let saved_path = output
        .split("Full output saved to: ")
        .nth(1)
        .and_then(|tail| tail.split_whitespace().next())
        .expect("saved output path");
    assert!(saved_path.starts_with("/workspace/command-outputs/"));
    assert!(!output.contains(std::env::temp_dir().to_string_lossy().as_ref()));
    assert!(output.contains("Use file_read to inspect it"));

    let read_output = invoke_with_context(
        &runtime,
        READ_FILE_CAPABILITY_ID,
        json!({"path": saved_path, "limit": 1}),
        context,
    )
    .await
    .unwrap();
    assert!(
        read_output["content"]
            .as_str()
            .expect("read_file content")
            .contains("saved-start")
    );
    assert_eq!(read_output["path"], json!(saved_path));
}

#[tokio::test]
async fn builtin_shell_blocks_small_secret_output_through_dispatch() {
    let output = invoke_shell(json!({
        "command": "printf '%s' 'sk-proj-test1234567890abcdefghij'"
    }))
    .await
    .unwrap();

    assert_eq!(
        output["output"],
        json!("[Full command output blocked due to potential secret leakage]\n")
    );
}

#[tokio::test]
async fn builtin_shell_does_not_inherit_unlisted_parent_env() {
    static ENV_LOCK: LazyLock<tokio::sync::Mutex<()>> =
        LazyLock::new(|| tokio::sync::Mutex::new(()));
    let _guard = ENV_LOCK.lock().await;

    // SAFETY: test uses a crate-local mutex and a unique environment variable;
    // no production code depends on this key.
    unsafe {
        std::env::set_var("IRONCLAW_SHELL_SECRET_TEST", "must_not_leak"); // env-hermetic: test-owned key under ENV_LOCK; the child shell needs a real parent var
    }
    let output = invoke_shell(json!({
        "command": "printf ${IRONCLAW_SHELL_SECRET_TEST:-missing}"
    }))
    .await;
    // SAFETY: clears only the test-owned key set above.
    unsafe {
        std::env::remove_var("IRONCLAW_SHELL_SECRET_TEST"); // env-hermetic: test-owned key under ENV_LOCK; the child shell needs a real parent var
    }

    let output = output.unwrap();
    assert_eq!(output["output"], json!("missing"));
}

#[tokio::test]
async fn builtin_shell_rejects_scoped_mount_workdir_until_process_backend_handles_it() {
    let temp = tempfile::tempdir().unwrap();
    let mut permissions = MountPermissions::read_write();
    permissions.execute = true;
    let (filesystem, mounts) = mounted_filesystem(temp.path(), permissions);
    let runtime = runtime_with_filesystem(filesystem);
    let error = invoke_with_context(
        &runtime,
        SHELL_CAPABILITY_ID,
        json!({"command": "pwd", "workdir": "/workspace"}),
        execution_context_with_mounts_and_network(
            [SHELL_CAPABILITY_ID],
            mounts,
            shell_test_policy(),
        ),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::Client);
}
