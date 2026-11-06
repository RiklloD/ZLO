//! Codex's plan and question traffic, beside the turn fold: the app-server
//! `turn/plan/updated` notification becomes the thread's plan, and the
//! person-facing requests (`item/tool/requestUserInput`,
//! `mcpServer/elicitation/request`) become question forms on the bridge.
//! Everything else in the protocol is the fold's and `answer_request`'s.

use std::sync::Arc;

use ironclaw_host_api::ids::ThreadId;
use serde_json::{Value, json};

use crate::bridge::CliBridge;
use crate::interaction;
use crate::rpc::RpcClient;

/// The turn a notification belongs to is ours (or it names none).
fn owns(params: &Value, turn_id: Option<&str>) -> bool {
    match (params.get("turnId").and_then(Value::as_str), turn_id) {
        (Some(id), Some(ours)) => id == ours,
        _ => true,
    }
}

/// A plan update of this turn reaches the bridge.
pub(super) fn on_notification(
    bridge: Option<&CliBridge>,
    thread: &ThreadId,
    run_id: &str,
    turn_id: Option<&str>,
    method: &str,
    params: &Value,
) {
    if method != "turn/plan/updated" || !owns(params, turn_id) {
        return;
    }
    if let (Some(bridge), Some(plan)) = (bridge, interaction::codex_plan(params)) {
        bridge.plan_updated(thread, run_id, plan);
    }
}

/// Answer a question request through a form. `false` for any other
/// method (the caller goes on to approvals); with no bridge there is nobody
/// to ask, and the question is dismissed.
pub(super) fn answer_question(
    bridge: Option<&Arc<CliBridge>>,
    client: &RpcClient,
    thread: &ThreadId,
    run_id: &str,
    id: &Value,
    method: &str,
    params: &Value,
) -> bool {
    let (asked, form) = match method {
        "item/tool/requestUserInput" => {
            (Asked::UserInput, interaction::codex_question_form(params))
        }
        "mcpServer/elicitation/request" => {
            // The request may sit at the top level or under `request`.
            let body = params.get("request").unwrap_or(params);
            let form_mode = body.get("mode").and_then(Value::as_str).unwrap_or("form") == "form";
            let form = form_mode
                .then(|| {
                    interaction::elicitation_form(
                        params
                            .get("serverName")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        body.get("message")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                        body.get("requestedSchema").unwrap_or(&Value::Null),
                    )
                })
                .flatten();
            (Asked::Elicitation, form)
        }
        _ => return false,
    };
    let (Some(bridge), Some(form)) = (bridge.cloned(), form) else {
        if let Err(error) = client.respond(id, reply(asked, None, None)) {
            tracing::debug!(error = %error.label(), "codex: question reply not sent");
        }
        return true;
    };
    let claim = bridge.claim_call(thread, run_id);
    let (client, id) = (client.clone(), id.clone());
    let (thread, run, method) = (thread.clone(), run_id.to_string(), method.to_string());
    tokio::spawn(async move {
        let outcome = bridge
            .ask(&thread, &run, &method, form.clone(), claim)
            .await;
        if let Err(error) = client.respond(&id, reply(asked, Some(&form), outcome.answer())) {
            tracing::debug!(error = %error.label(), "codex: question reply not sent");
        }
    });
    true
}

#[derive(Clone, Copy)]
enum Asked {
    UserInput,
    Elicitation,
}

fn reply(
    asked: Asked,
    form: Option<&ironclaw_host_api::agent_plan::QuestionForm>,
    answer: Option<&ironclaw_host_api::agent_plan::QuestionAnswer>,
) -> Value {
    match (asked, form) {
        (Asked::UserInput, _) => interaction::codex_question_reply(answer),
        (Asked::Elicitation, Some(form)) => interaction::elicitation_reply(form, answer),
        (Asked::Elicitation, None) => json!({"action": "decline"}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_this_turns_plan_updates_count() {
        let bridge = CliBridge::new(std::time::Duration::from_secs(5));
        let thread = ThreadId::new("codex-plan").unwrap();
        bridge.turn_started(&thread, "run");
        let params = |turn: &str| json!({"threadId": "c", "turnId": turn, "plan": [{"step": "Do it", "status": "inProgress"}]});
        on_notification(
            Some(&bridge),
            &thread,
            "run",
            Some("t1"),
            "turn/plan/updated",
            &params("t0"),
        );
        assert!(bridge.plan_of(&thread).is_none(), "another turn's plan");
        on_notification(
            Some(&bridge),
            &thread,
            "run",
            Some("t1"),
            "item/completed",
            &params("t1"),
        );
        assert!(bridge.plan_of(&thread).is_none(), "not a plan update");
        on_notification(
            Some(&bridge),
            &thread,
            "run",
            Some("t1"),
            "turn/plan/updated",
            &params("t1"),
        );
        let (_, _, plan) = bridge.plan_of(&thread).unwrap();
        assert_eq!(plan.current().unwrap().content, "Do it");
    }
}
