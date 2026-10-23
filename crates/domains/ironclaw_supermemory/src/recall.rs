//! Automatic recall: what is fetched before a turn and how it is rendered.
//!
//! Matches the Hermes plugin's `prefetch`: the profile endpoint on the primary
//! container, queried with the first 200 characters of the user message,
//! yields persistent profile facts, recent context and relevant memories. Each
//! read container adds a small semantic search with the same query. Lists are
//! deduplicated across sections (earlier sections win), capped per section,
//! and rendered with Hermes' `[3h ago] [87%]` hit annotations.

use chrono::{DateTime, Datelike, Utc};

use crate::binding::ContainerTag;
use crate::wire::{WireMemory, WireProfile};

/// Characters of the user message sent as the recall query (Hermes `query[:200]`).
pub(crate) const RECALL_QUERY_CHARS: usize = 200;
/// Hits per read container; the primary container gets the full list.
pub(crate) const READ_CONTAINER_RECALL_LIMIT: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecallKind {
    /// `profile.static`: durable facts about the user.
    ProfileFact,
    /// `profile.dynamic`: what the user has been doing lately.
    RecentContext,
    /// A semantic search hit.
    Memory,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecallItem {
    pub kind: RecallKind,
    pub container: ContainerTag,
    pub text: String,
    /// 0..1 similarity for search hits.
    pub similarity: Option<f64>,
    /// RFC 3339 timestamp of the memory's last update.
    pub updated_at: Option<String>,
}

impl RecallItem {
    /// One prompt line, e.g. `Memory [3h ago] [87%]: prefers tabs`.
    pub fn render(&self, primary: &ContainerTag, now: DateTime<Utc>) -> String {
        let label = match self.kind {
            RecallKind::ProfileFact => "Profile",
            RecallKind::RecentContext => "Recent context",
            RecallKind::Memory => "Memory",
        };
        let mut tags = Vec::new();
        if &self.container != primary {
            tags.push(format!("[{}]", self.container));
        }
        if let Some(relative) = self
            .updated_at
            .as_deref()
            .and_then(|stamp| relative_time(stamp, now))
        {
            tags.push(format!("[{relative}]"));
        }
        if let Some(similarity) = self.similarity {
            tags.push(format!("[{}%]", (similarity * 100.0).round() as i64));
        }
        if tags.is_empty() {
            format!("{label}: {}", self.text)
        } else {
            format!("{label} {}: {}", tags.join(" "), self.text)
        }
    }
}

/// First [`RECALL_QUERY_CHARS`] characters, trimmed; `None` when blank.
pub(crate) fn recall_query(message: &str) -> Option<String> {
    let query: String = message.trim().chars().take(RECALL_QUERY_CHARS).collect();
    let query = query.trim().to_string();
    (!query.is_empty()).then_some(query)
}

/// Merge the primary profile and the read-container hits into one ordered,
/// deduplicated list: profile facts, recent context, primary memories, then
/// read-container memories by similarity. Each section keeps `max_per_section`.
pub(crate) fn merge(
    primary: &ContainerTag,
    profile: WireProfile,
    read_hits: Vec<(ContainerTag, Vec<WireMemory>)>,
    max_per_section: usize,
) -> Vec<RecallItem> {
    let mut seen = std::collections::HashSet::new();
    let mut items = Vec::new();
    let mut section = |items: &mut Vec<RecallItem>, candidates: Vec<RecallItem>| {
        let kept = candidates
            .into_iter()
            .filter(|item| seen.insert(item.text.to_lowercase()))
            .take(max_per_section);
        items.extend(kept);
    };
    let fact = |kind, text: String| RecallItem {
        kind,
        container: primary.clone(),
        text,
        similarity: None,
        updated_at: None,
    };
    let hit = |container: &ContainerTag, memory: WireMemory| RecallItem {
        kind: RecallKind::Memory,
        container: container.clone(),
        text: memory.text,
        similarity: memory.similarity,
        updated_at: memory.updated_at,
    };
    section(
        &mut items,
        profile
            .static_facts
            .into_iter()
            .map(|text| fact(RecallKind::ProfileFact, text))
            .collect(),
    );
    section(
        &mut items,
        profile
            .dynamic_facts
            .into_iter()
            .map(|text| fact(RecallKind::RecentContext, text))
            .collect(),
    );
    section(
        &mut items,
        profile
            .search
            .into_iter()
            .map(|memory| hit(primary, memory))
            .collect(),
    );
    let mut others: Vec<RecallItem> = read_hits
        .into_iter()
        .flat_map(|(container, hits)| {
            hits.into_iter()
                .map(|memory| hit(&container, memory))
                .collect::<Vec<_>>()
        })
        .collect();
    others.sort_by(|a, b| {
        b.similarity
            .unwrap_or(0.0)
            .total_cmp(&a.similarity.unwrap_or(0.0))
    });
    section(&mut items, others);
    items
}

/// Hermes `_format_relative_time`: `just now`, `5m ago`, `3h ago`, `2d ago`,
/// then `07 Mar` (this year) or `07 Mar 2025`.
pub(crate) fn relative_time(stamp: &str, now: DateTime<Utc>) -> Option<String> {
    let then = DateTime::parse_from_rfc3339(stamp.trim())
        .ok()?
        .with_timezone(&Utc);
    let seconds = (now - then).num_seconds().max(0);
    Some(match seconds {
        0..1800 => "just now".to_string(),
        1800..3600 => format!("{}m ago", seconds / 60),
        3600..86_400 => format!("{}h ago", seconds / 3600),
        86_400..604_800 => format!("{}d ago", seconds / 86_400),
        _ if then.year() == now.year() => then.format("%d %b").to_string(),
        _ => then.format("%d %b %Y").to_string(),
    })
}
