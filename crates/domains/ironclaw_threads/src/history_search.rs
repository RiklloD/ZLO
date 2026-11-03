//! Full-text search over transcript message text (history search).
//!
//! The searchable projection of a message is its visible conversation text:
//! user messages and finalized/interrupted assistant replies. System text,
//! summaries, checkpoints, tool-result references and capability previews are
//! never indexed, so raw tool output and hidden context stay out of results.
//! Redacted/deleted/superseded rows project nothing.
//!
//! The filesystem store writes the projection into the message entry's
//! indexed map (`search_text`) and declares one `IndexKind::Fts` index at the
//! `/threads` root, so every backend (`fts5` on libSQL, `tsvector` on
//! PostgreSQL, the in-memory reference) serves the same ranked-OR query. To
//! make search-as-you-type work on word-level tokenizers, the projection
//! appends the word prefixes of every long word ("deployment" also indexes
//! `dep`, `depl`, …); the displayed snippet is always cut from the original
//! text, never from the projection.
//!
//! Ranking and grouping are backend-independent and happen here: hits are
//! re-scored by query-term coverage (then backend rank), grouped by thread,
//! and each hit carries a snippet with highlighted byte ranges.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use chrono::{DateTime, Utc};
use ironclaw_host_api::ids::ThreadId;
use serde::{Deserialize, Serialize};

use crate::{
    MessageKind, MessageStatus, SessionThreadRecord, ThreadMessageId, ThreadMessageRecord,
};

/// Indexed key carrying the searchable projection on message entries.
pub(crate) const SEARCH_TEXT_INDEX_KEY: &str = "search_text";
/// FTS index declared at the `/threads` alias root.
pub(crate) const SEARCH_TEXT_INDEX_NAME: &str = "thread_message_search_v1";

/// Longest stretch of one message's text that is indexed. Longer replies are
/// indexed on their head; the transcript itself is untouched.
pub const MESSAGE_SEARCH_TEXT_MAX_CHARS: usize = 32_000;
/// Shortest word prefix projected for search-as-you-type.
const PREFIX_MIN_CHARS: usize = 3;
/// Longest word prefix projected; longer prefixes add little recall.
const PREFIX_MAX_CHARS: usize = 12;
/// Upper bound on distinct prefix tokens projected for one message.
const PREFIX_MAX_TOKENS: usize = 4_000;
/// Upper bound on accepted query length (characters).
pub const HISTORY_SEARCH_QUERY_MAX_CHARS: usize = 512;
/// Upper bound on thread groups one search returns.
pub const HISTORY_SEARCH_MAX_THREADS: usize = 50;
/// Upper bound on hits returned per thread group.
pub const HISTORY_SEARCH_MAX_HITS_PER_THREAD: usize = 5;
/// Ranked candidate messages fetched from the index before grouping.
pub(crate) const HISTORY_SEARCH_CANDIDATES: u32 = 400;
/// Default snippet width (characters) around the first match.
pub const HISTORY_SNIPPET_CHARS: usize = 220;

/// Search request: the caller's scope plus plain user text. The query is
/// plain text, never backend query syntax.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchThreadMessagesRequest {
    pub scope: crate::ThreadScope,
    pub query: String,
    /// Thread groups to return; clamped to `1..=HISTORY_SEARCH_MAX_THREADS`.
    pub max_threads: usize,
    /// Hits per thread group; clamped to `1..=HISTORY_SEARCH_MAX_HITS_PER_THREAD`.
    pub max_hits_per_thread: usize,
}

/// Search answer, best thread first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchThreadMessagesResponse {
    pub threads: Vec<ThreadSearchResult>,
    /// `false` while older transcripts are still being backfilled into the
    /// index; results then cover only already-indexed messages.
    pub index_complete: bool,
}

/// One thread's hits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSearchResult {
    pub thread: SessionThreadRecord,
    /// Best hit first.
    pub hits: Vec<MessageSearchHit>,
    /// Distinct query terms the best hit carries (coverage score).
    pub matched_terms: usize,
}

