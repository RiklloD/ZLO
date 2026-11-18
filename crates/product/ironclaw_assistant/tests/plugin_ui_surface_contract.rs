//! `PluginUiSurface` contract at the product-surface seam: the exact input a
//! render tool receives, the render time bound, slot re-resolution, and that
//! every non-tree outcome is a plain `{ "error" }` answer. The end-to-end
//! path over a real runtime is `tests/integration/plugin_ui.rs`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use ironclaw_assistant::plugin_ui::PluginUiSurface;
use ironclaw_extension_contracts::plugin_ui::{PluginUiSlotDescriptor, PluginUiSlotKind};
use ironclaw_host_api::ids::{ExtensionId, TenantId, UserId};
use ironclaw_product_contracts::plugin_ui::{
    PLUGIN_UI_RENDER_VIEW, PluginUiHost, PluginUiRenderOutcome, PluginUiSlotBinding,
};
use ironclaw_product_contracts::surface::{
    ProductSurface, ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceInvokeRequest,
    ProductSurfaceInvokeResponse, ProductSurfaceQueryPage, ProductSurfaceQueryRequest,
    ProductSurfaceStreamRequest, ProductSurfaceStreamResponse,
};
use serde_json::{Value, json};

struct Unreachable;

#[async_trait]
impl ProductSurface for Unreachable {
    async fn invoke(
        &self,
        _: ProductSurfaceCaller,
        _: ProductSurfaceInvokeRequest,
    ) -> Result<ProductSurfaceInvokeResponse, ProductSurfaceError> {
        Err(ProductSurfaceError::internal())
    }
    async fn query(
        &self,
        _: ProductSurfaceCaller,
        _: ProductSurfaceQueryRequest,
    ) -> Result<ProductSurfaceQueryPage, ProductSurfaceError> {
        Err(ProductSurfaceError::internal())
    }
    async fn stream_events(
        &self,
        _: ProductSurfaceCaller,
        _: ProductSurfaceStreamRequest,
    ) -> Result<ProductSurfaceStreamResponse, ProductSurfaceError> {
        Err(ProductSurfaceError::internal())
    }
}

/// Records every render call; answers with a scripted outcome after a delay.
struct ScriptedHost {
    slot: PluginUiSlotKind,
    outcome: PluginUiRenderOutcome,
    delay: Duration,
    calls: Mutex<Vec<(UserId, Value)>>,
}

