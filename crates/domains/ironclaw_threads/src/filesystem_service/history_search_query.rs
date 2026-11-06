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
    ThreadMessageRecord, history_query_terms, term_coverage,
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
        let wanted = clamp_max_threads(request.max_threads);
        let Some(mut pool) = self
            .collect(&request, &terms, HISTORY_SEARCH_CANDIDATES, None)
            .await?
        else {
            // An undeclared index answers nothing yet: not complete.
            return Ok(SearchThreadMessagesResponse {
                threads: Vec::new(),
                index_complete: false,
            });
        };
        let mut groups = pool.group(&terms, &request);
        if pool.saturated && tier_zero_groups(&groups, &request) < wanted {
            if let Some(wide) = self
                .collect(&request, &terms, HISTORY_SEARCH_WIDE_CANDIDATES, None)
                .await?
            {
                pool = wide;
                groups = pool.group(&terms, &request);
            }
            // Better-ranked lower-tier threads can still fill every slot
            // beyond the widest fetch: search the person's own threads one
            // by one (most recent first, bounded) so a match there is found.
            if pool.saturated
                && request.tiering.is_some()
                && request.thread_id.is_none()
                && tier_zero_groups(&groups, &request) < wanted
            {
                self.scan_own_threads(&request, &terms, &mut pool, &groups, wanted)
                    .await?;
                groups = pool.group(&terms, &request);
            }
        }
        Ok(SearchThreadMessagesResponse {
            threads: groups,
            index_complete,
        })
    }

    /// Candidates under the request's prefixes (or one thread's, when
    /// `thread` is given); `None` when the FTS index is not declared yet.
    async fn collect(
        &self,
        request: &SearchThreadMessagesRequest,
        terms: &[String],
        limit: u32,
        thread: Option<&ThreadId>,
    ) -> Result<Option<CandidatePool>, SessionThreadError> {
        let scope = &request.scope;
        let narrowed = thread.or(request.thread_id.as_ref());
        let canonical_prefix = match narrowed {
            Some(id) => messages_root(scope, id)?,
            None => scoped_path(&format!("{}/threads", scope_axes_string(scope)))?,
        };
        let projection_prefix = projections_root(scope, narrowed)?;
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
        let mut pool = CandidatePool {
            saturated: canonical.len() >= limit as usize || external.len() >= limit as usize,
            ..CandidatePool::default()
        };
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
            pool.candidates
                .extend(RankedCandidate::from_record(&record, backend_rank));
        }
        let mut external_threads = Vec::new();
        for (backend_rank, row) in external.into_iter().enumerate() {
            if row.entry.kind.as_ref().map(RecordKind::as_str) != Some(EXTERNAL_PROJECTION_KIND) {
                continue;
            }
            let stored = deserialize::<StoredExternalProjection>(&row.entry.body)?;
            if stored.superseded {
                continue;
            }
            let expected = format!("/search-projections/{}/", stored.thread_id.as_str());
            if !row.path.as_str().contains(&expected) {
                continue;
            }
            if !external_threads.contains(&stored.thread_id) {
                external_threads.push(stored.thread_id.clone());
            }
            pool.candidates.extend(RankedCandidate::from_external(
                stored.thread_id,
                &stored.message,
                backend_rank,
            ));
        }
        let mut seen: Vec<ThreadId> = Vec::new();
        for candidate in &pool.candidates {
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
                    pool.threads.insert(thread_id, record);
                }
                // A CLI-only thread (a fork or delegated child the
                // orchestrator made) has no session record: its projection
                // summary names it instead.
                Err(SessionThreadError::UnknownThread { .. })
                    if external_threads.contains(&thread_id) =>
                {
                    if let Some(summary) = self.projection_summary(scope, &thread_id).await? {
                        pool.threads
                            .insert(thread_id, external_thread_record(scope, &summary));
                    }
                }
                // A message whose thread record is gone is not a result.
                Err(SessionThreadError::UnknownThread { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(Some(pool))
    }

    /// Search tier-0 threads one at a time, newest activity first, adding
    /// their candidates to `pool` until `wanted` tier-0 threads match or
    /// `OWN_SCAN_MAX_THREADS` were searched.
    async fn scan_own_threads(
        &self,
        request: &SearchThreadMessagesRequest,
        terms: &[String],
        pool: &mut CandidatePool,
        groups: &[crate::ThreadSearchResult],
        wanted: usize,
    ) -> Result<(), SessionThreadError> {
        let mut found = tier_zero_groups(groups, request);
        let mut searched = 0usize;
        let mut cursor = None;
        let in_groups: Vec<ThreadId> = groups.iter().map(|g| g.thread.thread_id.clone()).collect();
        loop {
            let page = self
                .list_threads_for_scope(crate::ListThreadsForScopeRequest {
                    scope: request.scope.clone(),
                    limit: Some(OWN_SCAN_PAGE),
                    cursor: cursor.take(),
                })
                .await?;
            for thread in page.threads {
                if found >= wanted || searched >= OWN_SCAN_MAX_THREADS {
                    return Ok(());
                }
                if request.tier_of(&thread) != 0 || in_groups.contains(&thread.thread_id) {
                    continue;
                }
                searched += 1;
                let Some(own) = self
                    .collect(request, terms, OWN_SCAN_HITS, Some(&thread.thread_id))
                    .await?
                else {
                    return Ok(());
                };
                if own
                    .candidates
                    .iter()
                    .any(|c| term_coverage(&c.text, terms) > 0)
                {
                    found += 1;
                }
                pool.absorb(own);
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => return Ok(()),
            }
        }
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

/// Threads listed per page while scanning own threads.
const OWN_SCAN_PAGE: u32 = 100;
/// Own threads searched one by one at most, per search.
const OWN_SCAN_MAX_THREADS: usize = 300;
/// Candidates fetched per own thread.
const OWN_SCAN_HITS: u32 = 20;

/// Candidates and the records of the threads they belong to.
#[derive(Default)]
struct CandidatePool {
    candidates: Vec<RankedCandidate>,
    threads: HashMap<ThreadId, crate::SessionThreadRecord>,
    saturated: bool,
}

impl CandidatePool {
    fn group(
        &self,
        terms: &[String],
        request: &SearchThreadMessagesRequest,
    ) -> Vec<crate::ThreadSearchResult> {
        group_ranked_candidates(self.candidates.clone(), &self.threads, terms, request)
    }

    /// Add another pool's candidates, ranked after this pool's.
    fn absorb(&mut self, other: CandidatePool) {
        let offset = self.candidates.len();
        self.candidates
            .extend(other.candidates.into_iter().map(|mut c| {
                c.backend_rank += offset;
                c
            }));
        self.threads.extend(other.threads);
    }
}
