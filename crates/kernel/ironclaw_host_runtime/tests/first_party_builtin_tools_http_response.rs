//! Built-in http/http_save: response status, truncation budgets, saved bodies, sanitization.

use std::sync::Arc;

use ironclaw_authorization::GrantAuthorizer;
use ironclaw_filesystem::DiskFilesystem;
use ironclaw_host_api::result_meta::{FailureKind, MODEL_DIAGNOSTIC_MAX_BYTES};
use ironclaw_host_api::{
    action::NetworkMethod,
    dispatch::DispatchFailureDetail,
    ids::TenantId,
    mount::{MountGrant, MountPermissions, MountView},
    path::{MountAlias, VirtualPath},
    resource::LOCAL_DEFAULT_TENANT_ID,
    runtime::RuntimeKind,
};
use ironclaw_host_runtime::{
    CapabilitySurfaceVersion, HTTP_CAPABILITY_ID, HTTP_SAVE_CAPABILITY_ID, HostRuntimeServices,
    builtin_first_party_handlers,
};
use ironclaw_resources::{InMemoryResourceGovernor, ResourceAccount};
use ironclaw_triggers::InMemoryTriggerRepository;
use serde_json::{Value, json};

mod first_party_builtin_tools_support;

use first_party_builtin_tools_support::{
    context::*, doubles::*, fixtures::*, invoke::*, runtimes::*,
};

#[tokio::test]
async fn builtin_http_invokes_through_host_runtime_egress() {
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_body(
        br#"{"accepted":true}"#.to_vec(),
    ));
    let runtime = runtime_with_http_egress(Arc::clone(&egress));

    let output = invoke_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({
            "method": "post",
            "url": "https://api.example.test/v1/items",
            "headers": {
                "content-type": "application/json",
                "x-request-id": "first-party-http"
            },
            "body": {"ok": true},
            "response_body_limit": 4096,
            "timeout_ms": 2500
        }),
        execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
    )
    .await
    .unwrap_or_else(|error| {
        panic!(
            "expected HTTP egress success, got {error:?}; recorded requests: {:?}",
            egress.requests()
        )
    });

    assert_eq!(output["status"], json!(200));
    assert_eq!(output["body_text"], json!(r#"{"accepted":true}"#));
    assert_eq!(output["request_bytes"], json!(11));
    assert_eq!(output["response_bytes"], json!(17));

    let requests = egress.requests();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.runtime, RuntimeKind::FirstParty);
    assert_eq!(request.capability_id, capability_id(HTTP_CAPABILITY_ID));
    assert_eq!(request.method, NetworkMethod::Post);
    assert_eq!(request.url, "https://api.example.test/v1/items");
    assert_eq!(request.body, br#"{"ok":true}"#);
    assert_eq!(request.response_body_limit, Some(4096));
    assert_eq!(request.save_body_to, None);
    assert_eq!(request.timeout_ms, Some(2500));
    assert!(request.credential_injections.is_empty());
}

#[tokio::test]
async fn builtin_http_surfaces_http_error_status_as_failed_outcome() {
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_status_and_body(
        403,
        br#"{"message":"authentication required"}"#.to_vec(),
    ));
    let governor = Arc::new(InMemoryResourceGovernor::new());
    let runtime = runtime_with_http_egress_and_governor(Arc::clone(&egress), Arc::clone(&governor));

    let failure = invoke_failure_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({
            "method": "post",
            "url": "https://api.example.test/private",
            "body": "paid"
        }),
        execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
    )
    .await;

    assert_eq!(failure.kind, FailureKind::OperationFailed);
    assert_eq!(
        failure.safe_summary().as_deref(),
        Some("HTTP request returned status 403")
    );
    let Some(DispatchFailureDetail::Diagnostic { text }) = failure.detail else {
        panic!("HTTP error response must remain available as diagnostic context");
    };
    let response: Value = serde_json::from_str(&text).expect("HTTP diagnostic must be JSON");
    assert_eq!(response["status"], json!(403));
    assert_eq!(
        response["body_text"],
        json!(r#"{"message":"authentication required"}"#)
    );
    assert!(response["auth_hint"].as_str().is_some_and(|hint| {
        hint.contains("authentication/authorization") && hint.contains("extension")
    }));
    assert_eq!(egress.requests().len(), 1);
    // The failure carries usage like the sibling dispatch paths: egress bytes
    // from the request body flow into the governor even for failed calls.
    // (wall_clock_ms is pinned at the classify_status unit seam instead of
    // here because integration-tier wall-clock is timing-dependent; the
    // governor records the full failed-call usage, wall_clock_ms included.)
    let tenant_account = ResourceAccount::tenant(TenantId::new(LOCAL_DEFAULT_TENANT_ID).unwrap());
    let usage = governor.usage_for(&tenant_account);
    assert_eq!(
        usage.network_egress_bytes, 4,
        "failed calls must still account egress bytes"
    );
}