impl ScriptedHost {
    fn new(slot: PluginUiSlotKind, outcome: PluginUiRenderOutcome) -> Arc<Self> {
        Arc::new(Self {
            slot,
            outcome,
            delay: Duration::ZERO,
            calls: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl PluginUiHost for ScriptedHost {
    async fn list_slots(
        &self,
        _caller: &ProductSurfaceCaller,
    ) -> Result<Vec<PluginUiSlotBinding>, ProductSurfaceError> {
        let descriptor: PluginUiSlotDescriptor = serde_json::from_value(json!({
            "id": "main", "slot": self.slot, "title": "Main", "render": "probe.render",
        }))
        .expect("descriptor");
        Ok(vec![PluginUiSlotBinding {
            extension_id: ExtensionId::new("probe").expect("id"),
            descriptor,
        }])
    }

    async fn invoke_render(
        &self,
        caller: &ProductSurfaceCaller,
        _binding: &PluginUiSlotBinding,
        input: Value,
    ) -> Result<PluginUiRenderOutcome, ProductSurfaceError> {
        self.calls
            .lock()
            .expect("lock")
            .push((caller.user_id.clone(), input));
        tokio::time::sleep(self.delay).await;
        Ok(self.outcome.clone())
    }
}

fn caller() -> ProductSurfaceCaller {
    ProductSurfaceCaller::new(
        TenantId::new("t").expect("tenant"),
        UserId::new("alice").expect("user"),
        None,
        None,
    )
}

async fn render(surface: &PluginUiSurface, input: Value) -> Result<Value, ProductSurfaceError> {
    let page = surface
        .query(
            caller(),
            ProductSurfaceQueryRequest {
                view_id: PLUGIN_UI_RENDER_VIEW.id.to_string(),
                input,
                cursor: None,
                limit: None,
            },
        )
        .await?;
    Ok(page.items.into_iter().next().expect("one payload"))
}

#[tokio::test]
async fn render_tool_receives_the_documented_input_and_its_tree_is_returned() {
    let host = ScriptedHost::new(
        PluginUiSlotKind::PanelTab,
        PluginUiRenderOutcome::Output(json!({ "type": "text", "text": "hi" })),
    );
    let surface = PluginUiSurface::new(Arc::new(Unreachable), host.clone());
    let answer = render(
        &surface,
        json!({
            "extension_id": "probe", "slot_id": "main",
            "context": { "thread_id": "thread-1" },
            "event": { "action": "go", "payload": { "n": 1 } },
        }),
    )
    .await
    .expect("renders");
    assert_eq!(answer, json!({ "tree": { "type": "text", "text": "hi" } }));
    let calls = host.calls.lock().expect("lock");
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0.as_str(), "alice");
    assert_eq!(
        calls[0].1,
        json!({
            "slot_id": "main",
            "context": { "thread_id": "thread-1" },
            "event": { "action": "go", "payload": { "n": 1 } },
        })
    );
}

#[tokio::test]
async fn a_slow_render_tool_becomes_a_plain_error_within_the_bound() {
    let host = Arc::new(ScriptedHost {
        slot: PluginUiSlotKind::PanelTab,
        outcome: PluginUiRenderOutcome::Output(json!({ "type": "divider" })),
        delay: Duration::from_secs(30),
        calls: Mutex::new(Vec::new()),
    });
    let surface = PluginUiSurface::new(Arc::new(Unreachable), host)
        .with_render_timeout(Duration::from_millis(50));
    let answer = render(
        &surface,
        json!({ "extension_id": "probe", "slot_id": "main" }),
    )
    .await
    .expect("answers");
    let error = answer["error"].as_str().expect("an error answer");
    assert!(error.contains("did not render within"), "{error}");
}

#[tokio::test]
async fn refusals_and_slot_constraints_are_errors_and_unknown_slots_are_not_found() {
    let refused = ScriptedHost::new(
        PluginUiSlotKind::PanelTab,
        PluginUiRenderOutcome::Refused("the plugin's render tool was not allowed to run".into()),
    );
    let surface = PluginUiSurface::new(Arc::new(Unreachable), refused);
    let answer = render(
        &surface,
        json!({ "extension_id": "probe", "slot_id": "main" }),
    )
    .await
    .expect("answers");
    assert_eq!(
        answer,
        json!({ "error": "the plugin's render tool was not allowed to run" })
    );

    let badge_slot = ScriptedHost::new(
        PluginUiSlotKind::SidebarSessionRowBadge,
        PluginUiRenderOutcome::Output(json!({ "type": "col", "children": [] })),
    );
    let surface = PluginUiSurface::new(Arc::new(Unreachable), badge_slot.clone());
    let answer = render(
        &surface,
        json!({ "extension_id": "probe", "slot_id": "main" }),
    )
    .await
    .expect("answers");
    assert_eq!(
        answer,
        json!({ "error": "a sidebar.session_row.badge slot must render a single badge or text element" })
    );

    let missing = render(
        &surface,
        json!({ "extension_id": "probe", "slot_id": "other" }),
    )
    .await
    .expect_err("unknown slot");
    assert_eq!(missing.status_code, 404);
    let foreign = render(
        &surface,
        json!({ "extension_id": "elsewhere", "slot_id": "main" }),
    )
    .await
    .expect_err("another extension's slot id");
    assert_eq!(foreign.status_code, 404);
    assert_eq!(
        badge_slot.calls.lock().expect("lock").len(),
        1,
        "no render tool runs for a slot that is not listed"
    );
}
