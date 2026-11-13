//! Built-in http: request validation, runtime policy, and egress error mapping.

#![deny(clippy::let_underscore_must_use)]
use std::{
    net::{IpAddr, Ipv4Addr},
    sync::Arc,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use ironclaw_host_api::result_meta::FailureKind;
use ironclaw_host_api::{
    http::RuntimeHttpEgressError,
    ids::TenantId,
    resource::{LOCAL_DEFAULT_TENANT_ID, ResourceEstimate},
};
use ironclaw_host_runtime::{HTTP_CAPABILITY_ID, HostRuntime, RuntimeCapabilityOutcome};
use ironclaw_network::{
    NetworkHttpError, NetworkHttpResponse, NetworkUsage, PolicyNetworkHttpEgress,
};
use ironclaw_resources::{InMemoryResourceGovernor, ResourceAccount};
use serde_json::json;

mod first_party_builtin_tools_support;

use first_party_builtin_tools_support::{
    context::*, doubles::*, fixtures::*, invoke::*, runtimes::*,
};

#[tokio::test]
async fn builtin_http_clamps_oversized_timeout_to_runtime_ceiling() {
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_body(b"ok".to_vec()));
    let runtime = runtime_with_http_egress(Arc::clone(&egress));

    invoke_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({
            "url": "https://api.example.test/v1/items",
            "timeout_ms": 120_000
        }),
        execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
    )
    .await
    .unwrap();

    let requests = egress.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].timeout_ms, Some(30_000));
}

#[tokio::test]
async fn builtin_http_runtime_policy_denial_stops_before_egress() {
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_body(
        br#"{"ok":true}"#.to_vec(),
    ));
    let runtime = runtime_with_http_egress_and_policy(Arc::clone(&egress), network_denied_policy());

    let outcome = runtime
        .invoke_capability((
            execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
            capability_id(HTTP_CAPABILITY_ID),
            ResourceEstimate::default(),
            json!({"url": "https://api.example.test/v1/items"}),
        ))
        .await
        .unwrap();

    let RuntimeCapabilityOutcome::Failed(failure) = outcome else {
        panic!("expected runtime-policy failure, got {outcome:?}");
    };
    assert_eq!(failure.kind, FailureKind::Authorization);
    assert_eq!(failure.capability_id, capability_id(HTTP_CAPABILITY_ID));
    // Message is now the sanitized `DenyReason::PolicyDenied` form the kernel
    // produces (see #6386): it conveys a policy denial and must not leak the
    // internal `NetworkMode::` planner enum token.
    let message = failure.message.as_deref().unwrap_or_default();
    assert!(message.contains("denied"), "unexpected message: {message}");
    assert!(
        !message.contains("NetworkMode::"),
        "message leaked internal planner enum token: {message}"
    );
    assert!(egress.requests().is_empty());
}

#[tokio::test]
async fn builtin_http_uses_hosted_allowlist_plan_through_configured_egress() {
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_body(
        br#"{"ok":true}"#.to_vec(),
    ));
    let runtime = runtime_with_http_egress_and_policy(Arc::clone(&egress), hosted_dev_policy());

    let output = invoke_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({"url": "https://api.example.test/v1/items"}),
        execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
    )
    .await
    .unwrap();

    assert_eq!(output["status"], json!(200));
    assert_eq!(output["body_text"], json!(r#"{"ok":true}"#));
    assert_eq!(egress.requests().len(), 1);
}

#[tokio::test]
async fn builtin_http_defaults_json_body_content_type() {
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_body(b"ok".to_vec()));
    let runtime = runtime_with_http_egress(Arc::clone(&egress));

    invoke_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({
            "url": "https://api.example.test/v1/items",
            "body": {"ok": true}
        }),
        execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
    )
    .await
    .unwrap();

    let requests = egress.requests();
    assert_eq!(
        requests[0].headers,
        vec![("content-type".to_string(), "application/json".to_string())]
    );
}

#[tokio::test]
async fn builtin_http_fails_closed_when_policy_allows_network_but_runtime_egress_is_missing() {
    let runtime = runtime_with_policy(local_host_policy());
    let error = invoke_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({"url": "https://api.example.test/v1/items"}),
        execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::NetworkDenied);
}