#[tokio::test]
async fn builtin_http_keeps_redirect_responses_model_visible() {
    let egress = Arc::new(
        RecordingRuntimeHttpEgress::with_status_and_body(302, Vec::new())
            .with_headers(vec![("location".to_string(), "/next".to_string())]),
    );
    let runtime = runtime_with_http_egress(Arc::clone(&egress));

    let output = invoke_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({"url": "https://api.example.test/redirect"}),
        execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
    )
    .await
    .expect("redirect responses must remain inspectable results");

    assert_eq!(output["status"], json!(302));
    assert_eq!(output["headers"][0]["name"], json!("location"));
    assert_eq!(output["headers"][0]["value"], json!("/next"));
}

#[tokio::test]
async fn builtin_http_surfaces_server_error_status_as_failed_outcome() {
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_status_and_body(
        500,
        br#"{"error":"internal"}"#.to_vec(),
    ));
    let runtime = runtime_with_http_egress(Arc::clone(&egress));

    let failure = invoke_failure_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({"url": "https://api.example.test/boom"}),
        execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
    )
    .await;

    assert_eq!(failure.kind, FailureKind::OperationFailed);
    assert_eq!(
        failure.safe_summary().as_deref(),
        Some("HTTP request returned status 500")
    );
    let Some(DispatchFailureDetail::Diagnostic { text }) = failure.detail else {
        panic!("HTTP error response must remain available as diagnostic context");
    };
    let response: Value = serde_json::from_str(&text).expect("HTTP diagnostic must be JSON");
    assert_eq!(response["status"], json!(500));
    assert_eq!(response["body_text"], json!(r#"{"error":"internal"}"#));
    assert_eq!(egress.requests().len(), 1);
}

#[tokio::test]
async fn builtin_http_save_surfaces_http_error_status_as_failed_outcome() {
    let egress = Arc::new(
        RecordingRuntimeHttpEgress::with_status_and_body(403, br#"{"message":"denied"}"#.to_vec())
            .with_saved_body("/workspace/denied.json", 20),
    );
    let runtime = runtime_with_http_egress(Arc::clone(&egress));
    let mounts = MountView::new(vec![MountGrant::new(
        MountAlias::new("/workspace").unwrap(),
        VirtualPath::new("/projects/workspace").unwrap(),
        MountPermissions::read_write(),
    )])
    .unwrap();

    let failure = invoke_failure_with_context(
        &runtime,
        HTTP_SAVE_CAPABILITY_ID,
        json!({
            "url": "https://api.example.test/private",
            "save_to": "/workspace/denied.json"
        }),
        execution_context_with_mounts_and_network(
            [HTTP_SAVE_CAPABILITY_ID],
            mounts,
            http_test_policy(),
        ),
    )
    .await;

    assert_eq!(failure.kind, FailureKind::OperationFailed);
    assert_eq!(
        failure.safe_summary().as_deref(),
        Some("HTTP request returned status 403")
    );
    let Some(DispatchFailureDetail::Diagnostic { text }) = failure.detail else {
        panic!("HTTP error response must remain available as diagnostic context");
    };
    let response: Value = serde_json::from_str(&text).expect("HTTP diagnostic must be JSON");
    assert_eq!(response["status"], json!(403));
    assert_eq!(
        response["saved_body"],
        json!({"path": "/workspace/denied.json", "bytes_written": 20})
    );
    assert!(
        response.get("body_text").is_none(),
        "save-mode diagnostics carry saved_body metadata, not the inline body"
    );

    let requests = egress.requests();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].save_body_to.is_some(),
        "save-mode error path must still use strict host egress with a save target"
    );
}

#[tokio::test]
async fn builtin_http_classifies_status_range_boundaries() {
    for status in [400u16, 599] {
        let egress = Arc::new(RecordingRuntimeHttpEgress::with_status_and_body(
            status,
            Vec::new(),
        ));
        let runtime = runtime_with_http_egress(Arc::clone(&egress));

        let failure = invoke_failure_with_context(
            &runtime,
            HTTP_CAPABILITY_ID,
            json!({"url": "https://api.example.test/edge"}),
            execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
        )
        .await;
        assert_eq!(
            failure.kind,
            FailureKind::OperationFailed,
            "status {status} must classify as a failure"
        );
    }
    for status in [100u16, 304, 600] {
        let egress = Arc::new(RecordingRuntimeHttpEgress::with_status_and_body(
            status,
            Vec::new(),
        ));
        let runtime = runtime_with_http_egress(Arc::clone(&egress));

        let output = invoke_with_context(
            &runtime,
            HTTP_CAPABILITY_ID,
            json!({"url": "https://api.example.test/edge"}),
            execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
        )
        .await
        .unwrap_or_else(|error| {
            panic!("status {status} must stay an inspectable result, got {error:?}")
        });
        assert_eq!(output["status"], json!(status));
    }
}

