//! Commentary: what the agent says between its tool calls ("I'll launch two
//! subagents…"). It is work, not the answer: drawn subdued and folded with
//! the turn's work once it settles. A model that re-announces the same plan
//! before every tool call would stack near-identical lines, so a turn keeps
//! the first phrasing and drops later ones that say the same thing.
//!
//! "The same thing" is deliberately narrow: a distinct step must never
//! vanish. Two lines match when they read the same, or when they do the
//! same kind of action (launch ~ start, but read ≠ edit ≠ fix ≠ run) to the
//! same object and share most of their remaining words. A line that names a
//! path or code identifier only matches an identical line.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use zlo_client::transcript::Row;

/// Content words both lines need before they are compared loosely; shorter
/// lines collapse only when they read the same.
const MIN_WORDS: usize = 4;
/// Shared content words (of the shorter line's) at which two lines whose
/// action and object agree say the same.
const SAME_PLAN: f32 = 0.5;
/// Kept lines of the turn a new line is compared with, newest first.
const RECENT: usize = 3;
/// Lines whose features stay cached between folds (the pacer re-folds
/// every tick while a reply streams).
const CACHE_LIMIT: usize = 512;

/// Words that carry no plan: pronouns, articles, glue.
const GLUE: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "can", "do", "each", "for", "from", "i", "id",
    "ill", "im", "in", "is", "it", "its", "ive", "let", "lets", "me", "my", "now", "of", "on",
    "or", "so", "that", "the", "them", "then", "these", "they", "this", "those", "to", "we",
    "well", "will", "with", "you", "your",
];

/// Words between a verb and its object that name no object.
const MODIFIERS: &[&str] = &[
    "two",
    "three",
    "four",
    "one",
    "more",
    "new",
    "fresh",
    "another",
    "some",
    "all",
    "both",
    "few",
    "quick",
    "different",
    "separate",
    "other",
    "parallel",
    "up",
    "off",
    "again",
];

/// Action verbs by kind: lines of different kinds are different steps.
const ACTIONS: &[(&str, &[&str])] = &[
    (
        "start",
        &[
            "launch",
            "launching",
            "start",
            "starting",
            "spin",
            "spinning",
            "spawn",
            "spawning",
            "kick",
            "kicking",
            "begin",
            "beginning",
            "delegate",
            "delegating",
        ],
    ),
    (
        "read",
        &[
            "read",
            "reading",
            "look",
            "looking",
            "inspect",
            "inspecting",
            "open",
            "opening",
            "check",
            "checking",
            "view",
            "viewing",
            "review",
            "reviewing",
            "scan",
            "scanning",
            "explore",
            "exploring",
        ],
    ),
    (
        "search",
        &["search", "searching", "grep", "grepping", "find", "finding"],
    ),
    (
        "edit",
        &[
            "edit",
            "editing",
            "update",
            "updating",
            "change",
            "changing",
            "write",
            "writing",
            "modify",
            "modifying",
            "patch",
            "patching",
            "add",
            "adding",
            "refactor",
            "refactoring",
            "rename",
            "renaming",
            "remove",
            "removing",
            "delete",
            "deleting",
        ],
    ),
    (
        "fix",
        &["fix", "fixing", "repair", "repairing", "debug", "debugging"],
    ),
    (
        "run",
        &[
            "run",
            "running",
            "execute",
            "executing",
            "build",
            "building",
            "rerun",
            "rerunning",
        ],
    ),
];

/// What a line says, for comparing.
struct Said {
    plain: String,
    words: HashSet<String>,
    action: Option<&'static str>,
    object: Option<String>,
    /// Names a path or code identifier: only an identical line matches.
    specific: bool,
}

thread_local! {
    static CACHE: RefCell<HashMap<String, Rc<Said>>> = RefCell::default();
}

/// `rows` without each turn's repeated commentary (see the module docs),
/// borrowed: the fold clones what it keeps once. A person's message starts
/// a new turn. The first phrasing stays, so a streaming turn never swaps a
/// line it already drew.
pub(super) fn collapse(rows: &[Row]) -> Vec<&Row> {
    let absorbed = absorbed_by(rows);
    rows.iter()
        .zip(absorbed)
        .filter_map(|(row, by)| by.is_none().then_some(row))
        .collect()
}

/// The key of the kept commentary row that `key` was folded into, when the
/// collapse dropped it: a search hit on a dropped line reveals that row.
pub(crate) fn kept_key<'a>(rows: &'a [Row], key: &str) -> Option<&'a str> {
    let at = rows.iter().position(|r| r.key() == key)?;
    absorbed_by(rows)[at].map(|kept| rows[kept].key())
}

