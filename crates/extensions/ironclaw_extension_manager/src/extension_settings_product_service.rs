//! The product face of package-declared settings: lists an active
//! extension's `[[settings]]` with the caller's values and sets one value,
//! implementing [`ExtensionSettingsSource`] over the active extension set and
//! the registry-owned per-user [`ExtensionSettingsStore`].
//!
//! The declaration comes from the *active* package (the same record the
//! runtime lanes and the hook factory read), so a value is always validated
//! against the schema the plugin will actually run with. Scope comes only
//! from the authenticated caller.
//!
//! Visibility: the active registry is tenant-global, so every read and write
//! first checks the installation owner for the caller
//! ([`crate::installation_visibility`]). An installation the caller may not
//! see answers exactly like an unknown extension (`not_found`), so a
//! non-member can neither read another user's private plugin's schema and
//! defaults nor write settings for it.

use std::sync::Arc;

use crate::installation_visibility::{InstallationOwnerSource, installation_visible_to};
use async_trait::async_trait;
use ironclaw_extension_registry::SharedExtensionRegistry;
use ironclaw_extension_registry::settings::{
    ExtensionSettingDecl, ExtensionSettingKind, ExtensionSettingsError, ExtensionSettingsReader,
    ExtensionSettingsStore, effective_setting_values,
};
use ironclaw_filesystem::RootFilesystem;
use ironclaw_host_api::ids::{ExtensionId, InvocationId};
use ironclaw_host_api::resource::ResourceScope;
use ironclaw_product_contracts::extension_settings::{
    ExtensionSettingKindView, ExtensionSettingOptionView, ExtensionSettingSetOutcome,
    ExtensionSettingView, ExtensionSettingsSource,
};
use ironclaw_product_contracts::surface::{
    ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceValidationCode,
};

pub struct ExtensionSettingsProductService<F: RootFilesystem + ?Sized> {
    active: Arc<SharedExtensionRegistry>,
    store: Arc<ExtensionSettingsStore<F>>,
    owners: Arc<dyn InstallationOwnerSource>,
}

impl<F: RootFilesystem + ?Sized> ExtensionSettingsProductService<F> {
    pub fn new(
        active: Arc<SharedExtensionRegistry>,
        store: Arc<ExtensionSettingsStore<F>>,
        owners: Arc<dyn InstallationOwnerSource>,
    ) -> Self {
        Self {
            active,
            store,
            owners,
        }
    }

    /// The declarations of an active extension the caller may see. Unknown,
    /// inactive, and invisible extensions are all `not_found`.
    async fn declarations(
        &self,
        caller: &ProductSurfaceCaller,
        extension_id: &str,
    ) -> Result<(ExtensionId, Vec<ExtensionSettingDecl>), ProductSurfaceError> {
        let id = ExtensionId::new(extension_id).map_err(|error| {
            tracing::debug!(%error, "extension settings: invalid extension id");
            ProductSurfaceError::validation("extension_id", ProductSurfaceValidationCode::InvalidId)
        })?;
        let declarations = {
            let snapshot = self.active.snapshot();
            let package = snapshot
                .get_extension(&id)
                .ok_or_else(ProductSurfaceError::not_found)?;
            package.manifest.settings.clone()
        };
        let owners = self
            .owners
            .installation_owners()
            .await
            .map_err(ProductSurfaceError::internal_from)?;
        if !installation_visible_to(&owners, &id, &caller.user_id) {
            tracing::debug!(
                extension_id = %id.as_str(),
                "extension settings: installation not visible to caller"
            );
            return Err(ProductSurfaceError::not_found());
        }
        Ok((id, declarations))
    }
}

#[async_trait]
impl<F: RootFilesystem + ?Sized + 'static> ExtensionSettingsSource
    for ExtensionSettingsProductService<F>
{
    async fn list(
        &self,
        caller: &ProductSurfaceCaller,
        extension_id: &str,
    ) -> Result<Vec<ExtensionSettingView>, ProductSurfaceError> {
        let (id, declarations) = self.declarations(caller, extension_id).await?;
        if declarations.is_empty() {
            return Ok(Vec::new());
        }
        let stored = self
            .store
            .stored_values(&caller_scope(caller), &id)
            .await
            .map_err(ProductSurfaceError::internal_from)?;
        let effective = effective_setting_values(&declarations, &stored);
        Ok(declarations
            .iter()
            .map(|declaration| {
                let value = effective
                    .get(declaration.id.as_str())
                    .cloned()
                    .unwrap_or_else(|| declaration.default.clone());
                setting_view(declaration, value)
            })
            .collect())
    }

    async fn set(
        &self,
        caller: &ProductSurfaceCaller,
        extension_id: &str,
        setting_id: &str,
        value: serde_json::Value,
    ) -> Result<ExtensionSettingSetOutcome, ProductSurfaceError> {
        let (id, declarations) = self.declarations(caller, extension_id).await?;
        match self
            .store
            .set(&caller_scope(caller), &id, &declarations, setting_id, value)
            .await
        {
            Ok(stored) => {
                let declaration = declarations
                    .iter()
                    .find(|declaration| declaration.id.as_str() == setting_id)
                    .ok_or_else(ProductSurfaceError::not_found)?;
                Ok(ExtensionSettingSetOutcome::Updated {
                    setting: setting_view(declaration, stored),
                })
            }
            Err(ExtensionSettingsError::UnknownSetting { .. }) => {
                Err(ProductSurfaceError::not_found())
            }
            Err(error @ ExtensionSettingsError::InvalidValue { .. }) => {
                Ok(ExtensionSettingSetOutcome::Rejected {
                    reason: error.to_string(),
                })
            }
            Err(error) => Err(ProductSurfaceError::internal_from(error)),
        }
    }
}

fn caller_scope(caller: &ProductSurfaceCaller) -> ResourceScope {
    ResourceScope {
        tenant_id: caller.tenant_id.clone(),
        user_id: caller.user_id.clone(),
        agent_id: caller.agent_id.clone(),
        project_id: caller.project_id.clone(),
        mission_id: None,
        thread_id: None,
        invocation_id: InvocationId::new(),
    }
}

fn setting_view(
    declaration: &ExtensionSettingDecl,
    value: serde_json::Value,
) -> ExtensionSettingView {
    ExtensionSettingView {
        id: declaration.id.as_str().to_string(),
        kind: match declaration.kind {
            ExtensionSettingKind::Switch => ExtensionSettingKindView::Switch,
            ExtensionSettingKind::Choice => ExtensionSettingKindView::Choice,
            ExtensionSettingKind::Text => ExtensionSettingKindView::Text,
            ExtensionSettingKind::Number => ExtensionSettingKindView::Number,
        },
        label: declaration.label.clone(),
        description: declaration.description.clone(),
        section: declaration.section.clone(),
        options: declaration
            .options
            .iter()
            .map(|option| ExtensionSettingOptionView {
                value: option.value.clone(),
                label: option.label.clone(),
                description: option.description.clone(),
            })
            .collect(),
        default: declaration.default.clone(),
        value,
    }
}
