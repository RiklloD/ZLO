//! Portable context handoffs: bounded, auditable transcript digests.
//!
//! When a target cannot receive context natively (a fresh thread on another
//! runtime, a merge-back delta), the engine materializes an explicit text
//! artifact, stores it on the transfer row, and injects exactly that text.

use serde::Serialize;

use crate::runtime::{HistoryMessage, HistoryRole};

/// One run: a visible user turn and everything until the next one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunSummary {
    pub ordinal: usize,
    pub start_index: usize,
    pub end_index: usize,
    pub run_id: Option<String>,
    pub user: String,
    pub reply: String,
    pub tool_calls: usize,
    pub status: &'static str,
}

fn starts(messages: &[HistoryMessage]) -> Vec<usize> {
    messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == HistoryRole::User && !m.hidden)
        .map(|(i, _)| i)
        .collect()
}

pub fn runs(messages: &[HistoryMessage]) -> Vec<RunSummary> {
    let starts = starts(messages);
    starts
        .iter()
        .enumerate()
        .map(|(n, &start)| {
            let end = starts.get(n + 1).copied().unwrap_or(messages.len());
            let chunk = &messages[start..end];
            let reply = chunk
                .iter()
                .rev()
                .find(|m| m.role == HistoryRole::Assistant && !m.text.trim().is_empty())
                .map(|m| m.text.clone())
                .unwrap_or_default();
            let tail = chunk.iter().rev().find(|m| m.role != HistoryRole::System);
            let ended = tail.is_some_and(|m| {
                m.role == HistoryRole::Assistant
                    && m.tool_calls.is_empty()
                    && !m.text.trim().is_empty()
            });
            let status = if ended {
                "completed"
            } else if chunk.len() > 1 {
                "interrupted"
            } else {
                "no_reply"
            };
            RunSummary {
                ordinal: n + 1,
                start_index: start,
                end_index: end,
                run_id: messages[start].run_id.clone(),
                user: crate::model::clip(&messages[start].text, 300),
                reply: crate::model::clip(&reply, 300),
                tool_calls: chunk.iter().map(|m| m.tool_calls.len()).sum(),
                status,
            }
        })
        .collect()
}

/// Number of leading messages that make up runs `1..=ordinal` (`None` = all).
pub fn cut_for_run(messages: &[HistoryMessage], ordinal: Option<usize>) -> Result<usize, String> {
    let Some(ordinal) = ordinal else {
        return Ok(messages.len());
    };
    let rs = runs(messages);
    if rs.is_empty() {
        return Ok(messages.len());
    }
    rs.get(ordinal.wrapping_sub(1))
        .map(|r| r.end_index)
        .ok_or_else(|| {
            format!(
                "run {ordinal} does not exist (this thread has {} runs)",
                rs.len()
            )
        })
}

pub fn last_reply(messages: &[HistoryMessage]) -> String {
    messages
        .iter()
        .rev()
        .find(|m| m.role == HistoryRole::Assistant && !m.text.trim().is_empty())
        .map(|m| m.text.trim().to_string())
        .unwrap_or_default()
}

/// Count of user/assistant messages with visible text.
pub fn visible_count(messages: &[HistoryMessage]) -> usize {
    messages.iter().filter(|m| is_visible(m)).count()
}

fn is_visible(m: &HistoryMessage) -> bool {
    !m.hidden
        && matches!(m.role, HistoryRole::User | HistoryRole::Assistant)
        && !m.text.trim().is_empty()
}

/// Index after the `n`-th visible message (where a seeded fork's own work starts).
pub fn index_after_visible(messages: &[HistoryMessage], n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    let mut seen = 0;
    for (i, m) in messages.iter().enumerate() {
        if is_visible(m) {
            seen += 1;
            if seen == n {
                return i + 1;
            }
        }
    }
    messages.len()
}

