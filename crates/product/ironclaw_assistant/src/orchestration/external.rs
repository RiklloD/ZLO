//! Wire answers for threads that run off IronClaw's loop (a profile bound to
//! a CLI runtime). Their transcript lives in the runtime's own session, so
//! the submit, cancel and timeline routes are answered from the engine in
//! the shapes the gateway already serves.

use ironclaw_host_api::ids::ThreadId;
use ironclaw_host_api::turn::{AcceptedMessageRef, EventCursor, TurnRunId, TurnStatus};
use ironclaw_orchestrator::profile_runtime::UserTurn;
use ironclaw_orchestrator::runtime::{HistoryMessage, HistoryRole};
use ironclaw_product_contracts::product_wire::{RebornCancelRunResponse, RebornSubmitTurnResponse};
use serde_json::{Value, json};
use uuid::Uuid;

/// Runtime run ids are UUIDs for the Claude CLI; anything else maps to a
/// stable UUID so the wire type holds.
fn run_id(raw: Option<&str>) -> TurnRunId {
    match raw {
        Some(raw) => TurnRunId::parse(raw).unwrap_or_else(|_| {
            TurnRunId::from_uuid(Uuid::new_v5(&Uuid::NAMESPACE_OID, raw.as_bytes()))
        }),
        None => TurnRunId::new(),
    }
}

pub(super) fn submit_response(
    thread: &ThreadId,
    turn: UserTurn,
) -> Option<RebornSubmitTurnResponse> {
    let accepted = AcceptedMessageRef::new(format!("external:{}", Uuid::new_v4())).ok()?;
    Some(match turn {
        UserTurn::Started { run_id: raw } => {
            let run = run_id(raw.as_deref());
            RebornSubmitTurnResponse::Submitted {
                thread_id: thread.clone(),
                accepted_message_ref: accepted,
                turn_id: run.to_string(),
                run_id: run,
                status: TurnStatus::Running,
                resolved_run_profile_id: "external".to_string(),
                resolved_run_profile_version: 1,
                event_cursor: EventCursor(0),
            }
        }
        UserTurn::Queued { active_run_id } => RebornSubmitTurnResponse::DeferredBusy {
            thread_id: thread.clone(),
            accepted_message_ref: accepted,
            active_run_id: run_id(active_run_id.as_deref()),
            status: TurnStatus::Running,
            event_cursor: EventCursor(0),
            notice: "Queued: it runs when the current turn ends.".to_string(),
        },
    })
}

pub(super) fn cancel_response(raw_run: &str) -> RebornCancelRunResponse {
    RebornCancelRunResponse {
        run_id: run_id(Some(raw_run)),
        status: TurnStatus::CancelRequested,
        event_cursor: EventCursor(0),
        already_terminal: false,
    }
}

/// The runtime's history as timeline rows, newest `limit`. Ids are stable
/// per position so a reload does not re-key the transcript.
pub(super) fn timeline_messages(
    thread: &ThreadId,
    history: &[HistoryMessage],
    limit: usize,
) -> Vec<Value> {
    let rows: Vec<Value> = history
        .iter()
        .enumerate()
        .filter(|(_, m)| !m.hidden)
        .filter_map(|(i, m)| {
            let kind = match m.role {
                HistoryRole::User => "user",
                HistoryRole::Assistant => "assistant",
                HistoryRole::System => "system",
                HistoryRole::Tool => return None,
            };
            let id = Uuid::new_v5(&Uuid::NAMESPACE_OID, format!("{thread}:{i}").as_bytes());
            Some(json!({
                "message_id": id,
                "thread_id": thread.as_str(),
                "sequence": i as u64 + 1,
                "kind": kind,
                "status": "finalized",
                "actor_id": null,
                "source_binding_id": null,
                "reply_target_binding_id": null,
                "turn_id": m.run_id,
                "turn_run_id": m.run_id,
                "content": m.text,
                "redaction_ref": null,
            }))
        })
        .collect();
    let skip = rows.len().saturating_sub(limit);
    rows.into_iter().skip(skip).collect()
}
