//! What fills a thread's context window.
//!
//! A [`ContextSample`] holds estimated token counts of one prompt, by
//! category and by tool owner. Native IronClaw-loop threads sample the real
//! prompt assembly at every model call; CLI threads are sampled on demand
//! from what ZLO injects plus the transcript. [`reconcile`] then fits the
//! sample to the provider's exact count when there is one, so the categories
//! of a report always sum to its `used` figure. The latest sample of each
//! thread is kept in `orch_context`, keyed by the turn it measured.

use ironclaw_host_api::ids::ThreadId;
use ironclaw_product_contracts::inbox::GoalStatus;
use ironclaw_product_contracts::orchestration::{
    ContextCategory, ContextCategoryUsage, ContextLevel, ContextSource, ContextToolOwner,
    ContextToolOwnerUsage, ThreadContextReport,
};
use libsql::Value;
use serde::{Deserialize, Serialize};

use crate::engine::{Engine, Result};
use crate::model::{Millis, ThreadRecord};
use crate::runtime::{HistoryMessage, HistoryRole};
use crate::store::{Cols, Store, s};

pub(crate) const CONTEXT_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS orch_context (
  thread_id TEXT PRIMARY KEY,
  doc TEXT NOT NULL,
  updated_at INTEGER NOT NULL
);
"#;

/// Tokens a provider charges for one image, roughly (Anthropic's cap is
/// ~1.6k for a full-size image; OpenAI's high-detail tiles land nearby).
pub const IMAGE_TOKENS: u64 = 1_600;

/// Capability-id prefixes of tools IronClaw itself serves.
const BUILTIN_PREFIXES: &[&str] = &[
    "builtin",
    "ironclaw",
    "orchestrator",
    "memory",
    "supermemory",
    "computer_use",
    "loop",
    "runtime",
];

/// Character-heuristic token estimate: ~4 ASCII characters per token, and a
/// token per non-ASCII character (CJK, emoji and accented text tokenize far
/// denser than English).
pub fn estimate_tokens(text: &str) -> u64 {
    let units = text.chars().fold(0_u64, |total, c| {
        total.saturating_add(if c.is_ascii() { 1 } else { 4 })
    });
    units.div_ceil(4)
}

/// Who serves a tool, from its capability id (`<owner>.<tool>`).
pub fn tool_owner(capability_id: &str) -> ContextToolOwner {
    let prefix = capability_id
        .split_once('.')
        .map_or(capability_id, |(owner, _)| owner);
    if BUILTIN_PREFIXES.contains(&prefix) {
        return ContextToolOwner::Builtin;
    }
    let mcp = prefix
        .strip_prefix("mcp-")
        .or_else(|| prefix.strip_prefix("mcp_"));
    match mcp {
        Some(server) if !server.is_empty() => ContextToolOwner::Mcp(server.to_string()),
        _ if prefix == "mcp" => ContextToolOwner::Mcp(prefix.to_string()),
        _ => ContextToolOwner::Extension(prefix.to_string()),
    }
}

/// Estimated token counts of one prompt.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextSample {
    pub categories: Vec<ContextCategoryUsage>,
    /// Tool definitions by owner; they sum to the `tool_definitions` category.
    pub tools: Vec<ContextToolOwnerUsage>,
    /// The model's window as the runtime knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<u64>,
    /// The turn this sample measured: a native run id, or the telemetry
    /// timestamp a CLI estimate was taken at. A report reuses the stored
    /// sample while the key still matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_key: Option<String>,
}

impl ContextSample {
    pub fn add(&mut self, category: ContextCategory, tokens: u64) {
        if tokens == 0 {
            return;
        }
        match self.categories.iter_mut().find(|c| c.category == category) {
            Some(entry) => entry.tokens = entry.tokens.saturating_add(tokens),
            None => {
                self.categories
                    .push(ContextCategoryUsage { category, tokens });
                self.categories.sort_by_key(|c| c.category);
            }
        }
    }

    pub fn add_text(&mut self, category: ContextCategory, text: &str) {
        self.add(category, estimate_tokens(text));
    }

    /// One tool definition of `chars` characters.
    pub fn add_tool(&mut self, capability_id: &str, chars: u64) {
        let tokens = chars.div_ceil(4).max(1);
        self.add(ContextCategory::ToolDefinitions, tokens);
        let owner = tool_owner(capability_id);
        match self.tools.iter_mut().find(|t| t.owner == owner) {
            Some(entry) => {
                entry.tokens = entry.tokens.saturating_add(tokens);
                entry.tools = entry.tools.saturating_add(1);
            }
            None => self.tools.push(ContextToolOwnerUsage {
                owner,
                tokens,
                tools: 1,
            }),
        }
    }

