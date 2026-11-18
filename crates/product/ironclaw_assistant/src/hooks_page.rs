//! `ProductSurface` decorator that answers the read-only hooks view
//! (`ironclaw_product_contracts::hooks_catalog`) from a [`HookCatalogSource`];
//! every other request passes through untouched.
//!
//! The catalog carries no hook code, predicates, or secrets. Builtin hooks
//! are deployment-level configuration any authenticated caller may read; an
//! extension's hooks are listed only to users its installation is visible to
//! (the source narrows by the caller), so the page never reveals another
//! user's private plugin.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_product_contracts::hooks_catalog::{HOOK_CATALOG_VIEW, HookCatalogSource};
use ironclaw_product_contracts::surface::{
    ProductSurface, ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceInvokeRequest,
    ProductSurfaceInvokeResponse, ProductSurfaceQueryPage, ProductSurfaceQueryRequest,
    ProductSurfaceStreamRequest, ProductSurfaceStreamResponse,
};

pub struct HooksPageSurface {
    inner: Arc<dyn ProductSurface>,
    source: Arc<dyn HookCatalogSource>,
}

impl HooksPageSurface {
    pub fn new(inner: Arc<dyn ProductSurface>, source: Arc<dyn HookCatalogSource>) -> Self {
        Self { inner, source }
    }
}

#[async_trait]
impl ProductSurface for HooksPageSurface {
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
        if request.view_id != HOOK_CATALOG_VIEW.id {
            return self.inner.query(caller, request).await;
        }
        let catalog = serde_json::to_value(self.source.catalog(&caller.user_id).await)
            .map_err(ProductSurfaceError::internal_from)?;
        Ok(ProductSurfaceQueryPage {
            items: vec![catalog],
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
