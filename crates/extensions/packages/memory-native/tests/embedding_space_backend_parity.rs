//! Embedding-space (fingerprint) vector search parity across the durable
//! filesystem backends.
//!
//! [`FilesystemMemoryDocumentRepository`] stores each chunk's vector under a
//! key derived from its embedder fingerprint and records the fingerprint
//! beside it; search ranks only vectors of the query's own space. Those keys
//! and the `Bytes` vector blob round-trip through each backend's indexed
//! projection (in-memory map, libSQL JSON, PostgreSQL JSONB), so the same
//! scenarios run on all three. The in-crate unit tests cover the in-memory
//! backend only.
//!
//! The re-embed backfill (`MemoryReembedJob`) runs on the same three
//! backends: it enumerates scopes by directory listing, attaches vectors with
//! a version-checked put, and keeps its cursor as a CAS record — all backend
//! behavior.
//!
//! PostgreSQL leg: `DATABASE_URL` when set (one isolated database per test),
//! else a testcontainers server. Without either it skips with a notice on
//! stderr; `IRONCLAW_REQUIRE_POSTGRES` turns that skip into a failure.

#![deny(clippy::let_underscore_must_use)]
use std::io::Write as _;
use std::sync::Arc;

use ironclaw_filesystem::{
    InMemoryBackend, IsolatedPostgresDatabase, IsolatedPostgresProvisioner, LibSqlRootFilesystem,
    PostgresRootFilesystem, PostgresUnreachable, RootFilesystem,
};
use ironclaw_memory::{MemoryDocumentPath, content_sha256};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use ironclaw_memory::MemoryDocumentScope;
use ironclaw_memory_native::{
    FilesystemMemoryDocumentRepository, MemoryChunkEmbedding, MemoryChunkWrite,
    MemoryDocumentIndexRepository, MemoryDocumentRepository, MemoryEmbedder, MemoryEmbeddingError,
    MemoryEmbeddingFingerprint, MemoryReembedJob, MemoryReembedOptions, MemoryReembedOutcome,
    MemoryReembedProgress, MemoryReembedReport, MemorySearchRequest,
};

type Repo = FilesystemMemoryDocumentRepository<dyn RootFilesystem>;

fn doc(relative: &str) -> MemoryDocumentPath {
    MemoryDocumentPath::new("tenant-a", "alice", Some("proj-1"), relative).unwrap()
}

fn embedding(fingerprint: &str, vector: Vec<f32>) -> MemoryChunkEmbedding {
    MemoryChunkEmbedding {
        fingerprint: MemoryEmbeddingFingerprint::new(fingerprint).unwrap(),
        vector,
    }
}

async fn write_embedded(
    repo: &Repo,
    path: &MemoryDocumentPath,
    content: &str,
    e: MemoryChunkEmbedding,
) {
    repo.write_document(path, content.as_bytes()).await.unwrap();
    repo.replace_document_chunks_if_current(
        path,
        &content_sha256(content),
        &[MemoryChunkWrite {
            content: content.to_string(),
            embedding: Some(e),
        }],
    )
    .await
    .unwrap();
}

/// Vector-only search: the query text matches no chunk, so every hit comes
/// from the vector branch.
async fn vector_hits(repo: &Repo, query: MemoryChunkEmbedding) -> Vec<String> {
    let scope = doc("x.md").scope().clone();
    repo.search_documents(
        &scope,
        &MemorySearchRequest::new("zzzqqq")
            .unwrap()
            .with_query_embedding(query)
            .with_min_vector_similarity(0.1),
    )
    .await
    .unwrap()
    .into_iter()
    .map(|hit| hit.path.relative_path().to_string())
    .collect()
}

// -- Scenarios ----------------------------------------------------------------

/// Same dimension, identical vector, different model: only the query's own
/// space ranks, and within it cosine similarity orders the hits.
async fn only_the_query_space_ranks_in_cosine_order(fs: Arc<dyn RootFilesystem>) {
    let repo = Repo::new(fs);
    write_embedded(
        &repo,
        &doc("notes/other.md"),
        "other",
        embedding("model-b:2", vec![1.0, 0.0]),
    )
    .await;
    write_embedded(
        &repo,
        &doc("notes/near.md"),
        "near",
        // Cosine 0.8 but a far larger magnitude: dot product would rank it
        // above `exact`, so the asserted order pins cosine, not dot product.
        embedding("model-a:2", vec![8.0, 6.0]),
    )
    .await;
    write_embedded(
        &repo,
        &doc("notes/exact.md"),
        "exact",
        embedding("model-a:2", vec![1.0, 0.0]),
    )
    .await;

    assert_eq!(
        vector_hits(&repo, embedding("model-a:2", vec![1.0, 0.0])).await,
        vec!["notes/exact.md", "notes/near.md"],
        "only model-a vectors rank, best cosine first"
    );
    assert_eq!(
        vector_hits(&repo, embedding("model-b:2", vec![1.0, 0.0])).await,
        vec!["notes/other.md"],
    );
    assert!(
        vector_hits(&repo, embedding("model-c:2", vec![1.0, 0.0]))
            .await
            .is_empty(),
        "a space with no vectors ranks nothing"
    );
}

