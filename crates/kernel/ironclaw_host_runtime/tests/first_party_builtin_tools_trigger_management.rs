//! Built-in trigger list/remove/pause/resume/run and repository error mapping.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_host_api::ids::{AgentId, ProjectId, TenantId, ThreadId, UserId};
use ironclaw_host_api::result_meta::FailureKind;
use ironclaw_host_runtime::{
    TRIGGER_CREATE_CAPABILITY_ID, TRIGGER_LIST_CAPABILITY_ID, TRIGGER_PAUSE_CAPABILITY_ID,
    TRIGGER_REMOVE_CAPABILITY_ID, TRIGGER_RESUME_CAPABILITY_ID, TRIGGER_RUN_CAPABILITY_ID,
};
use ironclaw_triggers::{
    ClaimDueFireRequest, ClearActiveFireRequest, FireAcceptedRequest, InMemoryTriggerRepository,
    TriggerError, TriggerManualFireOutcome, TriggerManualFireRunner, TriggerRecord,
    TriggerRepository, TriggerRunHistoryStatus, TriggerSchedule, TriggerState,
};
use ironclaw_turns::TurnRunId;
use serde_json::{Value, json};

mod first_party_builtin_tools_support;

use first_party_builtin_tools_support::{context::*, invoke::*, triggers::*};

