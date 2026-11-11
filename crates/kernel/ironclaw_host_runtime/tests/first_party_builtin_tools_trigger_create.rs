//! Built-in `trigger_create`: schema, validation, persistence, and create hooks.

use std::sync::Arc;

#[cfg(feature = "test-support")]
use chrono::{Datelike, TimeZone, Utc};
use ironclaw_host_api::dispatch::DispatchInputIssueCode;
use ironclaw_host_api::result_meta::FailureKind;
use ironclaw_host_runtime::{
    TRIGGER_CREATE_CAPABILITY_ID, TRIGGER_LIST_CAPABILITY_ID, TRIGGER_PAUSE_CAPABILITY_ID,
    TRIGGER_REMOVE_CAPABILITY_ID, TRIGGER_RESUME_CAPABILITY_ID,
};
use ironclaw_triggers::{
    InMemoryTriggerRepository, MAX_TRIGGER_NAME_BYTES, MAX_TRIGGER_PROMPT_BYTES, TriggerRepository,
    TriggerSchedule,
};
use serde_json::{Value, json};

mod first_party_builtin_tools_support;

use first_party_builtin_tools_support::{context::*, invoke::*, triggers::*};

#[tokio::test]
async fn builtin_trigger_create_stamps_caller_scope_and_persists_record() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let runtime = runtime_with_trigger_repository(repository.clone());
    let context = execution_context([TRIGGER_CREATE_CAPABILITY_ID]);

    let output = invoke_with_context(
        &runtime,
        TRIGGER_CREATE_CAPABILITY_ID,
        json!({
            "name": "Daily summary",
            "execution_contract": trigger_execution_contract("Summarize yesterday"),
            "schedule": { "kind": "cron", "expression": "0 8 * * *", "timezone": "UTC" }
        }),
        context.clone(),
    )
    .await
    .unwrap();

    let trigger = &output["trigger"];
    assert_eq!(trigger["name"], json!("Daily summary"));
    assert!(trigger.get("prompt").is_none());
    assert_eq!(trigger["source"], json!("schedule"));
    assert_eq!(trigger["schedule"]["kind"], json!("cron"));
    assert_eq!(trigger["state"], json!("scheduled"));
    assert!(trigger.get("tenant_id").is_none());
    assert!(trigger.get("creator_user_id").is_none());
    assert_eq!(trigger["agent_id"], json!("default"));
    assert_eq!(trigger["project_id"], json!("bootstrap"));
    assert_eq!(trigger["last_status"], Value::Null);
    assert_eq!(trigger["is_enabled"], json!(true));
    assert_eq!(trigger["is_active"], json!(true));
    assert_eq!(trigger["has_active_fire"], json!(false));
    assert_eq!(output["authoring_status"], json!("complete"));
    assert!(
        output["next_action"]
            .as_str()
            .is_some_and(|guidance| guidance.contains("Do not call trigger_create again")),
        "model-visible success must terminate authoring: {output}"
    );
    assert!(trigger.get("last_fired_slot").is_none());
    assert!(trigger.get("active_fire_slot").is_none());
    assert!(trigger.get("active_run_ref").is_none());

    let records = repository
        .list_triggers(context.resource_scope.tenant_id.clone())
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0]
            .execution_spec
            .as_ref()
            .expect("new trigger stores execution contract")
            .goal,
        "Summarize yesterday"
    );
    assert_eq!(records[0].creator_user_id, context.resource_scope.user_id);
    assert_eq!(records[0].agent_id, context.resource_scope.agent_id);
    assert_eq!(records[0].project_id, context.resource_scope.project_id);
}