/// Re-indexing a document under a new embedder moves its vector to the new
/// space: the old space no longer finds it.
async fn reembedding_moves_a_document_between_spaces(fs: Arc<dyn RootFilesystem>) {
    let repo = Repo::new(fs);
    let path = doc("notes/moved.md");
    write_embedded(&repo, &path, "v1", embedding("model-a:2", vec![1.0, 0.0])).await;
    assert_eq!(
        vector_hits(&repo, embedding("model-a:2", vec![1.0, 0.0])).await,
        vec!["notes/moved.md"]
    );

    write_embedded(&repo, &path, "v2", embedding("model-b:2", vec![0.0, 1.0])).await;
    assert!(
        vector_hits(&repo, embedding("model-a:2", vec![1.0, 0.0]))
            .await
            .is_empty(),
        "the old space's vector must not survive re-embedding"
    );
    assert_eq!(
        vector_hits(&repo, embedding("model-b:2", vec![0.0, 1.0])).await,
        vec!["notes/moved.md"]
    );
}

// -- Re-embed backfill ---------------------------------------------------------

/// What the scripted embedder does on one call (0-based).
#[derive(Clone, Copy)]
enum CallFault {
    Transient,
    Permanent,
}

/// Deterministic 2-d embedder: a text containing "alpha" embeds to `[1, 0]`,
/// anything else to `[0, 1]`. Records every call's inputs; `faults` scripts
/// failures by call number.
struct ScriptedEmbedder {
    fingerprint: &'static str,
    max_batch: usize,
    max_input_bytes: usize,
    faults: Vec<(usize, CallFault)>,
    calls: Mutex<Vec<Vec<String>>>,
    /// Runs inside the provider call (after the inputs are taken, before the
    /// answer returns) with the call number: the window between embed and
    /// attach where a concurrent write can land.
    during_call: Mutex<Option<DuringCall>>,
}

type DuringCall =
    Box<dyn FnMut(usize) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send>;

impl ScriptedEmbedder {
    fn new(fingerprint: &'static str, max_batch: usize) -> Self {
        Self {
            fingerprint,
            max_batch,
            max_input_bytes: 1_000,
            faults: Vec::new(),
            calls: Mutex::new(Vec::new()),
            during_call: Mutex::new(None),
        }
    }

    fn during_call(self, hook: DuringCall) -> Self {
        *self.during_call.lock().unwrap() = Some(hook);
        self
    }

    /// Every input the run handed the provider, retries included.
    fn dispatched_inputs(&self) -> usize {
        self.calls().iter().map(Vec::len).sum()
    }

    fn failing(mut self, call: usize, fault: CallFault) -> Self {
        self.faults.push((call, fault));
        self
    }

    fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }

    /// Every text that reached the provider and got an answer, in order.
    fn embedded_texts(&self) -> Vec<String> {
        let calls = self.calls();
        calls
            .iter()
            .enumerate()
            .filter(|(number, _)| !self.faults.iter().any(|(at, _)| at == number))
            .flat_map(|(_, texts)| texts.clone())
            .collect()
    }
}