    pub fn tokens(&self, category: ContextCategory) -> u64 {
        self.categories
            .iter()
            .find(|c| c.category == category)
            .map_or(0, |c| c.tokens)
    }

    pub fn total(&self) -> u64 {
        self.categories
            .iter()
            .fold(0, |sum, c| sum.saturating_add(c.tokens))
    }

    /// The transcript part of a CLI thread's prompt: everything since its
    /// latest compaction (the CLI drops what came before), hidden scaffolding
    /// (a compaction's summary, a context handoff) counted as summaries.
    ///
    /// `input_only` measures what the latest model call read, to match a
    /// provider count of that call's input: the turn's final reply was the
    /// call's output, so it is left out. Without a provider count the
    /// estimate is the thread's occupancy, reply included.
    pub fn add_history(&mut self, messages: &[HistoryMessage], input_only: bool) {
        let start = messages
            .iter()
            .rposition(is_compaction_marker)
            .map_or(0, |i| i + 1);
        let mut end = messages.len();
        if input_only {
            while end > start && is_final_reply(&messages[end - 1]) {
                end -= 1;
            }
        }
        for message in &messages[start..end] {
            let category = match message.role {
                _ if message.hidden => ContextCategory::CompactionSummaries,
                HistoryRole::User => ContextCategory::UserMessages,
                HistoryRole::Assistant => ContextCategory::AssistantMessages,
                HistoryRole::Tool => ContextCategory::ToolResults,
                HistoryRole::System => continue,
            };
            self.add_text(category, &message.text);
            for call in &message.tool_calls {
                self.add_text(ContextCategory::AssistantMessages, call);
            }
        }
    }
}

/// An answer row with no tool call after it: output of the turn's last call.
fn is_final_reply(message: &HistoryMessage) -> bool {
    message.role == HistoryRole::Assistant && !message.hidden && message.tool_calls.is_empty()
}

fn is_compaction_marker(message: &HistoryMessage) -> bool {
    message.role == HistoryRole::System && message.text.trim() == "Compacted"
}

/// A sample fitted to the thread's `used` figure.
#[derive(Debug, Clone, PartialEq)]
pub struct Reconciled {
    pub used: u64,
    pub source: ContextSource,
    pub categories: Vec<ContextCategoryUsage>,
    pub tool_owners: Vec<ContextToolOwnerUsage>,
}

/// Fits `sample` to the provider's exact count. A count above the estimate
/// leaves the gap as [`ContextCategory::Unattributed`] (a CLI's own system
/// prompt and tools, which ZLO cannot see); a count below it scales every
/// category down. Either way the categories sum to `used`.
pub fn reconcile(sample: &ContextSample, exact_used: Option<u64>) -> Reconciled {
    let estimate = sample.total();
    let (used, source) = match exact_used {
        Some(used) => (used, ContextSource::Provider),
        None => (estimate, ContextSource::Estimated),
    };
    let mut categories: Vec<ContextCategoryUsage> = if used >= estimate {
        let mut kept = sample.categories.clone();
        if used > estimate {
            kept.push(ContextCategoryUsage {
                category: ContextCategory::Unattributed,
                tokens: used - estimate,
            });
        }
        kept
    } else {
        let parts: Vec<u64> = sample.categories.iter().map(|c| c.tokens).collect();
        sample
            .categories
            .iter()
            .zip(scale(&parts, used))
            .map(|(c, tokens)| ContextCategoryUsage {
                category: c.category,
                tokens,
            })
            .collect()
    };
    categories.retain(|c| c.tokens > 0);
    categories.sort_by_key(|c| c.category);
    let tool_total = categories
        .iter()
        .find(|c| c.category == ContextCategory::ToolDefinitions)
        .map_or(0, |c| c.tokens);
    let parts: Vec<u64> = sample.tools.iter().map(|t| t.tokens).collect();
    let mut tool_owners: Vec<ContextToolOwnerUsage> = sample
        .tools
        .iter()
        .zip(scale(&parts, tool_total))
        .map(|(t, tokens)| ContextToolOwnerUsage {
            owner: t.owner.clone(),
            tokens,
            tools: t.tools,
        })
        .filter(|t| t.tokens > 0)
        .collect();
    tool_owners.sort_by(|a, b| b.tokens.cmp(&a.tokens).then(a.owner.cmp(&b.owner)));
    Reconciled {
        used,
        source,
        categories,
        tool_owners,
    }
}