#[tokio::test]
async fn builtin_http_rejects_ambiguous_body_zero_timeout_and_zero_response_limit() {
    let egress = Arc::new(RecordingRuntimeHttpEgress::default());
    let runtime = runtime_with_http_egress(egress);
    let context = execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy());

    for input in [
        json!({
            "url": "https://api.example.test/v1/items",
            "method": 123
        }),
        json!({
            "url": "https://api.example.test/v1/items",
            "body": "plain",
            "body_base64": "YmluYXJ5"
        }),
        json!({
            "url": "https://api.example.test/v1/items",
            "timeout_ms": 0
        }),
        json!({
            "url": "https://api.example.test/v1/items",
            "response_body_limit": 0
        }),
        json!({
            "url": "https://api.example.test/v1/items",
            "response_body_limit": 256 * 1024 + 1
        }),
    ] {
        let error = invoke_with_context(&runtime, HTTP_CAPABILITY_ID, input, context.clone())
            .await
            .unwrap_err();
        assert_eq!(error, FailureKind::InputEncode);
    }
}

#[tokio::test]
async fn builtin_http_rejects_request_bodies_over_network_egress_cap() {
    let egress = Arc::new(RecordingRuntimeHttpEgress::default());
    let runtime = runtime_with_http_egress(Arc::clone(&egress));
    let context = execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy());
    let oversized = vec![b'a'; 256 * 1024 + 1];

    for input in [
        json!({
            "url": "https://api.example.test/v1/items",
            "body": String::from_utf8(oversized.clone()).unwrap()
        }),
        json!({
            "url": "https://api.example.test/v1/items",
            "body_base64": BASE64_STANDARD.encode(&oversized)
        }),
    ] {
        let error = invoke_with_context(&runtime, HTTP_CAPABILITY_ID, input, context.clone())
            .await
            .unwrap_err();
        assert_eq!(error, FailureKind::InputEncode);
    }

    assert!(egress.requests().is_empty());
}

#[tokio::test]
async fn builtin_http_accounts_request_bytes_when_large_output_is_truncated() {
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_body(vec![
        b'\\';
        8 * 1024 * 1024
    ]));
    let governor = Arc::new(InMemoryResourceGovernor::new());
    let runtime = runtime_with_http_egress_and_governor(Arc::clone(&egress), Arc::clone(&governor));

    let output = invoke_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({
            "method": "post",
            "url": "https://api.example.test/v1/items",
            "body": "paid",
            "response_body_limit": 256 * 1024
        }),
        execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
    )
    .await
    .unwrap();

    let body_text = output["body_text"].as_str().expect("text response");
    assert_eq!(body_text.len(), 128 * 1024);
    assert_eq!(output["body_bytes_returned"], json!(128 * 1024));
    assert_eq!(output["body_truncated"], json!(true));
    let tenant_account = ResourceAccount::tenant(TenantId::new(LOCAL_DEFAULT_TENANT_ID).unwrap());
    assert_eq!(governor.usage_for(&tenant_account).network_egress_bytes, 4);
}

#[tokio::test]
async fn builtin_http_returns_binary_responses_as_base64() {
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_body(vec![0, 159, 255]));
    let runtime = runtime_with_http_egress(Arc::clone(&egress));

    let output = invoke_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({"url": "https://api.example.test/v1/items"}),
        execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
    )
    .await
    .unwrap();

    assert_eq!(output["body_base64"], json!("AJ//"));
    assert!(output.get("body_text").is_none());
}

#[tokio::test]
async fn builtin_http_documents_mixed_utf8_response_as_binary() {
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_body(
        b"hello\xFFworld".to_vec(),
    ));
    let runtime = runtime_with_http_egress(Arc::clone(&egress));

    let output = invoke_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({"url": "https://api.example.test/v1/items"}),
        execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
    )
    .await
    .unwrap();

    // Invalid UTF-8 makes the whole response binary so callers never receive
    // split or lossy text/body fragments.
    assert_eq!(output["body_base64"], json!("aGVsbG//d29ybGQ="));
    assert!(output.get("body_text").is_none());
}

