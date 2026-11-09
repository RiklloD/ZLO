//! Composer menu helpers, mirroring the web UI's `chat-commands.ts`.
//! Everything comes from the gateway (`GET /commands`, `/skills`,
//! `/extensions`); the app never hardcodes a name.
//!
//! Two triggers, like opencode and Claude Code:
//! - `/` opens commands (only as the draft's first word) and skills
//!   (anywhere). The backend force-activates a `/skill` mention.
//! - `@` opens mentions: files, folders, threads and a Changes-tab
//!   selection (`crate::mentions`, sent as Markdown links the
//!   backend expands), then extensions (the backend pins a mentioned
//!   extension's tools for that turn) and skills.

use ironclaw_product_contracts::product_wire::RebornProductCommandInfo;

use crate::mentions::{
    MentionContext, MentionEntry, Prepared, ThreadEntry, mention_matches, score,
};

/// One entry in the composer menu.
#[derive(Debug, Clone, PartialEq)]
pub enum MenuItem {
    Command(RebornProductCommandInfo),
    Skill {
        name: String,
        description: String,
    },
    Extension {
        name: String,
        description: String,
    },
    /// A file, folder, thread, diff or selection: inserted as a chip.
    Mention(MentionEntry),
}

impl MenuItem {
    pub fn name(&self) -> &str {
        match self {
            Self::Command(c) => &c.name,
            Self::Skill { name, .. } | Self::Extension { name, .. } => name,
            Self::Mention(m) => &m.label,
        }
    }

    /// The left column: usage for commands, the mention for the others.
    pub fn label(&self) -> String {
        match self {
            Self::Command(c) => c.usage.clone(),
            Self::Skill { name, .. } => format!("/{name}"),
            Self::Extension { name, .. } => format!("@{name}"),
            Self::Mention(m) => format!("@{}", m.label),
        }
    }

    pub fn description(&self) -> &str {
        match self {
            Self::Command(c) => &c.description,
            Self::Skill { description, .. } | Self::Extension { description, .. } => description,
            Self::Mention(m) => &m.detail,
        }
    }

    /// Section heading shown above the first item of each kind.
    pub fn section(&self) -> &'static str {
        match self {
            Self::Command(_) => "Commands",
            Self::Skill { .. } => "Skills",
            Self::Extension { .. } => "Tools",
            Self::Mention(m) => m.kind.section(),
        }
    }
}

/// What the menu can offer, loaded once per chat view.
#[derive(Debug, Default)]
pub struct MenuSources {
    pub commands: Vec<RebornProductCommandInfo>,
    pub skills: Vec<MenuItem>,
    pub extensions: Vec<MenuItem>,
    /// Recent threads for `@`, from the inbox list.
    pub threads: Vec<ThreadEntry>,
}

/// The inventory entry the draft's first word names, or `None` when the text
/// should be sent as an ordinary message (skill mentions included).
pub fn match_command<'a>(
    text: &str,
    commands: &'a [RebornProductCommandInfo],
) -> Option<&'a RebornProductCommandInfo> {
    let first = text.trim().strip_prefix('/')?.split_whitespace().next()?;
    let first = first.to_lowercase();
    commands.iter().find(|c| c.name == first)
}

/// The `/word` or `@word` being typed at the end of the draft, with its byte
/// offset. `None` once a space follows it.
pub fn trigger_token(text: &str) -> Option<(usize, &str)> {
    let start = text
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    let token = &text[start..];
    // A mention must start a word: "a/b" or "me@x" are not triggers.
    matches!(token.chars().next(), Some('/' | '@')).then_some((start, token))
}

/// Menu entries for the draft, empty when nothing is being typed.
pub fn menu_matches(text: &str, sources: &MenuSources) -> Vec<MenuItem> {
    menu_matches_with(text, sources, &MentionContext::default())
}

/// Tools and skills a bare `@` lists before anything is typed.
const AT_EXTRAS: usize = 5;

/// [`menu_matches`] with the composer's mention sources: `@` lists
/// mentions first, then tools and skills.
pub fn menu_matches_with(
    text: &str,
    sources: &MenuSources,
    mentions: &MentionContext<'_>,
) -> Vec<MenuItem> {
    let Some((start, token)) = trigger_token(text) else {
        return Vec::new();
    };
    let query = token[1..].to_lowercase();
    let matcher = Matcher::new(&query);
    let ranked = |items: &[MenuItem], limit: usize| -> Vec<MenuItem> {
        matcher
            .rank(items.iter().map(|i| (i.name(), i.description())), limit)
            .into_iter()
            .map(|ix| items[ix].clone())
            .collect()
    };
    if token.starts_with('@') {
        let threads = if mentions.threads.is_empty() {
            &sources.threads[..]
        } else {
            mentions.threads
        };
        let ctx = MentionContext {
            threads,
            ..*mentions
        };
        let extras = if query.is_empty() {
            AT_EXTRAS
        } else {
            usize::MAX
        };
        return mention_matches(&query, &ctx)
            .into_iter()
            .map(MenuItem::Mention)
            .chain(ranked(&sources.extensions, extras))
            .chain(ranked(&sources.skills, extras))
            .collect();
    }
    // Commands only run as the first word of the draft.
    let at_start = text[..start].trim().is_empty();
    let commands: &[RebornProductCommandInfo] = if at_start { &sources.commands } else { &[] };
    matcher
        .rank(
            commands
                .iter()
                .map(|c| (c.name.as_str(), c.description.as_str())),
            usize::MAX,
        )
        .into_iter()
        .map(|ix| MenuItem::Command(commands[ix].clone()))
        .chain(ranked(&sources.skills, usize::MAX))
        .collect()
}