fn items(messages: &[HistoryMessage]) -> Vec<String> {
    let mut out = Vec::new();
    for m in messages.iter().filter(|m| !m.hidden) {
        let text = m.text.trim();
        match m.role {
            HistoryRole::User if !text.is_empty() => out.push(format!("[user]\n{text}")),
            HistoryRole::Assistant => {
                if !m.tool_calls.is_empty() {
                    let calls: Vec<&str> =
                        m.tool_calls.iter().take(12).map(String::as_str).collect();
                    out.push(format!("[assistant tool calls] {}", calls.join("; ")));
                }
                if !text.is_empty() {
                    out.push(format!("[assistant]\n{text}"));
                }
            }
            // Tool output is summarized by the calls above; raw output is noise here.
            _ => {}
        }
    }
    out
}

/// Byte-bounded prefix/suffix of `s` on char boundaries.
fn head(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn tail(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut start = s.len() - max_bytes;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// Keep the newest items that fit `budget` bytes. Returns (kept, omitted).
fn fit(items: Vec<String>, budget: usize) -> (Vec<String>, usize) {
    let total = items.len();
    let mut kept = Vec::new();
    let mut used = 0;
    for item in items.into_iter().rev() {
        let piece = if item.len() <= 6000 {
            item
        } else {
            format!("{}\n…[clipped]…\n{}", head(&item, 3000), tail(&item, 2500))
        };
        let size = piece.len() + 2;
        if used + size > budget {
            if kept.is_empty() {
                kept.push(format!("{}…", head(&piece, budget.saturating_sub(64))));
            }
            break;
        }
        used += size;
        kept.push(piece);
    }
    kept.reverse();
    let omitted = total - kept.len();
    (kept, omitted)
}

fn body(header: String, items: Vec<String>, budget: usize, end: &str) -> String {
    let (kept, omitted) = fit(items, budget.saturating_sub(header.len() + 200).max(1024));
    let note = if omitted > 0 {
        format!("\n[{omitted} earlier item(s) omitted to fit the handoff budget]")
    } else {
        String::new()
    };
    format!("{header}{note}\n\n{}\n\n{end}", kept.join("\n\n"))
}

/// Full-thread handoff for a target that cannot share the source's state.
pub fn portable_context(
    messages: &[HistoryMessage],
    title: &str,
    source: &str,
    source_route: &str,
    reason: &str,
    budget: usize,
) -> String {
    let header = format!(
        "Context handoff ({reason}). Source thread: \"{title}\" ({source}), previously run on {source_route}.\n\
         The following is the earlier conversation of this thread, newest last. Reasoning, raw tool output and \
         attachments are not included; re-check files and state before relying on them."
    );
    body(header, items(messages), budget, "[End of context handoff]")
}

/// Merge-back delta: only what happened in the fork after it branched off.
pub fn delta_context(
    messages: &[HistoryMessage],
    fork_title: &str,
    fork: &str,
    base: usize,
    budget: usize,
) -> String {
    let delta = &messages[base.min(messages.len())..];
    let header = format!(
        "Merged context from fork \"{fork_title}\" ({fork}). This fork branched from this thread and explored \
         separately; below is everything it did after the fork point ({} message(s)). Use its decisions, findings \
         and file changes as context; verify anything it changed on disk.",
        delta.len()
    );
    if delta.is_empty() {
        return format!("{header}\n\n(The fork has no new activity since the fork point.)");
    }
    body(header, items(delta), budget, "[End of merged fork context]")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: HistoryRole, text: &str) -> HistoryMessage {
        HistoryMessage {
            role,
            text: text.into(),
            tool_calls: Vec::new(),
            run_id: None,
            hidden: false,
        }
    }

    #[test]
    fn runs_cut_and_budget() {
        let m = vec![
            msg(HistoryRole::User, "one"),
            msg(HistoryRole::Assistant, "a1"),
            msg(HistoryRole::User, "two"),
            msg(HistoryRole::Assistant, "a2"),
        ];
        let rs = runs(&m);
        assert_eq!(rs.len(), 2);
        assert_eq!(rs[0].status, "completed");
        assert_eq!(cut_for_run(&m, Some(1)), Ok(2));
        assert!(cut_for_run(&m, Some(3)).is_err());
        assert_eq!(index_after_visible(&m, 2), 2);
        let big = "é".repeat(5000);
        let text = portable_context(&[msg(HistoryRole::User, &big)], "t", "x", "r", "fork", 2048);
        assert!(text.len() < 4096);
        assert!(delta_context(&m, "f", "k", 2, 4096).contains("a2"));
    }
}