#[tokio::test]
async fn builtin_http_rejects_invalid_header_names_and_oversized_header_sets() {
    let egress = Arc::new(RecordingRuntimeHttpEgress::default());
    let runtime = runtime_with_http_egress(egress);
    let context = execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy());
    let too_many_headers = (0..65)
        .map(|index| json!({"name": format!("x-{index}"), "value": "ok"}))
        .collect::<Vec<_>>();

    for input in [
        json!({
            "url": "https://api.example.test/v1/items",
            "headers": {"x-bad": "value\0tail"}
        }),
        json!({
            "url": "https://api.example.test/v1/items",
            "headers": [{"name": "x\0bad", "value": "ok"}]
        }),
        json!({
            "url": "https://api.example.test/v1/items",
            "headers": [{"name": "x bad", "value": "ok"}]
        }),
        json!({
            "url": "https://api.example.test/v1/items",
            "headers": [{"name": "x:bad", "value": "ok"}]
        }),
        json!({
            "url": "https://api.example.test/v1/items",
            "headers": [{"name": "x-é", "value": "ok"}]
        }),
        json!({
            "url": "https://api.example.test/v1/items",
            "headers": too_many_headers
        }),
        json!({
            "url": "https://api.example.test/v1/items",
            "headers": {"x-large": "a".repeat(8 * 1024 + 1)}
        }),
    ] {
        let error = invoke_with_context(&runtime, HTTP_CAPABILITY_ID, input, context.clone())
            .await
            .unwrap_err();
        assert_eq!(error, FailureKind::InputEncode);
    }
}

#[tokio::test]
async fn builtin_http_maps_runtime_egress_errors_by_source() {
    let context = execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy());
    let cases = [
        (
            RuntimeHttpEgressError::Credential {
                reason: "credential missing".to_string(),
            },
            FailureKind::Client,
        ),
        (
            RuntimeHttpEgressError::Request {
                reason: "sensitive_header_denied:authorization".to_string(),
                request_bytes: 0,
                response_bytes: 0,
            },
            FailureKind::InputEncode,
        ),
        (
            RuntimeHttpEgressError::Network {
                reason: "policy_denied".to_string(),
                request_bytes: 0,
                response_bytes: 0,
            },
            FailureKind::PolicyDenied,
        ),
        (
            RuntimeHttpEgressError::Network {
                reason: "response_body_limit_exceeded".to_string(),
                request_bytes: 4,
                response_bytes: 1024,
            },
            FailureKind::OutputTooLarge,
        ),
        (
            RuntimeHttpEgressError::Response {
                reason: "response_body_limit_exceeded".to_string(),
                request_bytes: 4,
                response_bytes: 1024,
            },
            FailureKind::OutputTooLarge,
        ),
        (
            RuntimeHttpEgressError::Response {
                reason: "response_decode_failed".to_string(),
                request_bytes: 4,
                response_bytes: 1024,
            },
            FailureKind::OperationFailed,
        ),
    ];

    for (error, expected) in cases {
        let runtime =
            runtime_with_http_egress(Arc::new(RecordingRuntimeHttpEgress::with_error(error)));
        let actual = invoke_with_context(
            &runtime,
            HTTP_CAPABILITY_ID,
            json!({"url": "https://api.example.test/v1/items"}),
            context.clone(),
        )
        .await
        .unwrap_err();
        assert_eq!(actual, expected);
    }
}

#[tokio::test]
async fn builtin_http_offline_runtime_egress_returns_network_failure_without_panicking() {
    let egress = Arc::new(RecordingRuntimeHttpEgress::with_error(
        RuntimeHttpEgressError::Network {
            reason: "network_unavailable".to_string(),
            request_bytes: 7,
            response_bytes: 0,
        },
    ));
    let runtime = runtime_with_http_egress(Arc::clone(&egress));

    let error = invoke_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({"url": "https://api.example.test/v1/items"}),
        execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::Network);
    assert_eq!(egress.requests().len(), 1);
}

#[tokio::test]
async fn builtin_http_maps_panicking_runtime_egress_to_backend_failure() {
    let runtime = runtime_with_http_egress(Arc::new(PanickingRuntimeHttpEgress));
    let error = invoke_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({"url": "https://api.example.test/v1/items"}),
        execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::Backend);
}

