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
use std::fmt;
use std::ops::Range;
use std::sync::Arc;

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
/// Shortest word prefix projected for search-as-you-type (two characters, so
/// the desktop's two-character queries match partial words too).
const PREFIX_MIN_CHARS: usize = 2;
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
/// The wider fetch used when the first one was saturated and too few of the
/// person's own threads made it into the answer.
pub(crate) const HISTORY_SEARCH_WIDE_CANDIDATES: u32 = 1024;
/// Default snippet width (characters) around the first match.
pub const HISTORY_SNIPPET_CHARS: usize = 220;

/// Ranks thread groups before the answer is cut to `max_threads`: lower
/// tiers come first, relevance orders each tier. The product layer supplies
/// it (automation runs and subagent threads rank below the person's own
/// chats); without one every thread is tier 0.
pub trait ThreadSearchTiering: Send + Sync {
    fn tier(&self, thread: &SessionThreadRecord) -> u8;
}

/// User turns whose text the person never typed: runs the engine started
/// with its own text, by provenance (the orchestrator records them). Their
/// user rows are left out of results; assistant replies stay.
pub trait ExcludedRuns: Send + Sync {
    fn is_excluded(&self, thread: &ThreadId, run_id: &str) -> bool;
}

/// Search request: the caller's scope plus plain user text. The query is
/// plain text, never backend query syntax.
#[derive(Clone)]
pub struct SearchThreadMessagesRequest {
    pub scope: crate::ThreadScope,
    pub query: String,
    /// Search only this thread: candidates are selected inside it, so a
    /// match there is found however well other threads match.
    pub thread_id: Option<ThreadId>,
    /// Thread groups to return; clamped to `1..=HISTORY_SEARCH_MAX_THREADS`.
    pub max_threads: usize,
    /// Hits per thread group; clamped to `1..=HISTORY_SEARCH_MAX_HITS_PER_THREAD`.
    pub max_hits_per_thread: usize,
    pub tiering: Option<Arc<dyn ThreadSearchTiering>>,
    pub excluded_runs: Option<Arc<dyn ExcludedRuns>>,
}

impl SearchThreadMessagesRequest {
    /// A request over the whole scope with default tiering.
    pub fn new(scope: crate::ThreadScope, query: impl Into<String>) -> Self {
        Self {
            scope,
            query: query.into(),
            thread_id: None,
            max_threads: HISTORY_SEARCH_MAX_THREADS,
            max_hits_per_thread: HISTORY_SEARCH_MAX_HITS_PER_THREAD,
            tiering: None,
            excluded_runs: None,
        }
    }

    /// A user row of an excluded (engine-originated) run.
    pub(crate) fn excludes(&self, candidate: &RankedCandidate) -> bool {
        match (&self.excluded_runs, &candidate.run_id) {
            (Some(excluded), Some(run)) if candidate.role == SearchHitRole::User => {
                excluded.is_excluded(&candidate.thread_id, run)
            }
            _ => false,
        }
    }

    pub(crate) fn tier_of(&self, thread: &SessionThreadRecord) -> u8 {
        self.tiering.as_ref().map_or(0, |t| t.tier(thread))
    }
}

impl fmt::Debug for SearchThreadMessagesRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SearchThreadMessagesRequest")
            .field("scope", &self.scope)
            .field("thread_id", &self.thread_id)
            .field("max_threads", &self.max_threads)
            .field("max_hits_per_thread", &self.max_hits_per_thread)
            .field("tiering", &self.tiering.is_some())
            .finish_non_exhaustive()
    }
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
    let role = searchable_role(record)?;
    visible_text(role, record.content.as_deref()?)
}

/// Visible text of a message of `role`: engine scaffolding in user-kind rows
/// is dropped (see `crate::history_visibility`).
pub(crate) fn visible_text(role: SearchHitRole, text: &str) -> Option<&str> {
    let text = match role {
        SearchHitRole::User => crate::history_visibility::visible_user_text(text)?,
        SearchHitRole::Assistant => text,
    }
    .trim();
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
    Some(search_projection_text(searchable_message_text(record)?))
}

/// The indexed projection of already-visible text.
pub(crate) fn search_projection_text(text: &str) -> String {
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
        return head;
    }
    format!("{head}\n{}", prefixes.join(" "))
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

/// A candidate message the backend matched, in backend rank order: a
/// canonical transcript row or an external transcript projection.
pub(crate) struct RankedCandidate {
    pub(crate) thread_id: ThreadId,
    pub(crate) message_id: ThreadMessageId,
    pub(crate) sequence: u64,
    pub(crate) role: SearchHitRole,
    pub(crate) created_at: Option<DateTime<Utc>>,
    /// Visible text, already filtered.
    pub(crate) text: String,
    pub(crate) run_id: Option<String>,
    pub(crate) backend_rank: usize,
}

impl RankedCandidate {
    /// A canonical row, when it is searchable.
    pub(crate) fn from_record(record: &ThreadMessageRecord, backend_rank: usize) -> Option<Self> {
        let role = searchable_role(record)?;
        let text = visible_text(role, record.content.as_deref()?)?.to_string();
        Some(Self {
            thread_id: record.thread_id.clone(),
            message_id: record.message_id,
            sequence: record.sequence,
            role,
            created_at: record.created_at,
            text,
            run_id: record.turn_run_id.clone(),
            backend_rank,
        })
    }

