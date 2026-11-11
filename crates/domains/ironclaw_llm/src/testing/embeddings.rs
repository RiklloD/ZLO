//! Deterministic, offline [`EmbeddingProvider`] for tests.
//!
//! Each lowercase word hashes into one of `dimension` buckets; words declared
//! in the same *concept group* share a bucket, so a test can make "car" and
//! "automobile" semantically close while full-text search sees two unrelated
//! terms. Vectors are L2-normalized bag-of-buckets counts.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::embeddings::{
    EmbeddingError, EmbeddingLimits, EmbeddingProvider, EmbeddingRequest, EmbeddingResponse,
    EmbeddingUsage,
};

/// Offline embedder with configurable synonym groups. Records every batch it
/// was asked to embed so tests can assert on the inputs.
pub struct ConceptEmbeddings {
    model: String,
    dimension: u32,
    concepts: HashMap<String, usize>,
    calls: Mutex<Vec<Vec<String>>>,
    fail: bool,
}

impl ConceptEmbeddings {
    pub fn new(dimension: u32) -> Self {
        Self {
            model: "test-concept-embedder".to_string(),
            dimension: dimension.max(1),
            concepts: HashMap::new(),
            calls: Mutex::new(Vec::new()),
            fail: false,
        }
    }

    /// Words in one group map to the same bucket.
    pub fn with_concept<const N: usize>(mut self, words: [&str; N]) -> Self {
        let bucket = self.concepts.len() % self.dimension as usize;
        for word in words {
            self.concepts.insert(word.to_ascii_lowercase(), bucket);
        }
        self
    }

    /// Every call fails with a transport error (exercises degradation).
    pub fn failing(mut self) -> Self {
        self.fail = true;
        self
    }

    /// Batches embedded so far.
    pub fn calls(&self) -> Vec<Vec<String>> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn bucket(&self, word: &str) -> usize {
        if let Some(bucket) = self.concepts.get(word) {
            return *bucket;
        }
        // Words outside concept groups land after the reserved concept
        // buckets so they never collide with a declared concept.
        let reserved = self.concepts.values().max().map_or(0, |max| max + 1);
        let free = (self.dimension as usize).saturating_sub(reserved).max(1);
        let hash = word.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        });
        (reserved + (hash % free as u64) as usize) % self.dimension as usize
    }

    /// The vector for one text.
    pub fn vector(&self, text: &str) -> Vec<f32> {
        let mut vector = vec![0.0_f32; self.dimension as usize];
        for word in text
            .split(|ch: char| !ch.is_alphanumeric())
            .filter(|word| !word.is_empty())
        {
            vector[self.bucket(&word.to_lowercase())] += 1.0;
        }
        let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm > 0.0 {
            for value in &mut vector {
                *value /= norm;
            }
        } else {
            vector[0] = 1.0;
        }
        vector
    }
}

#[async_trait]
impl EmbeddingProvider for ConceptEmbeddings {
    fn model_name(&self) -> &str {
        &self.model
    }

    fn dimension(&self) -> u32 {
        self.dimension
    }

    fn limits(&self) -> EmbeddingLimits {
        EmbeddingLimits::default()
    }

    async fn embed(&self, request: EmbeddingRequest) -> Result<EmbeddingResponse, EmbeddingError> {
        request.validate(self.limits())?;
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.inputs.clone());
        if self.fail {
            return Err(EmbeddingError::Transport {
                reason: "test embedder configured to fail".to_string(),
            });
        }
        let embeddings = request
            .inputs
            .iter()
            .map(|input| self.vector(input))
            .collect();
        let tokens = request
            .inputs
            .iter()
            .map(|input| input.split_whitespace().count() as u64)
            .sum();
        Ok(EmbeddingResponse {
            embeddings,
            usage: EmbeddingUsage {
                prompt_tokens: tokens,
                total_tokens: tokens,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concept_words_are_identical_and_unrelated_words_differ() {
        let embedder = ConceptEmbeddings::new(16).with_concept(["car", "automobile"]);
        assert_eq!(embedder.vector("car"), embedder.vector("automobile"));
        assert_ne!(embedder.vector("car"), embedder.vector("banana"));
    }
}
