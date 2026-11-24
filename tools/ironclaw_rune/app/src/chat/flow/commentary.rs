//! Commentary: what the agent says between its tool calls ("I'll launch two
//! subagents…"). It is work, not the answer: drawn subdued and folded with
//! the turn's work once it settles. A model that announces the same plan
//! before every tool call would stack near-identical lines, so a turn keeps
//! only the first of each run of look-alikes.

use std::collections::HashSet;

use zlo_client::transcript::Row;

/// Content words two lines must both have before they are compared loosely;
/// shorter lines collapse only when they read the same.
const MIN_WORDS: usize = 4;
/// Shared share of content words (Jaccard) at which two lines say the same.
const SAME_PLAN: f32 = 0.5;

/// Words that carry no plan: pronouns, articles, glue.
const GLUE: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "can", "do", "each", "for", "from", "i", "id",
    "ill", "im", "in", "is", "it", "its", "ive", "let", "lets", "me", "my", "now", "of", "on",
    "or", "so", "that", "the", "them", "then", "these", "they", "this", "those", "to", "we",
    "well", "will", "with", "you", "your",
];

/// `rows` with each turn's near-duplicate commentary dropped. A line is
/// dropped when it says what any earlier commentary line of the same turn
/// (kept or dropped) already said; a person's message starts a new turn. The first phrasing
/// stays, so a streaming turn never swaps a line it already drew.
pub(super) fn collapse(rows: &[Row]) -> Vec<Row> {
    let mut seen: Vec<(String, HashSet<String>)> = Vec::new();
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        match row {
            Row::User { .. } => seen.clear(),
            Row::Narration { text, .. } => {
                let plain = normalized(text);
                let words = content_words(&plain);
                let repeat = seen.iter().any(|(p, w)| same_plan(p, w, &plain, &words));
                // A dropped line still names the plan: later look-alikes
                // of it are repeats too, even where the kept one differs.
                seen.push((plain, words));
                if repeat {
                    continue;
                }
            }
            _ => {}
        }
        out.push(row.clone());
    }
    out
}

fn normalized(text: &str) -> String {
    text.chars()
        .filter(|c| !matches!(c, '\'' | '\u{2019}'))
        .map(|c| {
            if c.is_alphanumeric() {
                c.to_lowercase().next().unwrap_or(c)
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Content words, plural `s` trimmed so "puzzle" and "puzzles" meet.
fn content_words(plain: &str) -> HashSet<String> {
    plain
        .split(' ')
        .filter(|w| !w.is_empty() && !GLUE.contains(w))
        .map(|w| match w.strip_suffix('s') {
            Some(stem) if stem.len() > 2 && !stem.ends_with('s') => stem.to_string(),
            _ => w.to_string(),
        })
        .collect()
}

fn same_plan(a: &str, aw: &HashSet<String>, b: &str, bw: &HashSet<String>) -> bool {
    if a == b {
        return true;
    }
    if aw.len() < MIN_WORDS || bw.len() < MIN_WORDS {
        return false;
    }
    let shared = aw.intersection(bw).count() as f32;
    let all = aw.union(bw).count() as f32;
    shared / all >= SAME_PLAN
}

#[cfg(test)]
mod tests {
    use super::*;

    fn said(key: &str, text: &str) -> Row {
        Row::Narration {
            key: key.into(),
            text: text.into(),
        }
    }

    fn texts(rows: &[Row]) -> Vec<&str> {
        rows.iter()
            .filter_map(|r| match r {
                Row::Narration { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The bug's own seven lines (ZLO screenshot, 2026-11-24): one stays.
    #[test]
    fn seven_rephrased_preambles_collapse_to_the_first() {
        let lines = [
            "I'll launch two more subagents with fresh puzzles you can watch.",
            "I'll launch two fresh subagents with different puzzles you can watch.",
            "I'll launch two new subagents in parallel with different puzzle tasks.",
            "I'll launch two more subagents with fresh puzzles to work through.",
            "I'll start two fresh subagents and give each a different puzzle.",
            "I'll launch two new subagents on separate puzzles you can watch.",
            "I'll launch two fresh subagents with puzzles you can watch them work through.",
        ];
        let rows: Vec<Row> = lines
            .iter()
            .enumerate()
            .map(|(i, t)| said(&format!("s{i}"), t))
            .collect();
        assert_eq!(texts(&collapse(&rows)), vec![lines[0]]);
    }

    /// Distinct steps stay, short look-alikes stay unless they read the
    /// same, and a new prompt starts over.
    #[test]
    fn distinct_lines_and_new_turns_are_kept() {
        let user = Row::User {
            key: "u2".into(),
            text: "again".into(),
            pending: false,
            attachments: Vec::new(),
            steer: None,
        };
        let rows = vec![
            said(
                "a",
                "First, the decision record. It's the contract that everything else follows.",
            ),
            said("b", "Now the engine contract: types, RNG and balance."),
            said("c", "Reading the config file."),
            said("d", "Editing the config file."),
            said("e", "Engine tests:"),
            said("f", "Engine tests:"),
            user,
            said("g", "Engine tests:"),
        ];
        assert_eq!(
            texts(&collapse(&rows)),
            vec![
                "First, the decision record. It's the contract that everything else follows.",
                "Now the engine contract: types, RNG and balance.",
                "Reading the config file.",
                "Editing the config file.",
                "Engine tests:",
                "Engine tests:",
            ]
        );
    }
}
