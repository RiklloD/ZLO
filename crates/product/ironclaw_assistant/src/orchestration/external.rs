//! Wire answers for threads that run off IronClaw's loop (a profile bound to
//! a CLI runtime). Their transcript lives in the runtime's own session, so
//! the submit, cancel and timeline routes are answered from the engine in
//! the shapes the gateway already serves. A running CLI turn streams from
//! the [`CliBridge`]: reply text, approval gates and run status as the same
//! projection items IronClaw's own turns produce, and the gate route answers
//! its pending tool calls.
//!
//! The live frames are ephemeral by design: the CLI child dies with the
//! backend, so a reconnect gets the running turn's snapshot and the durable
//! record is the CLI's own session journal (the timeline).

use std::sync::Arc;

use ironclaw_cli_providers::bridge::{CliBridge, GateView, LiveEvent, ToolState, ToolView};
use ironclaw_extension_contracts::external::ExternalConversationRef;
use ironclaw_host_api::ids::{CapabilityId, InvocationId, ThreadId};
use ironclaw_host_api::product_adapter::{AdapterInstallationId, ProductAdapterId};
use ironclaw_host_api::turn::{
    AcceptedMessageRef, EventCursor, ReplyTargetBindingRef, TurnRunId, TurnStatus,
};
use ironclaw_orchestrator::model::ThreadRecord;
use ironclaw_orchestrator::profile_runtime::UserTurn;
use ironclaw_orchestrator::runtime::{HistoryMessage, HistoryRole};
use ironclaw_product_contracts::outbound::{
    CapabilityActivityStatusView, CapabilityActivityView, CapabilityActivityViewInput,
    PROJECTION_TEXT_MAX_BYTES, ProductGateKind, ProductOutboundEnvelope, ProductOutboundPayload,
    ProductOutboundTarget, ProductProjectionItem, ProductProjectionState, ProjectionCursor,
};
use ironclaw_product_contracts::product_wire::{
    RebornCancelRunResponse, RebornResolveGateResponse, RebornResumeGateResponse,
    RebornSubmitTurnResponse,
};
use ironclaw_product_contracts::surface::{
    ProductSurfaceError, ProductSurfaceEventSubscription, ProductSurfaceQueryPage,
    ProductSurfaceStreamResponse,
};
use ironclaw_threads::{SessionThreadRecord, ThreadScope as SessionThreadScope};
use serde_json::{Value, json};

use crate::reborn_services::RebornTimelineResponse;
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

