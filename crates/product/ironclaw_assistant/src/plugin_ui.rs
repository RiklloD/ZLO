//! `ProductSurface` decorator that answers the plugin UI views
//! (`ironclaw_product_contracts::plugin_ui`) through a [`PluginUiHost`];
//! every other request passes through untouched.
//!
//! Product policy lives here: a slot renders only if the host lists it for
//! this caller, render time is bounded, and the render tool's output must
//! validate as a `reborn.plugin_ui.v1` tree for the slot's kind. A tree that
//! fails any of that becomes `{ "error": "<plain reason>" }` — shown to the
//! user, never to the model, and never a crash. Nothing is cached or stored.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ironclaw_extension_contracts::plugin_ui::{PLUGIN_UI_MAX_EVENT_PAYLOAD_BYTES, PluginUiNode};
use ironclaw_product_contracts::plugin_ui::{
    PLUGIN_UI_RENDER_VIEW, PLUGIN_UI_SLOTS_VIEW, PluginUiHost, PluginUiRenderOutcome,
    PluginUiRenderRequest, PluginUiRenderResponse, PluginUiSlotInfo, PluginUiSlotsRequest,
    PluginUiSlotsResponse,
};
use ironclaw_product_contracts::surface::{
    ProductSurface, ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceInvokeRequest,
    ProductSurfaceInvokeResponse, ProductSurfaceQueryPage, ProductSurfaceQueryRequest,
    ProductSurfaceStreamRequest, ProductSurfaceStreamResponse, ProductSurfaceValidationCode,
};
use serde::Serialize;
use serde_json::{Value, json};
use tracing::debug;

/// How long one render-tool call may take before the slot shows an error.
pub const PLUGIN_UI_RENDER_TIMEOUT: Duration = Duration::from_secs(5);

pub struct PluginUiSurface {
    inner: Arc<dyn ProductSurface>,
    host: Arc<dyn PluginUiHost>,
    render_timeout: Duration,
}

impl PluginUiSurface {
    pub fn new(inner: Arc<dyn ProductSurface>, host: Arc<dyn PluginUiHost>) -> Self {
        Self {
            inner,
            host,
            render_timeout: PLUGIN_UI_RENDER_TIMEOUT,
        }
    }

    /// Override the render bound (tests drive the timeout path with it).
    pub fn with_render_timeout(mut self, render_timeout: Duration) -> Self {
        self.render_timeout = render_timeout;
        self
    }

    async fn slots(
        &self,
        caller: &ProductSurfaceCaller,
        input: Value,
    ) -> Result<PluginUiSlotsResponse, ProductSurfaceError> {
        let _request: PluginUiSlotsRequest = parse_params(input)?;
        let slots = self
            .host
            .list_slots(caller)
            .await?
            .into_iter()
            .map(|binding| PluginUiSlotInfo {
                extension_id: binding.extension_id,
                slot_id: binding.descriptor.id,
                slot: binding.descriptor.slot,
                title: binding.descriptor.title,
                icon: binding.descriptor.icon,
            })
            .collect();
        Ok(PluginUiSlotsResponse { slots })
    }

    async fn render(
        &self,
        caller: &ProductSurfaceCaller,
        input: Value,
    ) -> Result<PluginUiRenderResponse, ProductSurfaceError> {
        let request: PluginUiRenderRequest = parse_params(input)?;
        if let Some(event) = &request.event {
            let payload_bytes = serde_json::to_vec(&event.payload)
                .map_err(ProductSurfaceError::internal_from)?
                .len();
            if payload_bytes > PLUGIN_UI_MAX_EVENT_PAYLOAD_BYTES {
                return Err(ProductSurfaceError::validation(
                    "payload",
                    ProductSurfaceValidationCode::TooLong,
                ));
            }
        }
        // Re-resolve the slot from the host's caller-scoped listing on every
        // render: a slot of an inactive, removed, or someone else's private
        // extension is not found, whatever the request names.
        let binding = self
            .host
            .list_slots(caller)
            .await?
            .into_iter()
            .find(|binding| {
                binding.extension_id == request.extension_id
                    && binding.descriptor.id == request.slot_id
            })
            .ok_or_else(ProductSurfaceError::not_found)?;
        let tool_input = json!({
            "slot_id": binding.descriptor.id,
            "context": { "thread_id": request.context.thread_id },
            "event": request.event.as_ref().map(|event| json!({
                "action": event.action,
                "payload": event.payload,
            })),
        });
        let outcome = tokio::time::timeout(
            self.render_timeout,
            self.host.invoke_render(caller, &binding, tool_input),
        )
        .await;
        let response = match outcome {
            Err(_elapsed) => PluginUiRenderResponse::Error(format!(
                "the plugin did not render within {} seconds",
                self.render_timeout.as_secs_f32()
            )),
            Ok(result) => match result? {
                PluginUiRenderOutcome::Refused(reason) => PluginUiRenderResponse::Error(reason),
                PluginUiRenderOutcome::Output(output) => {
                    match PluginUiNode::parse_for_slot(&output, binding.descriptor.slot) {
                        Ok(tree) => PluginUiRenderResponse::Tree(tree),
                        Err(error) => PluginUiRenderResponse::Error(error.to_string()),
                    }
                }
            },
        };
        if let PluginUiRenderResponse::Error(reason) = &response {
            debug!(
                extension_id = %binding.extension_id,
                slot_id = %binding.descriptor.id,
                %reason,
                "plugin ui render produced no tree"
            );
        }
        Ok(response)
    }
}

fn parse_params<T: serde::de::DeserializeOwned>(input: Value) -> Result<T, ProductSurfaceError> {
    serde_json::from_value(input).map_err(|error| {
        debug!(%error, "plugin ui request rejected");
        ProductSurfaceError::validation("body", ProductSurfaceValidationCode::InvalidValue)
    })
}

fn page(payload: impl Serialize) -> Result<ProductSurfaceQueryPage, ProductSurfaceError> {
    Ok(ProductSurfaceQueryPage {
        items: vec![serde_json::to_value(payload).map_err(ProductSurfaceError::internal_from)?],
        next_cursor: None,
    })
}

#[async_trait]
impl ProductSurface for PluginUiSurface {
    async fn invoke(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSurfaceInvokeRequest,
    ) -> Result<ProductSurfaceInvokeResponse, ProductSurfaceError> {
        self.inner.invoke(caller, request).await
    }

    async fn query(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSurfaceQueryRequest,
    ) -> Result<ProductSurfaceQueryPage, ProductSurfaceError> {
        if request.view_id == PLUGIN_UI_SLOTS_VIEW.id {
            return page(self.slots(&caller, request.input).await?);
        }
        if request.view_id == PLUGIN_UI_RENDER_VIEW.id {
            return page(self.render(&caller, request.input).await?);
        }
        self.inner.query(caller, request).await
    }

    async fn stream_events(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSurfaceStreamRequest,
    ) -> Result<ProductSurfaceStreamResponse, ProductSurfaceError> {
        self.inner.stream_events(caller, request).await
    }
}
