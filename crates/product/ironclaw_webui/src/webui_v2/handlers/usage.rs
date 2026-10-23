//! Usage route: subscription usage windows (CLI logins, coding-plan quotas)
//! for the Usage page. Forwards the `ironclaw_product_contracts::usage`
//! descriptor; the report is machine-level and carries no credentials.

use axum::Json;
use axum::extract::{Extension, State};
use ironclaw_product_contracts::surface::ProductSurfaceCaller;
use ironclaw_product_contracts::usage::USAGE_REPORT_VIEW;
use serde_json::Value;

use super::query_product_view;
use crate::webui_v2::error::WebUiV2HttpError;
use crate::webui_v2::router::WebUiV2State;

/// `GET /api/webchat/v2/usage`
pub async fn usage_report(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let params = serde_json::json!({});
    Ok(Json(
        query_product_view(
            state.services(),
            caller,
            USAGE_REPORT_VIEW.descriptor(),
            params,
            None,
        )
        .await?,
    ))
}