/// `parts` scaled to sum exactly to `target` (largest remainder).
fn scale(parts: &[u64], target: u64) -> Vec<u64> {
    let total: u128 = parts.iter().map(|&p| u128::from(p)).sum();
    if total == 0 {
        return vec![0; parts.len()];
    }
    let target = u128::from(target);
    let mut out: Vec<u128> = Vec::with_capacity(parts.len());
    let mut order: Vec<(u128, usize)> = Vec::with_capacity(parts.len());
    for (i, &p) in parts.iter().enumerate() {
        let exact = u128::from(p) * target;
        out.push(exact / total);
        order.push((exact % total, i));
    }
    order.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let left = target.saturating_sub(out.iter().sum::<u128>());
    for &(_, i) in order
        .iter()
        .take(usize::try_from(left).unwrap_or(usize::MAX))
    {
        out[i] += 1;
    }
    out.into_iter()
        .map(|v| u64::try_from(v).unwrap_or(u64::MAX))
        .collect()
}

impl Store {
    /// Replace the thread's latest context sample.
    pub async fn put_context_sample(
        &self,
        thread: &ThreadId,
        sample: &ContextSample,
        at: Millis,
    ) -> Result<()> {
        self.exec(
            "INSERT OR REPLACE INTO orch_context (thread_id, doc, updated_at) VALUES (?1, ?2, ?3)",
            vec![
                s(thread.as_str()),
                s(serde_json::to_string(sample)?),
                Value::Integer(at),
            ],
        )
        .await?;
        Ok(())
    }

    pub async fn context_sample(&self, thread: &ThreadId) -> Result<Option<ContextSample>> {
        let doc = self
            .row(
                "SELECT doc FROM orch_context WHERE thread_id = ?1",
                vec![s(thread.as_str())],
                |r| Cols::new(r).text(),
            )
            .await?;
        doc.map(|doc| Ok(serde_json::from_str(&doc)?)).transpose()
    }
}

impl Engine {
    /// Keep the latest sample of a native prompt (every model call).
    pub async fn record_context_sample(
        &self,
        thread: &ThreadId,
        sample: &ContextSample,
        at: Millis,
    ) -> Result<()> {
        self.store.put_context_sample(thread, sample, at).await
    }