#[tokio::test]
async fn builtin_http_error_diagnostic_respects_model_diagnostic_budget() {
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_status_and_body(
        403,
        vec![b'a'; 16 * 1024],
    ));
    let runtime = runtime_with_http_egress(Arc::clone(&egress));

    let failure = invoke_failure_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({"url": "https://api.example.test/private"}),
        execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
    )
    .await;

    assert_eq!(failure.kind, FailureKind::OperationFailed);
    let Some(DispatchFailureDetail::Diagnostic { text }) = failure.detail else {
        panic!("HTTP error response must remain available as diagnostic context");
    };
    assert!(
        text.len() <= MODEL_DIAGNOSTIC_MAX_BYTES,
        "diagnostic must fit the model-visible budget, got {} bytes",
        text.len()
    );
    let response: Value =
        serde_json::from_str(&text).expect("trimmed diagnostic must stay valid JSON");
    assert_eq!(
        response["status"],
        json!(403),
        "status must survive the budget trim"
    );
    assert_eq!(response["truncation"]["body"], json!(true));
    assert!(
        response["body_text"].as_str().is_some(),
        "trimmed error body must remain visible in the diagnostic"
    );
    assert_eq!(egress.requests().len(), 1);
}

#[tokio::test]
async fn builtin_http_error_diagnostic_preserves_egress_truncation_flag() {
    // The egress returns a partial body when it hits the caller's
    // response_body_limit; the failure diagnostic must keep reporting that
    // truncation instead of presenting the partial body as complete.
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_status_and_body(
        403,
        b"partial body that exceeds the one-byte cap".to_vec(),
    ));
    let runtime = runtime_with_http_egress(Arc::clone(&egress));

    let failure = invoke_failure_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({
            "url": "https://api.example.test/private",
            "response_body_limit": 1
        }),
        execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
    )
    .await;

    assert_eq!(failure.kind, FailureKind::OperationFailed);
    let Some(DispatchFailureDetail::Diagnostic { text }) = failure.detail else {
        panic!("HTTP error response must remain available as diagnostic context");
    };
    let response: Value = serde_json::from_str(&text).expect("diagnostic must stay valid JSON");
    assert_eq!(response["status"], json!(403));
    assert_eq!(
        response["body_truncated"],
        json!(true),
        "egress truncation at the caller cap must stay visible"
    );
    assert_eq!(response["truncation"]["body"], json!(true));
}

#[tokio::test]
async fn builtin_http_requires_tool_call_http_egress_for_inline_output() {
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_body(b"ok".to_vec()));
    let runtime = runtime_with_strict_http_egress_only(Arc::clone(&egress));

    let failure = invoke_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({
            "url": "https://api.example.test/v1/items"
        }),
        execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
    )
    .await
    .unwrap_err();

    assert_eq!(failure, FailureKind::Network);
    assert!(egress.requests().is_empty());
}

#[tokio::test]
async fn builtin_http_preserves_exact_body_cap_for_text_responses() {
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_body(vec![b'a'; 4096]));
    let runtime = runtime_with_http_egress(Arc::clone(&egress));

    let output = invoke_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({
            "url": "https://api.example.test/v1/items",
            "response_body_limit": 4096
        }),
        execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
    )
    .await
    .unwrap();

    let body_text = output["body_text"].as_str().expect("text response");
    assert_eq!(body_text.len(), 4096);
    assert_eq!(output.get("body_truncated"), None);
    assert!(output.get("body_base64").is_none());
    assert!(serialized_json_len(&output) <= 6_000);
}

#[tokio::test]
async fn builtin_http_truncates_one_byte_over_text_responses_with_a_hint() {
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_body(vec![b'a'; 4097]));
    let runtime = runtime_with_http_egress(Arc::clone(&egress));

    let output = invoke_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({
            "url": "https://api.example.test/v1/items",
            "response_body_limit": 4096
        }),
        execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
    )
    .await
    .unwrap();

    let body_text = output["body_text"].as_str().expect("text response");
    assert_eq!(body_text.len(), 4096);
    assert_eq!(output["body_truncated"], json!(true));
    assert_eq!(output["truncation"]["body"], json!(true));
    assert_eq!(output["truncation"]["headers"], json!(false));
    assert_eq!(output["truncation"]["bytes_returned"], json!(4096));
    assert_eq!(
        output["truncation"]["reason"],
        json!("model_visible_budget")
    );
    assert!(
        output["body_truncation_hint"]
            .as_str()
            .expect("hint")
            .contains("builtin.http.save")
    );
    assert!(output.get("body_base64").is_none());
    assert!(serialized_json_len(&output) <= 6_000);
}

