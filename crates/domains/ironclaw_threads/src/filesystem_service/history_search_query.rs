//! History search over the `search_text` FTS projection.
//!
//! One ranked full-text query under the scope's thread cell, then the
//! backend-independent grouping/snippet pass in `crate::history_search`.
//! The query is index-driven and bounded (`HISTORY_SEARCH_CANDIDATES` rows);
//! it never enumerates the collection. Older transcripts are brought into the
//! index by a bounded backfill step per search, so the first searches after
//! an upgrade answer `index_complete: false` until it finishes.

use std::collections::HashMap;

use ironclaw_filesystem::{
    FilesystemError, Filter, IndexKind, IndexSpec, Page, RecordKind, RootFilesystem,
};
use ironclaw_host_api::ids::ThreadId;

use crate::history_search::{
    HISTORY_SEARCH_CANDIDATES, RankedCandidate, SEARCH_TEXT_INDEX_KEY, SEARCH_TEXT_INDEX_NAME,
    clamp_max_hits, clamp_max_threads, group_ranked_candidates,
};
use crate::{
    FilesystemSessionThreadService, SearchThreadMessagesRequest, SearchThreadMessagesResponse,
    SessionThreadError, SessionThreadService, ThreadHistoryRequest, ThreadMessageRecord,
    history_query_terms,
};

use super::{
    THREAD_MESSAGE_KIND, THREADS_PREFIX, deserialize, fs_index_key, fs_index_name, is_not_found,
    scope_axes_string, scoped_path,
};

/// Threads re-projected per search while the backfill is incomplete. Keeps
/// the first searches after an upgrade responsive on large histories.
const BACKFILL_THREADS_PER_SEARCH: usize = 40;

pub(super) fn search_text_index_spec() -> Result<IndexSpec, SessionThreadError> {
    Ok(IndexSpec::new(
        fs_index_name(SEARCH_TEXT_INDEX_NAME)?,
        vec![fs_index_key(SEARCH_TEXT_INDEX_KEY)?],
        IndexKind::Fts,
    ))
}

impl<F> FilesystemSessionThreadService<F>
where
    F: RootFilesystem + 'static,
{
    pub(super) async fn search_history(
        &self,
        request: SearchThreadMessagesRequest,
    ) -> Result<SearchThreadMessagesResponse, SessionThreadError> {
        let terms = history_query_terms(&request.query);
        if terms.is_empty() {
            return Ok(SearchThreadMessagesResponse {
                threads: Vec::new(),
                index_complete: true,
            });
        }
        let scope = &request.scope;
        let resource_scope = scope.to_resource_scope();
        // Declared at the alias root like every other transcript index, so a
        // single declaration serves every thread cell on the mount. The
        // declaration is idempotent; backends cache it per resolved path.
        match self
            .filesystem
            .ensure_index(
                &resource_scope,
                &scoped_path(THREADS_PREFIX)?,
                &search_text_index_spec()?,
            )
            .await
        {
            Ok(()) => {}
            Err(FilesystemError::Unsupported { .. }) => {
                return Err(SessionThreadError::Backend(
                    "history search needs a storage backend with full-text indexing".to_string(),
                ));
            }
            Err(error) => return Err(error.into()),
        }
        self.ensure_transcript_indexes_migrated(scope).await?;
        let index_complete = self
            .advance_history_search_backfill(scope, BACKFILL_THREADS_PER_SEARCH)
            .await?;

        let prefix = scoped_path(&format!("{}/threads", scope_axes_string(scope)))?;
        let filter = Filter::FtsRanked {
            key: fs_index_key(SEARCH_TEXT_INDEX_KEY)?,
            query: terms.join(" "),
            limit: HISTORY_SEARCH_CANDIDATES,
        };
        let rows = match self
            .filesystem
            .query(
                &resource_scope,
                &prefix,
                &filter,
                Page::new(0, HISTORY_SEARCH_CANDIDATES),
            )
            .await
        {
            Ok(rows) => rows,
            Err(error) if is_not_found(&error) => Vec::new(),
            Err(error) => return Err(error.into()),
        };

        let mut candidates = Vec::with_capacity(rows.len());
        for (backend_rank, row) in rows.into_iter().enumerate() {
            if row.entry.kind.as_ref().map(RecordKind::as_str) != Some(THREAD_MESSAGE_KIND) {
                continue;
            }
            let record = deserialize::<ThreadMessageRecord>(&row.entry.body)?;
            // Belt and braces over the prefix: the row must sit in its own
            // thread's message directory.
            let expected = format!("/threads/{}/messages/", record.thread_id.as_str());
            if !row.path.as_str().contains(&expected) {
                continue;
            }
            candidates.push(RankedCandidate {
                record,
                backend_rank,
            });
        }

        let mut threads = HashMap::new();
        let mut seen: Vec<ThreadId> = Vec::new();
        for candidate in &candidates {
            if !seen.contains(&candidate.record.thread_id) {
                seen.push(candidate.record.thread_id.clone());
            }
        }
        for thread_id in seen {
            match self
                .read_thread(ThreadHistoryRequest {
                    scope: scope.clone(),
                    thread_id: thread_id.clone(),
                })
                .await
            {
                Ok(record) => {
                    threads.insert(thread_id, record);
                }
                // A message whose thread record is gone is not a result.
                Err(SessionThreadError::UnknownThread { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        let groups = group_ranked_candidates(
            candidates,
            &threads,
            &terms,
            clamp_max_threads(request.max_threads),
            clamp_max_hits(request.max_hits_per_thread),
        );
        Ok(SearchThreadMessagesResponse {
            threads: groups,
            index_complete,
        })
    }
}
