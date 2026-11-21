//! Composition's [`PluginUiHost`]: joins the three things only the assembly
//! root holds together — the live active-extension snapshot, the durable
//! installation owners and resolved manifests, and the product-origin
//! capability invoker. Policy (tree validation, slot constraints, the render
//! time bound) is `ironclaw_assistant::plugin_ui`'s; this file only looks
//! things up and invokes.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_extension_host::extension_lifecycle::RebornLocalExtensionManagementPort;
use ironclaw_extension_registry::SharedExtensionRegistry;
use ironclaw_host_api::result_meta::FailureKind;
use ironclaw_host_runtime::RuntimeCapabilityOutcome;
use ironclaw_product_contracts::plugin_ui::{
    PluginUiHost, PluginUiRenderOutcome, PluginUiSlotBinding,
};
use ironclaw_product_contracts::surface::{ProductSurfaceCaller, ProductSurfaceError};
use serde_json::Value;
use tracing::debug;

use crate::RebornRuntime;
use crate::product_capability::RuntimeProductCapabilityInvoker;

pub(crate) struct RuntimePluginUiHost {
    registry: Arc<SharedExtensionRegistry>,
    extensions: Arc<RebornLocalExtensionManagementPort>,
    invoker: RuntimeProductCapabilityInvoker,
}

impl RuntimePluginUiHost {
    pub(crate) fn from_runtime(runtime: &RebornRuntime) -> Self {
        Self {
            registry: Arc::clone(&runtime.shared_extension_registry),
            extensions: Arc::clone(&runtime.extension_management),
            invoker: RuntimeProductCapabilityInvoker::from_runtime(runtime),
        }
    }
}

#[async_trait]
impl PluginUiHost for RuntimePluginUiHost {
    async fn list_slots(
        &self,
        caller: &ProductSurfaceCaller,
    ) -> Result<Vec<PluginUiSlotBinding>, ProductSurfaceError> {
        let owners = self
            .extensions
            .installation_owners()
            .await
            .map_err(ProductSurfaceError::from)?;
        let manifests = self
            .extensions
            .installation_store_handle()
            .list_manifests()
            .await
            .map_err(ProductSurfaceError::internal_from)?;
        let active = self.registry.snapshot();
        let mut slots = Vec::new();
        for record in manifests {
            let resolved = record.resolved();
            // Active (published into the live snapshot) and visible to this
            // caller; a missing owner row fails closed, like the tool catalog.
            if resolved.ui_slots.is_empty()
                || active.get_extension(&resolved.id).is_none()
                || !owners
                    .get(&resolved.id)
                    .is_some_and(|owner| owner.visible_to(&caller.user_id))
            {
                continue;
            }
            for descriptor in &resolved.ui_slots {
                if let Err(error) = descriptor.validate() {
                    debug!(extension_id = %resolved.id, %error, "skipping invalid persisted ui slot");
                    continue;
                }
                slots.push(PluginUiSlotBinding {
                    extension_id: resolved.id.clone(),
                    descriptor: descriptor.clone(),
                });
            }
        }
        slots.sort_by(|left, right| left.extension_id.cmp(&right.extension_id));
        Ok(slots)
    }

    async fn invoke_render(
        &self,
        caller: &ProductSurfaceCaller,
        binding: &PluginUiSlotBinding,
        input: Value,
    ) -> Result<PluginUiRenderOutcome, ProductSurfaceError> {
        let descriptor = self
            .registry
            .snapshot()
            .get_capability(&binding.descriptor.render)
            .filter(|descriptor| descriptor.provider == binding.extension_id)
            .cloned();
        let Some(descriptor) = descriptor else {
            return Ok(PluginUiRenderOutcome::Refused(
                "the plugin's render tool is not active".to_string(),
            ));
        };
        if let Err(reason) = ironclaw_extension_registry::check_render_descriptor(&descriptor) {
            debug!(extension_id = %binding.extension_id, %reason, "refusing unsafe plugin ui render tool");
            return Ok(PluginUiRenderOutcome::Refused(
                "the plugin's render tool is not safe to run for a panel".to_string(),
            ));
        }
        let outcome = self
            .invoker
            .invoke_unpersisted(caller, &descriptor, input)
            .await?;
        Ok(match outcome {
            RuntimeCapabilityOutcome::Completed(completed) => {
                PluginUiRenderOutcome::Output(completed.output)
            }
            RuntimeCapabilityOutcome::Failed(failure) => {
                debug!(
                    capability = %failure.capability_id,
                    kind = failure.kind.as_str(),
                    "plugin ui render tool failed"
                );
                PluginUiRenderOutcome::Refused(match failure.kind {
                    FailureKind::Authorization | FailureKind::PolicyDenied => {
                        "the plugin's render tool was not allowed to run".to_string()
                    }
                    kind => format!("the plugin's render tool failed ({})", kind.as_str()),
                })
            }
            RuntimeCapabilityOutcome::ApprovalRequired(_)
            | RuntimeCapabilityOutcome::AuthRequired(_)
            | RuntimeCapabilityOutcome::ResourceBlocked(_)
            | RuntimeCapabilityOutcome::SpawnedProcess(_) => PluginUiRenderOutcome::Refused(
                "the plugin's render tool asked for something a plugin panel cannot provide"
                    .to_string(),
            ),
        })
    }
}