#[tokio::test]
async fn builtin_http_default_large_response_result_stays_under_documented_cap() {
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_body(vec![
        b'a';
        1024 * 1024
    ]));
    let runtime = runtime_with_http_egress(Arc::clone(&egress));

    let output = invoke_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({
            "url": "https://api.example.test/large-page"
        }),
        execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
    )
    .await
    .unwrap();

    let body_text = output["body_text"].as_str().expect("text response");
    assert_eq!(body_text.len(), 48 * 1024);
    assert_eq!(output["body_truncated"], json!(true));
    assert!(
        output["body_truncation_hint"]
            .as_str()
            .expect("hint")
            .contains("builtin.http.save")
    );
    assert!(serialized_json_len(&output) <= 50 * 1024);

    let requests = egress.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].response_body_limit, Some(48 * 1024));
}

#[tokio::test]
async fn builtin_http_truncates_escaped_text_to_serialized_body_budget() {
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_body(
        "\n".repeat(4096).into_bytes(),
    ));
    let runtime = runtime_with_http_egress(Arc::clone(&egress));

    let output = invoke_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({
            "url": "https://api.example.test/escaped",
            "response_body_limit": 4096
        }),
        execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
    )
    .await
    .unwrap();

    let body_text = output["body_text"].as_str().expect("text response");
    let serialized_body = serde_json::to_string(body_text).unwrap();
    assert!(serialized_body.len().saturating_sub(2) <= 4096);
    assert_eq!(body_text.len(), 2048);
    assert_eq!(output["body_bytes_returned"], json!(2048));
    assert_eq!(output["body_truncated"], json!(true));
    assert_eq!(output["truncation"]["bytes_returned"], json!(2048));
}

#[tokio::test]
async fn builtin_http_save_passes_save_to_and_returns_saved_body_metadata() {
    let egress = Arc::new(
        RecordingRuntimeHttpEgress::with_body(br#"{"accepted":true}"#.to_vec())
            .with_saved_body("/workspace/response.json", 17),
    );
    let runtime = runtime_with_http_egress(Arc::clone(&egress));
    let mounts = MountView::new(vec![MountGrant::new(
        MountAlias::new("/workspace").unwrap(),
        VirtualPath::new("/projects/workspace").unwrap(),
        MountPermissions::read_write(),
    )])
    .unwrap();

    let output = invoke_with_context(
        &runtime,
        HTTP_SAVE_CAPABILITY_ID,
        json!({
            "url": "https://api.example.test/v1/items",
            "save_to": "/workspace/response.json"
        }),
        execution_context_with_mounts_and_network(
            [HTTP_SAVE_CAPABILITY_ID],
            mounts,
            http_test_policy(),
        ),
    )
    .await
    .unwrap_or_else(|error| {
        panic!(
            "expected saved-body HTTP success, got {error:?}; recorded requests: {:?}",
            egress.requests()
        )
    });

    assert_eq!(output["status"], json!(200));
    assert_eq!(
        output["saved_body"],
        json!({
            "path": "/workspace/response.json",
            "bytes_written": 17
        })
    );
    assert!(output.get("body_text").is_none());
    assert!(output.get("body_base64").is_none());

    let requests = egress.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].response_body_limit, Some(10 * 1024 * 1024));
    let save_target = requests[0]
        .save_body_to
        .as_ref()
        .expect("save_to should be passed to host egress");
    assert_eq!(save_target.path.as_str(), "/workspace/response.json");
    let mount_grant = save_target
        .mount_grant
        .as_ref()
        .expect("save_to should carry narrowed mount authority");
    let mount_view = MountView::new(vec![mount_grant.clone()]).unwrap();
    let (virtual_path, grant) = mount_view
        .resolve_with_grant(&save_target.path)
        .expect("saved path should resolve through captured mount view");
    assert_eq!(
        virtual_path,
        VirtualPath::new("/projects/workspace/response.json").unwrap()
    );
    assert!(grant.permissions.write);
}

#[tokio::test]
async fn builtin_http_save_rejects_response_body_limit_above_save_ceiling_before_egress() {
    let egress = Arc::new(RecordingRuntimeHttpEgress::default());
    let runtime = runtime_with_http_egress(Arc::clone(&egress));
    let mounts = MountView::new(vec![MountGrant::new(
        MountAlias::new("/workspace").unwrap(),
        VirtualPath::new("/projects/workspace").unwrap(),
        MountPermissions::read_write(),
    )])
    .unwrap();

    let error = invoke_with_context(
        &runtime,
        HTTP_SAVE_CAPABILITY_ID,
        json!({
            "url": "https://api.example.test/v1/items",
            "save_to": "/workspace/response.json",
            "response_body_limit": 10 * 1024 * 1024 + 1
        }),
        execution_context_with_mounts_and_network(
            [HTTP_SAVE_CAPABILITY_ID],
            mounts,
            http_test_policy(),
        ),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::InputEncode);
    assert!(egress.requests().is_empty());
}