/// Fuzzy matching for menu names, like a command palette: the query's
/// letters in order anywhere in the name (`/hum` finds
/// `academic-thesis-humanizer`, `/ath` too), its `-`/`_`/`:` separated words
/// in any order (`/thesis-academic`), and, ranked below every name hit, a
/// plain substring of the description. Exact and prefix hits rank first.
struct Matcher {
    whole: Prepared,
    words: Vec<Prepared>,
    query: String,
}

/// Puts description-only hits below any name hit.
const DESCRIPTION_ONLY: i32 = -10_000;
/// One rank of [`Matcher::tier`], above any fuzzy score.
const TIER: i32 = 2_000;

fn starts_with_ignore_case(text: &str, prefix: &str) -> bool {
    text.get(..prefix.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
}

impl Matcher {
    fn new(query: &str) -> Self {
        let words: Vec<Prepared> = query
            .split(['-', '_', ':', '/', '.'])
            .filter(|w| !w.is_empty())
            .map(Prepared::new)
            .collect();
        Self {
            whole: Prepared::new(query),
            words: if words.len() > 1 { words } else { Vec::new() },
            query: query.to_lowercase(),
        }
    }

    fn score(&self, name: &str, description: &str) -> Option<i32> {
        if self.query.is_empty() {
            return Some(0);
        }
        self.name_score(name)
            .or_else(|| self.description_score(name, description))
    }

    fn name_score(&self, name: &str) -> Option<i32> {
        let whole = score(&self.whole, name);
        let words = (!self.words.is_empty())
            .then(|| {
                self.words
                    .iter()
                    .map(|w| score(w, name))
                    .sum::<Option<i32>>()
                    .map(|s| s - 50)
            })
            .flatten();
        let fuzzy = whole.max(words)?;
        // A direct hit is a direct hit: the shorter name wins. Greedy letter
        // scores only order scattered matches.
        Some(match self.tier(name) {
            0 => fuzzy,
            tier => tier * TIER - name.len() as i32,
        })
    }

    /// How directly the name answers the query: 3 when it or one of its
    /// words is the query, 2 when one starts with it, 1 for a substring,
    /// 0 for scattered letters.
    fn tier(&self, name: &str) -> i32 {
        let q = self.query.as_str();
        let mut words = name.split([':', '/', '-', '_', '.', ' ']);
        if name.eq_ignore_ascii_case(q) || words.clone().any(|w| w.eq_ignore_ascii_case(q)) {
            3
        } else if starts_with_ignore_case(name, q) || words.any(|w| starts_with_ignore_case(w, q)) {
            2
        } else if name.to_lowercase().contains(q) {
            1
        } else {
            0
        }
    }

    fn description_score(&self, name: &str, description: &str) -> Option<i32> {
        (self.query.len() >= 3 && description.to_lowercase().contains(&self.query))
            .then(|| DESCRIPTION_ONLY - name.len() as i32)
    }

    /// Indices of the matching `(name, description)` entries, best first;
    /// ties keep their inventory order.
    fn rank<'a>(
        &self,
        items: impl Iterator<Item = (&'a str, &'a str)>,
        limit: usize,
    ) -> Vec<usize> {
        let mut hits: Vec<(i32, usize)> = items
            .enumerate()
            .filter_map(|(ix, (name, description))| self.score(name, description).map(|s| (s, ix)))
            .collect();
        hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        hits.truncate(limit);
        hits.into_iter().map(|(_, ix)| ix).collect()
    }
}

/// The draft with its trailing trigger token replaced by `item`'s mention.
pub fn complete_mention(text: &str, item: &MenuItem) -> String {
    let start = trigger_token(text).map_or(text.len(), |(start, _)| start);
    format!("{}{} ", &text[..start], item.label())
}