#[async_trait]
impl MemoryEmbedder for ScriptedEmbedder {
    fn dimension(&self) -> u32 {
        2
    }
    fn max_batch(&self) -> usize {
        self.max_batch
    }
    fn max_input_bytes(&self) -> usize {
        self.max_input_bytes
    }
    fn fingerprint(&self) -> MemoryEmbeddingFingerprint {
        MemoryEmbeddingFingerprint::new(self.fingerprint).unwrap()
    }
    async fn embed(
        &self,
        _scope: &MemoryDocumentScope,
        texts: Vec<String>,
    ) -> Result<Vec<Vec<f32>>, MemoryEmbeddingError> {
        let number = {
            let mut calls = self.calls.lock().unwrap();
            calls.push(texts.clone());
            calls.len() - 1
        };
        let pending = self
            .during_call
            .lock()
            .unwrap()
            .as_mut()
            .map(|hook| hook(number));
        if let Some(pending) = pending {
            pending.await;
        }
        match self.faults.iter().find(|(at, _)| *at == number) {
            Some((_, CallFault::Transient)) => Err(MemoryEmbeddingError::transient(
                "HTTP 429",
                Some(Duration::from_millis(1)),
            )),
            Some((_, CallFault::Permanent)) => Err(MemoryEmbeddingError::new("HTTP 401")),
            None => Ok(texts
                .iter()
                .map(|text| {
                    if text.contains("alpha") {
                        vec![1.0, 0.0]
                    } else {
                        vec![0.0, 1.0]
                    }
                })
                .collect()),
        }
    }
}

/// Operator pacing with test-sized waits.
fn fast(max_chunks: Option<u64>) -> MemoryReembedOptions {
    MemoryReembedOptions {
        min_call_interval: Duration::ZERO,
        initial_backoff: Duration::from_millis(1),
        max_backoff: Duration::from_millis(5),
        max_transient_retries: 2,
        start_delay: Duration::ZERO,
        pause_between_runs: Duration::ZERO,
        ..MemoryReembedOptions::operator(false, max_chunks)
    }
}

/// A document as it was stored before embeddings existed: text-only chunks.
async fn write_text_only(repo: &Repo, path: &MemoryDocumentPath, chunks: &[&str]) {
    let content = chunks.join("\n\n");
    repo.write_document(path, content.as_bytes()).await.unwrap();
    let writes: Vec<MemoryChunkWrite> = chunks
        .iter()
        .map(|chunk| MemoryChunkWrite {
            content: chunk.to_string(),
            embedding: None,
        })
        .collect();
    repo.replace_document_chunks_if_current(path, &content_sha256(&content), &writes)
        .await
        .unwrap();
}

async fn run_job(
    fs: &Arc<dyn RootFilesystem>,
    embedder: &Arc<ScriptedEmbedder>,
    options: &MemoryReembedOptions,
) -> (MemoryReembedReport, Vec<MemoryReembedProgress>) {
    let job = MemoryReembedJob::new(
        Arc::clone(fs),
        Arc::clone(embedder) as Arc<dyn MemoryEmbedder>,
    );
    let mut samples = Vec::new();
    let mut sink = |progress| samples.push(progress);
    let report = job.run(options, &mut sink).await.unwrap();
    (report, samples)
}

/// Five pre-embedding chunks over three documents in two scopes.
async fn seed_unembedded(repo: &Repo) {
    write_text_only(repo, &doc("notes/a.md"), &["alpha one", "beta two"]).await;
    write_text_only(repo, &doc("notes/b.md"), &["beta three"]).await;
    let other_scope =
        MemoryDocumentPath::new_with_agent("tenant-a", "bob", Some("agent-1"), None, "c.md")
            .unwrap();
    write_text_only(repo, &other_scope, &["alpha four", "beta five"]).await;
}