/// Regression: a trigger-fired loop carries typed scheduled lineage all the
/// way to the first-party capability boundary. Every model-visible mutation
/// verb is denied there, while the same caller's ordinary interactive create
/// and read-only list remain available.
#[tokio::test]
async fn scheduled_loop_origin_denies_every_trigger_mutation_at_handler_boundary() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let runtime = runtime_with_trigger_repository(repository.clone());
    let mut context = execution_context([
        TRIGGER_CREATE_CAPABILITY_ID,
        TRIGGER_LIST_CAPABILITY_ID,
        TRIGGER_PAUSE_CAPABILITY_ID,
        TRIGGER_RESUME_CAPABILITY_ID,
        TRIGGER_REMOVE_CAPABILITY_ID,
    ]);

    let created = invoke_with_context(
        &runtime,
        TRIGGER_CREATE_CAPABILITY_ID,
        json!({
            "name": "interactive control",
            "execution_contract": trigger_execution_contract("remain scheduled"),
            "schedule": { "kind": "once", "at": "2999-01-01T00:00:00", "timezone": "UTC" }
        }),
        context.clone(),
    )
    .await
    .expect("ordinary interactive caller can create a trigger");
    let trigger_id = created["trigger"]["trigger_id"]
        .as_str()
        .expect("created trigger id")
        .to_string();

    let run_id = ironclaw_host_api::ids::RunId::new();
    context.run_id = Some(run_id);
    context.origin =
        Some(ironclaw_host_api::invocation::InvocationOrigin::ScheduledLoopRun(run_id));
    for (capability_id, input) in [
        (
            TRIGGER_CREATE_CAPABILITY_ID,
            json!({
                "name": "forbidden child",
                "execution_contract": trigger_execution_contract("replicate"),
                "schedule": { "kind": "once", "at": "2999-01-02T00:00:00", "timezone": "UTC" }
            }),
        ),
        (
            TRIGGER_PAUSE_CAPABILITY_ID,
            json!({"trigger_id": trigger_id}),
        ),
        (
            TRIGGER_RESUME_CAPABILITY_ID,
            json!({"trigger_id": trigger_id}),
        ),
        (
            TRIGGER_REMOVE_CAPABILITY_ID,
            json!({"trigger_id": trigger_id}),
        ),
    ] {
        let error = invoke_with_context(&runtime, capability_id, input, context.clone())
            .await
            .expect_err("scheduled loop mutation must be denied");
        assert_eq!(error, FailureKind::PolicyDenied, "{capability_id}");
    }

    let listed = invoke_with_context(&runtime, TRIGGER_LIST_CAPABILITY_ID, json!({}), context)
        .await
        .expect("scheduled loop retains read-only trigger_list");
    assert_eq!(listed["triggers"].as_array().map(Vec::len), Some(1));
    assert_eq!(listed["triggers"][0]["state"], json!("scheduled"));
}