/// Tab fills in the highlighted entry, like Hermes: arrows pick the row,
/// Tab takes it. Never sends: a command without arguments is left as
/// `/name` for Enter to run.
pub fn tab_complete(text: &str, items: &[MenuItem], selected: usize) -> Option<String> {
    trigger_token(text)?;
    let item = items.get(selected.min(items.len().checked_sub(1)?))?;
    Some(match item {
        MenuItem::Command(c) if takes_arguments(c) => format!("/{} ", c.name),
        MenuItem::Command(c) => format!("/{}", c.name),
        _ => complete_mention(text, item),
    })
}

/// Whether the command takes arguments, read from its usage line.
pub fn takes_arguments(command: &RebornProductCommandInfo) -> bool {
    command.usage.split_whitespace().nth(1).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(name: &str, usage: &str) -> RebornProductCommandInfo {
        RebornProductCommandInfo {
            name: name.into(),
            title: name.into(),
            description: String::new(),
            usage: usage.into(),
        }
    }

    fn skill(name: &str) -> MenuItem {
        MenuItem::Skill {
            name: name.into(),
            description: String::new(),
        }
    }

    #[test]
    fn menu_and_match_follow_the_draft() {
        let sources = MenuSources {
            commands: vec![
                cmd("status", "/status"),
                cmd("stop", "/stop"),
                cmd("model", "/model [use <model> | default]"),
            ],
            skills: vec![skill("security-review"), skill("summarize")],
            extensions: vec![MenuItem::Extension {
                name: "github".into(),
                description: String::new(),
            }],
            threads: Vec::new(),
        };
        let names = |v: Vec<MenuItem>| v.iter().map(|i| i.label()).collect::<Vec<_>>();
        assert_eq!(
            names(menu_matches("/s", &sources)),
            // Equally good prefixes: the shorter name first.
            ["/stop", "/status", "/summarize", "/security-review"]
        );
        // Mid-sentence `/` offers skills only, fuzzy: `/se` also finds
        // s-ummariz-e, below the prefix hit.
        assert_eq!(
            names(menu_matches("check this with /se", &sources)),
            ["/security-review", "/summarize"]
        );
        assert_eq!(names(menu_matches("look in @g", &sources)), ["@github"]);
        assert!(menu_matches("/model use", &sources).is_empty());
        assert!(menu_matches("a/b", &sources).is_empty());
        assert!(menu_matches("hello", &sources).is_empty());

        assert_eq!(
            complete_mention("check with /se", &skill("security-review")),
            "check with /security-review "
        );

        // Tab: the highlighted entry, never a send.
        let tab = |text: &str, selected| {
            tab_complete(text, &menu_matches(text, &sources), selected).unwrap_or_default()
        };
        let reviews = [skill("review-checklist"), skill("review-readiness")];
        assert_eq!(
            tab_complete("fix with /r", &reviews, 1).as_deref(),
            Some("fix with /review-readiness ")
        );
        assert_eq!(tab("fix with /se", 0), "fix with /security-review ");
        assert_eq!(tab("/st", 1), "/status");
        assert_eq!(tab("/mo", 0), "/model ");
        assert_eq!(tab("hello", 0), "");

        let all = &sources.commands;
        assert_eq!(
            match_command(" /Model use x", all).map(|c| c.name.as_str()),
            Some("model")
        );
        assert!(match_command("/security-review this", all).is_none());
        assert!(takes_arguments(&all[2]) && !takes_arguments(&all[0]));
    }

    #[test]
    fn skill_search_is_fuzzy() {
        let mut described = skill("pdf");
        if let MenuItem::Skill { description, .. } = &mut described {
            *description = "Read, merge and create PDF documents".into();
        }
        let sources = MenuSources {
            skills: vec![
                skill("ironclaw:academic-thesis-humanizer"),
                skill("anthropic-skills:humanizer"),
                skill("security-review"),
                described,
                skill("financial-ml-backtest-pitfalls"),
            ],
            ..MenuSources::default()
        };
        let names = |text: &str| {
            menu_matches(text, &sources)
                .iter()
                .map(|i| i.name().to_string())
                .collect::<Vec<_>>()
        };
        // Any word of the name, best (shorter, tighter) first.
        assert_eq!(
            names("/humanizer"),
            [
                "anthropic-skills:humanizer",
                "ironclaw:academic-thesis-humanizer"
            ]
        );
        // Scattered letters and words out of order.
        assert_eq!(names("/thesis"), ["ironclaw:academic-thesis-humanizer"]);
        assert_eq!(
            names("/thesis-academic"),
            ["ironclaw:academic-thesis-humanizer"]
        );
        assert_eq!(names("/btpit"), ["financial-ml-backtest-pitfalls"]);
        assert_eq!(names("/secrev"), ["security-review"]);
        // The description, below every name hit.
        assert_eq!(names("/merge"), ["pdf"]);
        assert!(names("/zzz").is_empty());
        // `@` ranks skills the same way.
        assert_eq!(names("@thesis"), ["ironclaw:academic-thesis-humanizer"]);
    }
}