/// Pre-embedding chunks get a vector in the current space, sent in batches no
/// larger than the embedder admits; a re-run finds nothing and calls nothing.
async fn reembed_backfills_in_bounded_batches_and_reruns_as_a_no_op(fs: Arc<dyn RootFilesystem>) {
    let repo = Repo::new(Arc::clone(&fs));
    seed_unembedded(&repo).await;
    // Plus one chunk the embedder can never take (over its input limit).
    write_text_only(&repo, &doc("notes/long.md"), &[&"x".repeat(1_001)]).await;
    assert!(
        vector_hits(&repo, embedding("model-a:2", vec![1.0, 0.0]))
            .await
            .is_empty(),
        "before the backfill nothing is vector-searchable"
    );

    let embedder = Arc::new(ScriptedEmbedder::new("model-a:2", 2));
    let (report, samples) = run_job(&fs, &embedder, &fast(None)).await;
    assert_eq!(report.outcome, MemoryReembedOutcome::Complete);
    assert_eq!((report.backlog, report.ineligible), (5, 1));
    assert_eq!((report.embedded, report.remaining), (5, 0));
    assert!(
        embedder.calls().iter().all(|batch| batch.len() <= 2),
        "batches respect max_batch: {:?}",
        embedder.calls()
    );
    let mut sent = embedder.embedded_texts();
    sent.sort();
    assert_eq!(
        sent,
        vec![
            "alpha four",
            "alpha one",
            "beta five",
            "beta three",
            "beta two"
        ],
        "every pending chunk was sent exactly once; the over-long one never"
    );
    assert_eq!(samples.first().map(|s| s.backlog), Some(5));
    assert_eq!(
        samples.last().map(|s| (s.backlog, s.embedded_total)),
        Some((0, 5))
    );
    assert_eq!(
        samples.iter().map(|s| s.embedded).sum::<u64>(),
        5,
        "progress deltas add up to the embedded total"
    );
    assert_eq!(
        vector_hits(&repo, embedding("model-a:2", vec![1.0, 0.0])).await,
        vec!["notes/a.md"],
        "a pre-existing document is now found by meaning"
    );

    let calls_before = embedder.calls().len();
    let (again, _) = run_job(&fs, &embedder, &fast(None)).await;
    assert_eq!(again.outcome, MemoryReembedOutcome::Complete);
    assert_eq!(
        (again.backlog, again.embedded, again.provider_calls),
        (0, 0, 0)
    );
    assert_eq!(
        embedder.calls().len(),
        calls_before,
        "a finished backfill calls no provider"
    );

    // Dry run counts, sends nothing, writes nothing.
    write_text_only(&repo, &doc("notes/late.md"), &["alpha late"]).await;
    let dry_run = MemoryReembedOptions {
        dry_run: true,
        ..fast(None)
    };
    let (dry, _) = run_job(&fs, &embedder, &dry_run).await;
    assert_eq!(
        (dry.outcome, dry.backlog, dry.remaining),
        (MemoryReembedOutcome::DryRun, 1, 1)
    );
    assert_eq!(embedder.calls().len(), calls_before);
}

/// A run stopped by its budget resumes after a restart (a fresh job over the
/// same storage) from the durable cursor: nothing is re-sent.
async fn reembed_resumes_from_its_durable_cursor(fs: Arc<dyn RootFilesystem>) {
    let repo = Repo::new(Arc::clone(&fs));
    seed_unembedded(&repo).await;
    let first = Arc::new(ScriptedEmbedder::new("model-a:2", 1));
    let (stopped, _) = run_job(&fs, &first, &fast(Some(2))).await;
    assert_eq!(stopped.outcome, MemoryReembedOutcome::BudgetExhausted);
    assert_eq!((stopped.embedded, stopped.remaining), (2, 3));
    assert!(!stopped.resumed, "the first run starts a fresh pass");

    let second = Arc::new(ScriptedEmbedder::new("model-a:2", 1));
    let (resumed, _) = run_job(&fs, &second, &fast(None)).await;
    assert_eq!(resumed.outcome, MemoryReembedOutcome::Complete);
    assert_eq!((resumed.backlog, resumed.embedded), (3, 3));
    assert!(
        resumed.resumed,
        "the restarted run continued from the durable cursor"
    );
    let mut all = first.embedded_texts();
    all.extend(second.embedded_texts());
    all.sort();
    assert_eq!(
        all,
        vec![
            "alpha four",
            "alpha one",
            "beta five",
            "beta three",
            "beta two"
        ],
        "each chunk embedded exactly once across the interruption"
    );
    // The finished pass reset the cursor: new backlog starts a fresh pass.
    write_text_only(&repo, &doc("notes/z.md"), &["alpha new"]).await;
    let third = Arc::new(ScriptedEmbedder::new("model-a:2", 1));
    let (fresh, _) = run_job(&fs, &third, &fast(None)).await;
    assert_eq!((fresh.embedded, fresh.resumed), (1, false));
}

