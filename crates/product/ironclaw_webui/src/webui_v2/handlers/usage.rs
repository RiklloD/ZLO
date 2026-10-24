//! Usage routes: subscription usage windows (CLI logins, coding-plan quotas)
//! and the coding CLIs' token history for the Usage page. Forward the
//! `ironclaw_product_contracts::usage` descriptors; both are machine-level
//! and carry no credentials.

use axum::Json;
use axum::extract::{Extension, Query, State};
use ironclaw_product_contracts::surface::ProductSurfaceCaller;
use ironclaw_product_contracts::usage::{USAGE_HISTORY_VIEW, USAGE_REPORT_VIEW};
use serde::Deserialize;
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageHistoryQuery {
    pub days: Option<u32>,
}

/// `GET /api/webchat/v2/usage/history?days=30`
pub async fn usage_history(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Query(query): Query<UsageHistoryQuery>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    let params = serde_json::json!({ "days": query.days });
    Ok(Json(
        query_product_view(
            state.services(),
            caller,
            USAGE_HISTORY_VIEW.descriptor(),
            params,
            None,
        )
        .await?,
    ))
}
