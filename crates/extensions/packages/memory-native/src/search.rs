//! Memory search request/result types and result ranking: full text alone,
//! or full text fused with a vector branch by reciprocal rank.

use ironclaw_host_api::error::HostApiError;

use ironclaw_memory::MemoryDocumentPath;

/// Upper bound on the requested final result count. Keeps a faulty caller
/// from translating `usize::MAX` into an unbounded SQL `LIMIT`.
const MAX_LIMIT: usize = 1_000;

/// Upper bound on the chunk candidate budget before ranking. 5x the
/// final-limit ceiling leaves headroom for several chunks of one document to
/// collapse into a single result without letting an attacker request millions
/// of rows.
const MAX_PRE_FUSION_LIMIT: usize = 5_000;

/// Reciprocal-rank constant: a chunk at rank `r` contributes `1 / (RANK_K + r)`.
const RANK_K: f32 = 60.0;

/// Search request passed to memory backends that expose search APIs.
#[derive(Debug, Clone, PartialEq)]
pub struct MemorySearchRequest {
    query: String,
    limit: usize,
    pre_fusion_limit: usize,
    /// The query's embedding, set only when an embedder is configured. When
    /// present the repository also runs a vector branch and fuses it with full
    /// text; when absent search is full-text only.
    query_embedding: Option<Vec<f32>>,
}

impl MemorySearchRequest {
    pub fn new(query: impl Into<String>) -> Result<Self, HostApiError> {
        let query = query.into();
        if query.trim().is_empty() {
            return Err(HostApiError::InvalidId {
                kind: "memory search query",
                value: query,
                reason: "query must not be empty".to_string(),
            });
        }
        Ok(Self {
            query,
            limit: 20,
            pre_fusion_limit: 50,
            query_embedding: None,
        })
    }

    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit.clamp(1, MAX_LIMIT);
        // Re-clamp pre_fusion_limit so the invariant `pre_fusion_limit >= limit`
        // holds regardless of builder call order: if the caller sets
        // `with_pre_fusion_limit(2).with_limit(5)`, the candidate budget must
        // also widen to at least 5, otherwise the SQL `LIMIT` would silently
        // be smaller than the requested final limit. Also keep the upper
        // DB-safety bound on pre_fusion_limit.
        self.pre_fusion_limit = self
            .pre_fusion_limit
            .max(self.limit)
            .min(MAX_PRE_FUSION_LIMIT);
        self
    }

    pub fn with_pre_fusion_limit(mut self, limit: usize) -> Self {
        // `pre_fusion_limit >= limit` (lower bound) and an upper DB-safety
        // ceiling so a `usize::MAX` from a faulty caller can't translate
        // into an unbounded SQL `LIMIT`.
        self.pre_fusion_limit = limit.max(self.limit).clamp(1, MAX_PRE_FUSION_LIMIT);
        self
    }

    /// Add a vector branch. An empty or non-finite embedding is ignored, so a
    /// broken query vector degrades to full text rather than failing search.
    pub fn with_query_embedding(mut self, embedding: Vec<f32>) -> Self {
        if !embedding.is_empty() && embedding.iter().all(|value| value.is_finite()) {
            self.query_embedding = Some(embedding);
        }
        self
    }

    pub fn query_embedding(&self) -> Option<&[f32]> {
        self.query_embedding.as_deref()
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    pub fn pre_fusion_limit(&self) -> usize {
        self.pre_fusion_limit
    }
}

/// Search result returned by memory backends that expose search APIs.
#[derive(Debug, Clone, PartialEq)]
pub struct MemorySearchResult {
    pub path: MemoryDocumentPath,
    pub score: f32,
    pub snippet: String,
    pub full_text_rank: Option<u32>,
    /// Best rank of this document's chunks in the vector branch; `None` when
    /// no vector branch ran or none of its chunks ranked.
    pub vector_rank: Option<u32>,
}

