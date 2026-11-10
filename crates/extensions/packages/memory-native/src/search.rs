//! Memory search request/result types and full-text result ranking.

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
}

#[derive(Debug, Clone)]
pub(crate) struct RankedMemorySearchResult {
    pub(crate) path: MemoryDocumentPath,
    pub(crate) snippet: String,
    pub(crate) rank: u32,
}

/// Collapse ranked chunk hits into one result per document, scored by summed
/// reciprocal rank and normalized so the best document scores `1.0`.
pub(crate) fn rank_memory_search_results(
    chunk_hits: Vec<RankedMemorySearchResult>,
    limit: usize,
) -> Vec<MemorySearchResult> {
    use std::collections::HashMap;

    let mut results = HashMap::<String, MemorySearchResult>::new();
    for hit in chunk_hits {
        let score = 1.0 / (RANK_K + hit.rank as f32);
        let document_key = hit.path.relative_path().to_string();
        results
            .entry(document_key)
            .and_modify(|existing| {
                existing.score += score;
                existing.full_text_rank = Some(
                    existing
                        .full_text_rank
                        .map_or(hit.rank, |rank| rank.min(hit.rank)),
                );
            })
            .or_insert(MemorySearchResult {
                path: hit.path,
                score,
                snippet: hit.snippet,
                full_text_rank: Some(hit.rank),
            });
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