#[tokio::test]
async fn builtin_trigger_list_and_remove_are_caller_scoped() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let runtime = runtime_with_trigger_repository(repository.clone());
    let owner_context = execution_context([
        TRIGGER_CREATE_CAPABILITY_ID,
        TRIGGER_LIST_CAPABILITY_ID,
        TRIGGER_REMOVE_CAPABILITY_ID,
    ]);
    let mut foreign_context =
        execution_context([TRIGGER_LIST_CAPABILITY_ID, TRIGGER_REMOVE_CAPABILITY_ID]);
    foreign_context.user_id = UserId::new("other-user").unwrap();
    foreign_context.resource_scope.user_id = foreign_context.user_id.clone();

    let created = invoke_with_context(
        &runtime,
        TRIGGER_CREATE_CAPABILITY_ID,
        json!({
            "name": "Owned trigger",
            "execution_contract": trigger_execution_contract("Run owned work"),
            "schedule": { "kind": "cron", "expression": "0 8 * * *", "timezone": "UTC" }
        }),
        owner_context.clone(),
    )
    .await
    .unwrap();
    assert!(created["trigger"].get("prompt").is_none());
    let trigger_id = created["trigger"]["trigger_id"].as_str().unwrap();

    let foreign_list = invoke_with_context(
        &runtime,
        TRIGGER_LIST_CAPABILITY_ID,
        json!({}),
        foreign_context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(foreign_list["triggers"], json!([]));

    let foreign_remove = invoke_with_context(
        &runtime,
        TRIGGER_REMOVE_CAPABILITY_ID,
        json!({ "trigger_id": trigger_id }),
        foreign_context,
    )
    .await
    .unwrap();
    assert_eq!(foreign_remove["removed"], json!(false));

    let owner_list = invoke_with_context(
        &runtime,
        TRIGGER_LIST_CAPABILITY_ID,
        json!({}),
        owner_context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(owner_list["triggers"].as_array().unwrap().len(), 1);
    assert!(owner_list["triggers"][0].get("last_status").is_some());
    assert_eq!(owner_list["triggers"][0]["is_enabled"], json!(true));
    assert_eq!(owner_list["triggers"][0]["is_active"], json!(true));
    assert_eq!(owner_list["triggers"][0]["has_active_fire"], json!(false));
    assert!(owner_list["triggers"][0].get("prompt").is_none());
    assert!(owner_list["triggers"][0].get("tenant_id").is_none());
    assert!(owner_list["triggers"][0].get("creator_user_id").is_none());
    assert!(owner_list["triggers"][0].get("last_fired_slot").is_none());
    assert!(owner_list["triggers"][0].get("active_fire_slot").is_none());
    assert!(owner_list["triggers"][0].get("active_run_ref").is_none());

    let owner_remove = invoke_with_context(
        &runtime,
        TRIGGER_REMOVE_CAPABILITY_ID,
        json!({ "trigger_id": trigger_id }),
        owner_context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(owner_remove["removed"], json!(true));
    assert_eq!(owner_remove["trigger"]["trigger_id"], json!(trigger_id));
    assert_eq!(owner_remove["trigger"]["name"], json!("Owned trigger"));
    assert!(owner_remove["trigger"].get("prompt").is_none());
    assert!(owner_remove["trigger"].get("tenant_id").is_none());
    assert!(owner_remove["trigger"].get("creator_user_id").is_none());
    assert!(owner_remove["trigger"].get("active_fire_slot").is_none());
    assert!(owner_remove["trigger"].get("active_run_ref").is_none());

    let records = repository
        .list_triggers(owner_context.resource_scope.tenant_id)
        .await
        .unwrap();
    assert!(records.is_empty());
}

#[tokio::test]
async fn builtin_trigger_list_separates_enabled_state_from_active_fire_state() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let runtime = runtime_with_trigger_repository(repository.clone());
    let context = execution_context([TRIGGER_CREATE_CAPABILITY_ID, TRIGGER_LIST_CAPABILITY_ID]);

    let created = invoke_with_context(
        &runtime,
        TRIGGER_CREATE_CAPABILITY_ID,
        json!({
            "name": "Active trigger",
            "execution_contract": trigger_execution_contract("Run active work"),
            "schedule": { "kind": "cron", "expression": "0 8 * * *", "timezone": "UTC" }
        }),
        context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(created["trigger"]["is_enabled"], json!(true));
    assert_eq!(created["trigger"]["is_active"], json!(true));
    assert_eq!(created["trigger"]["has_active_fire"], json!(false));

    let mut records = repository
        .list_triggers(context.resource_scope.tenant_id.clone())
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    let mut record = records.remove(0);
    record.state = TriggerState::Paused;
    repository.upsert_trigger(record.clone()).await.unwrap();

    let listed_paused = invoke_with_context(
        &runtime,
        TRIGGER_LIST_CAPABILITY_ID,
        json!({}),
        context.clone(),
    )
    .await
    .unwrap();
    let paused_trigger = &listed_paused["triggers"][0];
    assert_eq!(paused_trigger["state"], json!("paused"));
    assert_eq!(paused_trigger["is_enabled"], json!(false));
    assert_eq!(paused_trigger["is_active"], json!(false));
    assert_eq!(paused_trigger["has_active_fire"], json!(false));

    record.state = TriggerState::Scheduled;
    record.active_fire_slot = Some(record.next_run_at);
    record.active_run_ref = Some(TurnRunId::parse("01890f0f-9b6f-7a85-9e5b-9f21a93c4f5a").unwrap());
    repository.upsert_trigger(record).await.unwrap();

    let listed = invoke_with_context(&runtime, TRIGGER_LIST_CAPABILITY_ID, json!({}), context)
        .await
        .unwrap();
    let trigger = &listed["triggers"][0];
    assert_eq!(trigger["is_enabled"], json!(true));
    assert_eq!(trigger["is_active"], json!(true));
    assert_eq!(trigger["has_active_fire"], json!(true));
    assert!(trigger.get("active_fire_slot").is_none());
    assert!(trigger.get("active_run_ref").is_none());
}

#[tokio::test]
async fn builtin_trigger_pause_and_resume_are_caller_scoped() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let runtime = runtime_with_trigger_repository(repository.clone());
    let owner_context = execution_context([
        TRIGGER_CREATE_CAPABILITY_ID,
        TRIGGER_LIST_CAPABILITY_ID,
        TRIGGER_PAUSE_CAPABILITY_ID,
        TRIGGER_RESUME_CAPABILITY_ID,
    ]);
    let mut foreign_context = execution_context([TRIGGER_PAUSE_CAPABILITY_ID]);
    foreign_context.user_id = UserId::new("other-user").unwrap();
    foreign_context.resource_scope.user_id = foreign_context.user_id.clone();

    let created = invoke_with_context(
        &runtime,
        TRIGGER_CREATE_CAPABILITY_ID,
        json!({
            "name": "Pauseable trigger",
            "execution_contract": trigger_execution_contract("Run work"),
            "schedule": { "kind": "cron", "expression": "0 8 * * *", "timezone": "UTC" }
        }),
        owner_context.clone(),
    )
    .await
    .unwrap();
    let trigger_id = created["trigger"]["trigger_id"].as_str().unwrap();

    let foreign_pause = invoke_with_context(
        &runtime,
        TRIGGER_PAUSE_CAPABILITY_ID,
        json!({ "trigger_id": trigger_id }),
        foreign_context,
    )
    .await
    .unwrap();
    assert_eq!(foreign_pause["updated"], json!(false));

    let owner_pause = invoke_with_context(
        &runtime,
        TRIGGER_PAUSE_CAPABILITY_ID,
        json!({ "trigger_id": trigger_id }),
        owner_context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(owner_pause["updated"], json!(true));
    assert_eq!(owner_pause["trigger"]["trigger_id"], json!(trigger_id));
    assert_eq!(owner_pause["trigger"]["state"], json!("paused"));
    assert_eq!(owner_pause["trigger"]["is_enabled"], json!(false));
    assert_eq!(owner_pause["trigger"]["is_active"], json!(false));
    assert!(owner_pause["trigger"].get("prompt").is_none());

    let listed_paused = invoke_with_context(
        &runtime,
        TRIGGER_LIST_CAPABILITY_ID,
        json!({}),
        owner_context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(listed_paused["triggers"][0]["state"], json!("paused"));
    assert_eq!(listed_paused["triggers"][0]["is_enabled"], json!(false));

    let owner_resume = invoke_with_context(
        &runtime,
        TRIGGER_RESUME_CAPABILITY_ID,
        json!({ "trigger_id": trigger_id }),
        owner_context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(owner_resume["updated"], json!(true));
    assert_eq!(owner_resume["trigger"]["state"], json!("scheduled"));
    assert_eq!(owner_resume["trigger"]["is_enabled"], json!(true));
    assert_eq!(owner_resume["trigger"]["is_active"], json!(true));

    let mut records = repository
        .list_triggers(owner_context.resource_scope.tenant_id.clone())
        .await
        .unwrap();
    assert_eq!(records.len(), 1);
    let mut completed = records.remove(0);
    completed.state = TriggerState::Completed;
    repository.upsert_trigger(completed).await.unwrap();

    let completed_resume = invoke_with_context(
        &runtime,
        TRIGGER_RESUME_CAPABILITY_ID,
        json!({ "trigger_id": trigger_id }),
        owner_context,
    )
    .await
    .unwrap();
    assert_eq!(completed_resume["updated"], json!(false));
}

#[tokio::test]
async fn builtin_trigger_create_list_and_remove_use_full_request_scope() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let runtime = runtime_with_trigger_repository(repository.clone());
    let mut owner_context = execution_context([
        TRIGGER_CREATE_CAPABILITY_ID,
        TRIGGER_LIST_CAPABILITY_ID,
        TRIGGER_REMOVE_CAPABILITY_ID,
    ]);
    set_context_scope(
        &mut owner_context,
        TenantId::new("scoped-tenant").unwrap(),
        UserId::new("scoped-user").unwrap(),
        Some(AgentId::new("scoped-agent").unwrap()),
        Some(ProjectId::new("scoped-project").unwrap()),
    );

    let mut other_agent_context = owner_context.clone();
    other_agent_context.agent_id = Some(AgentId::new("other-agent").unwrap());
    other_agent_context.resource_scope.agent_id = other_agent_context.agent_id.clone();

    let mut other_project_context = owner_context.clone();
    other_project_context.project_id = Some(ProjectId::new("other-project").unwrap());
    other_project_context.resource_scope.project_id = other_project_context.project_id.clone();

    let mut other_tenant_context = owner_context.clone();
    other_tenant_context.tenant_id = TenantId::new("other-tenant").unwrap();
    other_tenant_context.resource_scope.tenant_id = other_tenant_context.tenant_id.clone();

    let created = invoke_with_context(
        &runtime,
        TRIGGER_CREATE_CAPABILITY_ID,
        json!({
            "name": "Scoped trigger",
            "execution_contract": trigger_execution_contract("Run scoped work"),
            "schedule": { "kind": "cron", "expression": "0 8 * * *", "timezone": "UTC" }
        }),
        owner_context.clone(),
    )
    .await
    .unwrap();
    let trigger = &created["trigger"];
    assert!(trigger.get("tenant_id").is_none());
    assert!(trigger.get("creator_user_id").is_none());
    assert_eq!(trigger["agent_id"], json!("scoped-agent"));
    assert_eq!(trigger["project_id"], json!("scoped-project"));
    assert_eq!(trigger["is_enabled"], json!(true));
    assert_eq!(trigger["is_active"], json!(true));
    assert_eq!(trigger["has_active_fire"], json!(false));
    assert!(trigger.get("prompt").is_none());
    assert!(trigger.get("last_fired_slot").is_none());
    assert!(trigger.get("active_fire_slot").is_none());
    assert!(trigger.get("active_run_ref").is_none());
    let trigger_id = trigger["trigger_id"].as_str().unwrap();

    let other_agent_list = invoke_with_context(
        &runtime,
        TRIGGER_LIST_CAPABILITY_ID,
        json!({}),
        other_agent_context,
    )
    .await
    .unwrap();
    assert_eq!(other_agent_list["triggers"], json!([]));

    let other_project_remove = invoke_with_context(
        &runtime,
        TRIGGER_REMOVE_CAPABILITY_ID,
        json!({ "trigger_id": trigger_id }),
        other_project_context,
    )
    .await
    .unwrap();
    assert_eq!(other_project_remove["removed"], json!(false));

    let other_tenant_remove = invoke_with_context(
        &runtime,
        TRIGGER_REMOVE_CAPABILITY_ID,
        json!({ "trigger_id": trigger_id }),
        other_tenant_context,
    )
    .await
    .unwrap();
    assert_eq!(other_tenant_remove["removed"], json!(false));

    let owner_remove = invoke_with_context(
        &runtime,
        TRIGGER_REMOVE_CAPABILITY_ID,
        json!({ "trigger_id": trigger_id }),
        owner_context,
    )
    .await
    .unwrap();
    assert_eq!(owner_remove["removed"], json!(true));
    assert_eq!(owner_remove["trigger"]["trigger_id"], json!(trigger_id));
    assert_eq!(owner_remove["trigger"]["name"], json!("Scoped trigger"));
    assert!(owner_remove["trigger"].get("prompt").is_none());
    assert!(owner_remove["trigger"].get("agent_id").is_none());
    assert!(owner_remove["trigger"].get("project_id").is_none());
    assert!(owner_remove["trigger"].get("active_fire_slot").is_none());
    assert!(owner_remove["trigger"].get("active_run_ref").is_none());
}

#[tokio::test]
async fn builtin_trigger_create_round_trips_nullable_agent_and_project_scope() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let runtime = runtime_with_trigger_repository(repository);
    let mut context = execution_context([TRIGGER_CREATE_CAPABILITY_ID]);
    context.agent_id = None;
    context.project_id = None;
    context.resource_scope.agent_id = None;
    context.resource_scope.project_id = None;

    let created = invoke_with_context(
        &runtime,
        TRIGGER_CREATE_CAPABILITY_ID,
        json!({
            "name": "Unscoped trigger",
            "execution_contract": trigger_execution_contract("Run unscoped work"),
            "schedule": { "kind": "cron", "expression": "0 8 * * *", "timezone": "UTC" }
        }),
        context,
    )
    .await
    .unwrap();

    assert_eq!(created["trigger"]["agent_id"], Value::Null);
    assert_eq!(created["trigger"]["project_id"], Value::Null);
    assert_eq!(created["trigger"]["is_enabled"], json!(true));
    assert_eq!(created["trigger"]["is_active"], json!(true));
    assert_eq!(created["trigger"]["has_active_fire"], json!(false));
}

#[tokio::test]
async fn builtin_trigger_list_applies_user_surface_limit_boundaries() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let runtime = runtime_with_trigger_repository(repository);
    let context = execution_context([TRIGGER_CREATE_CAPABILITY_ID, TRIGGER_LIST_CAPABILITY_ID]);

    for index in 0..101 {
        invoke_with_context(
            &runtime,
            TRIGGER_CREATE_CAPABILITY_ID,
            json!({
                "name": format!("Trigger {index}"),
                "execution_contract": trigger_execution_contract("Run work"),
                "schedule": { "kind": "cron", "expression": "0 8 * * *", "timezone": "UTC" }
            }),
            context.clone(),
        )
        .await
        .unwrap();
    }

    // #7474 review: `limit: 0` used to succeed with an empty list while 101
    // routines exist — exactly the false-absence result the trigger_list
    // description forbids the model to fabricate. It is now rejected as
    // invalid input (schema declares `minimum: 1`), so an empty `triggers`
    // array is always proof of absence.
    invoke_with_context(
        &runtime,
        TRIGGER_LIST_CAPABILITY_ID,
        json!({ "limit": 0 }),
        context.clone(),
    )
    .await
    .expect_err("a zero limit must be rejected, not answered with an empty list");

    let listed = invoke_with_context(
        &runtime,
        TRIGGER_LIST_CAPABILITY_ID,
        json!({ "limit": 2 }),
        context.clone(),
    )
    .await
    .unwrap();

    assert_eq!(listed["triggers"].as_array().unwrap().len(), 2);

    let defaulted = invoke_with_context(
        &runtime,
        TRIGGER_LIST_CAPABILITY_ID,
        json!({}),
        context.clone(),
    )
    .await
    .unwrap();
    assert_eq!(defaulted["triggers"].as_array().unwrap().len(), 100);

    let clamped = invoke_with_context(
        &runtime,
        TRIGGER_LIST_CAPABILITY_ID,
        json!({ "limit": 200 }),
        context,
    )
    .await
    .unwrap();
    assert_eq!(clamped["triggers"].as_array().unwrap().len(), 100);
}

#[tokio::test]
async fn builtin_trigger_list_embeds_recent_run_history_with_run_limit() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let runtime = runtime_with_trigger_repository(repository.clone());
    let context = execution_context([TRIGGER_CREATE_CAPABILITY_ID, TRIGGER_LIST_CAPABILITY_ID]);

    invoke_with_context(
        &runtime,
        TRIGGER_CREATE_CAPABILITY_ID,
        json!({
            "name": "Historical trigger",
            "execution_contract": trigger_execution_contract("Create history rows"),
            "schedule": { "kind": "cron", "expression": "* * * * *", "timezone": "UTC" }
        }),
        context.clone(),
    )
    .await
    .unwrap();

    let record = repository
        .list_triggers(context.resource_scope.tenant_id.clone())
        .await
        .unwrap()
        .pop()
        .expect("persisted trigger");
    let first_fire_slot = record.next_run_at;
    let first_run_id = TurnRunId::new();
    repository
        .claim_due_fire(ClaimDueFireRequest {
            tenant_id: record.tenant_id.clone(),
            trigger_id: record.trigger_id,
            fire_slot: first_fire_slot,
            now: first_fire_slot,
        })
        .await
        .unwrap();
    repository
        .mark_fire_accepted(FireAcceptedRequest {
            tenant_id: record.tenant_id.clone(),
            trigger_id: record.trigger_id,
            fire_slot: first_fire_slot,
            run_id: first_run_id,
            thread_id: ThreadId::new("01890f0f-0001-7000-8000-000000000001").unwrap(),
            submitted_at: first_fire_slot + chrono::Duration::seconds(1),
        })
        .await
        .unwrap();
    repository
        .clear_active_fire(ClearActiveFireRequest {
            tenant_id: record.tenant_id.clone(),
            trigger_id: record.trigger_id,
            fire_slot: first_fire_slot,
            run_id: first_run_id,
            status: TriggerRunHistoryStatus::Ok,
        })
        .await
        .unwrap();

    let second_fire_slot = first_fire_slot + chrono::Duration::minutes(1);
    let second_run_id = TurnRunId::new();
    repository
        .claim_due_fire(ClaimDueFireRequest {
            tenant_id: record.tenant_id.clone(),
            trigger_id: record.trigger_id,
            fire_slot: second_fire_slot,
            now: second_fire_slot,
        })
        .await
        .unwrap();
    repository
        .mark_fire_accepted(FireAcceptedRequest {
            tenant_id: record.tenant_id.clone(),
            trigger_id: record.trigger_id,
            fire_slot: second_fire_slot,
            run_id: second_run_id,
            thread_id: ThreadId::new("01890f0f-0002-7000-8000-000000000002").unwrap(),
            submitted_at: second_fire_slot + chrono::Duration::seconds(1),
        })
        .await
        .unwrap();
    repository
        .clear_active_fire(ClearActiveFireRequest {
            tenant_id: record.tenant_id.clone(),
            trigger_id: record.trigger_id,
            fire_slot: second_fire_slot,
            run_id: second_run_id,
            status: TriggerRunHistoryStatus::Error,
        })
        .await
        .unwrap();

    let third_fire_slot = second_fire_slot + chrono::Duration::minutes(1);
    let third_run_id = TurnRunId::new();
    repository
        .claim_due_fire(ClaimDueFireRequest {
            tenant_id: record.tenant_id.clone(),
            trigger_id: record.trigger_id,
            fire_slot: third_fire_slot,
            now: third_fire_slot,
        })
        .await
        .unwrap();
    repository
        .mark_fire_accepted(FireAcceptedRequest {
            tenant_id: record.tenant_id,
            trigger_id: record.trigger_id,
            fire_slot: third_fire_slot,
            run_id: third_run_id,
            thread_id: ThreadId::new("01890f0f-0003-7000-8000-000000000003").unwrap(),
            submitted_at: third_fire_slot + chrono::Duration::seconds(1),
        })
        .await
        .unwrap();

    let listed = invoke_with_context(
        &runtime,
        TRIGGER_LIST_CAPABILITY_ID,
        json!({ "run_limit": 3 }),
        context,
    )
    .await
    .unwrap();

    let runs = listed["triggers"][0]["recent_runs"].as_array().unwrap();
    assert_eq!(runs.len(), 3);
    assert_eq!(runs[0]["run_id"], json!(third_run_id.to_string()));
    assert_eq!(runs[0]["status"], json!("running"));
    assert_eq!(runs[0]["completed_at"], Value::Null);
    assert_eq!(runs[1]["run_id"], json!(second_run_id.to_string()));
    assert_eq!(runs[1]["status"], json!("error"));
    assert_ne!(runs[1]["completed_at"], Value::Null);
    assert_eq!(runs[2]["run_id"], json!(first_run_id.to_string()));
    assert_eq!(runs[2]["status"], json!("ok"));
    assert_ne!(runs[2]["completed_at"], Value::Null);
    assert!(
        uuid::Uuid::parse_str(runs[0]["thread_id"].as_str().unwrap()).is_ok(),
        "run thread ids are canonical conversation thread UUIDs, not route placeholders"
    );
}