/// Who wrote a matched message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchHitRole {
    User,
    Assistant,
}

/// One matched message with a highlighted snippet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageSearchHit {
    pub message_id: ThreadMessageId,
    pub sequence: u64,
    pub role: SearchHitRole,
    pub created_at: Option<DateTime<Utc>>,
    pub snippet: String,
    /// UTF-8 byte ranges into `snippet` that matched a query term.
    pub highlights: Vec<Range<usize>>,
}

/// Visible conversation text of a message, or `None` when the message is not
/// searchable (tool output, system/hidden text, drafts, redacted rows).
pub fn searchable_message_text(record: &ThreadMessageRecord) -> Option<&str> {
    searchable_role(record)?;
    let text = record.content.as_deref()?.trim();
    (!text.is_empty()).then_some(text)
}

fn searchable_role(record: &ThreadMessageRecord) -> Option<SearchHitRole> {
    if record.redaction_ref.is_some() {
        return None;
    }
    match (record.kind, record.status) {
        (
            MessageKind::User,
            MessageStatus::Accepted
            | MessageStatus::Queued
            | MessageStatus::Submitted
            | MessageStatus::Finalized,
        ) => Some(SearchHitRole::User),
        (MessageKind::Assistant, MessageStatus::Finalized | MessageStatus::Interrupted) => {
            Some(SearchHitRole::Assistant)
        }
        _ => None,
    }
}

/// The indexed projection: bounded visible text plus word prefixes. `None`
/// when the message is not searchable.
pub(crate) fn search_projection(record: &ThreadMessageRecord) -> Option<String> {
    let text = searchable_message_text(record)?;
    let head: String = text.chars().take(MESSAGE_SEARCH_TEXT_MAX_CHARS).collect();
    let mut seen = HashSet::new();
    let mut prefixes = Vec::new();
    'words: for word in words(&head) {
        let lower = word.to_lowercase();
        let chars: Vec<char> = lower.chars().collect();
        if chars.len() <= PREFIX_MIN_CHARS {
            continue;
        }
        let longest = (chars.len() - 1).min(PREFIX_MAX_CHARS);
        for len in PREFIX_MIN_CHARS..=longest {
            let prefix: String = chars[..len].iter().collect();
            if seen.insert(prefix.clone()) {
                prefixes.push(prefix);
                if prefixes.len() >= PREFIX_MAX_TOKENS {
                    break 'words;
                }
            }
        }
    }
    if prefixes.is_empty() {
        return Some(head);
    }
    Some(format!("{head}\n{}", prefixes.join(" ")))
}

fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
}

/// Lowercased content terms of a plain-text query, deduplicated, in order.
pub fn history_query_terms(query: &str) -> Vec<String> {
    let bounded: String = query.chars().take(HISTORY_SEARCH_QUERY_MAX_CHARS).collect();
    let mut seen = HashSet::new();
    words(&bounded)
        .map(str::to_lowercase)
        .filter(|term| seen.insert(term.clone()))
        .collect()
}

/// Byte ranges in `text` where a word starts with one of `terms`
/// (case-insensitive), covering the matched prefix only.
fn match_ranges(text: &str, terms: &[String]) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut word_start: Option<usize> = None;
    for (index, ch) in text.char_indices() {
        if ch.is_alphanumeric() {
            if word_start.is_none() {
                word_start = Some(index);
            }
        } else if let Some(start) = word_start.take() {
            push_word_match(text, start, index, terms, &mut ranges);
        }
    }
    if let Some(start) = word_start {
        push_word_match(text, start, text.len(), terms, &mut ranges);
    }
    ranges
}

