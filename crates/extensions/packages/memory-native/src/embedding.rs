//! The embedding port this provider uses for semantic (vector) search.
//!
//! The provider never talks to a model or the network itself (`AGENTS.md`:
//! no HTTP client here). When the host configures an embedding provider it
//! injects a [`MemoryEmbedder`]; chunk embeddings are then computed at index
//! time and stored on the chunk record's `embedding` projection, and search
//! fuses a vector branch with full text. With no embedder injected the
//! provider is exactly full-text, as before.

use async_trait::async_trait;
use ironclaw_memory::MemoryDocumentScope;

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

/// Text → fixed-dimension vectors, on behalf of one memory scope.
#[async_trait]
pub trait MemoryEmbedder: Send + Sync {
    /// Every returned vector has exactly this many floats.
    fn dimension(&self) -> u32;

    /// Maximum texts per [`embed`](Self::embed) call; larger batches are split
    /// by the caller.
    fn max_batch(&self) -> usize;

    /// Embed `texts` in order: one vector per text.
    async fn embed(
        &self,
        scope: &MemoryDocumentScope,
        texts: Vec<String>,
    ) -> Result<Vec<Vec<f32>>, MemoryEmbeddingError>;
}

/// Embed `texts` in `max_batch`-sized calls and check the answer's shape
/// (count and dimension) so a misbehaving embedder can never store a
/// mis-sized vector.
pub(crate) async fn embed_all(
    embedder: &dyn MemoryEmbedder,
    scope: &MemoryDocumentScope,
    texts: &[String],
) -> Result<Vec<Vec<f32>>, MemoryEmbeddingError> {
    let batch = embedder.max_batch().max(1);
    let dimension = embedder.dimension() as usize;
    let mut vectors = Vec::with_capacity(texts.len());
    for chunk in texts.chunks(batch) {
        let embedded = embedder.embed(scope, chunk.to_vec()).await?;
        if embedded.len() != chunk.len()
            || embedded
                .iter()
                .any(|vector| vector.len() != dimension || vector.iter().any(|v| !v.is_finite()))
        {
            return Err(MemoryEmbeddingError::new(
                "embedder returned a batch of the wrong shape",
            ));
        }
        vectors.extend(embedded);
    }
    Ok(vectors)
}