#[tokio::test]
async fn builtin_http_save_returns_saved_body_for_large_responses_without_inline_body() {
    let egress = Arc::new(
        RecordingRuntimeHttpEgress::with_body(vec![b'a'; 12 * 1024])
            .with_saved_body("/workspace/large-response.json", 12 * 1024),
    );
    let runtime = runtime_with_http_egress(Arc::clone(&egress));
    let mounts = MountView::new(vec![MountGrant::new(
        MountAlias::new("/workspace").unwrap(),
        VirtualPath::new("/projects/workspace").unwrap(),
        MountPermissions::read_write(),
    )])
    .unwrap();

    let output = invoke_with_context(
        &runtime,
        HTTP_SAVE_CAPABILITY_ID,
        json!({
            "url": "https://api.example.test/v1/items",
            "save_to": "/workspace/large-response.json",
            "response_body_limit": 4096
        }),
        execution_context_with_mounts_and_network(
            [HTTP_SAVE_CAPABILITY_ID],
            mounts,
            http_test_policy(),
        ),
    )
    .await
    .unwrap();

    assert_eq!(
        output["saved_body"],
        json!({
            "path": "/workspace/large-response.json",
            "bytes_written": 12 * 1024
        })
    );
    assert!(output.get("body_text").is_none());
    assert!(output.get("body_base64").is_none());
    assert!(serialized_json_len(&output) <= 2_000);

    let requests = egress.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].response_body_limit, Some(4096));
}

#[tokio::test]
async fn builtin_http_save_succeeds_with_strict_host_egress_only() {
    let egress = Arc::new(
        RecordingRuntimeHttpEgress::with_body(vec![b'a'; 12 * 1024])
            .with_saved_body("/workspace/strict-only-save.json", 12 * 1024),
    );
    let runtime = runtime_with_strict_http_egress_only(Arc::clone(&egress));
    let mounts = MountView::new(vec![MountGrant::new(
        MountAlias::new("/workspace").unwrap(),
        VirtualPath::new("/projects/workspace").unwrap(),
        MountPermissions::read_write(),
    )])
    .unwrap();

    let output = invoke_with_context(
        &runtime,
        HTTP_SAVE_CAPABILITY_ID,
        json!({
            "url": "https://api.example.test/v1/items",
            "save_to": "/workspace/strict-only-save.json",
            "response_body_limit": 4096
        }),
        execution_context_with_mounts_and_network(
            [HTTP_SAVE_CAPABILITY_ID],
            mounts,
            http_test_policy(),
        ),
    )
    .await
    .unwrap();

    assert_eq!(
        output["saved_body"],
        json!({
            "path": "/workspace/strict-only-save.json",
            "bytes_written": 12 * 1024
        })
    );
    let requests = egress.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].response_body_limit, Some(4096));
}

#[tokio::test]
async fn builtin_http_save_uses_strict_host_egress_when_tool_call_port_exists() {
    let strict_egress = Arc::new(
        RecordingRuntimeHttpEgress::with_body(vec![b'a'; 12 * 1024])
            .with_saved_body("/workspace/strict-save.json", 12 * 1024),
    );
    let runtime = HostRuntimeServices::new(
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
    .with_runtime_http_egress(Arc::clone(&strict_egress))
    .with_tool_call_http_egress(Arc::new(PanickingToolCallHttpEgress))
    .with_trust_policy(Arc::new(trust_policy()))
    .host_runtime_for_local_testing();
    let mounts = MountView::new(vec![MountGrant::new(
        MountAlias::new("/workspace").unwrap(),
        VirtualPath::new("/projects/workspace").unwrap(),
        MountPermissions::read_write(),
    )])
    .unwrap();

    let output = invoke_with_context(
        &runtime,
        HTTP_SAVE_CAPABILITY_ID,
        json!({
            "url": "https://api.example.test/v1/items",
            "save_to": "/workspace/strict-save.json",
            "response_body_limit": 4096
        }),
        execution_context_with_mounts_and_network(
            [HTTP_SAVE_CAPABILITY_ID],
            mounts,
            http_test_policy(),
        ),
    )
    .await
    .unwrap();

    assert_eq!(
        output["saved_body"],
        json!({
            "path": "/workspace/strict-save.json",
            "bytes_written": 12 * 1024
        })
    );
    let requests = strict_egress.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].response_body_limit, Some(4096));
}