fn push_word_match(
    text: &str,
    start: usize,
    end: usize,
    terms: &[String],
    ranges: &mut Vec<Range<usize>>,
) {
    let word = &text[start..end];
    let lower = word.to_lowercase();
    // The longest matching term wins so overlapping terms highlight once.
    let best = terms
        .iter()
        .filter(|term| lower.starts_with(term.as_str()))
        .max_by_key(|term| term.len());
    if let Some(term) = best {
        let term_chars = term.chars().count();
        let end_byte = word
            .char_indices()
            .nth(term_chars)
            .map_or(end, |(offset, _)| start + offset);
        ranges.push(start..end_byte);
    }
}

/// Distinct query terms that start a word of `text`.
pub fn term_coverage(text: &str, terms: &[String]) -> usize {
    let lower = text.to_lowercase();
    let word_set: Vec<&str> = words(&lower).collect();
    terms
        .iter()
        .filter(|term| word_set.iter().any(|w| w.starts_with(term.as_str())))
        .count()
}

/// Cut a window of about `width` characters around the first match and
/// return it with highlight ranges relative to the snippet. Ellipses mark
/// elided text; truncation never splits a character.
pub fn snippet_with_highlights(
    text: &str,
    terms: &[String],
    width: usize,
) -> (String, Vec<Range<usize>>) {
    let collapsed = collapse_whitespace(text);
    let ranges = match_ranges(&collapsed, terms);
    let total_chars = collapsed.chars().count();
    let first_match_char = ranges
        .first()
        .map_or(0, |r| collapsed[..r.start].chars().count());
    let lead = width / 4;
    let start_char = if total_chars <= width {
        0
    } else {
        first_match_char
            .saturating_sub(lead)
            .min(total_chars.saturating_sub(width))
    };
    let end_char = (start_char + width).min(total_chars);
    let start_byte = char_to_byte(&collapsed, start_char);
    let end_byte = char_to_byte(&collapsed, end_char);
    let mut snippet = String::new();
    let prefix_len = if start_byte > 0 {
        snippet.push('…');
        '…'.len_utf8()
    } else {
        0
    };
    snippet.push_str(&collapsed[start_byte..end_byte]);
    if end_byte < collapsed.len() {
        snippet.push('…');
    }
    let highlights = ranges
        .into_iter()
        .filter(|r| r.start >= start_byte && r.end <= end_byte)
        .map(|r| (r.start - start_byte + prefix_len)..(r.end - start_byte + prefix_len))
        .collect();
    (snippet, highlights)
}

fn char_to_byte(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map_or(text.len(), |(offset, _)| offset)
}

fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(ch);
        }
    }
    out
}

/// A candidate message the backend matched, in backend rank order.
pub(crate) struct RankedCandidate {
    pub(crate) record: ThreadMessageRecord,
    pub(crate) backend_rank: usize,
}