impl MemorySearchResult {
    /// Matched by both full text and vector similarity.
    pub fn is_hybrid(&self) -> bool {
        self.full_text_rank.is_some() && self.vector_rank.is_some()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RankedMemorySearchResult {
    pub(crate) path: MemoryDocumentPath,
    pub(crate) snippet: String,
    pub(crate) rank: u32,
}

/// Collapse ranked full-text chunk hits into one result per document, scored
/// by summed reciprocal rank and normalized so the best document scores `1.0`.
#[cfg(test)]
pub(crate) fn rank_memory_search_results(
    chunk_hits: Vec<RankedMemorySearchResult>,
    limit: usize,
) -> Vec<MemorySearchResult> {
    fuse_memory_search_results(chunk_hits, Vec::new(), limit)
}

#[derive(Clone, Copy)]
enum Branch {
    FullText,
    Vector,
}

/// Fuse full-text and vector chunk hits into one result per document by
/// reciprocal rank: every chunk hit contributes `1 / (RANK_K + rank)` from
/// whichever branch ranked it, and the sum is normalized so the best document
/// scores `1.0`. With no vector hits this is exactly the full-text ranking.
/// A document first seen in the vector branch keeps that chunk's snippet; a
/// full-text hit's snippet wins when both branches rank the document.
pub(crate) fn fuse_memory_search_results(
    full_text_hits: Vec<RankedMemorySearchResult>,
    vector_hits: Vec<RankedMemorySearchResult>,
    limit: usize,
) -> Vec<MemorySearchResult> {
    use std::collections::HashMap;

    let mut results = HashMap::<String, MemorySearchResult>::new();
    let hits = full_text_hits
        .into_iter()
        .map(|hit| (Branch::FullText, hit))
        .chain(vector_hits.into_iter().map(|hit| (Branch::Vector, hit)));
    for (branch, hit) in hits {
        let score = 1.0 / (RANK_K + hit.rank as f32);
        let document_key = hit.path.relative_path().to_string();
        let best = |current: Option<u32>| Some(current.map_or(hit.rank, |rank| rank.min(hit.rank)));
        match results.get_mut(&document_key) {
            Some(existing) => {
                existing.score += score;
                match branch {
                    Branch::FullText => existing.full_text_rank = best(existing.full_text_rank),
                    Branch::Vector => existing.vector_rank = best(existing.vector_rank),
                }
            }
            None => {
                let (full_text_rank, vector_rank) = match branch {
                    Branch::FullText => (Some(hit.rank), None),
                    Branch::Vector => (None, Some(hit.rank)),
                };
                results.insert(
                    document_key,
                    MemorySearchResult {
                        path: hit.path,
                        score,
                        snippet: hit.snippet,
                        full_text_rank,
                        vector_rank,
                    },
                );
            }
        }
    }

    let mut ranked = results.into_values().collect::<Vec<_>>();
    if let Some(max_score) = ranked.iter().map(|result| result.score).reduce(f32::max)
        && max_score > 0.0
    {
        for result in &mut ranked {
            result.score /= max_score;
        }
    }
    ranked.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.path.relative_path().cmp(right.path.relative_path()))
    });
    ranked.truncate(limit);
    ranked
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_memory::MemoryDocumentPath;

    fn ranked(relative_path: &str, rank: u32) -> RankedMemorySearchResult {
        RankedMemorySearchResult {
            path: MemoryDocumentPath::new("tenant-a", "alice", None, relative_path).expect("path"),
            snippet: format!("snippet for {relative_path}"),
            rank,
        }
    }

    fn paths(results: &[MemorySearchResult]) -> Vec<String> {
        results
            .iter()
            .map(|result| result.path.relative_path().to_string())
            .collect()
    }

    #[test]
    fn ties_break_deterministically_by_path_ascending() {
        let results = rank_memory_search_results(vec![ranked("z.md", 1), ranked("a.md", 1)], 10);
        assert_eq!(
            paths(&results),
            vec!["a.md".to_string(), "z.md".to_string()],
            "tied scores must sort by path ascending"
        );

        let results = rank_memory_search_results(vec![ranked("a.md", 1), ranked("z.md", 1)], 10);
        assert_eq!(
            paths(&results),
            vec!["a.md".to_string(), "z.md".to_string()],
            "the tiebreak must not depend on insertion order"
        );
    }

    #[test]
    fn scores_are_normalized_and_ordered_by_rank() {
        let results = rank_memory_search_results(
            vec![
                ranked("alpha.md", 1),
                ranked("beta.md", 2),
                ranked("gamma.md", 3),
            ],
            10,
        );
        assert_eq!(
            paths(&results),
            vec![
                "alpha.md".to_string(),
                "beta.md".to_string(),
                "gamma.md".to_string()
            ]
        );
        assert_eq!(results[0].score, 1.0);
        assert!(results[1].score < 1.0 && results[1].score > results[2].score);
        assert!((results[1].score - 61.0 / 62.0).abs() < 1e-6);
        assert!((results[2].score - 61.0 / 63.0).abs() < 1e-6);
    }

    #[test]
    fn multiple_chunks_of_one_document_collapse_into_one_result() {
        let results = rank_memory_search_results(
            vec![
                ranked("other.md", 1),
                ranked("same.md", 2),
                ranked("same.md", 3),
            ],
            10,
        );
        assert_eq!(
            paths(&results),
            vec!["same.md".to_string(), "other.md".to_string()],
            "a document's chunk scores add up and it consumes one result slot"
        );
        assert_eq!(results[0].full_text_rank, Some(2));
        assert_eq!(results[1].full_text_rank, Some(1));
    }

    #[test]
    fn vector_hits_fuse_with_full_text_by_reciprocal_rank() {
        // `both.md` is rank 2 in full text and rank 1 in vector; `text.md`
        // only in full text at rank 1; `meaning.md` only in vector at rank 2.
        let results = fuse_memory_search_results(
            vec![ranked("text.md", 1), ranked("both.md", 2)],
            vec![ranked("both.md", 1), ranked("meaning.md", 2)],
            10,
        );
        assert_eq!(
            paths(&results),
            vec![
                "both.md".to_string(),
                "text.md".to_string(),
                "meaning.md".to_string()
            ]
        );
        assert!(results[0].is_hybrid());
        assert_eq!(results[0].full_text_rank, Some(2));
        assert_eq!(results[0].vector_rank, Some(1));
        assert_eq!(results[0].score, 1.0);
        assert!(!results[1].is_hybrid() && results[1].vector_rank.is_none());
        assert!(results[2].full_text_rank.is_none() && results[2].vector_rank == Some(2));
        // A vector-only document still ranks: semantic recall where full text
        // finds nothing.
        let vector_only = fuse_memory_search_results(Vec::new(), vec![ranked("m.md", 1)], 10);
        assert_eq!(paths(&vector_only), vec!["m.md".to_string()]);
        assert_eq!(vector_only[0].score, 1.0);
    }

    #[test]
    fn no_vector_hits_is_exactly_the_full_text_ranking() {
        let hits = vec![ranked("a.md", 1), ranked("b.md", 2), ranked("a.md", 3)];
        assert_eq!(
            fuse_memory_search_results(hits.clone(), Vec::new(), 10),
            rank_memory_search_results(hits, 10)
        );
    }

    #[test]
    fn non_finite_or_empty_query_embeddings_are_ignored() {
        let request = MemorySearchRequest::new("q").unwrap();
        assert!(
            request
                .clone()
                .with_query_embedding(Vec::new())
                .query_embedding()
                .is_none()
        );
        assert!(
            request
                .clone()
                .with_query_embedding(vec![f32::NAN])
                .query_embedding()
                .is_none()
        );
        assert_eq!(
            request.with_query_embedding(vec![0.5]).query_embedding(),
            Some(&[0.5][..])
        );
    }

    #[test]
    fn results_are_truncated_to_the_limit() {
        let results = rank_memory_search_results(
            vec![
                ranked("alpha.md", 1),
                ranked("beta.md", 2),
                ranked("gamma.md", 3),
            ],
            2,
        );
        assert_eq!(
            paths(&results),
            vec!["alpha.md".to_string(), "beta.md".to_string()]
        );
    }
}
