//! Client-tool denial through the production coordinator and PlannedDriver.

#[allow(dead_code)]
#[path = "support/mod.rs"]
mod reborn_support;
#[allow(dead_code)]
#[path = "../support/mod.rs"]
mod support;

use ironclaw_turns::{ExternalToolSpec, TurnStatus};
use reborn_support::group::RebornIntegrationGroup;
use reborn_support::reply::RebornScriptedReply;
use serde_json::json;

#[tokio::test]
async fn external_tool_gate_denied_resume_does_not_redispatch_parked_call() {
    let external_tool = ExternalToolSpec::new(
        "client_weather",
        "Look up weather in the caller",
        json!({
            "type": "object",
            "properties": { "city": { "type": "string" } },
            "required": ["city"],
            "additionalProperties": false
        }),
    )
    .expect("valid external-tool spec");
    let group = RebornIntegrationGroup::builder()
        .with_external_tools_for_test(vec![external_tool])
        .builtin_tools()
        .await
        .expect("external-tool group builds");
    let harness = group
        .thread("conv-external-tool-denied-resume")
        .script([
            RebornScriptedReply::tool_call("client_weather", json!({ "city": "Paris" })),
            RebornScriptedReply::tool_call("builtin.time", json!({})),
            RebornScriptedReply::text("the client tool was cancelled; the host tool still ran"),
        ])
        .build()
        .await
        .expect("thread builds");

    let run_id = harness
        .submit_turn_async("check the weather, then report the current time")
        .await
        .expect("turn accepted");

    let blocked = harness
        .wait_for_status(run_id, TurnStatus::BlockedExternalTool)
        .await
        .expect("client tool parks the real run");
    let gate_ref = blocked
        .gate_ref
        .expect("blocked external-tool run carries a gate ref");
    harness
        .deny_external_tool_gate(run_id, &gate_ref)
        .await
        .expect("external-tool gate denied through coordinator");
    harness
        .wait_for_status(run_id, TurnStatus::Completed)
        .await
        .expect("denied parked call is not re-dispatched into another external-tool block");

    harness
        .assert_capability_result_count("external_tool.client_weather", 0)
        .await
        .expect("denied client tool never fabricates an execution result");
    harness
        .assert_tool_error_summary_contains("external tool gate cancelled by client")
        .await
        .expect("denial is persisted as a model-visible tool outcome");
    harness
        .assert_model_request_contains("pending gate was denied or cancelled")
        .await
        .expect("the external-tool diagnostic is gate-neutral");
    harness
        .assert_tool_invocation_count("builtin.time", 1)
        .await
        .expect("an unrelated capability still dispatches after denial");
}

/// The native `ask_user` question through the production loop: the real
/// spec parks the run, the parked call reads back as the form the desktop
/// draws, the person's answer is submitted as the call's output (as gate
/// resolution does), and the resumed run hands the model that answer.
#[tokio::test]
async fn native_ask_user_answer_reaches_the_model_after_resume() {
    use ironclaw_assistant::native_questions::{ask_user_spec, pending_question, tool_output};
    use ironclaw_host_api::agent_plan::{QuestionAnswer, QuestionValue};
    use ironclaw_turns::ExternalToolCatalog;
    use std::sync::Arc;

    let catalog: Arc<dyn ExternalToolCatalog> =
        Arc::new(ironclaw_turns::InMemoryExternalToolCatalog::new());
    let group = RebornIntegrationGroup::builder()
        .with_external_tools_for_test(vec![ask_user_spec().expect("valid ask_user spec")])
        .with_external_tool_catalog_for_test(Arc::clone(&catalog))
        .builtin_tools()
        .await
        .expect("ask_user group builds");
    let harness = group
        .thread("conv-native-ask-user")
        .script([
            RebornScriptedReply::tool_call(
                "ask_user",
                json!({"questions": [{
                    "question": "Which database?",
                    "options": [{"label": "Postgres"}, {"label": "SQLite"}]
                }]}),
            ),
            RebornScriptedReply::text("Going with SQLite."),
        ])
        .build()
        .await
        .expect("thread builds");

    let run_id = harness
        .submit_turn_async("set up storage")
        .await
        .expect("turn accepted");
    let blocked = harness
        .wait_for_status(run_id, TurnStatus::BlockedExternalTool)
        .await
        .expect("ask_user parks the run");
    let gate_ref = blocked.gate_ref.expect("parked run carries a gate ref");

    let (call_id, form) = pending_question(catalog.as_ref(), run_id)
        .await
        .expect("catalog read")
        .expect("the parked call is a question form");
    assert_eq!(form.fields[0].label, "Which database?");
    let mut answer = QuestionAnswer::default();
    answer
        .values
        .insert("q1".into(), QuestionValue::Choices(vec!["SQLite".into()]));
    form.check_answer(&answer).expect("the answer fits");
    catalog
        .submit_output_for_pending_call(run_id, call_id, tool_output(&form, Some(&answer)))
        .await
        .expect("answer submitted");
    harness
        .resume_external_tool_gate(run_id, &gate_ref)
        .await
        .expect("resumed");
    harness
        .wait_for_status(run_id, TurnStatus::Completed)
        .await
        .expect("the run completes with the answer");

    // The answer is the parked call's capability result (the harness's
    // writer stores it by reference; production's adds the inline preview).
    let result = harness
        .tool_result_output("external_tool.ask_user")
        .await
        .expect("the ask_user call completed with a result");
    assert_eq!(result["answered"], true);
    assert_eq!(result["answers"]["Which database?"], json!(["SQLite"]));
    assert!(
        result["summary"]
            .as_str()
            .is_some_and(|s| s.contains("Which database?: SQLite")),
        "{result}"
    );
    harness
        .assert_reply_contains("Going with SQLite.")
        .await
        .expect("the run finishes after the answer");
}
