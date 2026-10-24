//! The timeline of a thread that switched provider (`ironclaw_orchestrator::
//! segments`): each segment's messages from the runtime that ran them, with
//! an inline system marker where the switch happened. IronClaw-loop
//! segments keep their full transcript items (tool cards included); CLI
//! segments come from the runtime's session journal. The whole thread is
//! served at once (bounded by the request limit), like a CLI thread's.

use std::sync::Arc;

use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::handoff::strip_context;
use ironclaw_orchestrator::model::ThreadRecord;
use ironclaw_orchestrator::segments::Segment;
use ironclaw_product_contracts::surface::{
    ProductSurface, ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceQueryPage,
    ProductSurfaceQueryRequest,
};
use serde_json::{Value, json};
use uuid::Uuid;

use super::external;
use crate::reborn_services::TIMELINE_VIEW;

const PAGE: u64 = 200;
/// Same ceiling as the engine's history read.
const MAX_LOOP_ITEMS: usize = 2_000;

pub(super) async fn timeline(
    inner: &Arc<dyn ProductSurface>,
    engine: &Engine,
    caller: ProductSurfaceCaller,
    request: ProductSurfaceQueryRequest,
    thread: &ThreadRecord,
    segments: &[Segment],
) -> Result<ProductSurfaceQueryPage, ProductSurfaceError> {
    let id = &thread.thread_id;
    let limit = request.input["limit"].as_u64().unwrap_or(200).clamp(1, 500) as usize;
    let (mut envelope, loop_items) = loop_transcript(inner, &caller, request).await?;

    let mut rows: Vec<Value> = Vec::new();
    for (k, seg) in segments.iter().enumerate() {
        if k > 0 {
            rows.push(marker(id.as_str(), k, seg));
        }
        if engine.is_default_runtime(&seg.runtime_id) {
            let end = seg.end.unwrap_or(loop_items.len()).min(loop_items.len());
            for item in &loop_items[seg.start.min(end)..end] {
                let mut item = item.clone();
                if item["kind"] == "user"
                    && let Some(typed) = item["content"].as_str().and_then(strip_context)
                {
                    if typed.is_empty() {
                        continue;
                    }
                    item["content"] = Value::String(typed.to_string());
                }
                rows.push(item);
            }
        } else {
            let slice = engine
                .segment_slices(thread, std::slice::from_ref(seg))
                .await
                .map_err(ProductSurfaceError::internal_from)?
                .pop()
                .unwrap_or_default();
            for mut row in external::timeline_messages(id, &slice, usize::MAX) {
                // Message ids restart per segment; keep them unique.
                let old = row["message_id"].to_string();
                row["message_id"] = json!(Uuid::new_v5(
                    &Uuid::NAMESPACE_OID,
                    format!("{id}:{k}:{old}").as_bytes()
                ));
                rows.push(row);
            }
        }
    }
    let skip = rows.len().saturating_sub(limit);
    let mut rows: Vec<Value> = rows.into_iter().skip(skip).collect();
    for (i, row) in rows.iter_mut().enumerate() {
        row["sequence"] = json!(skip + i + 1);
    }
    if let Some(item) = envelope.items.first_mut() {
        item["messages"] = Value::Array(rows);
        item["next_cursor"] = Value::Null;
    }
    envelope.next_cursor = None;
    Ok(envelope)
}

fn marker(thread: &str, k: usize, seg: &Segment) -> Value {
    json!({
        "message_id": Uuid::new_v5(&Uuid::NAMESPACE_OID, format!("{thread}:switch:{k}").as_bytes()),
        "thread_id": thread,
        "sequence": 0,
        "kind": "system",
        "status": "finalized",
        "actor_id": null,
        "source_binding_id": null,
        "reply_target_binding_id": null,
        "turn_id": null,
        "turn_run_id": null,
        "content": seg.marker(),
        "redaction_ref": null,
    })
}

/// The first timeline page (the response envelope) and every IronClaw
/// transcript item of the thread, oldest first, counted the way the loop
/// runtime's history counts them (checkpoints are not messages).
async fn loop_transcript(
    inner: &Arc<dyn ProductSurface>,
    caller: &ProductSurfaceCaller,
    request: ProductSurfaceQueryRequest,
) -> Result<(ProductSurfaceQueryPage, Vec<Value>), ProductSurfaceError> {
    let mut input = request.input.clone();
    input["limit"] = json!(PAGE);
    input["cursor"] = Value::Null;
    let mut query = ProductSurfaceQueryRequest {
        view_id: TIMELINE_VIEW.id.to_string(),
        input,
        cursor: None,
        limit: None,
    };
    let mut envelope: Option<ProductSurfaceQueryPage> = None;
    let mut pages: Vec<Vec<Value>> = Vec::new();
    let mut total = 0;
    loop {
        let page = inner.query(caller.clone(), query.clone()).await?;
        let item = page.items.first().cloned().unwrap_or(Value::Null);
        let messages = item["messages"].as_array().cloned().unwrap_or_default();
        total += messages.len();
        pages.push(messages);
        let next = page
            .next_cursor
            .clone()
            .or_else(|| item["next_cursor"].as_str().map(str::to_string));
        envelope.get_or_insert(page);
        match next {
            Some(cursor) if total < MAX_LOOP_ITEMS => query.cursor = Some(cursor),
            _ => break,
        }
    }
    let envelope = envelope.ok_or_else(ProductSurfaceError::internal)?;
    // Pages arrive newest first; each page is oldest-first inside.
    let items = pages
        .into_iter()
        .rev()
        .flatten()
        .filter(|m| m["kind"] != "checkpoint_reference")
        .collect();
    Ok((envelope, items))
}
