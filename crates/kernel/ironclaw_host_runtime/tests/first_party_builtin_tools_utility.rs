//! Built-in echo, plan, time, json, and spawn-subagent capabilities.

use ironclaw_host_api::result_meta::FailureKind;
use ironclaw_host_api::{
    dispatch::DispatchInputIssueCode, mount::MountPermissions, resource::ResourceEstimate,
};
use ironclaw_host_runtime::{
    ECHO_CAPABILITY_ID, GLOB_CAPABILITY_ID, GREP_CAPABILITY_ID, HostRuntime, JSON_CAPABILITY_ID,
    LIST_DIR_CAPABILITY_ID, PLAN_UPDATE_CAPABILITY_ID, RuntimeCapabilityOutcome,
    SPAWN_SUBAGENT_CAPABILITY_ID, TIME_CAPABILITY_ID,
};
use serde_json::{Value, json};

mod first_party_builtin_tools_support;

use first_party_builtin_tools_support::{context::*, fixtures::*, invoke::*, runtimes::*};

#[tokio::test]
async fn builtin_echo_invokes_through_host_runtime() {
    let output = invoke(ECHO_CAPABILITY_ID, json!({"message": "hello reborn"}))
        .await
        .unwrap();
    assert_eq!(output, Value::String("hello reborn".to_string()));
}

/// The native agent's checklist runs ungated through the host and hands the
/// plan back as an `agent_plan` display preview the projection can stream.
#[tokio::test]
async fn builtin_plan_update_runs_ungated_and_returns_the_plan_as_a_preview() {
    let runtime = runtime();
    let outcome = runtime
        .invoke_capability((
            execution_context([PLAN_UPDATE_CAPABILITY_ID]),
            capability_id(PLAN_UPDATE_CAPABILITY_ID),
            ResourceEstimate::default(),
            json!({"entries": [
                {"content": "Find the bug", "status": "completed"},
                {"content": "Fix it", "status": "in_progress", "active_form": "Fixing it"}
            ]}),
        ))
        .await
        .unwrap();
    let RuntimeCapabilityOutcome::Completed(completed) = outcome else {
        panic!("plan update should complete without a gate, got {outcome:?}");
    };
    assert_eq!(
        completed.output["progress"],
        json!({"completed": 1, "total": 2})
    );
    let preview = completed.display_preview.expect("plan preview");
    assert_eq!(
        preview.output_kind,
        ironclaw_host_api::agent_plan::AGENT_PLAN_OUTPUT_KIND
    );
    let plan: ironclaw_host_api::agent_plan::AgentPlan =
        serde_json::from_str(&preview.output_preview).unwrap();
    assert_eq!(plan.current().map(|e| e.content.as_str()), Some("Fix it"));

    let refused = invoke(PLAN_UPDATE_CAPABILITY_ID, json!({"entries": []})).await;
    assert!(refused.is_err(), "an empty plan is refused: {refused:?}");
}

#[tokio::test]
async fn builtin_time_tolerates_null_string_sentinels_in_optional_fields() {
    // Weaker models (e.g. quantized local models) tend to fill every optional
    // parameter with the literal string "null" instead of omitting it. Those
    // sentinels in optional fields must be treated as absent, not abort the run.
    let output = invoke(
        TIME_CAPABILITY_ID,
        json!({
            "operation": "now",
            "timezone": "null",
            "from_timezone": "null",
            "format": "null"
        }),
    )
    .await
    .unwrap();
    assert!(
        output.get("iso").and_then(Value::as_str).is_some(),
        "time `now` should return an iso timestamp, got {output:?}"
    );
}

#[tokio::test]
async fn builtin_time_now_accepts_utc_offset_compatibility_input() {
    let output = invoke(
        TIME_CAPABILITY_ID,
        json!({
            "operation": "now",
            "utc_offset": "+03:00"
        }),
    )
    .await
    .unwrap();

    assert_eq!(output["utc_offset"], json!("+03:00"));
    assert!(
        output
            .get("local_iso")
            .and_then(Value::as_str)
            .is_some_and(|value| value.ends_with("+03:00")),
        "time `now` should return local_iso with requested offset, got {output:?}"
    );
}

#[tokio::test]
async fn builtin_time_now_rejects_invalid_utc_offset() {
    let failure = invoke(
        TIME_CAPABILITY_ID,
        json!({
            "operation": "now",
            "utc_offset": "not-an-offset"
        }),
    )
    .await
    .unwrap_err();

    assert_eq!(failure, FailureKind::InputEncode);
}