/// Re-score candidates by term coverage, group by thread, and cut snippets.
/// `threads` supplies the record for each candidate thread; candidates whose
/// thread record is missing are dropped. Subagent threads are kept — ranking
/// them below the person's own threads is the product layer's call.
pub(crate) fn group_ranked_candidates(
    candidates: Vec<RankedCandidate>,
    threads: &HashMap<ThreadId, SessionThreadRecord>,
    terms: &[String],
    max_threads: usize,
    max_hits_per_thread: usize,
) -> Vec<ThreadSearchResult> {
    struct Scored {
        record: ThreadMessageRecord,
        role: SearchHitRole,
        coverage: usize,
        backend_rank: usize,
    }
    let mut by_thread: HashMap<ThreadId, Vec<Scored>> = HashMap::new();
    for candidate in candidates {
        let Some(role) = searchable_role(&candidate.record) else {
            continue;
        };
        let Some(text) = searchable_message_text(&candidate.record) else {
            continue;
        };
        let coverage = term_coverage(text, terms);
        if coverage == 0 {
            continue;
        }
        by_thread
            .entry(candidate.record.thread_id.clone())
            .or_default()
            .push(Scored {
                record: candidate.record,
                role,
                coverage,
                backend_rank: candidate.backend_rank,
            });
    }
    let mut groups: Vec<(ThreadSearchResult, usize)> = by_thread
        .into_iter()
        .filter_map(|(thread_id, mut hits)| {
            let thread = threads.get(&thread_id)?.clone();
            hits.sort_by(|a, b| {
                b.coverage
                    .cmp(&a.coverage)
                    .then(a.backend_rank.cmp(&b.backend_rank))
                    .then(b.record.sequence.cmp(&a.record.sequence))
            });
            let best_coverage = hits.first().map_or(0, |h| h.coverage);
            let best_rank = hits.first().map_or(usize::MAX, |h| h.backend_rank);
            let hits = hits
                .into_iter()
                .take(max_hits_per_thread)
                .map(|scored| {
                    let text = searchable_message_text(&scored.record).unwrap_or_default();
                    let (snippet, highlights) =
                        snippet_with_highlights(text, terms, HISTORY_SNIPPET_CHARS);
                    MessageSearchHit {
                        message_id: scored.record.message_id,
                        sequence: scored.record.sequence,
                        role: scored.role,
                        created_at: scored.record.created_at,
                        snippet,
                        highlights,
                    }
                })
                .collect();
            Some((
                ThreadSearchResult {
                    thread,
                    hits,
                    matched_terms: best_coverage,
                },
                best_rank,
            ))
        })
        .collect();
    groups.sort_by(|(a, a_rank), (b, b_rank)| {
        b.matched_terms
            .cmp(&a.matched_terms)
            .then(a_rank.cmp(b_rank))
            .then(b.thread.updated_at.cmp(&a.thread.updated_at))
            .then(a.thread.thread_id.as_str().cmp(b.thread.thread_id.as_str()))
    });
    groups
        .into_iter()
        .take(max_threads)
        .map(|(group, _)| group)
        .collect()
}

pub(crate) fn clamp_max_threads(requested: usize) -> usize {
    requested.clamp(1, HISTORY_SEARCH_MAX_THREADS)
}

pub(crate) fn clamp_max_hits(requested: usize) -> usize {
    requested.clamp(1, HISTORY_SEARCH_MAX_HITS_PER_THREAD)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn terms(q: &str) -> Vec<String> {
        history_query_terms(q)
    }

    #[test]
    fn query_terms_are_lowercased_and_deduplicated() {
        assert_eq!(
            terms("Deploy the DEPLOY, staging!"),
            ["deploy", "the", "staging"]
        );
    }

    #[test]
    fn prefix_terms_match_longer_words() {
        assert_eq!(
            term_coverage("We shipped the deployment", &terms("deplo")),
            1
        );
        assert_eq!(term_coverage("We shipped it", &terms("deplo")), 0);
    }

    #[test]
    fn snippet_highlights_point_at_matches_after_ellipsis() {
        let text = format!(
            "{} needle here {}",
            "word ".repeat(100),
            "tail ".repeat(100)
        );
        let (snippet, highlights) = snippet_with_highlights(&text, &terms("needle"), 60);
        assert!(snippet.starts_with('…'));
        assert!(snippet.ends_with('…'));
        assert_eq!(highlights.len(), 1);
        assert_eq!(&snippet[highlights[0].clone()], "needle");
    }

    #[test]
    fn snippet_never_splits_multibyte_characters() {
        let text = format!("{}ünïcødé match {}", "é".repeat(300), "ß".repeat(300));
        let (snippet, highlights) = snippet_with_highlights(&text, &terms("match"), 40);
        assert_eq!(&snippet[highlights[0].clone()], "match");
    }

    #[test]
    fn highlight_covers_only_the_matched_prefix() {
        let (snippet, highlights) =
            snippet_with_highlights("Deployment done", &terms("deplo"), 100);
        assert_eq!(&snippet[highlights[0].clone()], "Deplo");
    }
}