#[tokio::test]
async fn builtin_http_does_not_inline_huge_binary_payloads() {
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_body(vec![0xFF; 8 * 1024]));
    let runtime = runtime_with_http_egress(Arc::clone(&egress));

    let output = invoke_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({
            "url": "https://api.example.test/v1/items",
            "response_body_limit": 4096
        }),
        execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
    )
    .await
    .unwrap();

    assert_eq!(output["body_truncated"], json!(true));
    assert_eq!(output["body_base64_omitted"], json!(true));
    assert_eq!(output["body_bytes_returned"], json!(0));
    assert_eq!(output["truncation"]["body"], json!(true));
    assert_eq!(output["truncation"]["bytes_returned"], json!(0));
    assert!(
        output["body_truncation_hint"]
            .as_str()
            .expect("hint")
            .contains("builtin.http.save")
    );
    assert!(output.get("body_base64").is_none());
    assert!(output.get("body_text").is_none());
    assert!(serialized_json_len(&output) <= 6_000);
}

#[tokio::test]
async fn builtin_http_truncates_tiny_binary_responses_without_panicking() {
    for response_body_limit in 1..=3 {
        let egress = Arc::new(RecordingRuntimeHttpEgress::with_body(vec![0xFF; 8]));
        let runtime = runtime_with_http_egress(Arc::clone(&egress));

        let output = invoke_with_context(
            &runtime,
            HTTP_CAPABILITY_ID,
            json!({
                "url": "https://api.example.test/v1/items",
                "response_body_limit": response_body_limit
            }),
            execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
        )
        .await
        .unwrap();

        assert_eq!(output["body_base64"], json!(""));
        assert_eq!(output["body_truncated"], json!(true));
        assert_eq!(output["body_bytes_returned"], json!(0));
        assert_eq!(output["truncation"]["body"], json!(true));
        assert_eq!(output["truncation"]["bytes_returned"], json!(0));
        assert!(serialized_json_len(&output) <= 2 * 1024 + response_body_limit as usize);
    }
}

#[tokio::test]
async fn builtin_http_final_budget_trim_preserves_base64_alignment() {
    let headers = (0..2)
        .map(|index| (format!("x-large-{index}"), "h".repeat(512)))
        .collect::<Vec<_>>();
    let egress =
        Arc::new(RecordingRuntimeHttpEgress::with_body(vec![0xFF; 512]).with_headers(headers));
    let runtime = runtime_with_http_egress(Arc::clone(&egress));

    let output = invoke_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({
            "url": "https://api.example.test/v1/items",
            "response_body_limit": 512
        }),
        execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
    )
    .await
    .unwrap();

    let body_base64 = output["body_base64"].as_str().expect("binary response");
    assert_eq!(output["body_truncated"], json!(true));
    assert!(!body_base64.is_empty());
    assert!(body_base64.len() < 684);
    assert_eq!(body_base64.len() % 4, 0);
    assert_eq!(output["truncation"]["body"], json!(true));
    assert!(serialized_json_len(&output) <= 3_000);
}

#[tokio::test]
async fn builtin_http_final_budget_trims_headers_when_body_cannot_absorb_overage() {
    let headers = (0..32)
        .map(|index| (format!("x-large-{index}"), "h".repeat(1024)))
        .collect::<Vec<_>>();
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_body(Vec::new()).with_headers(headers));
    let runtime = runtime_with_http_egress(Arc::clone(&egress));

    let output = invoke_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({
            "url": "https://api.example.test/v1/items",
            "response_body_limit": 1
        }),
        execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
    )
    .await
    .unwrap();

    assert_eq!(output["headers_truncated"], json!(true));
    assert_eq!(output["truncation"]["headers"], json!(true));
    assert!(serialized_json_len(&output) <= 2 * 1024 + 1);
}

#[tokio::test]
async fn builtin_http_save_final_budget_trims_headers_without_inlining_body() {
    let headers = (0..32)
        .map(|index| (format!("x-large-{index}"), "h".repeat(1024)))
        .collect::<Vec<_>>();
    let egress = Arc::new(
        RecordingRuntimeHttpEgress::with_body(vec![b'a'; 12 * 1024])
            .with_saved_body("/workspace/header-heavy-save.json", 12 * 1024)
            .with_headers(headers),
    );
    let runtime = runtime_with_http_egress(Arc::clone(&egress));
    let mounts = MountView::new(vec![MountGrant::new(
        MountAlias::new("/workspace").unwrap(),
        VirtualPath::new("/projects/workspace").unwrap(),
        MountPermissions::read_write(),
    )])
    .unwrap();

    let output = invoke_with_context(
        &runtime,
        HTTP_SAVE_CAPABILITY_ID,
        json!({
            "url": "https://api.example.test/v1/items",
            "save_to": "/workspace/header-heavy-save.json",
            "response_body_limit": 1
        }),
        execution_context_with_mounts_and_network(
            [HTTP_SAVE_CAPABILITY_ID],
            mounts,
            http_test_policy(),
        ),
    )
    .await
    .unwrap();

    assert!(output.get("body_text").is_none());
    assert!(output.get("body_base64").is_none());
    assert_eq!(output["headers_truncated"], json!(true));
    assert_eq!(output["truncation"]["headers"], json!(true));
    assert!(serialized_json_len(&output) <= 2 * 1024 + 1);
}

