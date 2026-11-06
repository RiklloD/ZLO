//! History search over the `search_text` FTS projection — read-only.
//!
//! Two ranked full-text queries under the scope's cell: canonical message
//! rows (`<cell>/threads`) and external transcript projections
//! (`<cell>/search-projections`), each narrowed to one thread's directory
//! when the request names a thread, so candidates are selected inside that
//! thread. The backend-independent grouping/snippet pass in
//! `crate::history_search` ranks groups by tier, then relevance. A fetch that
//! saturates while too few tier-0 (the person's own) threads made the answer
//! is retried once wider. The query is index-driven and bounded; it never
//! enumerates the collection and never writes: indexing is
//! `advance_message_search_index`, run off the request path.

use std::collections::HashMap;

use ironclaw_filesystem::{
    FilesystemError, Filter, IndexKind, IndexSpec, Page, RecordKind, RootFilesystem, VersionedEntry,
};
use ironclaw_host_api::{ids::ThreadId, path::ScopedPath};

use crate::external_transcript::StoredExternalProjection;
use crate::history_search::{
    HISTORY_SEARCH_CANDIDATES, HISTORY_SEARCH_WIDE_CANDIDATES, RankedCandidate,
    SEARCH_TEXT_INDEX_KEY, SEARCH_TEXT_INDEX_NAME, clamp_max_threads, external_thread_record,
    group_ranked_candidates, tier_zero_groups,
};
use crate::{
    ExternalThreadSummary, FilesystemSessionThreadService, SearchThreadMessagesRequest,
    SearchThreadMessagesResponse, SessionThreadError, SessionThreadService, ThreadHistoryRequest,
    ThreadMessageRecord, history_query_terms,
};

use super::{
    THREAD_MESSAGE_KIND, deserialize, fs_index_key, fs_index_name, is_not_found, messages_root,
    scope_axes_string, scoped_path,
};

pub(super) const EXTERNAL_PROJECTION_KIND: &str = "thread_search_projection";
pub(super) const EXTERNAL_THREAD_SUMMARY_KIND: &str = "thread_search_projection_summary";

pub(super) fn search_text_index_spec() -> Result<IndexSpec, SessionThreadError> {
    Ok(IndexSpec::new(
        fs_index_name(SEARCH_TEXT_INDEX_NAME)?,
        vec![fs_index_key(SEARCH_TEXT_INDEX_KEY)?],
        IndexKind::Fts,
    ))
}

/// `<cell>/search-projections`, or one thread's directory under it.
pub(super) fn projections_root(
    scope: &crate::ThreadScope,
    thread_id: Option<&ThreadId>,
) -> Result<ScopedPath, SessionThreadError> {
    let base = format!("{}/search-projections", scope_axes_string(scope));
    scoped_path(&match thread_id {
        Some(id) => format!("{base}/{}", id.as_str()),
        None => base,
    })
}