#[tokio::test]
async fn builtin_echo_preserves_null_string_in_required_field() {
    // The optional-sentinel normalization must never touch required fields, so a
    // deliberate "null" payload still round-trips unchanged.
    let output = invoke(ECHO_CAPABILITY_ID, json!({"message": "null"}))
        .await
        .unwrap();
    assert_eq!(output, Value::String("null".to_string()));
}

#[tokio::test]
async fn builtin_coding_tools_tolerate_null_sentinels_in_optional_fields() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("qa-builtins.md"), "Alpha\nBeta\nGamma\n").unwrap();

    let (filesystem, mounts) = mounted_filesystem(temp.path(), MountPermissions::read_only());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts(
        [
            LIST_DIR_CAPABILITY_ID,
            GLOB_CAPABILITY_ID,
            GREP_CAPABILITY_ID,
        ],
        mounts,
    );

    let listed = invoke_with_context(
        &runtime,
        LIST_DIR_CAPABILITY_ID,
        json!({
            "path": "/workspace",
            "recursive": "null",
            "max_depth": "null"
        }),
        context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(listed["entries"], json!(["qa-builtins.md (17B)"]));

    let globbed = invoke_with_context(
        &runtime,
        GLOB_CAPABILITY_ID,
        json!({
            "path": "/workspace",
            "pattern": "qa-*.md",
            "max_results": "null"
        }),
        context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(globbed["files"], json!(["qa-builtins.md"]));

    let grepped = invoke_with_context(
        &runtime,
        GREP_CAPABILITY_ID,
        json!({
            "path": "/workspace",
            "pattern": "Beta",
            "context": "null",
            "before_context": "null",
            "after_context": "null",
            "head_limit": "null",
            "offset": "null"
        }),
        context,
    )
    .await
    .unwrap();
    assert_eq!(grepped["files"], json!(["qa-builtins.md"]));
}

#[tokio::test]
async fn builtin_spawn_subagent_authorization_invokes_through_host_runtime() {
    let output = invoke(SPAWN_SUBAGENT_CAPABILITY_ID, json!({}))
        .await
        .unwrap();
    assert_eq!(output, json!({"authorized": true}));
}

#[tokio::test]
async fn builtin_time_parse_convert_and_diff_are_deterministic() {
    let parsed = invoke(
        TIME_CAPABILITY_ID,
        json!({"operation": "parse", "input": "2026-05-12T13:00:00Z"}),
    )
    .await
    .unwrap();
    assert_eq!(parsed["unix"], json!(1778590800));

    let converted = invoke(
        TIME_CAPABILITY_ID,
        json!({
            "operation": "convert",
            "input": "2026-05-12T13:00:00Z",
            "to_timezone": "America/New_York"
        }),
    )
    .await
    .unwrap();
    assert_eq!(converted["output"], json!("2026-05-12T09:00:00-04:00"));

    let diff = invoke(
        TIME_CAPABILITY_ID,
        json!({
            "operation": "diff",
            "input": "2026-05-12T13:00:00Z",
            "timestamp2": "2026-05-12T15:30:00Z"
        }),
    )
    .await
    .unwrap();
    assert_eq!(diff["minutes"], json!(150));
}

#[tokio::test]
async fn builtin_time_accepts_unix_seconds_millis_and_slack_fractional_timestamps() {
    let parsed_seconds = invoke(
        TIME_CAPABILITY_ID,
        json!({"operation": "parse", "input": 1778590800}),
    )
    .await
    .unwrap();
    assert_eq!(parsed_seconds["iso"], json!("2026-05-12T13:00:00+00:00"));

    let parsed_millis = invoke(
        TIME_CAPABILITY_ID,
        json!({"operation": "parse", "timestamp": 1778590800123_i64}),
    )
    .await
    .unwrap();
    assert_eq!(parsed_millis["unix_millis"], json!(1778590800123_i64));

    let parsed_float_millis = invoke(
        TIME_CAPABILITY_ID,
        json!({"operation": "parse", "input": 1778590800123.0}),
    )
    .await
    .unwrap();
    assert_eq!(parsed_float_millis["unix_millis"], json!(1778590800123_i64));

    let exponent_millis_input =
        serde_json::from_str(r#"{"operation":"parse","input":1.778590800123e12}"#).unwrap();
    let parsed_exponent_millis = invoke(TIME_CAPABILITY_ID, exponent_millis_input)
        .await
        .unwrap();
    assert_eq!(
        parsed_exponent_millis["unix_millis"],
        json!(1778590800123_i64)
    );

    let parsed_string_float_millis = invoke(
        TIME_CAPABILITY_ID,
        json!({"operation": "parse", "input": "1778590800123.0"}),
    )
    .await
    .unwrap();
    assert_eq!(
        parsed_string_float_millis["unix_millis"],
        json!(1778590800123_i64)
    );

    let fractional_exponent_input =
        serde_json::from_str(r#"{"operation":"parse","input":1e-9}"#).unwrap();
    let parsed_fractional_exponent = invoke(TIME_CAPABILITY_ID, fractional_exponent_input)
        .await
        .unwrap();
    assert_eq!(
        parsed_fractional_exponent["iso"],
        json!("1970-01-01T00:00:00.000000001+00:00")
    );

    let parsed_negative_fraction = invoke(
        TIME_CAPABILITY_ID,
        json!({"operation": "parse", "input": "-0.25"}),
    )
    .await
    .unwrap();
    assert_eq!(
        parsed_negative_fraction["iso"],
        json!("1969-12-31T23:59:59.750+00:00")
    );

    let parsed_slack_timestamp = invoke(
        TIME_CAPABILITY_ID,
        json!({"operation": "parse", "input": "1783634967.123456"}),
    )
    .await
    .unwrap();
    assert_eq!(parsed_slack_timestamp["unix"], json!(1783634967));
    assert_eq!(
        parsed_slack_timestamp["iso"],
        json!("2026-07-09T22:09:27.123456+00:00")
    );

    let converted = invoke(
        TIME_CAPABILITY_ID,
        json!({
            "operation": "convert",
            "input": 1778590800,
            "to_timezone": "America/New_York"
        }),
    )
    .await
    .unwrap();
    assert_eq!(converted["output"], json!("2026-05-12T09:00:00-04:00"));

    let formatted = invoke(
        TIME_CAPABILITY_ID,
        json!({
            "operation": "format",
            "input": 1778590800,
            "timezone": "America/New_York",
            "format": "%Y-%m-%d %H:%M:%S %Z"
        }),
    )
    .await
    .unwrap();
    assert_eq!(formatted["formatted"], json!("2026-05-12 09:00:00 EDT"));

    let diff = invoke(
        TIME_CAPABILITY_ID,
        json!({
            "operation": "diff",
            "input": 1778590800,
            "timestamp2": 1778599800
        }),
    )
    .await
    .unwrap();
    assert_eq!(diff["minutes"], json!(150));
}

#[tokio::test]
async fn builtin_time_rejects_naive_without_timezone_and_ambiguous_local_time() {
    let missing_timezone = invoke(
        TIME_CAPABILITY_ID,
        json!({"operation": "parse", "input": "2026-05-12 13:00:00"}),
    )
    .await
    .unwrap_err();
    assert_eq!(missing_timezone, FailureKind::InputEncode);

    let ambiguous = invoke(
        TIME_CAPABILITY_ID,
        json!({
            "operation": "parse",
            "input": "2026-11-01 01:30:00",
            "timezone": "America/New_York"
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(ambiguous, FailureKind::InputEncode);
}

#[tokio::test]
async fn builtin_json_parse_query_stringify_and_validate_work() {
    let parsed = invoke(
        JSON_CAPABILITY_ID,
        json!({"operation": "parse", "data": "{\"items\":[{\"name\":\"alpha\"}]}"}),
    )
    .await
    .unwrap();
    assert_eq!(parsed["items"][0]["name"], json!("alpha"));

    let queried = invoke(
        JSON_CAPABILITY_ID,
        json!({
            "operation": "query",
            "data": {"items":[{"name":"alpha"}]},
            "path": "items[0].name"
        }),
    )
    .await
    .unwrap();
    assert_eq!(queried, json!("alpha"));

    let valid = invoke(
        JSON_CAPABILITY_ID,
        json!({"operation": "validate", "data": "{\"ok\":true}"}),
    )
    .await
    .unwrap();
    assert_eq!(valid, json!({"valid": true}));

    let stringified = invoke(
        JSON_CAPABILITY_ID,
        json!({"operation": "stringify", "data": {"ok": true}}),
    )
    .await
    .unwrap();
    assert!(stringified.as_str().unwrap().contains("\"ok\": true"));
}

#[tokio::test]
async fn builtin_json_parse_and_stringify_report_actionable_invalid_json() {
    for (operation, data) in [
        ("parse", "/workspace/source.json"),
        ("stringify", "100 * 1.25"),
    ] {
        let runtime = runtime();
        let failure = invoke_failure_with_context(
            &runtime,
            JSON_CAPABILITY_ID,
            json!({"operation": operation, "data": data}),
            execution_context([JSON_CAPABILITY_ID]),
        )
        .await;

        assert_eq!(failure.kind, FailureKind::InputEncode, "{operation}");
        assert_eq!(
            failure.message.as_deref(),
            Some("JSON input is not valid JSON"),
            "{operation}"
        );
        let issue = failure_input_issue(
            &failure,
            "data",
            DispatchInputIssueCode::InvalidValue,
            operation,
        );
        assert_eq!(issue.expected.as_deref(), Some("valid JSON"), "{operation}");
        assert!(
            issue
                .received
                .as_deref()
                .is_some_and(|received| received.starts_with("invalid JSON at line ")),
            "{operation}: expected bounded parser location, got {:?}",
            issue.received
        );
        assert!(
            issue
                .received
                .as_ref()
                .is_some_and(|received| received.len() < 80),
            "{operation}: parser diagnostic must remain bounded"
        );
    }
}

#[tokio::test]
async fn builtin_json_queries_authorized_workspace_file_with_adjacent_indices() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("source.json"),
        serde_json::to_vec(&json!({
            "nodes": [null, null, {"data": vec![vec!["value"]; 16]}]
        }))
        .unwrap(),
    )
    .unwrap();
    let (filesystem, mounts) = mounted_filesystem(root.path(), MountPermissions::read_only());
    let runtime = runtime_with_filesystem(filesystem);

    let queried = invoke_with_context(
        &runtime,
        JSON_CAPABILITY_ID,
        json!({
            "operation": "query",
            "file_path": "/workspace/source.json",
            "path": "nodes[2].data[15][0]"
        }),
        execution_context_with_mounts([JSON_CAPABILITY_ID], mounts),
    )
    .await
    .unwrap();

    assert_eq!(queried, json!("value"));
}

#[tokio::test]
async fn builtin_json_file_query_accepts_eight_mib_and_rejects_one_byte_over() {
    const MAX_JSON_FILE_BYTES: usize = 8 * 1_024 * 1_024;
    let root = tempfile::tempdir().unwrap();
    let file_path = root.path().join("source.json");
    let mut exact_limit_json = br#"{"value":"ok"}"#.to_vec();
    exact_limit_json.resize(MAX_JSON_FILE_BYTES, b' ');
    std::fs::write(&file_path, exact_limit_json).unwrap();
    let (filesystem, mounts) = mounted_filesystem(root.path(), MountPermissions::read_only());
    let runtime = runtime_with_filesystem(filesystem);
    let context = execution_context_with_mounts([JSON_CAPABILITY_ID], mounts);
    let input = json!({
        "operation": "query",
        "file_path": "/workspace/source.json",
        "path": "value"
    });

    let queried = invoke_with_context(&runtime, JSON_CAPABILITY_ID, input.clone(), context.clone())
        .await
        .unwrap();
    assert_eq!(queried, json!("ok"));

    std::fs::write(&file_path, vec![b' '; MAX_JSON_FILE_BYTES + 1]).unwrap();
    let failure = invoke_failure_with_context(&runtime, JSON_CAPABILITY_ID, input, context).await;
    assert_eq!(failure.kind, FailureKind::Resource);
    assert_eq!(
        failure.safe_summary().as_deref(),
        Some("JSON query file exceeds the 8388608-byte limit")
    );
}

#[tokio::test]
async fn builtin_json_file_query_denies_unmounted_workspace_path() {
    let failure = invoke_failure_with_context(
        &runtime(),
        JSON_CAPABILITY_ID,
        json!({
            "operation": "query",
            "file_path": "/workspace/source.json",
            "path": "value"
        }),
        execution_context([JSON_CAPABILITY_ID]),
    )
    .await;

    assert_eq!(failure.kind, FailureKind::FilesystemDenied);
}

#[tokio::test]
async fn builtin_json_stringify_rejects_invalid_json_strings() {
    let error = invoke(
        JSON_CAPABILITY_ID,
        json!({"operation": "stringify", "data": "not json"}),
    )
    .await
    .unwrap_err();
    assert_eq!(error, FailureKind::InputEncode);
}

#[tokio::test]
async fn builtin_json_rejects_v1_tool_output_stash_refs_without_leaking_input() {
    let outcome = runtime()
        .invoke_capability((
            execution_context([JSON_CAPABILITY_ID]),
            capability_id(JSON_CAPABILITY_ID),
            ResourceEstimate::default(),
            json!({
                "operation": "parse",
                "source_tool_call_id": "call_RAW_SECRET_sk-provider-secret"
            }),
        ))
        .await
        .unwrap();

    let RuntimeCapabilityOutcome::Failed(failure) = outcome else {
        panic!("expected sanitized failure, got {outcome:?}");
    };
    assert_eq!(failure.kind, FailureKind::InputEncode);
    let debug = format!("{failure:?}");
    assert!(!debug.contains("RAW_SECRET"));
    assert!(!debug.contains("sk-provider-secret"));
}
