//! The embedding port this provider uses for semantic (vector) search.
//!
//! The provider never talks to a model or the network itself (`AGENTS.md`:
//! no HTTP client here). When the host configures an embedding provider it
//! injects a [`MemoryEmbedder`]; chunk embeddings are then computed at index
//! time and stored on the chunk record under a key derived from the
//! embedder's [`MemoryEmbeddingFingerprint`], and search fuses a vector
//! branch over vectors of that same fingerprint with full text. With no
//! embedder injected the provider is exactly full-text, as before.

use std::time::Duration;

use async_trait::async_trait;
use ironclaw_memory::MemoryDocumentScope;

/// Most texts sent in one embedding call from memory indexing, whatever the
/// embedder admits: a large batch makes a large response (2048 × 3072 floats
/// is ~100 MB of JSON) and one slow call holds the write.
pub(crate) const MAX_MEMORY_EMBEDDING_BATCH: usize = 256;

/// Longest a search or prompt-context load waits for its query embedding
/// before it searches full text only. The prompt is built after recall, so
/// this bounds how long an embedding provider can delay a turn.
pub(crate) const QUERY_EMBEDDING_BUDGET: Duration = Duration::from_millis(1500);

/// Longest a document reindex waits for its chunk embeddings in total. On
/// expiry the chunks are stored text-only (full text stays current) and the
/// next write of the document re-embeds them.
pub(crate) const INDEX_EMBEDDING_BUDGET: Duration = Duration::from_secs(20);

/// Why an embedding call failed. Callers degrade (index without vectors,
/// search full-text only); the reason is for server-side diagnostics only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEmbeddingError {
    pub reason: String,
}

impl std::fmt::Display for MemoryEmbeddingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "memory embedding failed: {}", self.reason)
    }
}

impl std::error::Error for MemoryEmbeddingError {}

impl MemoryEmbeddingError {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

/// Identity of the vector space an embedder produces (provider, model,
/// dimension, request parameters). Stored with every chunk vector; the vector
/// branch only ranks vectors whose fingerprint equals the query's, so a model
/// switch never mixes two spaces in one ranking.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MemoryEmbeddingFingerprint(String);

impl MemoryEmbeddingFingerprint {
    /// Longest accepted fingerprint, in bytes.
    pub const MAX_LEN: usize = 512;

    pub fn new(raw: impl Into<String>) -> Result<Self, MemoryEmbeddingError> {
        let raw = raw.into();
        if raw.trim().is_empty() {
            return Err(MemoryEmbeddingError::new("embedding fingerprint is empty"));
        }
        if raw.len() > Self::MAX_LEN || raw.chars().any(char::is_control) {
            return Err(MemoryEmbeddingError::new(
                "embedding fingerprint is too long or holds control characters",
            ));
        }
        Ok(Self(raw))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The indexed key this fingerprint's vectors are stored under:
    /// `embedding_` + 128 bits of the fingerprint's SHA-256. Keying vectors by
    /// fingerprint makes the substrate's nearest-neighbour scan see only
    /// compatible vectors (an exact filter, not a post-filter of a top-k).
    pub(crate) fn vector_key(&self) -> String {
        let hash = ironclaw_memory::content_sha256(&self.0);
        let hex = hash.strip_prefix("sha256:").unwrap_or(&hash);
        let short = hex.get(..32).unwrap_or(hex);
        format!("embedding_{short}")
    }
}

impl std::fmt::Display for MemoryEmbeddingFingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One chunk's vector and the space it lives in.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryChunkEmbedding {
    pub fingerprint: MemoryEmbeddingFingerprint,
    pub vector: Vec<f32>,
}

/// Text → fixed-dimension vectors, on behalf of one memory scope.
#[async_trait]
pub trait MemoryEmbedder: Send + Sync {
    /// Every returned vector has exactly this many floats.
    fn dimension(&self) -> u32;

    /// Maximum texts per [`embed`](Self::embed) call; larger batches are split
    /// by the caller (and never exceed [`MAX_MEMORY_EMBEDDING_BATCH`]).
    fn max_batch(&self) -> usize;

    /// Maximum UTF-8 bytes per text. A longer chunk is not sent: it is stored
    /// without a vector and stays reachable by full text.
    fn max_input_bytes(&self) -> usize;

    /// The vector space this embedder produces.
    fn fingerprint(&self) -> MemoryEmbeddingFingerprint;

