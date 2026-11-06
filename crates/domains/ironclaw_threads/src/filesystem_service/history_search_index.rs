//! Writes behind history search, all off the request path.
//!
//! * `advance_search_index` — one bounded indexing step: declares the FTS
//!   index, then walks the scope's messages in `(thread_id, sequence)` order
//!   through the declared ordered index (a keyset page — no directory listing,
//!   and a huge thread is just more pages), re-projecting at most `budget`
//!   rows whose `search_text` is stale (written before the projection, or by
//!   an older visibility rule). Bodies are never rewritten. The resume cursor
//!   advances only by compare-and-swap, so an overlapping step that loses the
//!   race leaves the newer cursor alone; it also only ever moves forward.
//! * `upsert_projections` / `projection_cursor` — idempotent search
//!   projections of external (CLI runtime) transcripts with a monotonic
//!   per-source cursor (see `crate::external_transcript`).

use ironclaw_filesystem::{
    CasExpectation, ContentType, Entry, FilesystemError, Filter, IndexValue, OrderedPage,
    OrderedQueryCursor, RecordKind, RootFilesystem, SortDirection, VersionedEntry,
};
use ironclaw_host_api::path::ScopedPath;
use serde::{Deserialize, Serialize};

use crate::external_transcript::StoredExternalProjection;
use crate::history_search::{SEARCH_TEXT_INDEX_KEY, search_projection, search_projection_text};
use crate::{
    ExternalTranscriptCursor, ExternalTranscriptCursorRequest, ExternalTranscriptUpsert,
    FilesystemSessionThreadService, MessageSearchIndexProgress, SessionThreadError,
    ThreadMessageRecord, ThreadScope, UpsertExternalTranscriptRequest,
};

use super::history_search_query::{
    EXTERNAL_PROJECTION_KIND, EXTERNAL_THREAD_SUMMARY_KIND, projection_summary_path,
    projections_root, search_text_index_spec,
};
use super::transcript_migration::TranscriptPageOutcome;
use super::{
    THREAD_MESSAGE_KIND, THREADS_PREFIX, deserialize, fs_index_key, fs_index_name, is_not_found,
    scope_axes_string, scoped_path,
};

/// Bump to re-project every message when the visibility rule changes.
const SEARCH_INDEX_GENERATION: &str = "history-search-v2";
/// Compare-and-swap retries for one projection or cursor write.
const CAS_RETRIES: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SearchIndexCursor {
    thread_id: String,
    sequence: i64,
}

fn marker_path(scope: &ThreadScope) -> Result<ScopedPath, SessionThreadError> {
    scoped_path(&format!(
        "{}/index-migrations/{SEARCH_INDEX_GENERATION}.complete",
        scope_axes_string(scope)
    ))
}

fn cursor_path(scope: &ThreadScope) -> Result<ScopedPath, SessionThreadError> {
    scoped_path(&format!(
        "{}/index-migrations/{SEARCH_INDEX_GENERATION}.cursor",
        scope_axes_string(scope)
    ))
}

fn json_entry(kind: &str, body: Vec<u8>) -> Result<Entry, SessionThreadError> {
    let kind = RecordKind::new(kind).map_err(|error| {
        SessionThreadError::Backend(format!("invalid search projection record kind: {error}"))
    })?;
    let mut entry = Entry::bytes(body).with_content_type(ContentType::json());
    entry.kind = Some(kind);
    Ok(entry)
}

fn serialize<T: Serialize>(value: &T) -> Result<Vec<u8>, SessionThreadError> {
    serde_json::to_vec(value).map_err(|error| SessionThreadError::Serialization(error.to_string()))
}

fn is_cas_conflict(error: &FilesystemError) -> bool {
    matches!(
        error,
        FilesystemError::VersionMismatch { .. } | FilesystemError::BackendBusy { .. }
    )
}

