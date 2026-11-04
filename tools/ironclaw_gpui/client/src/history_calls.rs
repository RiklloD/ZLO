//! History search across past chats (`GET /history/search`): discover
//! (a query), scroll (a thread and an anchor message), read (a thread) and
//! browse (nothing). Results are the signed-in person's own threads.

use ironclaw_product_contracts::history::{HistorySearchRequest, HistorySearchResponse};

use crate::api::{Client, V2, seg};
use crate::error::ClientError;

/// The query string for `request`: every set field, percent-encoded.
pub(crate) fn history_query(request: &HistorySearchRequest) -> String {
    let numbers = [
        ("around_sequence", request.around_sequence),
        ("limit", request.limit.map(u64::from)),
        ("window", request.window.map(u64::from)),
        ("from_sequence", request.from_sequence),
    ];
    [
        ("query", request.query.clone()),
        ("thread_id", request.thread_id.clone()),
        ("around_message_id", request.around_message_id.clone()),
    ]
    .into_iter()
    .chain(numbers.map(|(key, value)| (key, value.map(|v| v.to_string()))))
    .filter_map(|(key, value)| Some(format!("{key}={}", seg(&value?))))
    .collect::<Vec<_>>()
    .join("&")
}

impl Client {
    pub async fn history_search(
        &self,
        request: &HistorySearchRequest,
    ) -> Result<HistorySearchResponse, ClientError> {
        let query = history_query(request);
        let path = if query.is_empty() {
            format!("{V2}/history/search")
        } else {
            format!("{V2}/history/search?{query}")
        };
        self.get(&path).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_encodes_only_the_set_fields() {
        let request = HistorySearchRequest {
            query: Some("deploy & ship".into()),
            thread_id: Some("t 1".into()),
            limit: Some(5),
            ..Default::default()
        };
        assert_eq!(
            history_query(&request),
            "query=deploy%20%26%20ship&thread_id=t%201&limit=5"
        );
        assert_eq!(history_query(&HistorySearchRequest::default()), "");
    }
}