const LIVE_ADAPTER_ID: &str = "webui_v2";
const LIVE_INSTALLATION_ID: &str = "webui_v2.local";
const LIVE_SUBSCRIPTION_DEPTH: usize = 64;

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
        UserTurn::Steered { active_run_id } => RebornSubmitTurnResponse::DeferredBusy {
            thread_id: thread.clone(),
            accepted_message_ref: accepted,
            active_run_id: run_id(active_run_id.as_deref()),
            status: TurnStatus::Running,
            event_cursor: EventCursor(0),
            notice: "Sent to the running turn; the agent reads it at its next step.".to_string(),
        },
        UserTurn::Queued {
            active_run_id,
            steerable,
        } => RebornSubmitTurnResponse::DeferredBusy {
            thread_id: thread.clone(),
            accepted_message_ref: accepted,
            active_run_id: run_id(active_run_id.as_deref()),
            status: TurnStatus::Running,
            event_cursor: EventCursor(0),
            notice: if steerable {
                "Queued: it runs when the current turn ends."
            } else {
                "Queued: this engine can't steer mid-turn; it runs when the current turn ends."
            }
            .to_string(),
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
/// per position so a reload does not re-key the transcript. A run's
/// assistant text (one journal row per text block, split by tool calls) is
/// one reply, joined like the live stream joins it, so the streamed bubble
/// and the durable reply match.
/// The timeline envelope of a CLI thread the orchestrator created itself (a
/// fork or a delegated child), which the session thread store never saw:
/// the thread header from the orchestrator's record, messages filled in by
/// the caller. `None` when the thread has no agent to scope the header to.
pub(super) fn bare_timeline(thread: &ThreadRecord) -> Option<ProductSurfaceQueryPage> {
    let scope = &thread.scope;
    let at = chrono::DateTime::<chrono::Utc>::from_timestamp_millis;
    let header = RebornTimelineResponse {
        thread: SessionThreadRecord {
            scope: SessionThreadScope {
                tenant_id: scope.tenant_id.clone(),
                agent_id: scope.agent_id.clone()?,
                project_id: scope.project_id.clone(),
                owner_user_id: Some(scope.user_id.clone()),
                mission_id: None,
            },
            thread_id: thread.thread_id.clone(),
            created_by_actor_id: scope.user_id.as_str().to_string(),
            title: thread.title.clone(),
            metadata_json: None,
            goal: None,
            created_at: at(thread.created_at),
            updated_at: at(thread.updated_at),
        },
        messages: Vec::new(),
        summary_artifacts: Vec::new(),
        next_cursor: None,
    };
    Some(ProductSurfaceQueryPage {
        // silent-ok: a struct of ids, strings and timestamps always serializes.
        items: vec![serde_json::to_value(header).ok()?],
        next_cursor: None,
    })
}

pub(super) fn timeline_messages(
    thread: &ThreadId,
    history: &[HistoryMessage],
    limit: usize,
) -> Vec<Value> {
    let mut rows: Vec<Value> = Vec::new();
    for (i, m) in history.iter().enumerate().filter(|(_, m)| !m.hidden) {
        let kind = match m.role {
            HistoryRole::User => "user",
            HistoryRole::Assistant => "assistant",
            HistoryRole::System => "system",
            HistoryRole::Tool => continue,
        };
        if kind == "assistant"
            && m.run_id.is_some()
            && let Some(last) = rows.last_mut()
            && last["kind"] == "assistant"
            && last["turn_run_id"].as_str() == m.run_id.as_deref()
        {
            if !m.text.is_empty() {
                let joined = match last["content"].as_str() {
                    Some(prev) if !prev.is_empty() => format!("{prev}\n\n{}", m.text),
                    _ => m.text.clone(),
                };
                last["content"] = Value::String(joined);
            }
            continue;
        }
        if kind == "assistant" && m.text.is_empty() {
            continue; // a tool-call-only step; its text, if any, follows
        }
        let id = Uuid::new_v5(&Uuid::NAMESPACE_OID, format!("{thread}:{i}").as_bytes());
        rows.push(json!({
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
        }));
    }
    let skip = rows.len().saturating_sub(limit);
    rows.into_iter().skip(skip).collect()
}

/// `gate.resolve` on a CLI thread: answer the pending tool call. "Always"
/// is not offered (it would be an allow rule) and counts as once.
pub(super) fn resolve_gate(
    bridge: &CliBridge,
    thread: &ThreadId,
    run: &str,
    gate_ref: &str,
    approved: bool,
) -> Result<RebornResolveGateResponse, ProductSurfaceError> {
    if !bridge.resolve_gate(thread, gate_ref, approved) {
        return Err(ProductSurfaceError::not_found());
    }
    Ok(RebornResolveGateResponse::Resumed(
        RebornResumeGateResponse {
            run_id: run_id(Some(run)),
            status: TurnStatus::Running,
            event_cursor: EventCursor(0),
        },
    ))
}

/// The thread's live stream: the running turn's snapshot now, then every
/// change while the subscription is held.
pub(super) fn live_stream(
    bridge: Arc<CliBridge>,
    thread: ThreadId,
) -> Result<ProductSurfaceStreamResponse, ProductSurfaceError> {
    // Subscribe before the snapshot so nothing falls between the two.
    let mut events = bridge.subscribe();
    let initial = snapshot_events(&bridge, &thread);
    let (tx, rx) = mpsc::channel(LIVE_SUBSCRIPTION_DEPTH);
    tokio::spawn(async move {
        loop {
            let frames = match events.recv().await {
                Ok(event) if event.thread() == &thread => items_for(&event)
                    .and_then(|items| envelope(&thread, items))
                    .into_iter()
                    .collect(),
                Ok(_) => continue,
                // Fell behind: a fresh snapshot replaces what was missed.
                Err(broadcast::error::RecvError::Lagged(_)) => snapshot_events(&bridge, &thread),
                Err(broadcast::error::RecvError::Closed) => return,
            };
            if frames.is_empty() {
                continue;
            }
            let response = ProductSurfaceStreamResponse {
                next_cursor: None,
                events: frames,
                subscription: None,
            };
            if tx.send(Ok(response)).await.is_err() {
                return; // the stream closed
            }
        }
    });
    Ok(ProductSurfaceStreamResponse {
        events: initial,
        next_cursor: None,
        subscription: Some(ProductSurfaceEventSubscription::new(rx)),
    })
}

fn snapshot_events(bridge: &CliBridge, thread: &ThreadId) -> Vec<Value> {
    let Some(snap) = bridge.snapshot(thread) else {
        return Vec::new();
    };
    let Ok(run) = TurnRunId::parse(&snap.run_id) else {
        return Vec::new();
    };
    let mut items = vec![run_status(run, "running")];
    // Segments and calls interleave as they happened: a call follows the
    // segment that led to it.
    for seg in &snap.segments {
        items.extend(thinking_item(
            thread,
            &snap.run_id,
            seg.segment,
            &seg.thinking,
        ));
        items.extend(text_item(thread, &snap.run_id, seg.segment, &seg.text));
    }
    items.extend(snap.tools.iter().filter_map(|t| tool_item(thread, t)));
    items.extend(snap.gates.iter().filter_map(gate_item));
    envelope(thread, items).into_iter().collect()
}

fn items_for(event: &LiveEvent) -> Option<Vec<ProductProjectionItem>> {
    Some(match event {
        LiveEvent::Started { run_id, .. } | LiveEvent::GateClosed { run_id, .. } => {
            // A later status closes an answered gate in the client.
            vec![run_status(TurnRunId::parse(run_id).ok()?, "running")]
        }
        LiveEvent::Text {
            thread,
            run_id,
            segment,
            body,
        } => vec![text_item(thread, run_id, *segment, body)?],
        LiveEvent::Thinking {
            thread,
            run_id,
            segment,
            body,
        } => vec![thinking_item(thread, run_id, *segment, body)?],
        LiveEvent::Tool { thread, tool } => vec![tool_item(thread, tool)?],
        LiveEvent::GateOpened { gate, .. } => vec![gate_item(gate)?],
        LiveEvent::Ended { run_id, status, .. } => {
            let status = match *status {
                "completed" | "cancelled" => *status,
                _ => "failed",
            };
            vec![run_status(TurnRunId::parse(run_id).ok()?, status)]
        }
    })
}

fn run_status(run_id: TurnRunId, status: &str) -> ProductProjectionItem {
    ProductProjectionItem::RunStatus {
        run_id,
        status: status.to_string(),
        failure_category: None,
        failure_summary: None,
        retryable: None,
    }
}

/// The streamed reply. Past the projection's text bound the bubble stops
/// growing; the durable reply replaces it when the turn ends.
fn text_item(
    thread: &ThreadId,
    run_id: &str,
    segment: u32,
    body: &str,
) -> Option<ProductProjectionItem> {
    if body.is_empty() || body.len() > PROJECTION_TEXT_MAX_BYTES {
        return None;
    }
    Some(ProductProjectionItem::Text {
        id: format!("text:{run_id}:cli:{thread}:{segment}"),
        run_id: Some(TurnRunId::parse(run_id).ok()?),
        body: body.to_string(),
        finalized: false,
        narration: false,
    })
}

/// The streamed thinking, bounded like the reply.
fn thinking_item(
    thread: &ThreadId,
    run_id: &str,
    segment: u32,
    body: &str,
) -> Option<ProductProjectionItem> {
    if body.is_empty() || body.len() > PROJECTION_TEXT_MAX_BYTES {
        return None;
    }
    Some(ProductProjectionItem::Thinking {
        id: format!("thinking:{run_id}:cli:{thread}:{segment}"),
        run_id: Some(TurnRunId::parse(run_id).ok()?),
        body: body.to_string(),
    })
}

/// A CLI tool call as an activity row: `cli.<tool>` (`cli.bash`,
/// `cli.mcp.server.tool`), its subject as the input summary.
fn tool_item(thread: &ThreadId, tool: &ToolView) -> Option<ProductProjectionItem> {
    let status = match tool.state {
        ToolState::Running => CapabilityActivityStatusView::Running,
        ToolState::Completed => CapabilityActivityStatusView::Completed,
        ToolState::Failed => CapabilityActivityStatusView::Failed,
    };
    let view = CapabilityActivityView::new(CapabilityActivityViewInput {
        invocation_id: InvocationId::parse(&tool.invocation).ok()?,
        turn_run_id: Some(TurnRunId::parse(&tool.run_id).ok()?),
        thread_id: Some(thread.clone()),
        capability_id: cli_capability_id(&tool.tool_name)?,
        status,
        provider: None,
        runtime: None,
        process_id: None,
        output_bytes: None,
        error_kind: None,
        error_detail: None,
        subtitle: None,
        input_summary: tool.subject.clone(),
        updated_at: chrono::Utc::now(),
        activity_order: None,
    });
    match view {
        Ok(view) => Some(ProductProjectionItem::CapabilityActivity(view)),
        Err(error) => {
            tracing::debug!(%error, tool = %tool.tool_name, "cli tool row not projected");
            None
        }
    }
}

/// `Bash` → `cli.bash`; `mcp__srv__do_it` → `cli.mcp.srv.do_it`. Each
/// segment is folded to the id grammar (lowercase, `[a-z0-9_-]`).
fn cli_capability_id(tool: &str) -> Option<CapabilityId> {
    let segments: Vec<String> = tool
        .split("__")
        .filter(|s| !s.is_empty())
        .map(|s| {
            let s: String = s
                .chars()
                .map(|c| match c.to_ascii_lowercase() {
                    c @ ('a'..='z' | '0'..='9' | '_' | '-') => c,
                    _ => '_',
                })
                .take(100)
                .collect();
            match s.starts_with(|c: char| c.is_ascii_alphanumeric()) {
                true => s,
                false => format!("t{s}"),
            }
        })
        .collect();
    CapabilityId::new(format!("cli.{}", segments.join("."))).ok()
}

fn gate_item(gate: &GateView) -> Option<ProductProjectionItem> {
    Some(ProductProjectionItem::Gate {
        run_id: TurnRunId::parse(&gate.run_id).ok()?,
        gate_kind: ProductGateKind::Approval,
        gate_ref: gate.gate_ref.clone(),
        invocation_id: None,
        headline: gate.headline.clone(),
        body: gate.body.clone(),
        allow_always: false,
        reason: gate.reason.clone(),
        auth_context: None,
    })
}

fn envelope(thread: &ThreadId, items: Vec<ProductProjectionItem>) -> Option<Value> {
    let state = ProductProjectionState::new(thread.as_str(), items).ok()?;
    let target = ProductOutboundTarget::new(
        ReplyTargetBindingRef::new("reply:cli-bridge").ok()?,
        ExternalConversationRef::new(None, thread.as_str(), None, None).ok()?,
        None,
    );
    let envelope = ProductOutboundEnvelope::new(
        ProductAdapterId::new(LIVE_ADAPTER_ID).ok()?,
        AdapterInstallationId::new(LIVE_INSTALLATION_ID).ok()?,
        target,
        ProjectionCursor::new(format!("cli:{}", Uuid::new_v4().simple())).ok()?,
        ProductOutboundPayload::ProjectionUpdate { state },
    );
    serde_json::to_value(envelope).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: HistoryRole, text: &str, run: &str) -> HistoryMessage {
        HistoryMessage {
            role,
            text: text.to_string(),
            tool_calls: Vec::new(),
            run_id: Some(run.to_string()),
            hidden: false,
            at: None,
        }
    }

    /// The streamed bubble joins text blocks with a blank line; the durable
    /// reply must come out identical so the client keeps one copy.
    #[test]
    fn a_runs_assistant_steps_become_one_reply() {
        let thread = ThreadId::new("t").unwrap();
        let history = [
            msg(HistoryRole::User, "hi", "r1"),
            msg(HistoryRole::Assistant, "Let me look.", "r1"),
            msg(HistoryRole::Tool, "", "r1"),
            msg(HistoryRole::Assistant, "", "r1"),
            msg(HistoryRole::Assistant, "Done.", "r1"),
            msg(HistoryRole::User, "again", "r2"),
            msg(HistoryRole::Assistant, "Sure.", "r2"),
        ];
        let rows = timeline_messages(&thread, &history, 10);
        let shape: Vec<(&str, &str, &str)> = rows
            .iter()
            .map(|r| {
                (
                    r["kind"].as_str().unwrap(),
                    r["turn_run_id"].as_str().unwrap(),
                    r["content"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            [
                ("user", "r1", "hi"),
                ("assistant", "r1", "Let me look.\n\nDone."),
                ("user", "r2", "again"),
                ("assistant", "r2", "Sure."),
            ]
        );
    }
}
