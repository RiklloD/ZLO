//! `ProductSurface` decorator that answers the `usage.report` and
//! `usage.history` views from a [`UsageReportSource`]; every other request
//! passes through untouched.
//!
//! The report is machine-level (the CLI logins and coding-plan key of the
//! host), not per-thread, so any authenticated caller of this surface may
//! read it. It carries percentages and reset times only, never credentials.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_product_contracts::surface::{
    ProductSurface, ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceInvokeRequest,
    ProductSurfaceInvokeResponse, ProductSurfaceQueryPage, ProductSurfaceQueryRequest,
    ProductSurfaceStreamRequest, ProductSurfaceStreamResponse,
};
use ironclaw_product_contracts::usage::{
    USAGE_HISTORY_VIEW, USAGE_REPORT_VIEW, UsageHistoryRequest, UsageReportSource,
};

pub struct UsageProductSurface {
    inner: Arc<dyn ProductSurface>,
    source: Arc<dyn UsageReportSource>,
}

impl UsageProductSurface {
    pub fn new(inner: Arc<dyn ProductSurface>, source: Arc<dyn UsageReportSource>) -> Self {
        Self { inner, source }
    }
}

#[async_trait]
impl ProductSurface for UsageProductSurface {
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
        let value = if request.view_id == USAGE_REPORT_VIEW.id {
            serde_json::to_value(self.source.report().await)
        } else if request.view_id == USAGE_HISTORY_VIEW.id {
            let input: UsageHistoryRequest =
                serde_json::from_value(request.input).map_err(|error| {
                    tracing::debug!(%error, "usage history: invalid input");
                    ProductSurfaceError::validation(
                        "days",
                        ironclaw_product_contracts::surface::ProductSurfaceValidationCode::InvalidValue,
                    )
                })?;
            serde_json::to_value(self.source.history(input).await)
        } else {
            return self.inner.query(caller, request).await;
        }
        .map_err(ProductSurfaceError::internal_from)?;
        Ok(ProductSurfaceQueryPage {
            items: vec![value],
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