pub(super) fn projection_summary_path(
    scope: &crate::ThreadScope,
    thread_id: &ThreadId,
) -> Result<ScopedPath, SessionThreadError> {
    scoped_path(&format!(
        "{}/summary.json",
        projections_root(scope, Some(thread_id))?.as_str()
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
        let index_complete = self.search_index_complete(&request.scope).await?;
        let mut groups = self
            .search_with_candidates(&request, &terms, HISTORY_SEARCH_CANDIDATES)
            .await?;
        if let Some(groups_first) = groups.as_ref()
            && groups_first.1
            && tier_zero_groups(&groups_first.0, &request) < clamp_max_threads(request.max_threads)
        {
            groups = self
                .search_with_candidates(&request, &terms, HISTORY_SEARCH_WIDE_CANDIDATES)
                .await?;
        }
        // An undeclared index answers nothing yet: not complete.
        let declared = groups.is_some();
        Ok(SearchThreadMessagesResponse {
            threads: groups.map(|(g, _)| g).unwrap_or_default(),
            index_complete: index_complete && declared,
        })
    }

    /// Grouped results and whether either fetch was saturated; `None` when
    /// the FTS index is not declared yet.
    async fn search_with_candidates(
        &self,
        request: &SearchThreadMessagesRequest,
        terms: &[String],
        limit: u32,
    ) -> Result<Option<(Vec<crate::ThreadSearchResult>, bool)>, SessionThreadError> {
        let scope = &request.scope;
        let canonical_prefix = match &request.thread_id {
            Some(id) => messages_root(scope, id)?,
            None => scoped_path(&format!("{}/threads", scope_axes_string(scope)))?,
        };
        let projection_prefix = projections_root(scope, request.thread_id.as_ref())?;
        let query = terms.join(" ");
        let Some(canonical) = self
            .fts_rows(scope, &canonical_prefix, &query, limit)
            .await?
        else {
            return Ok(None);
        };
        let external = self
            .fts_rows(scope, &projection_prefix, &query, limit)
            .await?
            .unwrap_or_default();
        let saturated = canonical.len() >= limit as usize || external.len() >= limit as usize;

        let mut candidates = Vec::with_capacity(canonical.len() + external.len());
        for (backend_rank, row) in canonical.into_iter().enumerate() {
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
            candidates.extend(RankedCandidate::from_record(&record, backend_rank));
        }
        let mut external_threads = Vec::new();
        for (backend_rank, row) in external.into_iter().enumerate() {
            if row.entry.kind.as_ref().map(RecordKind::as_str) != Some(EXTERNAL_PROJECTION_KIND) {
                continue;
            }
            let stored = deserialize::<StoredExternalProjection>(&row.entry.body)?;
            let expected = format!("/search-projections/{}/", stored.thread_id.as_str());
            if !row.path.as_str().contains(&expected) {
                continue;
            }
            if !external_threads.contains(&stored.thread_id) {
                external_threads.push(stored.thread_id.clone());
            }
            candidates.extend(RankedCandidate::from_external(
                stored.thread_id,
                &stored.message,
                backend_rank,
            ));
        }

        let mut threads = HashMap::new();
        let mut seen: Vec<ThreadId> = Vec::new();
        for candidate in &candidates {
            if !seen.contains(&candidate.thread_id) {
                seen.push(candidate.thread_id.clone());
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
                // A CLI-only thread (a fork or delegated child the
                // orchestrator made) has no session record: its projection
                // summary names it instead.
                Err(SessionThreadError::UnknownThread { .. })
                    if external_threads.contains(&thread_id) =>
                {
                    if let Some(summary) = self.projection_summary(scope, &thread_id).await? {
                        threads.insert(thread_id, external_thread_record(scope, &summary));
                    }
                }
                // A message whose thread record is gone is not a result.
                Err(SessionThreadError::UnknownThread { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(Some((
            group_ranked_candidates(candidates, &threads, terms, request),
            saturated,
        )))
    }

    /// Ranked rows under `prefix`; `None` when the index is not declared.
    async fn fts_rows(
        &self,
        scope: &crate::ThreadScope,
        prefix: &ScopedPath,
        query: &str,
        limit: u32,
    ) -> Result<Option<Vec<VersionedEntry>>, SessionThreadError> {
        let filter = Filter::FtsRanked {
            key: fs_index_key(SEARCH_TEXT_INDEX_KEY)?,
            query: query.to_string(),
            limit,
        };
        match self
            .filesystem
            .query(
                &scope.to_resource_scope(),
                prefix,
                &filter,
                Page::new(0, limit),
            )
            .await
        {
            Ok(rows) => Ok(Some(rows)),
            Err(error) if is_not_found(&error) => Ok(Some(Vec::new())),
            Err(FilesystemError::Unsupported { .. }) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(super) async fn projection_summary(
        &self,
        scope: &crate::ThreadScope,
        thread_id: &ThreadId,
    ) -> Result<Option<ExternalThreadSummary>, SessionThreadError> {
        let path = projection_summary_path(scope, thread_id)?;
        match self
            .filesystem
            .get(&scope.to_resource_scope(), &path)
            .await?
        {
            Some(row) => Ok(Some(deserialize::<ExternalThreadSummary>(&row.entry.body)?)),
            None => Ok(None),
        }
    }
}