/// For each row, the index of the kept row it repeats (None: kept).
fn absorbed_by(rows: &[Row]) -> Vec<Option<usize>> {
    let mut out = vec![None; rows.len()];
    // Two lines are needed before anything can repeat.
    if rows
        .iter()
        .filter(|r| matches!(r, Row::Narration { .. }))
        .nth(1)
        .is_none()
    {
        return out;
    }
    let mut kept: Vec<(usize, Rc<Said>)> = Vec::new();
    for (ix, row) in rows.iter().enumerate() {
        match row {
            Row::User { .. } => kept.clear(),
            Row::Narration { text, .. } => {
                let said = said(text);
                let repeat = kept
                    .iter()
                    .rev()
                    .take(RECENT)
                    .find(|(_, k)| same_plan(k, &said))
                    .map(|(at, _)| *at);
                match repeat {
                    Some(at) => out[ix] = Some(at),
                    None => kept.push((ix, said)),
                }
            }
            _ => {}
        }
    }
    out
}

fn said(text: &str) -> Rc<Said> {
    if let Some(hit) = CACHE.with(|c| c.borrow().get(text).cloned()) {
        return hit;
    }
    let plain = normalized(text);
    let specific = text.split_whitespace().any(|w| {
        let w = w.trim_matches(|c: char| !c.is_alphanumeric());
        w.contains(['/', '_', '`', ':', '.'])
    }) || text.contains('`');
    let mut action = None;
    let mut object = None;
    let mut words = HashSet::new();
    for w in plain
        .split(' ')
        .filter(|w| !w.is_empty() && !GLUE.contains(w))
    {
        let kind = ACTIONS
            .iter()
            .find(|(_, forms)| forms.contains(&w))
            .map(|(kind, _)| *kind);
        match kind {
            Some(kind) => {
                action.get_or_insert(kind);
                words.insert(kind.to_string());
            }
            None => {
                let stem = stem(w);
                if object.is_none()
                    && !MODIFIERS.contains(&w)
                    && !w.chars().all(|c| c.is_ascii_digit())
                {
                    object = Some(stem.clone());
                }
                words.insert(stem);
            }
        }
    }
    let said = Rc::new(Said {
        plain,
        words,
        action,
        object,
        specific,
    });
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= CACHE_LIMIT {
            c.clear();
        }
        c.insert(text.to_string(), said.clone());
    });
    said
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

/// Plural `s` trimmed so "puzzle" and "puzzles" meet.
fn stem(w: &str) -> String {
    match w.strip_suffix('s') {
        Some(stem) if stem.len() > 2 && !stem.ends_with('s') => stem.to_string(),
        _ => w.to_string(),
    }
}

fn same_plan(a: &Said, b: &Said) -> bool {
    if a.plain == b.plain {
        return true;
    }
    if a.specific || b.specific || a.action != b.action || a.object != b.object {
        return false;
    }
    if a.words.len() < MIN_WORDS || b.words.len() < MIN_WORDS {
        return false;
    }
    let shared = a.words.intersection(&b.words).count();
    let shorter = a.words.len().min(b.words.len());
    shared >= MIN_WORDS && shared as f32 / shorter as f32 >= SAME_PLAN
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

    fn texts<'a>(rows: &[&'a Row]) -> Vec<&'a str> {
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

    /// Review regressions: distinct steps on the same subject never fold,
    /// whether they differ in action, object or a named file.
    #[test]
    fn distinct_steps_on_the_same_subject_are_kept() {
        let pairs = [
            (
                "Reading src/auth/session.rs to check token refresh",
                "Editing src/auth/session.rs to fix token refresh",
            ),
            (
                "Reading the session module to check token refresh handling",
                "Editing the session module to fix token refresh handling",
            ),
            (
                "Running the database migration tests now",
                "Fixing the database migration tests now",
            ),
            (
                "I'll update the parser to handle trailing commas",
                "I'll update the lexer to handle trailing commas",
            ),
            (
                "Checking `Pacer::apply` for the hold timing",
                "Checking `Pacer::step` for the hold timing",
            ),
        ];
        for (a, b) in pairs {
            let rows = vec![said("a", a), said("b", b)];
            assert_eq!(texts(&collapse(&rows)), vec![a, b], "{a} / {b}");
        }
    }

    /// A line is compared with kept lines only: a chain of small drifts
    /// does not swallow a line unlike the one kept.
    #[test]
    fn repeats_do_not_chain_through_dropped_lines() {
        let rows = vec![
            said(
                "a",
                "I'll launch two subagents with fresh puzzles you can watch.",
            ),
            said(
                "b",
                "I'll launch two subagents with fresh puzzles for the benchmark run.",
            ),
            said(
                "c",
                "I'll launch subagents for the benchmark run on the nightly cluster.",
            ),
        ];
        assert_eq!(
            texts(&collapse(&rows)),
            vec![
                "I'll launch two subagents with fresh puzzles you can watch.",
                "I'll launch subagents for the benchmark run on the nightly cluster.",
            ]
        );
    }

    /// A search hit on a dropped line reveals the line it folded into.
    #[test]
    fn a_dropped_line_aliases_to_the_kept_one() {
        let rows = vec![
            said(
                "a",
                "I'll launch two more subagents with fresh puzzles you can watch.",
            ),
            said(
                "b",
                "I'll launch two fresh subagents with different puzzles you can watch.",
            ),
            said("c", "Both are running."),
        ];
        assert_eq!(kept_key(&rows, "b"), Some("a"));
        assert_eq!(kept_key(&rows, "a"), None);
        assert_eq!(kept_key(&rows, "c"), None);
    }
}
