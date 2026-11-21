//! `ProductSurface` decorator that answers the package-settings view and
//! command (`ironclaw_product_contracts::extension_settings`) from an
//! [`ExtensionSettingsSource`]; every other request passes through untouched.
//!
//! The scope is always the authenticated caller's own: settings are per-user
//! preferences, and the source reads and writes them in the caller's scope.
//! Input is bounded here before it reaches the source.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_product_contracts::extension_settings::{
    EXTENSION_SETTING_SET_COMMAND_ID, EXTENSION_SETTINGS_VIEW, ExtensionSettingSetRequest,
    ExtensionSettingsRequest, ExtensionSettingsResponse, ExtensionSettingsSource,
    MAX_EXTENSION_SETTINGS_ID_BYTES,
};
use ironclaw_product_contracts::surface::{
    ProductSurface, ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceInvokeRequest,
    ProductSurfaceInvokeResponse, ProductSurfaceQueryPage, ProductSurfaceQueryRequest,
    ProductSurfaceStreamRequest, ProductSurfaceStreamResponse, ProductSurfaceValidationCode,
};

/// Largest JSON value a client may submit for one setting, in bytes.
const MAX_SETTING_VALUE_BYTES: usize = 4 * 1024;

pub struct ExtensionSettingsSurface {
    inner: Arc<dyn ProductSurface>,
    source: Arc<dyn ExtensionSettingsSource>,
}

impl ExtensionSettingsSurface {
    pub fn new(inner: Arc<dyn ProductSurface>, source: Arc<dyn ExtensionSettingsSource>) -> Self {
        Self { inner, source }
    }
}

#[async_trait]
impl ProductSurface for ExtensionSettingsSurface {
    async fn invoke(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSurfaceInvokeRequest,
    ) -> Result<ProductSurfaceInvokeResponse, ProductSurfaceError> {
        if request.operation_id.as_str() != EXTENSION_SETTING_SET_COMMAND_ID {
            return self.inner.invoke(caller, request).await;
        }
        let body: ExtensionSettingSetRequest = parse(request.input)?;
        let extension_id = bounded_id("extension_id", &body.extension_id)?;
        let setting_id = bounded_id("setting_id", &body.setting_id)?;
        let encoded =
            serde_json::to_vec(&body.value).map_err(ProductSurfaceError::internal_from)?;
        if encoded.len() > MAX_SETTING_VALUE_BYTES {
            return Err(ProductSurfaceError::validation(
                "value",
                ProductSurfaceValidationCode::TooLong,
            ));
        }
        let outcome = self
            .source
            .set(&caller, extension_id, setting_id, body.value)
            .await?;
        Ok(ProductSurfaceInvokeResponse {
            output: serde_json::to_value(outcome).map_err(ProductSurfaceError::internal_from)?,
        })
    }

    async fn query(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSurfaceQueryRequest,
    ) -> Result<ProductSurfaceQueryPage, ProductSurfaceError> {
        if request.view_id != EXTENSION_SETTINGS_VIEW.id {
            return self.inner.query(caller, request).await;
        }
        let params: ExtensionSettingsRequest = parse(request.input)?;
        let extension_id = bounded_id("extension_id", &params.extension_id)?;
        let settings = self.source.list(&caller, extension_id).await?;
        let page = serde_json::to_value(ExtensionSettingsResponse { settings })
            .map_err(ProductSurfaceError::internal_from)?;
        Ok(ProductSurfaceQueryPage {
            items: vec![page],
            next_cursor: None,
        })
    }

    async fn stream_events(
        &self,
        caller: ProductSurfaceCaller,
        request: ProductSurfaceStreamRequest,
    ) -> Result<ProductSurfaceStreamResponse, ProductSurfaceError> {
        self.inner.stream_events(caller, request).await
    }
}

fn parse<T: serde::de::DeserializeOwned>(
    input: serde_json::Value,
) -> Result<T, ProductSurfaceError> {
    serde_json::from_value(input).map_err(|error| {
        tracing::debug!(%error, "extension settings: invalid input");
        ProductSurfaceError::validation("input", ProductSurfaceValidationCode::InvalidValue)
    })
}

fn bounded_id<'a>(field: &'static str, raw: &'a str) -> Result<&'a str, ProductSurfaceError> {
    if raw.is_empty()
        || raw.len() > MAX_EXTENSION_SETTINGS_ID_BYTES
        || raw
            .chars()
            .any(|character| character.is_control() || character == '/')
    {
        return Err(ProductSurfaceError::validation(
            field,
            ProductSurfaceValidationCode::InvalidId,
        ));
    }
    Ok(raw)
}