/// A provider failure part-way keeps the batches that landed; transient
/// failures are retried with backoff, permanent ones stop the run at once.
async fn reembed_survives_partial_provider_failure(fs: Arc<dyn RootFilesystem>) {
    let repo = Repo::new(Arc::clone(&fs));
    seed_unembedded(&repo).await;

    // A 429 on the first call is retried after a backoff; the batch lands.
    // The retry is charged too: 2 inputs sent twice = the budget of 4.
    let flaky = Arc::new(ScriptedEmbedder::new("model-a:2", 2).failing(0, CallFault::Transient));
    let (report, _) = run_job(&fs, &flaky, &fast(Some(4))).await;
    assert_eq!(report.outcome, MemoryReembedOutcome::BudgetExhausted);
    assert_eq!(
        (report.embedded, report.provider_calls, report.dispatched),
        (2, 2, 4)
    );

    // Permanent failure on the next call: stop, keep what landed.
    let rejecting =
        Arc::new(ScriptedEmbedder::new("model-a:2", 2).failing(0, CallFault::Permanent));
    let (rejected, _) = run_job(&fs, &rejecting, &fast(None)).await;
    assert_eq!(rejected.outcome, MemoryReembedOutcome::ProviderRejected);
    assert!(rejected.outcome.is_failure());
    assert_eq!(
        (rejected.backlog, rejected.embedded, rejected.provider_calls),
        (3, 0, 1)
    );

    // Transient on every attempt: retries exhausted, reported unavailable.
    let down = Arc::new(
        ScriptedEmbedder::new("model-a:2", 2)
            .failing(0, CallFault::Transient)
            .failing(1, CallFault::Transient)
            .failing(2, CallFault::Transient),
    );
    let (unavailable, _) = run_job(&fs, &down, &fast(None)).await;
    assert_eq!(
        unavailable.outcome,
        MemoryReembedOutcome::ProviderUnavailable
    );
    assert_eq!(
        unavailable.provider_calls, 3,
        "one call plus max_transient_retries"
    );

    // A healthy provider then finishes only what is left, once a 429 clears.
    let healthy = Arc::new(ScriptedEmbedder::new("model-a:2", 2).failing(0, CallFault::Transient));
    let (finished, _) = run_job(&fs, &healthy, &fast(None)).await;
    assert_eq!(finished.outcome, MemoryReembedOutcome::Complete);
    assert_eq!((finished.backlog, finished.embedded), (3, 3));
    let mut sent = flaky.embedded_texts();
    sent.extend(healthy.embedded_texts());
    sent.sort();
    assert_eq!(
        sent,
        vec![
            "alpha four",
            "alpha one",
            "beta five",
            "beta three",
            "beta two"
        ],
        "batches that landed before the failure were not re-sent"
    );
}

/// Switching models backfills the new space and keeps the old space's
/// vectors (never deleted — just not queried by the new model).
async fn reembed_after_a_model_switch_keeps_the_old_space(fs: Arc<dyn RootFilesystem>) {
    let repo = Repo::new(Arc::clone(&fs));
    write_embedded(
        &repo,
        &doc("notes/old.md"),
        "alpha old",
        embedding("model-a:2", vec![1.0, 0.0]),
    )
    .await;
    let model_b = Arc::new(ScriptedEmbedder::new("model-b:2", 8));
    let (report, _) = run_job(&fs, &model_b, &fast(None)).await;
    assert_eq!(
        (report.outcome, report.backlog, report.embedded),
        (MemoryReembedOutcome::Complete, 1, 1)
    );
    assert_eq!(
        vector_hits(&repo, embedding("model-b:2", vec![1.0, 0.0])).await,
        vec!["notes/old.md"],
        "the new space finds the document"
    );
    assert_eq!(
        vector_hits(&repo, embedding("model-a:2", vec![1.0, 0.0])).await,
        vec!["notes/old.md"],
        "the old space's vector was kept"
    );
    // Back on model A: nothing to do — its vectors were never removed.
    let model_a = Arc::new(ScriptedEmbedder::new("model-a:2", 8));
    let (back, _) = run_job(&fs, &model_a, &fast(None)).await;
    assert_eq!((back.backlog, back.provider_calls), (0, 0));
}

/// The boot-time task runs the same job in the background and reports
/// progress through its observer.
async fn reembed_background_task_backfills_and_reports(fs: Arc<dyn RootFilesystem>) {
    let repo = Repo::new(Arc::clone(&fs));
    seed_unembedded(&repo).await;
    let embedder = Arc::new(ScriptedEmbedder::new("model-a:2", 2));
    let samples = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&samples);
    let task = MemoryReembedJob::new(
        Arc::clone(&fs),
        Arc::clone(&embedder) as Arc<dyn MemoryEmbedder>,
    )
    .spawn_background(fast(Some(2)), move |progress| {
        sink.lock().unwrap().push(progress)
    });
    task.wait().await;
    let samples = samples.lock().unwrap().clone();
    assert_eq!(
        samples.iter().map(|s| s.embedded).sum::<u64>(),
        5,
        "budgeted runs repeat until the backlog is done"
    );
    assert_eq!(samples.last().map(|s| s.backlog), Some(0));
    assert_eq!(
        vector_hits(&repo, embedding("model-a:2", vec![1.0, 0.0])).await,
        vec!["notes/a.md"]
    );
}