impl<F> FilesystemSessionThreadService<F>
where
    F: RootFilesystem + 'static,
{
    pub(super) async fn search_index_complete(
        &self,
        scope: &ThreadScope,
    ) -> Result<bool, SessionThreadError> {
        Ok(self
            .filesystem
            .get(&scope.to_resource_scope(), &marker_path(scope)?)
            .await?
            .is_some())
    }

    pub(super) async fn advance_search_index(
        &self,
        scope: &ThreadScope,
        budget: usize,
    ) -> Result<MessageSearchIndexProgress, SessionThreadError> {
        let resource_scope = scope.to_resource_scope();
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
        if self.search_index_complete(scope).await? {
            return Ok(MessageSearchIndexProgress {
                complete: true,
                ..Default::default()
            });
        }
        // Every one-time migration a read path would otherwise run on first
        // use happens here, off the request path, so that once the scope is
        // ready (`message_search_ready`) search and history reads need no
        // write: the transcript projection rebuild, the prepared-context
        // markers, the root index declarations and the thread-listing
        // projection. Each is marker-gated and a no-op after its first run.
        self.ensure_transcript_indexes_migrated(scope).await?;
        self.ensure_prepared_markers_migrated(scope).await?;
        self.declare_root_indexes(scope, super::IndexDeclarationPolicy::Required)
            .await?;
        self.ensure_thread_index_query(scope, true).await?;

        let path = cursor_path(scope)?;
        let current = self.filesystem.get(&resource_scope, &path).await?;
        let (after, expectation) = match &current {
            Some(row) => (
                Some(deserialize::<SearchIndexCursor>(&row.entry.body)?),
                CasExpectation::Version(row.version),
            ),
            None => (None, CasExpectation::Absent),
        };
        let limit = u32::try_from(budget.max(1))
            .unwrap_or(u32::MAX)
            .min(ironclaw_filesystem::Page::MAX_LIMIT);
        let mut page = OrderedPage::new(
            fs_index_name("thread_message_sequence_v3")?,
            fs_index_key("thread_id")?,
            fs_index_key("sequence")?,
            SortDirection::Ascending,
            limit,
        );
        page.after = after.as_ref().map(|c| OrderedQueryCursor {
            value: IndexValue::Text(c.thread_id.clone()),
            tie_breaker: IndexValue::I64(c.sequence),
        });
        let root = scoped_path(&format!("{}/threads", scope_axes_string(scope)))?;
        let rows = match self
            .filesystem
            .query_ordered(&resource_scope, &root, &Filter::All, &page)
            .await
        {
            Ok(rows) => rows,
            Err(error) if is_not_found(&error) => Vec::new(),
            Err(error) => return Err(error.into()),
        };
        let scanned = rows.len();
        let next = rows.last().map(row_cursor).transpose()?.flatten();
        let stale: Vec<VersionedEntry> = rows
            .into_iter()
            .filter_map(|row| match projection_is_stale(&row) {
                Ok(true) => Some(Ok(row)),
                Ok(false) => None,
                Err(error) => Some(Err(error)),
            })
            .collect::<Result<_, _>>()?;
        let reprojected = stale.len();
        if !stale.is_empty() {
            match self.migrate_transcript_page(scope, true, stale).await? {
                TranscriptPageOutcome::Committed => {}
                // A live writer won the page; the next step re-reads it.
                TranscriptPageOutcome::Conflict(error) => {
                    tracing::debug!(%error, "history search index step lost a write race");
                    return Ok(MessageSearchIndexProgress {
                        complete: false,
                        scanned,
                        reprojected: 0,
                    });
                }
            }
        }
        if scanned < limit as usize {
            self.filesystem
                .put(
                    &resource_scope,
                    &marker_path(scope)?,
                    Entry::bytes(SEARCH_INDEX_GENERATION.as_bytes().to_vec()),
                    CasExpectation::Any,
                )
                .await?;
            return Ok(MessageSearchIndexProgress {
                complete: true,
                scanned,
                reprojected,
            });
        }
        if let Some(next) = next {
            // Forward only, and only from the cursor this step started at:
            // a concurrent step that already moved it wins.
            match self
                .filesystem
                .put(
                    &resource_scope,
                    &path,
                    Entry::bytes(serialize(&next)?),
                    expectation,
                )
                .await
            {
                Ok(_) => {}
                Err(error) if is_cas_conflict(&error) => {
                    tracing::debug!("history search index cursor moved concurrently");
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(MessageSearchIndexProgress {
            complete: false,
            scanned,
            reprojected,
        })
    }

    pub(super) async fn upsert_projections(
        &self,
        request: UpsertExternalTranscriptRequest,
    ) -> Result<ExternalTranscriptUpsert, SessionThreadError> {
        let scope = &request.scope;
        let resource_scope = scope.to_resource_scope();
        let thread_id = &request.thread.thread_id;
        let source_key = request.source.key();
        let dir = format!(
            "{}/{source_key}",
            projections_root(scope, Some(thread_id))?.as_str()
        );
        let cursor_path = scoped_path(&format!("{dir}/cursor.json"))?;
        let before = self
            .read_projection_cursor(&resource_scope, &cursor_path)
            .await?;
        let mut outcome = ExternalTranscriptUpsert::default();
        let mut highest = 0u64;
        for message in request.messages {
            highest = highest.max(message.ordinal.saturating_add(1));
            let path = projection_path(&dir, message.ordinal)?;
            let stored = StoredExternalProjection {
                thread_id: thread_id.clone(),
                source_key: source_key.clone(),
                segment: request.source.segment,
                message,
                superseded: false,
            };
            if self.put_projection(scope, &path, &stored).await? {
                outcome.written += 1;
            } else {
                outcome.unchanged += 1;
            }
        }
        if request.replace {
            // Rows the rewritten source no longer has: kept, unsearchable.
            for ordinal in highest..before.ingested_through {
                let path = projection_path(&dir, ordinal)?;
                let Some(row) = self.filesystem.get(&resource_scope, &path).await? else {
                    continue;
                };
                let mut stored = deserialize::<StoredExternalProjection>(&row.entry.body)?;
                if stored.superseded {
                    continue;
                }
                stored.superseded = true;
                if self.put_projection(scope, &path, &stored).await? {
                    outcome.superseded += 1;
                }
            }
        }
        let summary_body = serialize(&request.thread)?;
        let summary = json_entry(EXTERNAL_THREAD_SUMMARY_KIND, summary_body.clone())?;
        self.put_if_changed(
            scope,
            &projection_summary_path(scope, thread_id)?,
            summary,
            &summary_body,
        )
        .await?;
        let after = self
            .move_projection_cursor(
                &resource_scope,
                &cursor_path,
                highest,
                request.replace,
                request.anchor,
                request.source_complete,
            )
            .await?;
        outcome.ingested_through = after.ingested_through;
        outcome.generation = after.generation;
        Ok(outcome)
    }

    /// Store one projection: searchable text indexed unless superseded.
    async fn put_projection(
        &self,
        scope: &ThreadScope,
        path: &ScopedPath,
        stored: &StoredExternalProjection,
    ) -> Result<bool, SessionThreadError> {
        let body = serialize(stored)?;
        let mut entry = json_entry(EXTERNAL_PROJECTION_KIND, body.clone())?.with_indexed(
            fs_index_key("thread_id")?,
            IndexValue::Text(stored.thread_id.as_str().to_string()),
        );
        if !stored.superseded
            && let Some(text) =
                crate::history_search::visible_text(stored.message.role, &stored.message.text)
        {
            entry = entry.with_indexed(
                fs_index_key(SEARCH_TEXT_INDEX_KEY)?,
                IndexValue::Text(search_projection_text(text)),
            );
        }
        self.put_if_changed(scope, path, entry, &body).await
    }

    async fn read_projection_cursor(
        &self,
        resource_scope: &ironclaw_host_api::resource::ResourceScope,
        path: &ScopedPath,
    ) -> Result<ExternalTranscriptCursor, SessionThreadError> {
        Ok(match self.filesystem.get(resource_scope, path).await? {
            Some(row) => deserialize::<ExternalTranscriptCursor>(&row.entry.body)?,
            None => ExternalTranscriptCursor::default(),
        })
    }

    pub(super) async fn projection_cursor(
        &self,
        request: ExternalTranscriptCursorRequest,
    ) -> Result<ExternalTranscriptCursor, SessionThreadError> {
        let path = scoped_path(&format!(
            "{}/{}/cursor.json",
            projections_root(&request.scope, Some(&request.thread_id))?.as_str(),
            request.source.key()
        ))?;
        self.read_projection_cursor(&request.scope.to_resource_scope(), &path)
            .await
    }

    /// Write `entry` unless the stored body already equals `body`. CAS on the
    /// version read, retried a few times against concurrent writers.
    async fn put_if_changed(
        &self,
        scope: &ThreadScope,
        path: &ScopedPath,
        entry: Entry,
        body: &[u8],
    ) -> Result<bool, SessionThreadError> {
        let resource_scope = scope.to_resource_scope();
        for _ in 0..CAS_RETRIES {
            let current = self.filesystem.get(&resource_scope, path).await?;
            let expectation = match &current {
                Some(row) if row.entry.body == body => return Ok(false),
                Some(row) => CasExpectation::Version(row.version),
                None => CasExpectation::Absent,
            };
            match self
                .filesystem
                .put(&resource_scope, path, entry.clone(), expectation)
                .await
            {
                Ok(_) => return Ok(true),
                Err(error) if is_cas_conflict(&error) => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(SessionThreadError::Backend(
            "search projection write kept losing to concurrent writers".to_string(),
        ))
    }

    /// Move the cursor: forward only within a generation; a `replace`
    /// starts the next generation at exactly `through` (possibly backwards).
    /// The anchor follows the cursor. CAS on the version read, retried.
    async fn move_projection_cursor(
        &self,
        resource_scope: &ironclaw_host_api::resource::ResourceScope,
        path: &ScopedPath,
        through: u64,
        replace: bool,
        anchor: Option<String>,
        complete: bool,
    ) -> Result<ExternalTranscriptCursor, SessionThreadError> {
        for _ in 0..CAS_RETRIES {
            let current = self.filesystem.get(resource_scope, path).await?;
            let (existing, expectation) = match &current {
                Some(row) => (
                    deserialize::<ExternalTranscriptCursor>(&row.entry.body)?,
                    CasExpectation::Version(row.version),
                ),
                None => (ExternalTranscriptCursor::default(), CasExpectation::Absent),
            };
            let next = if replace {
                ExternalTranscriptCursor {
                    ingested_through: through,
                    generation: existing.generation.saturating_add(1),
                    anchor: anchor.clone(),
                    complete,
                }
            } else if through > existing.ingested_through || (complete && !existing.complete) {
                ExternalTranscriptCursor {
                    ingested_through: through.max(existing.ingested_through),
                    generation: existing.generation,
                    anchor: if through > existing.ingested_through {
                        anchor.clone()
                    } else {
                        existing.anchor.clone()
                    },
                    complete: complete || existing.complete,
                }
            } else {
                return Ok(existing);
            };
            match self
                .filesystem
                .put(
                    resource_scope,
                    path,
                    Entry::bytes(serialize(&next)?),
                    expectation,
                )
                .await
            {
                Ok(_) => return Ok(next),
                Err(error) if is_cas_conflict(&error) => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(SessionThreadError::Backend(
            "search projection cursor kept losing to concurrent writers".to_string(),
        ))
    }
}

fn projection_path(dir: &str, ordinal: u64) -> Result<ScopedPath, SessionThreadError> {
    scoped_path(&format!("{dir}/m-{ordinal:010}.json"))
}

/// The `(thread_id, sequence)` keyset position of a message row.
fn row_cursor(row: &VersionedEntry) -> Result<Option<SearchIndexCursor>, SessionThreadError> {
    let thread = row.entry.indexed.get(&fs_index_key("thread_id")?);
    let sequence = row.entry.indexed.get(&fs_index_key("sequence")?);
    Ok(match (thread, sequence) {
        (Some(IndexValue::Text(thread_id)), Some(IndexValue::I64(sequence))) => {
            Some(SearchIndexCursor {
                thread_id: thread_id.clone(),
                sequence: *sequence,
            })
        }
        _ => None,
    })
}

/// Whether a message row's indexed `search_text` differs from what the
/// current projection rule computes for its body.
fn projection_is_stale(row: &VersionedEntry) -> Result<bool, SessionThreadError> {
    if row.entry.kind.as_ref().map(RecordKind::as_str) != Some(THREAD_MESSAGE_KIND) {
        return Ok(false);
    }
    let record = deserialize::<ThreadMessageRecord>(&row.entry.body)?;
    let expected = search_projection(&record);
    let current = match row.entry.indexed.get(&fs_index_key(SEARCH_TEXT_INDEX_KEY)?) {
        Some(IndexValue::Text(text)) => Some(text.as_str()),
        _ => None,
    };
    Ok(expected.as_deref() != current)
}
