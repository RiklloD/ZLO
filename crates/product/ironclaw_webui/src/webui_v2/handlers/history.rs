//! History search route: search across the caller's past chats. Forwards
//! the one `ironclaw_product_contracts::history` view; the mode (discover,
//! scroll, read, browse) follows from the query parameters. Results are
//! always the authenticated caller's own threads.

use axum::Json;
use axum::extract::{Extension, Query, State};
use ironclaw_product_contracts::history::{
    HISTORY_QUERY_MAX_CHARS, HISTORY_SEARCH_VIEW, HistorySearchRequest,
};
use ironclaw_product_contracts::surface::{
    ProductSurfaceCaller, ProductSurfaceError, ProductSurfaceValidationCode,
};
use serde_json::Value;

use super::query_product_view;
use crate::webui_v2::error::WebUiV2HttpError;
use crate::webui_v2::router::WebUiV2State;

/// `GET /api/webchat/v2/history/search?query=&thread_id=&around_message_id=&around_sequence=&limit=&window=&from_sequence=`
pub async fn history_search(
    State(state): State<WebUiV2State>,
    Extension(caller): Extension<ProductSurfaceCaller>,
    Query(request): Query<HistorySearchRequest>,
) -> Result<Json<Value>, WebUiV2HttpError> {
    // Bound the original text at the edge, before it reaches the index.
    if request
        .query
        .as_deref()
        .is_some_and(|q| q.chars().count() > HISTORY_QUERY_MAX_CHARS)
    {
        return Err(WebUiV2HttpError::from(ProductSurfaceError::validation(
            "query",
            ProductSurfaceValidationCode::TooLong,
        )));
    }
    let params = serde_json::to_value(request).map_err(ProductSurfaceError::internal_from)?;
    Ok(Json(
        query_product_view(
            state.services(),
            caller,
            HISTORY_SEARCH_VIEW.descriptor(),
            params,
            None,
        )
        .await?,
    ))
}