fn memory_invocation() -> ironclaw_memory::MemoryInvocation {
    use ironclaw_host_api::ids::{InvocationId, ProjectId, TenantId, UserId};
    ironclaw_memory::MemoryInvocation {
        scope: ironclaw_host_api::resource::ResourceScope {
            tenant_id: TenantId::new("tenant-a").unwrap(),
            user_id: UserId::new("alice").unwrap(),
            agent_id: None,
            project_id: Some(ProjectId::new("proj-1").unwrap()),
            mission_id: None,
            thread_id: None,
            invocation_id: InvocationId::new(),
        },
        correlation_id: ironclaw_host_api::ids::CorrelationId::new(),
    }
}

/// Full-text hits for `query` with their snippets (the chunk bodies).
async fn text_hits(repo: &Repo, query: &str) -> Vec<(String, String)> {
    let scope = doc("x.md").scope().clone();
    repo.search_documents(&scope, &MemorySearchRequest::new(query).unwrap())
        .await
        .unwrap()
        .into_iter()
        .map(|hit| (hit.path.relative_path().to_string(), hit.snippet))
        .collect()
}

/// `memory.write` (the production `NativeMemoryService::write` caller)
/// rewrites a document after the backfill embedded its old chunk and before
/// the vector is attached. The rewrite deletes and recreates the chunk path
/// (versions restart), so a CAS on the scanned version could succeed against
/// the NEW chunk; the backfill must notice the chunk is not the one it
/// embedded and write nothing.
async fn reembed_never_overwrites_a_chunk_rewritten_mid_batch(fs: Arc<dyn RootFilesystem>) {
    let repo = Repo::new(Arc::clone(&fs));
    write_text_only(&repo, &doc("notes/race.md"), &["alpha old"]).await;
    let service = Arc::new(
        ironclaw_memory_native::NativeMemoryService::from_filesystem_with_embedder(
            Arc::clone(&fs),
            None,
            Some(Arc::new(ScriptedEmbedder::new("model-b:2", 8)) as Arc<dyn MemoryEmbedder>),
        ),
    );
    let writer = Arc::clone(&service);
    let racing = Arc::new(ScriptedEmbedder::new("model-b:2", 8).during_call(Box::new(
        move |number| {
            let writer = Arc::clone(&writer);
            Box::pin(async move {
                if number == 0 {
                    use ironclaw_memory::MemoryServiceWriteRequest;
                    writer
                        .write(
                            memory_invocation(),
                            MemoryServiceWriteRequest {
                                target: "notes/race.md".to_string(),
                                content: "beta new".to_string(),
                                append: false,
                                old_string: None,
                                new_string: None,
                                replace_all: false,
                                metadata: None,
                                timezone: None,
                                expected_content_hash: None,
                            },
                        )
                        .await
                        .expect("the concurrent memory.write succeeds");
                }
            })
        },
    )));
    let (report, _) = run_job(&fs, &racing, &fast(None)).await;
    assert_eq!(
        (report.embedded, report.superseded),
        (0, 1),
        "the chunk the backfill embedded was replaced: nothing is attached"
    );
    assert_eq!(
        text_hits(&repo, "beta").await,
        vec![("notes/race.md".to_string(), "beta new".to_string())],
        "the new chunk's body survives"
    );
    assert!(
        text_hits(&repo, "alpha").await.is_empty(),
        "the old body was not written back"
    );
    assert_eq!(
        vector_hits(&repo, embedding("model-b:2", vec![0.0, 1.0])).await,
        vec!["notes/race.md"],
        "the new chunk keeps the vector its own write gave it"
    );
    assert!(
        vector_hits(&repo, embedding("model-b:2", vec![1.0, 0.0]))
            .await
            .is_empty(),
        "the old text's vector was not attached to the new chunk"
    );
}