#[tokio::test]
async fn builtin_trigger_list_with_zero_run_limit_returns_empty_recent_runs() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let runtime = runtime_with_trigger_repository(repository.clone());
    let context = execution_context([TRIGGER_CREATE_CAPABILITY_ID, TRIGGER_LIST_CAPABILITY_ID]);

    invoke_with_context(
        &runtime,
        TRIGGER_CREATE_CAPABILITY_ID,
        json!({
            "name": "Zero run limit trigger",
            "execution_contract": trigger_execution_contract("Create history rows"),
            "schedule": { "kind": "cron", "expression": "0 8 * * *", "timezone": "UTC" }
        }),
        context.clone(),
    )
    .await
    .unwrap();

    let record = repository
        .list_triggers(context.resource_scope.tenant_id.clone())
        .await
        .unwrap()
        .pop()
        .expect("persisted trigger");
    seed_completed_trigger_runs(&repository, &record, 1).await;

    let listed = invoke_with_context(
        &runtime,
        TRIGGER_LIST_CAPABILITY_ID,
        json!({ "run_limit": 0 }),
        context,
    )
    .await
    .unwrap();

    assert_eq!(listed["triggers"][0]["recent_runs"], json!([]));
}

#[tokio::test]
async fn builtin_trigger_list_clamps_oversized_run_limit_to_max() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let runtime = runtime_with_trigger_repository(repository.clone());
    let context = execution_context([TRIGGER_CREATE_CAPABILITY_ID, TRIGGER_LIST_CAPABILITY_ID]);

    invoke_with_context(
        &runtime,
        TRIGGER_CREATE_CAPABILITY_ID,
        json!({
            "name": "Oversized run limit trigger",
            "execution_contract": trigger_execution_contract("Create many history rows"),
            "schedule": { "kind": "cron", "expression": "0 8 * * *", "timezone": "UTC" }
        }),
        context.clone(),
    )
    .await
    .unwrap();

    let record = repository
        .list_triggers(context.resource_scope.tenant_id.clone())
        .await
        .unwrap()
        .pop()
        .expect("persisted trigger");
    seed_completed_trigger_runs(&repository, &record, 101).await;

    let listed = invoke_with_context(
        &runtime,
        TRIGGER_LIST_CAPABILITY_ID,
        json!({ "run_limit": 200 }),
        context,
    )
    .await
    .unwrap();

    assert_eq!(
        listed["triggers"][0]["recent_runs"]
            .as_array()
            .unwrap()
            .len(),
        100
    );
}

