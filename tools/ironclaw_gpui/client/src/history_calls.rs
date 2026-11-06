//! History search across past chats (`GET /history/search`): discover
//! (a query), scroll (a thread and an anchor message), read (a thread) and
//! browse (nothing). Results are the signed-in person's own threads.

use std::future::Future;

use ironclaw_product_contracts::history::{HistorySearchRequest, HistorySearchResponse};

use crate::wire::TimelinePage;

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

/// Rows asked for per timeline page while looking for a message.
const REVEAL_PAGE: u32 = 500;
/// Pages read before giving up on finding the message.
const REVEAL_MAX_PAGES: usize = 40;

/// Read timeline pages, newest first, until one holds `message_id`, there
/// are no older pages, or `max_pages` were read; the pages merge into one
/// (the oldest page's cursor kept).
pub(crate) async fn pages_until<F, Fut>(
    message_id: &str,
    max_pages: usize,
    mut fetch: F,
) -> Result<TimelinePage, ClientError>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: Future<Output = Result<TimelinePage, ClientError>>,
{
    let mut merged = fetch(None).await?;
    for _ in 1..max_pages {
        if merged.messages.iter().any(|m| m.message_id == message_id) {
            break;
        }
        let Some(cursor) = merged.next_cursor.clone() else {
            break;
        };
        let older = fetch(Some(cursor)).await?;
        merged.next_cursor = older.next_cursor;
        merged.messages.extend(older.messages);
    }
    Ok(merged)
}

impl Client {
    /// One timeline page, older than `cursor` when given.
    pub async fn timeline_page(
        &self,
        thread_id: &str,
        limit: u32,
        cursor: Option<&str>,
    ) -> Result<TimelinePage, ClientError> {
        let mut path = format!("{V2}/threads/{}/timeline?limit={limit}", seg(thread_id));
        if let Some(cursor) = cursor {
            path.push_str(&format!("&cursor={}", seg(cursor)));
        }
        self.get(&path).await
    }

    /// The thread's timeline back to (at least) `message_id`, so a search
    /// hit older than the latest page can be shown.
    pub async fn timeline_through(
        &self,
        thread_id: &str,
        message_id: &str,
    ) -> Result<TimelinePage, ClientError> {
        pages_until(message_id, REVEAL_MAX_PAGES, |cursor| async move {
            self.timeline_page(thread_id, REVEAL_PAGE, cursor.as_deref())
                .await
        })
        .await
    }

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

    fn page(ids: &[&str], next: Option<&str>) -> TimelinePage {
        serde_json::from_value(serde_json::json!({
            "thread": {"thread_id": "t", "scope": {"tenant_id": "x", "agent_id": "a"},
                       "created_by_actor_id": "u"},
            "messages": ids.iter().enumerate().map(|(n, id)| serde_json::json!({
                "message_id": id, "thread_id": "t", "sequence": n as u64 + 1,
                "kind": "user", "status": "finalized", "content": "x"
            })).collect::<Vec<_>>(),
            "next_cursor": next,
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn older_pages_are_read_until_the_message_is_in() {
        let pages = std::collections::HashMap::from([
            (None, page(&["m5", "m6"], Some("c1"))),
            (Some("c1".to_string()), page(&["m3", "m4"], Some("c2"))),
            (Some("c2".to_string()), page(&["m1", "m2"], None)),
        ]);
        let asked = std::cell::RefCell::new(Vec::new());
        let fetch = |cursor: Option<String>| {
            asked.borrow_mut().push(cursor.clone());
            let page = pages.get(&cursor).cloned();
            async move { page.ok_or(ClientError::Config("no page".into())) }
        };
        let merged = pages_until("m3", 10, fetch).await.unwrap();
        let ids: Vec<&str> = merged
            .messages
            .iter()
            .map(|m| m.message_id.as_str())
            .collect();
        assert_eq!(ids, ["m5", "m6", "m3", "m4"]);
        assert_eq!(asked.borrow().len(), 2, "stops once the message is loaded");
        // Missing everywhere: reads to the oldest page and stops.
        asked.borrow_mut().clear();
        let all = pages_until("gone", 10, fetch).await.unwrap();
        assert_eq!(all.messages.len(), 6);
        assert_eq!(asked.borrow().len(), 3);
        // The page budget bounds the reads.
        asked.borrow_mut().clear();
        pages_until("gone", 2, fetch).await.unwrap();
        assert_eq!(asked.borrow().len(), 2);
    }

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