/// Retired stored routing: a routine delivers externally only by calling
/// `builtin.outbound_deliver` from its own prompt, so the create surface no
/// longer accepts a delivery route. A call that still passes one is refused as
/// invalid input through the real runtime dispatch path — never accepted and
/// silently ignored, which would leave the caller believing a route was sealed
/// — and nothing persists.
#[tokio::test]
async fn builtin_trigger_create_rejects_the_retired_delivery_target_id() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let runtime = runtime_with_trigger_repository(repository.clone());
    let context = execution_context([TRIGGER_CREATE_CAPABILITY_ID]);

    let error = invoke_with_context(
        &runtime,
        TRIGGER_CREATE_CAPABILITY_ID,
        json!({
            "name": "Routed summary",
            "execution_contract": trigger_execution_contract("Summarize yesterday"),
            "schedule": { "kind": "cron", "expression": "0 8 * * *", "timezone": "UTC" },
            "delivery_target_id": "slack:personal-dm:T123:user-a"
        }),
        context.clone(),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::InputEncode);
    assert!(
        repository
            .list_triggers(context.resource_scope.tenant_id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn builtin_trigger_create_rejects_new_legacy_prompt_before_persistence() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let runtime = runtime_with_trigger_repository(repository.clone());
    let context = execution_context([TRIGGER_CREATE_CAPABILITY_ID]);

    let failure = invoke_failure_with_context(
        &runtime,
        TRIGGER_CREATE_CAPABILITY_ID,
        json!({
            "name": "Legacy raw prompt",
            "prompt": "Summarize yesterday",
            "schedule": { "kind": "cron", "expression": "0 8 * * *", "timezone": "UTC" }
        }),
        context.clone(),
    )
    .await;

    assert_eq!(failure.kind, FailureKind::InputEncode);
    assert_failure_has_input_issue(
        &failure,
        "unexpected_field",
        DispatchInputIssueCode::UnexpectedField,
        "legacy raw prompt",
    );
    assert_failure_has_input_issue(
        &failure,
        "execution_contract",
        DispatchInputIssueCode::MissingRequired,
        "legacy raw prompt",
    );
    assert!(
        repository
            .list_triggers(context.resource_scope.tenant_id)
            .await
            .unwrap()
            .is_empty(),
        "rejected legacy creation must not persist a trigger"
    );
}

#[tokio::test]
async fn builtin_trigger_create_accepts_weekly_tuesday_cron_schedule() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let runtime = runtime_with_trigger_repository(repository.clone());
    let context = execution_context([TRIGGER_CREATE_CAPABILITY_ID]);

    let output = invoke_with_context(
        &runtime,
        TRIGGER_CREATE_CAPABILITY_ID,
        json!({
            "name": "Tuesday reminder",
            "execution_contract": trigger_execution_contract("Send the Tuesday reminder"),
            "schedule": {
                "kind": "cron",
                "expression": "0 14 * * 2",
                "timezone": "America/Los_Angeles"
            }
        }),
        context.clone(),
    )
    .await
    .expect("weekly Tuesday cron schedule must be accepted");

    assert_eq!(output["trigger"]["name"], json!("Tuesday reminder"));
    assert_eq!(output["trigger"]["schedule"]["kind"], json!("cron"));

    let records = repository
        .list_triggers(context.resource_scope.tenant_id.clone())
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].name, "Tuesday reminder");
    assert_eq!(
        records[0]
            .execution_spec
            .as_ref()
            .expect("new trigger stores execution contract")
            .goal,
        "Send the Tuesday reminder"
    );
}

#[tokio::test]
async fn builtin_trigger_create_runs_create_hook_after_persistence() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let hook = Arc::new(PersistedRecordTriggerCreateHook::new(repository.clone()));
    let runtime = runtime_with_trigger_repository_and_create_hook(repository.clone(), hook.clone());
    let context = execution_context([TRIGGER_CREATE_CAPABILITY_ID]);

    invoke_with_context(
        &runtime,
        TRIGGER_CREATE_CAPABILITY_ID,
        json!({
            "name": "Hooked trigger",
            "execution_contract": trigger_execution_contract("Pair trigger creator"),
            "schedule": { "kind": "cron", "expression": "0 8 * * *", "timezone": "UTC" }
        }),
        context.clone(),
    )
    .await
    .unwrap();

    let hooked_records = hook.records();
    assert_eq!(hooked_records.len(), 1);
    assert_eq!(hooked_records[0].name, "Hooked trigger");
    assert_eq!(
        hooked_records[0]
            .execution_spec
            .as_ref()
            .expect("new trigger stores execution contract")
            .goal,
        "Pair trigger creator"
    );
    assert_eq!(
        hooked_records[0].creator_user_id,
        context.resource_scope.user_id
    );
    assert_eq!(hooked_records[0].agent_id, context.resource_scope.agent_id);
    assert_eq!(
        hooked_records[0].project_id,
        context.resource_scope.project_id
    );

    let records = repository
        .list_triggers(context.resource_scope.tenant_id)
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].trigger_id, hooked_records[0].trigger_id);
}