/// Regression guard: `builtin.trigger_list` (model-facing) must return triggers
/// in ALL states, including `Completed` (soft-completed fire-once triggers).
/// The model needs to see completed one-shots so it can report their history.
///
/// Contrast with `list_automations` (panel-facing) which EXCLUDES Completed.
/// The difference is encoded by `list_triggers` passing `&[]` (no exclusions)
/// while `list_automations` passes `&[TriggerState::Completed]`.
#[tokio::test]
async fn builtin_trigger_list_includes_completed_fire_once_triggers() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let runtime = runtime_with_trigger_repository(repository.clone());
    let context = execution_context([TRIGGER_CREATE_CAPABILITY_ID, TRIGGER_LIST_CAPABILITY_ID]);

    // Create a fire-once trigger so it can be soft-completed.
    let created = invoke_with_context(
        &runtime,
        TRIGGER_CREATE_CAPABILITY_ID,
        json!({
            "name": "One-shot reminder",
            "execution_contract": trigger_execution_contract("Remind me about the meeting"),
            "schedule": { "kind": "once", "at": "2099-06-24T17:00:00", "timezone": "UTC" }
        }),
        context.clone(),
    )
    .await
    .unwrap();
    let trigger_id_str = created["trigger"]["trigger_id"]
        .as_str()
        .expect("trigger_id in create output");

    // Transition the trigger to Completed via a fire cycle (claim → clear).
    let record = repository
        .list_triggers(context.resource_scope.tenant_id.clone())
        .await
        .unwrap()
        .pop()
        .expect("persisted fire-once trigger");
    assert!(
        matches!(record.schedule, TriggerSchedule::Once { .. }),
        "persisted record must have an Once schedule"
    );
    let fire_slot = record.next_run_at;
    let run_id = ironclaw_turns::TurnRunId::new();

    repository
        .claim_due_fire(ClaimDueFireRequest {
            tenant_id: record.tenant_id.clone(),
            trigger_id: record.trigger_id,
            fire_slot,
            now: fire_slot,
        })
        .await
        .unwrap();
    repository
        .mark_fire_accepted(FireAcceptedRequest {
            tenant_id: record.tenant_id.clone(),
            trigger_id: record.trigger_id,
            fire_slot,
            run_id,
            thread_id: ironclaw_host_api::ids::ThreadId::new(
                "01890f0f-fire-7000-8000-000000000001",
            )
            .unwrap(),
            submitted_at: fire_slot,
        })
        .await
        .unwrap();
    repository
        .clear_active_fire(ClearActiveFireRequest {
            tenant_id: record.tenant_id.clone(),
            trigger_id: record.trigger_id,
            fire_slot,
            run_id,
            status: TriggerRunHistoryStatus::Ok,
        })
        .await
        .unwrap();

    // Verify the repository has Completed state.
    let persisted = repository
        .get_trigger(record.tenant_id.clone(), record.trigger_id)
        .await
        .unwrap()
        .expect("trigger record after clear");
    assert_eq!(
        persisted.state,
        TriggerState::Completed,
        "fire-once trigger must be Completed after clear_active_fire"
    );

    // trigger_list must include Completed triggers (model needs the history).
    let listed = invoke_with_context(&runtime, TRIGGER_LIST_CAPABILITY_ID, json!({}), context)
        .await
        .unwrap();

    let triggers = listed["triggers"].as_array().expect("triggers array");
    assert_eq!(
        triggers.len(),
        1,
        "trigger_list must return the completed fire-once trigger"
    );
    assert_eq!(
        triggers[0]["trigger_id"].as_str().unwrap(),
        trigger_id_str,
        "trigger_list must include the completed fire-once trigger by id"
    );
    assert_eq!(
        triggers[0]["state"],
        json!("completed"),
        "trigger_list must expose the Completed state to the model"
    );
}