/// Every provider call rewrites the documents, so every attach is
/// superseded. The input budget is charged at dispatch, so the run still
/// stops after sending `max_chunks` inputs; and a separate call cap bounds
/// retries.
async fn reembed_budget_charges_inputs_at_dispatch(fs: Arc<dyn RootFilesystem>) {
    let repo = Arc::new(Repo::new(Arc::clone(&fs)));
    seed_unembedded(&repo).await;
    let rewriter = Arc::clone(&repo);
    let conflicting = Arc::new(ScriptedEmbedder::new("model-a:2", 1).during_call(Box::new(
        move |number| {
            let repo = Arc::clone(&rewriter);
            Box::pin(async move {
                let other = MemoryDocumentPath::new_with_agent(
                    "tenant-a",
                    "bob",
                    Some("agent-1"),
                    None,
                    "c.md",
                )
                .unwrap();
                let suffix = format!(" v{number}");
                for (path, chunks) in [
                    (doc("notes/a.md"), vec!["alpha one", "beta two"]),
                    (doc("notes/b.md"), vec!["beta three"]),
                    (other, vec!["alpha four", "beta five"]),
                ] {
                    let chunks: Vec<String> = chunks
                        .iter()
                        .map(|chunk| format!("{chunk}{suffix}"))
                        .collect();
                    let chunks: Vec<&str> = chunks.iter().map(String::as_str).collect();
                    write_text_only(&repo, &path, &chunks).await;
                }
            })
        },
    )));
    let (report, _) = run_job(&fs, &conflicting, &fast(Some(2))).await;
    assert_eq!(report.outcome, MemoryReembedOutcome::BudgetExhausted);
    assert_eq!((report.embedded, report.superseded), (0, 2));
    assert_eq!(report.dispatched, 2);
    assert_eq!(
        conflicting.dispatched_inputs(),
        2,
        "superseded inputs are charged: the provider saw at most the budget"
    );

    // The call cap bounds a failing batch's retries.
    let down = Arc::new(
        (0..10).fold(ScriptedEmbedder::new("model-a:2", 8), |embedder, call| {
            embedder.failing(call, CallFault::Transient)
        }),
    );
    let capped = MemoryReembedOptions {
        max_transient_retries: 9,
        max_provider_calls: Some(3),
        ..fast(None)
    };
    let (unavailable, _) = run_job(&fs, &down, &capped).await;
    assert_eq!(
        unavailable.outcome,
        MemoryReembedOutcome::ProviderUnavailable
    );
    assert_eq!(
        unavailable.provider_calls, 3,
        "retries count against the call cap"
    );
    assert_eq!(down.calls().len(), 3);

    // Retried inputs are charged too: one 8-input batch retried once is 16.
    let flaky = Arc::new(ScriptedEmbedder::new("model-a:2", 8).failing(0, CallFault::Transient));
    let (retried, _) = run_job(&fs, &flaky, &fast(Some(9))).await;
    assert_eq!(retried.dispatched, flaky.dispatched_inputs() as u64);
    assert!(
        flaky.dispatched_inputs() <= 9,
        "a retry is charged before it is sent: {}",
        flaky.dispatched_inputs()
    );
}

// -- Backends -----------------------------------------------------------------

enum Guard {
    None,
    Dir(#[allow(dead_code, reason = "held only to keep the directory alive")] tempfile::TempDir),
    Postgres(Box<IsolatedPostgresDatabase>),
}

impl Guard {
    async fn finish(self) {
        if let Guard::Postgres(db) = self {
            db.cleanup().await;
        }
    }
}

type Fixture = Option<(Guard, Arc<dyn RootFilesystem>)>;

async fn in_memory() -> Fixture {
    Some((Guard::None, Arc::new(InMemoryBackend::new())))
}

async fn libsql() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let database = Arc::new(
        ::libsql::Builder::new_local(dir.path().join("memory.db"))
            .build()
            .await
            .unwrap(),
    );
    let backend = LibSqlRootFilesystem::new(database).unwrap();
    backend.run_migrations().await.unwrap();
    Some((Guard::Dir(dir), Arc::new(backend)))
}

const POSTGRES_URL_ENV: &str = "DATABASE_URL";

static POSTGRES: IsolatedPostgresProvisioner = IsolatedPostgresProvisioner::new(
    "postgres memory embedding-space parity",
    POSTGRES_URL_ENV,
    "memory_embedding_isolated_",
    PostgresUnreachable::Panic,
);

struct ContainerServer {
    config: tokio_postgres::Config,
    _container: testcontainers_modules::testcontainers::ContainerAsync<
        testcontainers_modules::postgres::Postgres,
    >,
}

static CONTAINER: tokio::sync::OnceCell<Option<ContainerServer>> =
    tokio::sync::OnceCell::const_new();
static CONTAINER_DATABASES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn skip_postgres(reason: &str) -> Fixture {
    assert!(
        std::env::var_os("IRONCLAW_REQUIRE_POSTGRES").is_none(),
        "IRONCLAW_REQUIRE_POSTGRES is set but the memory embedding PostgreSQL leg cannot run: \
         {reason}"
    );
    // Written past libtest's capture so a passing run that tested nothing on
    // PostgreSQL still says so. A failed write to stderr leaves nowhere else
    // to report the skip, so its result is deliberately discarded.
    #[allow(
        clippy::let_underscore_must_use,
        reason = "stderr write failure has no further reporting channel"
    )]
    let _ = writeln!(
        std::io::stderr(),
        "SKIPPED: postgres memory embedding-space parity did not run: {reason}"
    );
    None
}