    /// An external projection, re-checked against the visibility rule.
    pub(crate) fn from_external(
        thread_id: ThreadId,
        message: &crate::ExternalTranscriptMessage,
        backend_rank: usize,
    ) -> Option<Self> {
        let text = visible_text(message.role, &message.text)?.to_string();
        Some(Self {
            thread_id,
            message_id: message.navigation_message_id,
            sequence: message.ordinal.saturating_add(1),
            role: message.role,
            created_at: message.created_at,
            text,
            run_id: message.run_id.clone(),
            backend_rank,
        })
    }
}

/// Re-score candidates by term coverage, group by thread, rank groups by
/// tier then relevance, and cut snippets. `threads` supplies the record for
/// each candidate thread; candidates whose thread record is missing are
/// dropped.
pub(crate) fn group_ranked_candidates(
    candidates: Vec<RankedCandidate>,
    threads: &HashMap<ThreadId, SessionThreadRecord>,
    terms: &[String],
    request: &SearchThreadMessagesRequest,
) -> Vec<ThreadSearchResult> {
    struct Scored {
        candidate: RankedCandidate,
        coverage: usize,
    }
    let max_threads = clamp_max_threads(request.max_threads);
    let max_hits_per_thread = clamp_max_hits(request.max_hits_per_thread);
    let mut by_thread: HashMap<ThreadId, Vec<Scored>> = HashMap::new();
    for candidate in candidates {
        if request
            .thread_id
            .as_ref()
            .is_some_and(|only| only != &candidate.thread_id)
        {
            continue;
        }
        if request.excludes(&candidate) {
            continue;
        }
        let coverage = term_coverage(&candidate.text, terms);
        if coverage == 0 {
            continue;
        }
        by_thread
            .entry(candidate.thread_id.clone())
            .or_default()
            .push(Scored {
                candidate,
                coverage,
            });
    }
    let mut groups: Vec<(ThreadSearchResult, u8, usize)> = by_thread
        .into_iter()
        .filter_map(|(thread_id, mut hits)| {
            let thread = threads.get(&thread_id)?.clone();
            hits.sort_by(|a, b| {
                b.coverage
                    .cmp(&a.coverage)
                    .then(a.candidate.backend_rank.cmp(&b.candidate.backend_rank))
                    .then(b.candidate.sequence.cmp(&a.candidate.sequence))
            });
            // One message may appear twice (a canonical row and a projection
            // of the same thread); keep the better-ranked copy.
            let mut seen = HashSet::new();
            hits.retain(|h| seen.insert(h.candidate.message_id));
            let best_coverage = hits.first().map_or(0, |h| h.coverage);
            let best_rank = hits
                .first()
                .map_or(usize::MAX, |h| h.candidate.backend_rank);
            let hits = hits
                .into_iter()
                .take(max_hits_per_thread)
                .map(|scored| {
                    let c = scored.candidate;
                    let (snippet, highlights) =
                        snippet_with_highlights(&c.text, terms, HISTORY_SNIPPET_CHARS);
                    MessageSearchHit {
                        message_id: c.message_id,
                        sequence: c.sequence,
                        role: c.role,
                        created_at: c.created_at,
                        snippet,
                        highlights,
                    }
                })
                .collect();
            let tier = request.tier_of(&thread);
            Some((
                ThreadSearchResult {
                    thread,
                    hits,
                    matched_terms: best_coverage,
                },
                tier,
                best_rank,
            ))
        })
        .collect();
    groups.sort_by(|(a, a_tier, a_rank), (b, b_tier, b_rank)| {
        a_tier
            .cmp(b_tier)
            .then(b.matched_terms.cmp(&a.matched_terms))
            .then(a_rank.cmp(b_rank))
            .then(b.thread.updated_at.cmp(&a.thread.updated_at))
            .then(a.thread.thread_id.as_str().cmp(b.thread.thread_id.as_str()))
    });
    groups
        .into_iter()
        .take(max_threads)
        .map(|(group, _, _)| group)
        .collect()
}

/// Groups of tier 0 in an answer: whether a saturated fetch crowded out the
/// person's own threads.
pub(crate) fn tier_zero_groups(
    groups: &[ThreadSearchResult],
    request: &SearchThreadMessagesRequest,
) -> usize {
    groups
        .iter()
        .filter(|g| request.tier_of(&g.thread) == 0)
        .count()
}

/// The record a hit in an external-only thread groups under: the caller's
/// scope (the projection was stored under it) plus the ingested summary.
pub(crate) fn external_thread_record(
    scope: &crate::ThreadScope,
    summary: &crate::ExternalThreadSummary,
) -> SessionThreadRecord {
    SessionThreadRecord {
        scope: scope.clone(),
        thread_id: summary.thread_id.clone(),
        created_by_actor_id: scope
            .owner_user_id
            .as_ref()
            .map(|u| u.as_str().to_string())
            .unwrap_or_default(),
        title: summary.title.clone(),
        metadata_json: None,
        goal: None,
        created_at: summary.created_at,
        updated_at: summary.updated_at,
    }
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
