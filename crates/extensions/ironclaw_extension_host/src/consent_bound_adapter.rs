//! Dispatch-time consent for third-party packages.
//!
//! A capability grant is minted from the active surface when a run builds its
//! port, and it names no package generation: a run may hold an authorization
//! minted while package A was published and dispatch after A was removed or
//! replaced by B under the same extension/tool ids. Dispatch resolves the
//! binding published *now*, so the consent check that admits a principal has
//! to be made against that binding, at the moment it is invoked — not when
//! the grant was minted.
//!
//! [`ConsentBoundToolAdapter`] is that check. The generic host's loader wraps
//! every lane-bound tool adapter of a package that needs consent in it,
//! together with the consent digest the package's execution pins were granted
//! under (the same pins record the runtime lane verifies the executed bytes
//! against). Each invocation — a loop run, a credential-gate or approval
//! resume, a product-origin call such as a plugin UI render: every path
//! reaches the adapter through the snapshot resolver — first requires the
//! invoking user's governing principal to hold a decision naming exactly that
//! digest ([`ExtensionConsentStore::execution_consented`]). Otherwise the
//! call is refused as a model-visible `policy_denied` outcome with a plain
//! reason, its forwarded reservation released, and the inner adapter is never
//! reached.
//!
//! Host-bundled and user-registered packages need no consent; the loader does
//! not wrap them.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_extension_contracts::tool_adapter::{
    ToolAdapter, ToolCall, ToolError, ToolPorts, ToolResult,
};
use ironclaw_extension_registry::{
    ExtensionInstallationId, ExtensionInstallationStorePort, PackageContentPins,
};
use ironclaw_host_api::dispatch::{ProviderDiagnostic, RuntimeDispatchErrorKind};
use ironclaw_host_api::ids::ExtensionId;
use ironclaw_resources::ResourceGovernor;

use crate::extension_consent::ExtensionConsentStore;

/// A lane-bound tool adapter of a consent-requiring package, admitted per
/// invocation only for principals who consented to this exact generation.
pub(crate) struct ConsentBoundToolAdapter {
    inner: Arc<dyn ToolAdapter>,
    extension_id: ExtensionId,
    installation_id: ExtensionInstallationId,
    /// The execution pins this binding verifies against; their consent
    /// digest names the generation a principal must have consented to.
    pins: Option<PackageContentPins>,
    consent: ExtensionConsentStore,
    installation_store: Arc<dyn ExtensionInstallationStorePort>,
    governor: Arc<dyn ResourceGovernor>,
}

impl ConsentBoundToolAdapter {
    pub(crate) fn new(
        inner: Arc<dyn ToolAdapter>,
        extension_id: ExtensionId,
        installation_id: ExtensionInstallationId,
        pins: Option<PackageContentPins>,
        consent: ExtensionConsentStore,
        installation_store: Arc<dyn ExtensionInstallationStorePort>,
        governor: Arc<dyn ResourceGovernor>,
    ) -> Self {
        Self {
            inner,
            extension_id,
            installation_id,
            pins,
            consent,
            installation_store,
            governor,
        }
    }

    async fn consented(&self, call: &ToolCall) -> Result<bool, String> {
        let installation = self
            .installation_store
            .get_installation(&self.installation_id)
            .await
            .map_err(|error| format!("installation unavailable: {error}"))?;
        self.consent
            .execution_consented(
                &self.extension_id,
                self.pins.as_ref(),
                installation.as_ref().map(|row| row.owner()),
                &call.scope.user_id,
            )
            .await
            .map_err(|error| format!("consent record unavailable: {error}"))
    }

    fn refuse(
        &self,
        call: &mut ToolCall,
        kind: RuntimeDispatchErrorKind,
        reason: String,
    ) -> ToolError {
        // The binding owns the forwarded reservation's settle leg; a refused
        // call never reaches the lane that would otherwise release it.
        if let Some(reservation) = call.resources.reservation.take()
            && let Err(error) = self.governor.release(reservation.id)
        {
            tracing::debug!(
                extension_id = self.extension_id.as_str(),
                %error,
                "failed to release the reservation of a call refused for consent"
            );
        }
        ToolError::Rejected {
            kind,
            diagnostic: Some(Box::new(ProviderDiagnostic {
                code: None,
                message: Some(ironclaw_host_api::dispatch::UntrustedProviderMessage::new(
                    reason,
                )),
                retry_after: None,
            })),
            detail: None,
        }
    }
}

#[async_trait]
impl ToolAdapter for ConsentBoundToolAdapter {
    async fn invoke(
        &self,
        mut call: ToolCall,
        ports: &ToolPorts<'_>,
    ) -> Result<ToolResult, ToolError> {
        match self.consented(&call).await {
            Ok(true) => self.inner.invoke(call, ports).await,
            Ok(false) => {
                tracing::debug!(
                    extension_id = self.extension_id.as_str(),
                    capability_id = call.capability_id.as_str(),
                    "refusing a plugin call: the invoking user has not consented to the published package"
                );
                let reason = format!(
                    "plugin {} runs only for users who consented to its current files, and this \
                     user has not consented to the version now installed; review it and grant \
                     consent to use it",
                    self.extension_id.as_str()
                );
                Err(self.refuse(&mut call, RuntimeDispatchErrorKind::PolicyDenied, reason))
            }
            Err(cause) => {
                tracing::debug!(
                    extension_id = self.extension_id.as_str(),
                    %cause,
                    "refusing a plugin call: consent could not be checked"
                );
                let reason = format!(
                    "plugin {} could not be run: its consent could not be checked",
                    self.extension_id.as_str()
                );
                Err(self.refuse(&mut call, RuntimeDispatchErrorKind::Backend, reason))
            }
        }
    }
}