    /// What fills `thread`'s context window. The split of a CLI thread is
    /// estimated once per turn (keyed by its latest telemetry) and reused
    /// until the next turn reports.
    pub async fn context_report(&self, thread: &ThreadRecord) -> Result<ThreadContextReport> {
        let id = &thread.thread_id;
        let (telemetry, updated_at) = match self.store.telemetry(id).await? {
            Some(record) => (record.telemetry, Some(record.updated_at)),
            None => (Default::default(), None),
        };
        let runtime = self.runtime(&thread.runtime_id)?;
        let native = self.is_default_runtime(&thread.runtime_id);
        let stored = self.store.context_sample(id).await?;
        let sample = if native {
            stored.unwrap_or_default()
        } else {
            let key = updated_at.map(|at| at.to_string());
            match stored {
                Some(sample) if key.is_some() && sample.turn_key == key => sample,
                _ => {
                    let profile = self.turn_profile(id).await;
                    let goal = self
                        .store
                        .goal(id)
                        .await?
                        .filter(|g| g.status == GoalStatus::Active)
                        .map(|g| g.objective);
                    let mut sample = runtime.injected_context(profile.as_ref(), goal.as_deref());
                    sample.add_history(
                        &self.history(thread).await?,
                        telemetry.context_used.is_some(),
                    );
                    sample.turn_key = key;
                    if let Some(at) = updated_at {
                        self.store.put_context_sample(id, &sample, at).await?;
                    }
                    sample
                }
            }
        };
        let max = telemetry.context_max.or(sample.window);
        let fitted = reconcile(&sample, telemetry.context_used);
        let compaction = runtime.compaction();
        Ok(ThreadContextReport {
            thread_id: id.as_str().to_string(),
            runtime_id: thread.runtime_id.as_str().to_string(),
            used: fitted.used,
            max,
            level: ContextLevel::of(fitted.used, max),
            source: fitted.source,
            categories: fitted.categories,
            tool_owners: fitted.tool_owners,
            compact_prompt: compaction.manual_prompt.map(str::to_string),
            auto_compacts: native || compaction.automatic,
            updated_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_product_contracts::orchestration::ContextLevel;

    fn sum(categories: &[ContextCategoryUsage]) -> u64 {
        categories.iter().map(|c| c.tokens).sum()
    }

    fn sample() -> ContextSample {
        let mut s = ContextSample::default();
        s.add(ContextCategory::SystemPrompt, 1_000);
        s.add(ContextCategory::Instructions, 333);
        s.add(ContextCategory::UserMessages, 2_001);
        s.add(ContextCategory::ToolResults, 7);
        s.add_tool("builtin.shell", 4_000);
        s.add_tool("builtin.read_file", 2_000);
        s.add_tool("mcp-notion.search", 6_000);
        s.add_tool("github.search_issues", 800);
        s
    }

    #[test]
    fn categories_sum_to_the_provider_total_above_and_below_the_estimate() {
        let s = sample();
        let estimate = s.total();
        for exact in [estimate + 12_345, estimate, estimate - 1, 997, 1, 0] {
            let r = reconcile(&s, Some(exact));
            assert_eq!(r.used, exact);
            assert_eq!(sum(&r.categories), exact, "exact {exact}");
            assert_eq!(r.source, ContextSource::Provider);
            let tools: u64 = r.tool_owners.iter().map(|t| t.tokens).sum();
            let tool_category = r
                .categories
                .iter()
                .find(|c| c.category == ContextCategory::ToolDefinitions)
                .map_or(0, |c| c.tokens);
            assert_eq!(tools, tool_category, "owners sum to the tool category");
        }
        let above = reconcile(&s, Some(estimate + 500));
        assert!(above.categories.contains(&ContextCategoryUsage {
            category: ContextCategory::Unattributed,
            tokens: 500,
        }));
    }

    #[test]
    fn without_a_provider_count_the_estimate_is_the_total_and_says_so() {
        let s = sample();
        let r = reconcile(&s, None);
        assert_eq!(r.source, ContextSource::Estimated);
        assert_eq!(r.used, s.total());
        assert_eq!(sum(&r.categories), r.used);
        assert!(
            r.categories
                .iter()
                .all(|c| c.category != ContextCategory::Unattributed)
        );
    }

    #[test]
    fn tool_owners_group_by_server_largest_first() {
        let r = reconcile(&sample(), None);
        let owners: Vec<_> = r.tool_owners.iter().map(|t| t.owner.clone()).collect();
        assert_eq!(
            owners,
            vec![
                ContextToolOwner::Builtin,
                ContextToolOwner::Mcp("notion".into()),
                ContextToolOwner::Extension("github".into()),
            ]
        );
        assert_eq!(r.tool_owners[0].tools, 2);
        assert_eq!(
            tool_owner("orchestrator.t3_delegate"),
            ContextToolOwner::Builtin
        );
        assert_eq!(
            tool_owner("mcp_linear.issue"),
            ContextToolOwner::Mcp("linear".into())
        );
        assert_eq!(
            tool_owner("bare"),
            ContextToolOwner::Extension("bare".into())
        );
    }

    #[test]
    fn history_after_the_latest_compaction_is_all_that_counts() {
        let msg = |role, text: &str, hidden| HistoryMessage {
            role,
            text: text.into(),
            tool_calls: Vec::new(),
            run_id: None,
            hidden,
            at: None,
        };
        let history = vec![
            msg(HistoryRole::User, &"old ".repeat(400), false),
            msg(HistoryRole::System, "Compacted", false),
            msg(HistoryRole::User, "/compact", true),
            msg(HistoryRole::Assistant, &"summary ".repeat(50), true),
            msg(HistoryRole::User, "next question", false),
            msg(HistoryRole::Tool, "tool output here", false),
            msg(HistoryRole::Assistant, "an answer", false),
        ];
        let mut s = ContextSample::default();
        s.add_history(&history, false);
        assert!(s.tokens(ContextCategory::CompactionSummaries) >= 100);
        assert_eq!(
            s.tokens(ContextCategory::UserMessages),
            estimate_tokens("next question")
        );
        assert_eq!(
            s.tokens(ContextCategory::ToolResults),
            estimate_tokens("tool output here")
        );
        assert_eq!(
            s.tokens(ContextCategory::AssistantMessages),
            estimate_tokens("an answer")
        );
    }

    #[test]
    fn estimates_weigh_non_ascii_text_denser() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        assert_eq!(estimate_tokens("日本語"), 3);
    }

    #[test]
    fn meter_levels_warn_at_eighty_percent_and_exceed_at_the_window() {
        assert_eq!(
            ContextLevel::of(79_999, Some(100_000)),
            ContextLevel::Normal
        );
        assert_eq!(
            ContextLevel::of(80_000, Some(100_000)),
            ContextLevel::Warning
        );
        assert_eq!(
            ContextLevel::of(99_999, Some(100_000)),
            ContextLevel::Warning
        );
        assert_eq!(
            ContextLevel::of(100_000, Some(100_000)),
            ContextLevel::Exceeded
        );
        assert_eq!(
            ContextLevel::of(250_000, Some(100_000)),
            ContextLevel::Exceeded
        );
        assert_eq!(ContextLevel::of(5_000_000, None), ContextLevel::Normal);
        assert_eq!(ContextLevel::of(1, Some(0)), ContextLevel::Normal);
    }

    #[test]
    fn samples_round_trip_through_their_stored_shape() {
        let mut s = sample();
        s.window = Some(200_000);
        s.turn_key = Some("run-1".into());
        let doc = serde_json::to_string(&s).expect("serialize");
        let back: ContextSample = serde_json::from_str(&doc).expect("deserialize");
        assert_eq!(back, s);
    }
}