#[tokio::test]
async fn builtin_http_rejects_sensitive_headers_through_host_validator() {
    let transport = RecordingTransport::ok(NetworkHttpResponse {
        status: 200,
        headers: vec![],
        body: b"ok".to_vec(),
        usage: NetworkUsage::default(),
    });
    let requests = transport.requests.clone();
    let network = PolicyNetworkHttpEgress::new_with_resolver(
        transport,
        StaticResolver::new(vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))]),
    );
    let runtime = runtime_with_host_http_egress(network);

    let error = invoke_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({
            "url": "https://api.example.test/v1/items",
            "headers": {"authorization": "Bearer token"}
        }),
        execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::InputEncode);
    assert!(requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn builtin_http_exercises_real_policy_private_ip_rejection() {
    let transport = RecordingTransport::ok(NetworkHttpResponse {
        status: 200,
        headers: vec![],
        body: b"ok".to_vec(),
        usage: NetworkUsage::default(),
    });
    let requests = transport.requests.clone();
    let network = PolicyNetworkHttpEgress::new_with_resolver(
        transport,
        StaticResolver::new(vec![IpAddr::V4(Ipv4Addr::new(10, 0, 0, 7))]),
    );
    let runtime = runtime_with_host_http_egress(network);

    let error = invoke_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({"url": "https://api.example.test/v1/items"}),
        execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::PolicyDenied);
    assert!(requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn builtin_http_uses_tool_call_port_for_model_visible_partial_response() {
    let transport = RecordingTransport::err(NetworkHttpError::ResponseBodyLimit {
        limit: 4,
        request_bytes: 0,
        response_bytes: 5,
        partial_response: Some(NetworkHttpResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "text/plain".to_string())],
            body: b"abcd".to_vec(),
            usage: NetworkUsage {
                request_bytes: 0,
                response_bytes: 5,
                resolved_ip: None,
            },
        }),
    });
    let requests = transport.requests.clone();
    let network = PolicyNetworkHttpEgress::new_with_resolver(
        transport,
        StaticResolver::new(vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))]),
    );
    let runtime = runtime_with_host_http_egress(network);

    let output = invoke_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({
            "url": "https://api.example.test/v1/items",
            "response_body_limit": 4
        }),
        execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
    )
    .await
    .unwrap();

    assert_eq!(output["status"], json!(200));
    assert_eq!(output["body_text"], json!("abcd"));
    assert_eq!(output["body_truncated"], json!(true));
    assert_eq!(output["truncation"]["body"], json!(true));
    assert_eq!(output["response_bytes"], json!(5));
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn builtin_http_blocks_redirect_to_private_location() {
    // #6140: redirect following is host-mediated and re-authorizes every hop.
    // A 302 whose `Location` points at the link-local metadata address
    // (169.254.169.254) is followed but denied by the private-IP policy at the
    // second hop — before any request reaches it, so the SSRF target is never
    // contacted. (Previously the egress did not follow and surfaced the raw 302;
    // the SSRF outcome is preserved, now as an explicit block.)
    let transport = RecordingTransport::ok(NetworkHttpResponse {
        status: 302,
        headers: vec![(
            "location".to_string(),
            "http://169.254.169.254/latest/meta-data".to_string(),
        )],
        body: Vec::new(),
        usage: NetworkUsage::default(),
    });
    let requests = transport.requests.clone();
    let network = PolicyNetworkHttpEgress::new_with_resolver(
        transport,
        StaticResolver::new(vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))]),
    );
    let runtime = runtime_with_host_http_egress(network);

    let error = invoke_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({"url": "https://api.example.test/v1/items"}),
        execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::PolicyDenied);
    // Only the initial request was attempted; the redirect's link-local target
    // was rejected at re-authorization, never contacted.
    let recorded = requests.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].url, "https://api.example.test/v1/items");
}

#[tokio::test]
async fn builtin_http_awaits_async_egress_without_blocking_tokio_worker() {
    let egress = Arc::new(SleepingRuntimeHttpEgress {
        delay: Duration::from_millis(100),
        body: Vec::new(),
    });
    let runtime = runtime_with_http_egress(egress);
    let invocation = invoke_with_context(
        &runtime,
        HTTP_CAPABILITY_ID,
        json!({"url": "https://api.example.test/v1/items"}),
        execution_context_with_network([HTTP_CAPABILITY_ID], http_test_policy()),
    );
    tokio::pin!(invocation);

    tokio::select! {
        _ = &mut invocation => panic!("HTTP dispatch should remain pending while async egress sleeps"),
        _ = tokio::time::sleep(Duration::from_millis(20)) => {}
    }

    invocation.await.unwrap();
}