    /// Embed `texts` in order: one vector per text.
    async fn embed(
        &self,
        scope: &MemoryDocumentScope,
        texts: Vec<String>,
    ) -> Result<Vec<Vec<f32>>, MemoryEmbeddingError>;
}

fn well_shaped(vector: &[f32], dimension: usize) -> bool {
    vector.len() == dimension && vector.iter().all(|value| value.is_finite())
}

/// Embed one text (a search query) within [`QUERY_EMBEDDING_BUDGET`].
pub(crate) async fn embed_query(
    embedder: &dyn MemoryEmbedder,
    scope: &MemoryDocumentScope,
    query: &str,
) -> Result<MemoryChunkEmbedding, MemoryEmbeddingError> {
    if query.trim().is_empty() || query.len() > embedder.max_input_bytes() {
        return Err(MemoryEmbeddingError::new(
            "query is empty or over the embedder's input limit",
        ));
    }
    let fingerprint = embedder.fingerprint();
    let dimension = embedder.dimension() as usize;
    let call = embedder.embed(scope, vec![query.to_string()]);
    let mut vectors = tokio::time::timeout(QUERY_EMBEDDING_BUDGET, call)
        .await
        .map_err(|_| MemoryEmbeddingError::new("query embedding exceeded its time budget"))??;
    match vectors.pop() {
        Some(vector) if vectors.is_empty() && well_shaped(&vector, dimension) => {
            Ok(MemoryChunkEmbedding {
                fingerprint,
                vector,
            })
        }
        _ => Err(MemoryEmbeddingError::new(
            "embedder returned a query vector of the wrong shape",
        )),
    }
}

/// Embed chunk `texts` for indexing: one entry per text, `None` where that
/// chunk gets no vector.
///
/// - A blank chunk, or one over the embedder's per-input byte limit, is
///   skipped on its own instead of failing the whole document's batch.
/// - Calls go out in batches of at most
///   `min(max_batch, MAX_MEMORY_EMBEDDING_BATCH)`; every answer's shape
///   (count, dimension, finiteness) is checked so a misbehaving embedder can
///   never store a mis-sized vector.
/// - The whole run is bounded by [`INDEX_EMBEDDING_BUDGET`]. A failed or
///   timed-out batch stops the run: chunks embedded by earlier batches keep
///   their vectors, the rest are `None`. The error is returned alongside for
///   diagnostics.
pub(crate) async fn embed_chunks(
    embedder: &dyn MemoryEmbedder,
    scope: &MemoryDocumentScope,
    texts: &[String],
) -> (
    Vec<Option<MemoryChunkEmbedding>>,
    Option<MemoryEmbeddingError>,
) {
    let mut out: Vec<Option<MemoryChunkEmbedding>> = vec![None; texts.len()];
    let max_bytes = embedder.max_input_bytes();
    let eligible: Vec<usize> = texts
        .iter()
        .enumerate()
        .filter(|(_, text)| !text.trim().is_empty() && text.len() <= max_bytes)
        .map(|(index, _)| index)
        .collect();
    if eligible.is_empty() {
        return (out, None);
    }
    let fingerprint = embedder.fingerprint();
    let dimension = embedder.dimension() as usize;
    let batch = embedder.max_batch().clamp(1, MAX_MEMORY_EMBEDDING_BATCH);
    let run = async {
        for indexes in eligible.chunks(batch) {
            let inputs = indexes.iter().map(|&index| texts[index].clone()).collect();
            let embedded = embedder.embed(scope, inputs).await?;
            if embedded.len() != indexes.len()
                || embedded
                    .iter()
                    .any(|vector| !well_shaped(vector, dimension))
            {
                return Err(MemoryEmbeddingError::new(
                    "embedder returned a batch of the wrong shape",
                ));
            }
            for (&index, vector) in indexes.iter().zip(embedded) {
                out[index] = Some(MemoryChunkEmbedding {
                    fingerprint: fingerprint.clone(),
                    vector,
                });
            }
        }
        Ok(())
    };
    let error = match tokio::time::timeout(INDEX_EMBEDDING_BUDGET, run).await {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(error),
        Err(_) => Some(MemoryEmbeddingError::new(
            "chunk embedding exceeded its time budget",
        )),
    };
    (out, error)
}

/// Cosine similarity of two equal-length vectors; `None` for mismatched
/// lengths, empty or zero-magnitude vectors, or a non-finite result.
pub(crate) fn cosine_similarity(left: &[f32], right: &[f32]) -> Option<f32> {
    if left.len() != right.len() || left.is_empty() {
        return None;
    }
    let (mut dot, mut left_norm, mut right_norm) = (0.0_f32, 0.0_f32, 0.0_f32);
    for (l, r) in left.iter().zip(right) {
        dot += l * r;
        left_norm += l * l;
        right_norm += r * r;
    }
    if left_norm <= 0.0 || right_norm <= 0.0 {
        return None;
    }
    let score = dot / (left_norm.sqrt() * right_norm.sqrt());
    score.is_finite().then_some(score)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// Embeds `[len, 1.0]`; fails every call after `fail_after` calls.
    struct Counting {
        max_batch: usize,
        max_input_bytes: usize,
        fail_after: Option<usize>,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl Counting {
        fn new(max_batch: usize, max_input_bytes: usize, fail_after: Option<usize>) -> Self {
            Self {
                max_batch,
                max_input_bytes,
                fail_after,
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl MemoryEmbedder for Counting {
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
            MemoryEmbeddingFingerprint::new("test:counting:2").unwrap()
        }
        async fn embed(
            &self,
            _scope: &MemoryDocumentScope,
            texts: Vec<String>,
        ) -> Result<Vec<Vec<f32>>, MemoryEmbeddingError> {
            let mut calls = self.calls.lock().unwrap();
            if self.fail_after.is_some_and(|limit| calls.len() >= limit) {
                return Err(MemoryEmbeddingError::new("offline"));
            }
            calls.push(texts.clone());
            Ok(texts.iter().map(|t| vec![t.len() as f32, 1.0]).collect())
        }
    }

    fn scope() -> MemoryDocumentScope {
        MemoryDocumentScope::new("tenant", "user", None).unwrap()
    }

    #[tokio::test]
    async fn an_oversized_chunk_is_skipped_alone_not_the_whole_document() {
        let embedder = Counting::new(10, 8, None);
        let texts = vec![
            "short".to_string(),
            "x".repeat(9),
            "   ".to_string(),
            "tiny".to_string(),
        ];
        let (vectors, error) = embed_chunks(&embedder, &scope(), &texts).await;
        assert!(error.is_none());
        assert!(vectors[0].is_some() && vectors[3].is_some());
        assert!(vectors[1].is_none(), "the oversized chunk gets no vector");
        assert!(vectors[2].is_none(), "a blank chunk gets no vector");
        assert_eq!(
            embedder.calls.lock().unwrap().clone(),
            vec![vec!["short".to_string(), "tiny".to_string()]],
            "only eligible chunks are sent"
        );
        assert_eq!(vectors[3].as_ref().unwrap().vector, vec![4.0, 1.0]);
    }

    #[tokio::test]
    async fn a_failed_later_batch_keeps_the_earlier_batches_vectors() {
        let embedder = Counting::new(2, 100, Some(1));
        let texts: Vec<String> = ["a", "bb", "ccc", "dddd"].map(String::from).to_vec();
        let (vectors, error) = embed_chunks(&embedder, &scope(), &texts).await;
        assert!(error.is_some(), "the second batch failed");
        assert!(vectors[0].is_some() && vectors[1].is_some());
        assert!(vectors[2].is_none() && vectors[3].is_none());
    }

    #[tokio::test]
    async fn batches_never_exceed_the_memory_cap() {
        let embedder = Counting::new(usize::MAX, 100, None);
        let texts = vec!["w".to_string(); MAX_MEMORY_EMBEDDING_BATCH + 1];
        let (vectors, error) = embed_chunks(&embedder, &scope(), &texts).await;
        assert!(error.is_none());
        assert!(vectors.iter().all(Option::is_some));
        let sizes: Vec<usize> = embedder
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(Vec::len)
            .collect();
        assert_eq!(sizes, vec![MAX_MEMORY_EMBEDDING_BATCH, 1]);
    }

    #[test]
    fn fingerprints_key_vectors_by_space() {
        let a =
            MemoryEmbeddingFingerprint::new("openai:text-embedding-3-small:1536:native").unwrap();
        let b =
            MemoryEmbeddingFingerprint::new("openai:text-embedding-ada-002:1536:native").unwrap();
        assert_ne!(a.vector_key(), b.vector_key());
        assert_eq!(a.vector_key().len(), "embedding_".len() + 32);
        assert!(MemoryEmbeddingFingerprint::new("  ").is_err());
        assert!(MemoryEmbeddingFingerprint::new("a\nb").is_err());
    }
}