#[tokio::test]
async fn builtin_http_truncates_overlong_response_headers_to_model_visible_budget() {
    let mut headers = vec![(format!("x-{}", "n".repeat(200)), "\n".repeat(2 * 1024))];
    for index in 0..40 {
        headers.push((format!("x-extra-{index}"), "ok".to_string()));
    }
    let egress =
        Arc::new(RecordingRuntimeHttpEgress::with_body(b"ok".to_vec()).with_headers(headers));
    let runtime = runtime_with_http_egress(Arc::clone(&egress));

    let output = invoke_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({
            "url": "https://api.example.test/v1/items",
            "response_body_limit": 4096
        }),
        execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
    )
    .await
    .unwrap();

    let headers = output["headers"].as_array().expect("headers array");
    assert!(headers.len() <= 32);
    assert_eq!(output["headers_truncated"], json!(true));
    assert_eq!(output["truncation"]["headers"], json!(true));
    assert_eq!(output["truncation"]["body"], json!(false));
    assert_eq!(headers[0]["name"].as_str().unwrap().len(), 128);
    let header_value = headers[0]["value"].as_str().unwrap();
    assert_eq!(header_value.len(), 512);
    assert!(
        serde_json::to_string(header_value)
            .unwrap()
            .len()
            .saturating_sub(2)
            <= 1024
    );
    assert_eq!(headers[0]["truncated"], json!(true));
    assert!(serialized_json_len(&output["headers"]) <= 8 * 1024);
}

#[tokio::test]
async fn builtin_http_reports_body_and_header_truncation_together() {
    let mut headers = vec![(format!("x-{}", "n".repeat(200)), "\n".repeat(2 * 1024))];
    for index in 0..40 {
        headers.push((format!("x-extra-{index}"), "ok".to_string()));
    }
    let egress = Arc::new(
        RecordingRuntimeHttpEgress::with_body(vec![b'a'; 1024 * 1024]).with_headers(headers),
    );
    let runtime = runtime_with_http_egress(Arc::clone(&egress));

    let output = invoke_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({
            "url": "https://api.example.test/large-page"
        }),
        execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
    )
    .await
    .unwrap();

    assert_eq!(output["headers_truncated"], json!(true));
    assert_eq!(output["body_truncated"], json!(true));
    assert_eq!(output["truncation"]["headers"], json!(true));
    assert_eq!(output["truncation"]["body"], json!(true));
    assert!(
        output["truncation"]["bytes_returned"]
            .as_u64()
            .expect("bytes returned")
            < 48 * 1024
    );
    assert!(output["headers"][0]["truncated"].as_bool().unwrap());
    assert!(serialized_json_len(&output) <= 50 * 1024);
}

#[tokio::test]
async fn builtin_http_keeps_sensitive_material_out_of_sanitized_output() {
    let egress = Arc::new(
        RecordingRuntimeHttpEgress::with_status_and_body(200, b"sanitized response body".to_vec())
            .with_headers(vec![
                (
                    "content-type".to_string(),
                    "text/plain; charset=utf-8".to_string(),
                ),
                ("x-request-id".to_string(), "sanitized-request".to_string()),
            ])
            .with_redaction_applied(true),
    );
    let runtime = runtime_with_http_egress(Arc::clone(&egress));

    let output = invoke_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({
            "url": "https://api.example.test/v1/items",
            "headers": {
                "authorization": "Bearer sk-provider-secret",
                "x-debug-token": "RAW_SECRET"
            },
            "body": {
                "token": "RAW_SECRET"
            },
            "response_body_limit": 4096
        }),
        execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
    )
    .await
    .unwrap();

    assert_eq!(output["redaction_applied"], json!(true));
    assert_eq!(output["headers"][0]["name"], json!("content-type"));
    assert_eq!(output["headers"][1]["value"], json!("sanitized-request"));

    let serialized = serde_json::to_string(&output).unwrap();
    assert!(!serialized.contains("sk-provider-secret"));
    assert!(!serialized.contains("RAW_SECRET"));
    assert!(serialized.contains("sanitized response body"));
}