#[tokio::test]
async fn builtin_trigger_create_maps_create_hook_error_to_backend_and_rolls_back_record() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let runtime = runtime_with_trigger_repository_and_create_hook(
        repository.clone(),
        Arc::new(FailingTriggerCreateHook),
    );
    let context = execution_context([TRIGGER_CREATE_CAPABILITY_ID]);

    let error = invoke_with_context(
        &runtime,
        TRIGGER_CREATE_CAPABILITY_ID,
        json!({
            "name": "Hook failure",
            "execution_contract": trigger_execution_contract("Do not persist this trigger"),
            "schedule": { "kind": "cron", "expression": "0 8 * * *", "timezone": "UTC" }
        }),
        context.clone(),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::Backend);
    assert!(
        repository
            .list_triggers(context.resource_scope.tenant_id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn builtin_trigger_create_surfaces_rollback_error_when_cleanup_fails() {
    let repository = Arc::new(RemoveFailingTriggerRepository::default());
    let runtime = runtime_with_trigger_repository_and_create_hook(
        repository.clone(),
        Arc::new(FailingTriggerCreateHook),
    );
    let context = execution_context([TRIGGER_CREATE_CAPABILITY_ID]);

    let failure = invoke_failure_with_context(
        &runtime,
        TRIGGER_CREATE_CAPABILITY_ID,
        json!({
            "name": "Rollback failure",
            "execution_contract": trigger_execution_contract("Surface the rollback failure as the user-visible cause"),
            "schedule": { "kind": "cron", "expression": "0 8 * * *", "timezone": "UTC" }
        }),
        context.clone(),
    )
    .await;

    assert_eq!(failure.kind, FailureKind::Backend);
    assert_eq!(
        failure.safe_summary().as_deref(),
        Some("trigger create rollback failed after hook error")
    );
    assert_eq!(repository.remove_attempts(), 1);
    assert_eq!(
        repository
            .list_triggers(context.resource_scope.tenant_id)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn builtin_trigger_create_rejects_sub_minute_schedule_before_persistence() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let runtime = runtime_with_trigger_repository(repository.clone());
    let context = execution_context([TRIGGER_CREATE_CAPABILITY_ID]);

    let error = invoke_with_context(
        &runtime,
        TRIGGER_CREATE_CAPABILITY_ID,
        json!({
            "name": "Too fast",
            "execution_contract": trigger_execution_contract("Run constantly"),
            "schedule": { "kind": "cron", "expression": "* * * * * *", "timezone": "UTC" }
        }),
        context.clone(),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::InputEncode);
    assert!(
        repository
            .list_triggers(context.resource_scope.tenant_id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[cfg(feature = "test-support")]
#[tokio::test]
async fn builtin_trigger_create_rejects_schedule_with_no_future_slot_before_persistence() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let future_year = Utc::now().year() + 1;
    let after_schedule_expires = Utc
        .with_ymd_and_hms(future_year + 1, 1, 1, 0, 0, 0)
        .unwrap();
    let runtime = runtime_with_trigger_repository_and_clock(
        repository.clone(),
        Arc::new(FixedTriggerClock(after_schedule_expires)),
    );
    let context = execution_context([TRIGGER_CREATE_CAPABILITY_ID]);

    let failure = invoke_failure_with_context(
        &runtime,
        TRIGGER_CREATE_CAPABILITY_ID,
        json!({
            "name": "Expired finite schedule",
            "execution_contract": trigger_execution_contract("Run once in the finite year"),
            "schedule": { "kind": "cron", "expression": format!("0 0 8 * * * {future_year}"), "timezone": "UTC" }
        }),
        context.clone(),
    )
    .await;
    assert_failure_input_issue_expected(
        &failure,
        "schedule.expression",
        DispatchInputIssueCode::InvalidValue,
        "cron expression with at least one future fire time",
        "schedule with no future slot",
    );
    assert!(
        repository
            .list_triggers(context.resource_scope.tenant_id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn builtin_trigger_create_rejects_malformed_input_before_persistence() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let runtime = runtime_with_trigger_repository(repository.clone());
    let context = execution_context([TRIGGER_CREATE_CAPABILITY_ID]);

    let error = invoke_with_context(
        &runtime,
        TRIGGER_CREATE_CAPABILITY_ID,
        json!({
            "name": "Missing prompt",
            "schedule": { "kind": "cron", "expression": "0 8 * * *", "timezone": "UTC" }
        }),
        context.clone(),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::InputEncode);
    assert!(
        repository
            .list_triggers(context.resource_scope.tenant_id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn builtin_trigger_create_requires_explicit_result_delivery_before_persistence() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let runtime = runtime_with_trigger_repository(repository.clone());
    let context = execution_context([TRIGGER_CREATE_CAPABILITY_ID]);

    let failure = invoke_failure_with_context(
        &runtime,
        TRIGGER_CREATE_CAPABILITY_ID,
        json!({
            "name": "Ambiguous notification",
            "execution_contract": {
                "version": 1,
                "goal": "Check whether example.com is reachable",
                "success_criteria": ["Reachability checked"],
                "output_instructions": "Return the result",
                "no_result_text": "No change"
            },
            "schedule": { "kind": "cron", "expression": "0 9 * * *", "timezone": "Europe/Istanbul" }
        }),
        context.clone(),
    )
    .await;

    assert_failure_input_issue_expected(
        &failure,
        "execution_contract.policy.result_delivery",
        DispatchInputIssueCode::MissingRequired,
        "deliver or suppress_when_nothing_to_report",
        "ambiguous delivery choice",
    );
    assert!(
        repository
            .list_triggers(context.resource_scope.tenant_id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn builtin_trigger_create_rejects_invalid_timezone_before_persistence() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let runtime = runtime_with_trigger_repository(repository.clone());
    let context = execution_context([TRIGGER_CREATE_CAPABILITY_ID]);

    let error = invoke_with_context(
        &runtime,
        TRIGGER_CREATE_CAPABILITY_ID,
        json!({
            "name": "Invalid timezone trigger",
            "execution_contract": trigger_execution_contract("Run something"),
            "schedule": { "kind": "cron", "expression": "0 9 * * *", "timezone": "Not/A/Timezone" }
        }),
        context.clone(),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::InputEncode);
    assert!(
        repository
            .list_triggers(context.resource_scope.tenant_id)
            .await
            .unwrap()
            .is_empty(),
        "no trigger should be persisted when timezone is invalid"
    );
}

#[tokio::test]
async fn builtin_trigger_create_rejects_blank_name_or_goal_before_persistence() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let runtime = runtime_with_trigger_repository(repository.clone());
    let context = execution_context([TRIGGER_CREATE_CAPABILITY_ID]);

    for (case_name, input, issue_path, expected) in [
        (
            "blank name",
            json!({
                "name": " ",
                "execution_contract": trigger_execution_contract("Run work"),
                "schedule": { "kind": "cron", "expression": "0 8 * * *", "timezone": "UTC" }
            }),
            "name",
            "non-empty trigger name",
        ),
        (
            "blank goal",
            json!({
                "name": "Blank goal",
                "execution_contract": trigger_execution_contract(" "),
                "schedule": { "kind": "cron", "expression": "0 8 * * *", "timezone": "UTC" }
            }),
            "trigger",
            "invalid_record",
        ),
    ] {
        let failure = invoke_failure_with_context(
            &runtime,
            TRIGGER_CREATE_CAPABILITY_ID,
            input,
            context.clone(),
        )
        .await;
        assert_eq!(failure.kind, FailureKind::InputEncode, "{case_name}");
        assert_failure_input_issue_expected(
            &failure,
            issue_path,
            DispatchInputIssueCode::InvalidValue,
            expected,
            case_name,
        );
    }

    assert!(
        repository
            .list_triggers(context.resource_scope.tenant_id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn builtin_trigger_create_rejects_oversized_name_or_goal_before_persistence() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let runtime = runtime_with_trigger_repository(repository.clone());
    let context = execution_context([TRIGGER_CREATE_CAPABILITY_ID]);

    for (case_name, input, issue_path, expected) in [
        (
            "oversized name",
            json!({
                "name": "x".repeat(MAX_TRIGGER_NAME_BYTES + 1),
                "execution_contract": trigger_execution_contract("Run work"),
                "schedule": { "kind": "cron", "expression": "0 8 * * *", "timezone": "UTC" }
            }),
            "name",
            "trigger name within the allowed byte limit",
        ),
        (
            "oversized goal",
            json!({
                "name": "Oversized goal",
                "execution_contract": trigger_execution_contract("x".repeat(MAX_TRIGGER_PROMPT_BYTES + 1)),
                "schedule": { "kind": "cron", "expression": "0 8 * * *", "timezone": "UTC" }
            }),
            "trigger",
            "invalid_record",
        ),
    ] {
        let failure = invoke_failure_with_context(
            &runtime,
            TRIGGER_CREATE_CAPABILITY_ID,
            input,
            context.clone(),
        )
        .await;
        assert_eq!(failure.kind, FailureKind::InputEncode, "{case_name}");
        assert_failure_input_issue_expected(
            &failure,
            issue_path,
            DispatchInputIssueCode::InvalidValue,
            expected,
            case_name,
        );
    }

    assert!(
        repository
            .list_triggers(context.resource_scope.tenant_id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn builtin_trigger_create_applies_first_party_input_size_bound() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let runtime = runtime_with_trigger_repository(repository.clone());
    let context = execution_context([TRIGGER_CREATE_CAPABILITY_ID]);

    let error = invoke_with_context(
        &runtime,
        TRIGGER_CREATE_CAPABILITY_ID,
        json!({
            "name": "Large ignored field",
            "execution_contract": trigger_execution_contract("Run work"),
            "schedule": { "kind": "cron", "expression": "0 8 * * *", "timezone": "UTC" },
            "padding": "x".repeat(1_048_576)
        }),
        context.clone(),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::Resource);
    assert!(
        repository
            .list_triggers(context.resource_scope.tenant_id)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn builtin_trigger_create_rejects_invalid_schedule_kind_before_persistence() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let runtime = runtime_with_trigger_repository(repository.clone());
    let context = execution_context([TRIGGER_CREATE_CAPABILITY_ID]);

    let error = invoke_with_context(
        &runtime,
        TRIGGER_CREATE_CAPABILITY_ID,
        json!({
            "name": "Invalid schedule kind trigger",
            "execution_contract": trigger_execution_contract("Run work"),
            "schedule": { "kind": "monthly", "expression": "0 8 1 * *", "timezone": "UTC" }
        }),
        context.clone(),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::InputEncode);
    assert!(
        repository
            .list_triggers(context.resource_scope.tenant_id)
            .await
            .unwrap()
            .is_empty(),
        "no trigger should be persisted when schedule kind is invalid"
    );
}

#[tokio::test]
async fn builtin_trigger_create_rejects_missing_schedule_before_persistence() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let runtime = runtime_with_trigger_repository(repository.clone());
    let context = execution_context([TRIGGER_CREATE_CAPABILITY_ID]);

    let error = invoke_with_context(
        &runtime,
        TRIGGER_CREATE_CAPABILITY_ID,
        json!({
            "name": "Missing schedule trigger",
            "execution_contract": trigger_execution_contract("Run work")
        }),
        context.clone(),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::InputEncode);
    assert!(
        repository
            .list_triggers(context.resource_scope.tenant_id)
            .await
            .unwrap()
            .is_empty(),
        "no trigger should be persisted when schedule is absent"
    );
}

#[tokio::test]
async fn builtin_trigger_create_surfaces_structured_invalid_input_detail() {
    let cases = [
        (
            "old flat cron field",
            json!({
                "name": "Legacy shape",
                "execution_contract": trigger_execution_contract("Run work"),
                "cron": "*/3 * * * *",
                "timezone": "UTC"
            }),
            vec![
                ("unexpected_field", DispatchInputIssueCode::UnexpectedField),
                ("schedule", DispatchInputIssueCode::MissingRequired),
            ],
        ),
        (
            "non-object input",
            json!("not an object"),
            vec![("input", DispatchInputIssueCode::TypeMismatch)],
        ),
        (
            "non-string name",
            json!({
                "name": 42,
                "execution_contract": trigger_execution_contract("Run work"),
                "schedule": { "kind": "cron", "expression": "*/3 * * * *", "timezone": "UTC" }
            }),
            vec![("name", DispatchInputIssueCode::TypeMismatch)],
        ),
        (
            "non-object schedule",
            json!({
                "name": "Bad schedule",
                "execution_contract": trigger_execution_contract("Run work"),
                "schedule": "*/3 * * * *"
            }),
            vec![("schedule", DispatchInputIssueCode::TypeMismatch)],
        ),
        (
            "missing schedule kind",
            json!({
                "name": "Missing kind",
                "execution_contract": trigger_execution_contract("Run work"),
                "schedule": { "expression": "*/3 * * * *", "timezone": "UTC" }
            }),
            vec![("schedule.kind", DispatchInputIssueCode::MissingRequired)],
        ),
        (
            "non-string schedule kind",
            json!({
                "name": "Bad kind",
                "execution_contract": trigger_execution_contract("Run work"),
                "schedule": { "kind": 7, "expression": "*/3 * * * *", "timezone": "UTC" }
            }),
            vec![("schedule.kind", DispatchInputIssueCode::TypeMismatch)],
        ),
        (
            "non-string result delivery",
            json!({
                "name": "Bad result delivery",
                "execution_contract": {
                    "version": 1,
                    "goal": "Run work",
                    "success_criteria": ["Work completed"],
                    "output_instructions": "Return the result",
                    "no_result_text": "No change",
                    "policy": { "result_delivery": 7 }
                },
                "schedule": { "kind": "cron", "expression": "*/3 * * * *", "timezone": "UTC" }
            }),
            vec![(
                "execution_contract.policy.result_delivery",
                DispatchInputIssueCode::TypeMismatch,
            )],
        ),
        (
            "missing schedule timezone",
            json!({
                "name": "Missing timezone",
                "execution_contract": trigger_execution_contract("Run work"),
                "schedule": { "kind": "cron", "expression": "*/3 * * * *" }
            }),
            vec![("schedule.timezone", DispatchInputIssueCode::MissingRequired)],
        ),
        (
            "unexpected root field",
            json!({
                "name": "Extra root",
                "execution_contract": trigger_execution_contract("Run work"),
                "extra": true,
                "schedule": { "kind": "cron", "expression": "*/3 * * * *", "timezone": "UTC" }
            }),
            vec![("unexpected_field", DispatchInputIssueCode::UnexpectedField)],
        ),
        (
            "unexpected schedule field",
            json!({
                "name": "Extra schedule",
                "execution_contract": trigger_execution_contract("Run work"),
                "schedule": {
                    "kind": "cron",
                    "expression": "*/3 * * * *",
                    "timezone": "UTC",
                    "extra": true
                }
            }),
            vec![(
                "schedule.unexpected_field",
                DispatchInputIssueCode::UnexpectedField,
            )],
        ),
        (
            "invalid cron cadence",
            json!({
                "name": "Too fast",
                "execution_contract": trigger_execution_contract("Run work"),
                "schedule": { "kind": "cron", "expression": "* * * * * *", "timezone": "UTC" }
            }),
            vec![("schedule.expression", DispatchInputIssueCode::InvalidValue)],
        ),
        (
            "invalid timezone",
            json!({
                "name": "Invalid timezone",
                "execution_contract": trigger_execution_contract("Run work"),
                "schedule": { "kind": "cron", "expression": "*/3 * * * *", "timezone": "Not/A/Timezone" }
            }),
            vec![("schedule.timezone", DispatchInputIssueCode::InvalidValue)],
        ),
    ];

    for (case_name, input, expected_issues) in cases {
        let repository = Arc::new(InMemoryTriggerRepository::default());
        let runtime = runtime_with_trigger_repository(repository.clone());
        let context = execution_context([TRIGGER_CREATE_CAPABILITY_ID]);

        let failure = invoke_failure_with_context(
            &runtime,
            TRIGGER_CREATE_CAPABILITY_ID,
            input,
            context.clone(),
        )
        .await;

        assert_eq!(failure.kind, FailureKind::InputEncode, "{case_name}");
        for (path, code) in expected_issues {
            assert_failure_has_input_issue(&failure, path, code, case_name);
        }
        assert!(
            repository
                .list_triggers(context.resource_scope.tenant_id)
                .await
                .unwrap()
                .is_empty(),
            "{case_name}: no trigger should be persisted"
        );
    }
}

/// Positive path: a future `once` schedule must be accepted, persisted,
/// and round-trip the TriggerSchedule::Once variant correctly.
///
/// Year 2099 is used so the `at` datetime always has a future slot regardless
/// of the real wall-clock — no fixed test clock is needed.
#[tokio::test]
async fn builtin_trigger_create_accepts_once_schedule() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let runtime = runtime_with_trigger_repository(repository.clone());
    let context = execution_context([TRIGGER_CREATE_CAPABILITY_ID]);

    let output = invoke_with_context(
        &runtime,
        TRIGGER_CREATE_CAPABILITY_ID,
        json!({
            "name": "One-shot reminder 2099",
            "execution_contract": trigger_execution_contract("Check the archives"),
            "schedule": { "kind": "once", "at": "2099-06-24T17:00:00", "timezone": "UTC" }
        }),
        context.clone(),
    )
    .await
    .expect("once schedule trigger must be accepted");

    // The response must surface the schedule kind correctly.
    let trigger = &output["trigger"];
    assert_eq!(trigger["name"], json!("One-shot reminder 2099"));
    assert_eq!(trigger["schedule"]["kind"], json!("once"));
    assert_eq!(trigger["state"], json!("scheduled"));
    // completion_policy must NOT appear in the output
    assert!(trigger.get("completion_policy").is_none() || trigger["completion_policy"].is_null());

    // The record must be persisted as TriggerSchedule::Once.
    let records = repository
        .list_triggers(context.resource_scope.tenant_id.clone())
        .await
        .unwrap();
    assert_eq!(records.len(), 1, "exactly one trigger must be persisted");

    let record = &records[0];
    assert_eq!(record.name, "One-shot reminder 2099");
    assert_eq!(
        record
            .execution_spec
            .as_ref()
            .expect("new trigger stores execution contract")
            .goal,
        "Check the archives"
    );

    // Verify the stored schedule is Once with the correct UTC instant.
    match &record.schedule {
        TriggerSchedule::Once { at, timezone } => {
            // 2099-06-24T17:00:00 UTC
            assert_eq!(
                at.to_rfc3339(),
                "2099-06-24T17:00:00+00:00",
                "stored at must match the submitted wall-clock converted to UTC"
            );
            assert_eq!(
                timezone, "UTC",
                "stored timezone must match the submitted value"
            );
        }
        TriggerSchedule::Cron { .. } => panic!("expected Once schedule variant"),
    }
}

/// Negative path: a `once` schedule whose `at` falls in a DST ambiguous fold
/// (America/New_York on 2026-11-01 01:30:00 is a known overlap) must be
/// rejected before any trigger is written to the repository.
#[tokio::test]
async fn builtin_trigger_create_rejects_invalid_once_schedule_before_persistence() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let runtime = runtime_with_trigger_repository(repository.clone());
    let context = execution_context([TRIGGER_CREATE_CAPABILITY_ID]);

    let error = invoke_with_context(
        &runtime,
        TRIGGER_CREATE_CAPABILITY_ID,
        json!({
            "name": "DST overlap reminder",
            "execution_contract": trigger_execution_contract("This should be rejected"),
            "schedule": { "kind": "once", "at": "2026-11-01T01:30:00", "timezone": "America/New_York" }
        }),
        context.clone(),
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::InputEncode);
    assert!(
        repository
            .list_triggers(context.resource_scope.tenant_id)
            .await
            .unwrap()
            .is_empty(),
        "no trigger should be persisted when the once schedule is ambiguous (DST overlap)"
    );
}