async fn seed_completed_trigger_runs(
    repository: &InMemoryTriggerRepository,
    record: &TriggerRecord,
    count: usize,
) {
    // Re-derive each fire slot from the trigger's CURRENT next_run_at: clear_active_fire
    // advances next_run_at via the schedule, so we follow whatever cadence the schedule
    // dictates instead of assuming consecutive one-minute slots.
    for _ in 0..count {
        let current = repository
            .get_trigger(record.tenant_id.clone(), record.trigger_id)
            .await
            .unwrap()
            .expect("trigger present while seeding runs");
        let fire_slot = current.next_run_at;
        let run_id = TurnRunId::new();
        repository
            .claim_due_fire(ClaimDueFireRequest {
                tenant_id: record.tenant_id.clone(),
                trigger_id: record.trigger_id,
                fire_slot,
                now: fire_slot,
            })
            .await
            .unwrap();
        repository
            .mark_fire_accepted(FireAcceptedRequest {
                tenant_id: record.tenant_id.clone(),
                trigger_id: record.trigger_id,
                fire_slot,
                run_id,
                thread_id: ThreadId::new("01890f0f-0004-7000-8000-000000000004").unwrap(),
                submitted_at: fire_slot + chrono::Duration::seconds(1),
            })
            .await
            .unwrap();
        repository
            .clear_active_fire(ClearActiveFireRequest {
                tenant_id: record.tenant_id.clone(),
                trigger_id: record.trigger_id,
                fire_slot,
                run_id,
                status: TriggerRunHistoryStatus::Ok,
            })
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn builtin_trigger_remove_rejects_invalid_trigger_id() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let runtime = runtime_with_trigger_repository(repository);
    let context = execution_context([TRIGGER_REMOVE_CAPABILITY_ID]);

    let error = invoke_with_context(
        &runtime,
        TRIGGER_REMOVE_CAPABILITY_ID,
        json!({ "trigger_id": "not a trigger id" }),
        context,
    )
    .await
    .unwrap_err();

    assert_eq!(error, FailureKind::InputEncode);
}

#[tokio::test]
async fn builtin_trigger_list_rejects_non_integer_limit() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let runtime = runtime_with_trigger_repository(repository.clone());
    let context = execution_context([TRIGGER_LIST_CAPABILITY_ID]);

    let error = invoke_with_context(
        &runtime,
        TRIGGER_LIST_CAPABILITY_ID,
        json!({ "limit": "many" }),
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
async fn builtin_trigger_list_rejects_non_integer_run_limit() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let runtime = runtime_with_trigger_repository(repository.clone());
    let context = execution_context([TRIGGER_LIST_CAPABILITY_ID]);

    let error = invoke_with_context(
        &runtime,
        TRIGGER_LIST_CAPABILITY_ID,
        json!({ "run_limit": "many" }),
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
async fn builtin_trigger_remove_rejects_malformed_input() {
    let repository = Arc::new(InMemoryTriggerRepository::default());
    let runtime = runtime_with_trigger_repository(repository);
    let context = execution_context([TRIGGER_REMOVE_CAPABILITY_ID]);

    for input in [json!({}), json!({ "trigger_id": 123 })] {
        let error = invoke_with_context(
            &runtime,
            TRIGGER_REMOVE_CAPABILITY_ID,
            input,
            context.clone(),
        )
        .await
        .unwrap_err();

        assert_eq!(error, FailureKind::InputEncode);
    }
}

#[derive(Debug)]
struct FixedTriggerManualFireRunner {
    outcome: TriggerManualFireOutcome,
    calls: std::sync::Mutex<Vec<(TenantId, ironclaw_triggers::TriggerId)>>,
}

#[async_trait]
impl TriggerManualFireRunner for FixedTriggerManualFireRunner {
    async fn run_manual_fire(
        &self,
        tenant_id: TenantId,
        trigger_id: ironclaw_triggers::TriggerId,
        _now: chrono::DateTime<chrono::Utc>,
    ) -> Result<TriggerManualFireOutcome, TriggerError> {
        self.calls
            .lock()
            .expect("manual fire calls lock")
            .push((tenant_id, trigger_id));
        Ok(self.outcome.clone())
    }
}

#[tokio::test]
async fn builtin_trigger_run_dispatches_submitted_and_replayed_through_host_runtime() {
    for (outcome, expected_status, expected_run_id) in [
        {
            let run_id = TurnRunId::new();
            (
                TriggerManualFireOutcome::Submitted { run_id },
                "submitted",
                run_id,
            )
        },
        {
            let run_id = TurnRunId::new();
            (
                TriggerManualFireOutcome::Replayed {
                    original_run_id: run_id,
                },
                "replayed",
                run_id,
            )
        },
    ] {
        let repository = Arc::new(InMemoryTriggerRepository::default());
        let runner = Arc::new(FixedTriggerManualFireRunner {
            outcome,
            calls: std::sync::Mutex::new(Vec::new()),
        });
        let runtime = runtime_with_trigger_repository_and_manual_runner(repository, runner.clone());
        let context = execution_context([TRIGGER_CREATE_CAPABILITY_ID, TRIGGER_RUN_CAPABILITY_ID]);
        let expected_tenant_id = context.tenant_id.clone();
        let created = invoke_with_context(
            &runtime,
            TRIGGER_CREATE_CAPABILITY_ID,
            json!({
                "name": "Manual runtime dispatch",
                "execution_contract": trigger_execution_contract("Run work"),
                "schedule": { "kind": "cron", "expression": "0 8 * * *", "timezone": "UTC" }
            }),
            context.clone(),
        )
        .await
        .expect("create trigger through host runtime");
        let trigger_id = created["trigger"]["trigger_id"]
            .as_str()
            .expect("trigger id");

        let output = invoke_with_context(
            &runtime,
            TRIGGER_RUN_CAPABILITY_ID,
            json!({"trigger_id": trigger_id}),
            context,
        )
        .await
        .expect("run trigger through host runtime");

        assert_eq!(output["source"], "manual");
        assert_eq!(output["status"], expected_status);
        assert_eq!(output["run_id"], expected_run_id.to_string());
        let calls = runner.calls.lock().expect("manual fire calls lock");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, expected_tenant_id);
        assert_eq!(calls[0].1.to_string(), trigger_id);
    }

    for (outcome, expected_kind, expected_summary) in [
        (
            TriggerManualFireOutcome::Failed {
                reason: ironclaw_triggers::TriggerPollerFailureReason::Backend,
            },
            FailureKind::OperationFailed,
            Some("trigger run failed"),
        ),
        (
            TriggerManualFireOutcome::NotFound,
            FailureKind::InputEncode,
            Some("trigger_run input failed validation"),
        ),
    ] {
        let repository = Arc::new(InMemoryTriggerRepository::default());
        let runner = Arc::new(FixedTriggerManualFireRunner {
            outcome,
            calls: std::sync::Mutex::new(Vec::new()),
        });
        let runtime = runtime_with_trigger_repository_and_manual_runner(repository, runner.clone());
        let context = execution_context([TRIGGER_CREATE_CAPABILITY_ID, TRIGGER_RUN_CAPABILITY_ID]);
        let created = invoke_with_context(
            &runtime,
            TRIGGER_CREATE_CAPABILITY_ID,
            json!({
                "name": "Manual runtime failure",
                "execution_contract": trigger_execution_contract("Run work"),
                "schedule": { "kind": "cron", "expression": "0 8 * * *", "timezone": "UTC" }
            }),
            context.clone(),
        )
        .await
        .expect("create caller-visible trigger through host runtime");
        let trigger_id = created["trigger"]["trigger_id"]
            .as_str()
            .expect("trigger id");

        let failure = invoke_failure_with_context(
            &runtime,
            TRIGGER_RUN_CAPABILITY_ID,
            json!({"trigger_id": trigger_id}),
            context,
        )
        .await;

        assert_eq!(failure.kind, expected_kind);
        assert_eq!(failure.safe_summary().as_deref(), expected_summary);
        assert_eq!(
            runner.calls.lock().expect("manual fire calls lock").len(),
            1
        );
    }
}

#[tokio::test]
async fn builtin_trigger_management_maps_repository_errors_to_backend() {
    let runtime = runtime_with_trigger_repository(Arc::new(FailingTriggerRepository));
    let context = execution_context([
        TRIGGER_CREATE_CAPABILITY_ID,
        TRIGGER_LIST_CAPABILITY_ID,
        TRIGGER_REMOVE_CAPABILITY_ID,
    ]);

    let create_error = invoke_with_context(
        &runtime,
        TRIGGER_CREATE_CAPABILITY_ID,
        json!({
            "name": "Backend create",
            "execution_contract": trigger_execution_contract("Run work"),
            "schedule": { "kind": "cron", "expression": "0 8 * * *", "timezone": "UTC" }
        }),
        context.clone(),
    )
    .await
    .unwrap_err();
    assert_eq!(create_error, FailureKind::Backend);

    let list_error = invoke_with_context(
        &runtime,
        TRIGGER_LIST_CAPABILITY_ID,
        json!({}),
        context.clone(),
    )
    .await
    .unwrap_err();
    assert_eq!(list_error, FailureKind::Backend);

    let remove_error = invoke_with_context(
        &runtime,
        TRIGGER_REMOVE_CAPABILITY_ID,
        json!({ "trigger_id": "01HZZZZZZZZZZZZZZZZZZZZZZZ" }),
        context,
    )
    .await
    .unwrap_err();
    assert_eq!(remove_error, FailureKind::Backend);
}

#[tokio::test]
async fn builtin_trigger_list_maps_batch_run_history_repository_error_to_backend() {
    let repository = Arc::new(BatchRunHistoryFailingTriggerRepository::default());
    let runtime = runtime_with_trigger_repository(repository);
    let context = execution_context([TRIGGER_CREATE_CAPABILITY_ID, TRIGGER_LIST_CAPABILITY_ID]);

    invoke_with_context(
        &runtime,
        TRIGGER_CREATE_CAPABILITY_ID,
        json!({
            "name": "Batch history failure",
            "execution_contract": trigger_execution_contract("Create trigger before listing history"),
            "schedule": { "kind": "cron", "expression": "0 8 * * *", "timezone": "UTC" }
        }),
        context.clone(),
    )
    .await
    .unwrap();

    let error = invoke_with_context(&runtime, TRIGGER_LIST_CAPABILITY_ID, json!({}), context)
        .await
        .unwrap_err();

    assert_eq!(error, FailureKind::Backend);
}