#[tokio::test]
async fn builtin_http_does_not_report_redaction_as_truncation() {
    let egress = Arc::new(
        RecordingRuntimeHttpEgress::with_body(b"sanitized".to_vec())
            .with_redaction_applied(true)
            .with_response_bytes(1024),
    );
    let runtime = runtime_with_http_egress(Arc::clone(&egress));

    let output = invoke_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({
            "url": "https://api.example.test/v1/items",
            "response_body_limit": 4096
        }),
        execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
    )
    .await
    .unwrap();

    assert_eq!(output["redaction_applied"], json!(true));
    assert_eq!(output["body_text"], json!("sanitized"));
    assert!(output.get("body_truncated").is_none());
    assert!(output.get("body_bytes_returned").is_none());
    assert!(output.get("truncation").is_none());
}

#[tokio::test]
async fn builtin_http_rejects_save_to_on_network_only_capability_before_egress() {
    let egress = Arc::new(RecordingRuntimeHttpEgress::default());
    let runtime = runtime_with_http_egress(Arc::clone(&egress));
    let mounts = MountView::new(vec![MountGrant::new(
        MountAlias::new("/workspace").unwrap(),
        VirtualPath::new("/projects/workspace").unwrap(),
        MountPermissions::read_write(),
    )])
    .unwrap();

    let error = invoke_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({
            "url": "https://api.example.test/v1/items",
            "save_to": "/workspace/response.json"
        }),
        execution_context_with_mounts_and_network([HTTP_CAPABILITY_ID], mounts, http_test_policy()),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::InputEncode);
    assert!(egress.requests().is_empty());
}

#[tokio::test]
async fn builtin_http_save_rejects_missing_save_to_before_egress() {
    let egress = Arc::new(RecordingRuntimeHttpEgress::default());
    let runtime = runtime_with_http_egress(Arc::clone(&egress));
    let mounts = MountView::new(vec![MountGrant::new(
        MountAlias::new("/workspace").unwrap(),
        VirtualPath::new("/projects/workspace").unwrap(),
        MountPermissions::read_write(),
    )])
    .unwrap();

    let error = invoke_with_context(
        &runtime,
        HTTP_SAVE_CAPABILITY_ID,
        json!({
            "url": "https://api.example.test/v1/items"
        }),
        execution_context_with_mounts_and_network(
            [HTTP_SAVE_CAPABILITY_ID],
            mounts,
            http_test_policy(),
        ),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::InputEncode);
    assert!(egress.requests().is_empty());
}

#[tokio::test]
async fn builtin_http_save_rejects_save_to_without_mount_authority_before_egress() {
    let egress = Arc::new(RecordingRuntimeHttpEgress::default());
    let runtime = runtime_with_http_egress(Arc::clone(&egress));

    let error = invoke_with_context(
        &runtime,
        HTTP_SAVE_CAPABILITY_ID,
        json!({
            "url": "https://api.example.test/v1/items",
            "save_to": "/workspace/response.json"
        }),
        execution_context_with_network([HTTP_SAVE_CAPABILITY_ID], http_test_policy()),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::InputEncode);
    assert!(egress.requests().is_empty());
}

#[tokio::test]
async fn builtin_http_save_rejects_save_to_without_write_mount_before_egress() {
    let egress = Arc::new(RecordingRuntimeHttpEgress::default());
    let runtime = runtime_with_http_egress(Arc::clone(&egress));
    let mounts = MountView::new(vec![MountGrant::new(
        MountAlias::new("/workspace").unwrap(),
        VirtualPath::new("/projects/workspace").unwrap(),
        MountPermissions::read_only(),
    )])
    .unwrap();

    let error = invoke_with_context(
        &runtime,
        HTTP_SAVE_CAPABILITY_ID,
        json!({
            "url": "https://api.example.test/v1/items",
            "save_to": "/workspace/response.json"
        }),
        execution_context_with_mounts_and_network(
            [HTTP_SAVE_CAPABILITY_ID],
            mounts,
            http_test_policy(),
        ),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::InputEncode);
    assert!(egress.requests().is_empty());
}

#[tokio::test]
async fn builtin_http_save_rejects_invalid_or_unresolved_save_to_before_egress() {
    for save_to in ["file:///tmp/response.json", "/other/response.json"] {
        let egress = Arc::new(RecordingRuntimeHttpEgress::default());
        let runtime = runtime_with_http_egress(Arc::clone(&egress));
        let mounts = MountView::new(vec![MountGrant::new(
            MountAlias::new("/workspace").unwrap(),
            VirtualPath::new("/projects/workspace").unwrap(),
            MountPermissions::read_write(),
        )])
        .unwrap();

        let error = invoke_with_context(
            &runtime,
            HTTP_SAVE_CAPABILITY_ID,
            json!({
                "url": "https://api.example.test/v1/items",
                "save_to": save_to
            }),
            execution_context_with_mounts_and_network(
                [HTTP_SAVE_CAPABILITY_ID],
                mounts,
                http_test_policy(),
            ),
        )
        .await
        .unwrap_err();

        assert_eq!(error, FailureKind::InputEncode);
        assert!(egress.requests().is_empty());
    }
}