async fn container_database() -> Result<tokio_postgres::Config, String> {
    let server = CONTAINER
        .get_or_init(|| async {
            use testcontainers_modules::testcontainers::{ImageExt, runners::AsyncRunner};
            let container = testcontainers_modules::postgres::Postgres::default()
                .with_user("postgres")
                .with_password("postgres")
                .with_tag("16-alpine")
                .start()
                .await
                .ok()?;
            let host = container.get_host().await.ok()?;
            let port = container.get_host_port_ipv4(5432).await.ok()?;
            let config = format!("postgres://postgres:postgres@{host}:{port}/postgres")
                .parse::<tokio_postgres::Config>()
                .ok()?;
            Some(ContainerServer {
                config,
                _container: container,
            })
        })
        .await
        .as_ref()
        .ok_or_else(|| {
            format!("{POSTGRES_URL_ENV} not set and docker/testcontainers unavailable")
        })?;
    let (admin, connection) = server
        .config
        .connect(tokio_postgres::NoTls)
        .await
        .map_err(|error| format!("container postgres unreachable ({error})"))?;
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            eprintln!("container postgres admin connection ended: {error}");
        }
    });
    let name = format!(
        "memory_embedding_{}",
        CONTAINER_DATABASES.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    admin
        .execute(&format!("CREATE DATABASE {name}"), &[])
        .await
        .expect("create a per-test database in the container");
    let mut config = server.config.clone();
    config.dbname(&name);
    Ok(config)
}

async fn postgres() -> Fixture {
    let (guard, config) = if std::env::var_os(POSTGRES_URL_ENV).is_some() {
        let db = POSTGRES
            .provision()
            .await
            .expect("configured postgres provisions");
        let config = db.config().clone();
        (Guard::Postgres(Box::new(db)), config)
    } else {
        match container_database().await {
            Ok(config) => (Guard::None, config),
            Err(reason) => return skip_postgres(&reason),
        }
    };
    let manager = deadpool_postgres::Manager::new(config, tokio_postgres::NoTls);
    let pool = deadpool_postgres::Pool::builder(manager)
        .max_size(4)
        .build()
        .expect("postgres pool");
    let backend = PostgresRootFilesystem::new(pool);
    backend.run_migrations().await.expect("migrate postgres");
    Some((guard, Arc::new(backend)))
}

macro_rules! backend_suite {
    ($backend:ident: $($scenario:ident),+ $(,)?) => {
        mod $backend {
            $(
                #[tokio::test]
                async fn $scenario() {
                    let Some((guard, backend)) = super::$backend().await else {
                        return;
                    };
                    super::$scenario(backend).await;
                    guard.finish().await;
                }
            )+
        }
    };
}

backend_suite!(in_memory:
    only_the_query_space_ranks_in_cosine_order,
    reembedding_moves_a_document_between_spaces,
    reembed_backfills_in_bounded_batches_and_reruns_as_a_no_op,
    reembed_resumes_from_its_durable_cursor,
    reembed_survives_partial_provider_failure,
    reembed_after_a_model_switch_keeps_the_old_space,
    reembed_background_task_backfills_and_reports,
    reembed_never_overwrites_a_chunk_rewritten_mid_batch,
    reembed_budget_charges_inputs_at_dispatch,
);
backend_suite!(libsql:
    only_the_query_space_ranks_in_cosine_order,
    reembedding_moves_a_document_between_spaces,
    reembed_backfills_in_bounded_batches_and_reruns_as_a_no_op,
    reembed_resumes_from_its_durable_cursor,
    reembed_survives_partial_provider_failure,
    reembed_after_a_model_switch_keeps_the_old_space,
    reembed_background_task_backfills_and_reports,
    reembed_never_overwrites_a_chunk_rewritten_mid_batch,
    reembed_budget_charges_inputs_at_dispatch,
);
backend_suite!(postgres:
    only_the_query_space_ranks_in_cosine_order,
    reembedding_moves_a_document_between_spaces,
    reembed_backfills_in_bounded_batches_and_reruns_as_a_no_op,
    reembed_resumes_from_its_durable_cursor,
    reembed_survives_partial_provider_failure,
    reembed_after_a_model_switch_keeps_the_old_space,
    reembed_background_task_backfills_and_reports,
    reembed_never_overwrites_a_chunk_rewritten_mid_batch,
    reembed_budget_charges_inputs_at_dispatch,
);
