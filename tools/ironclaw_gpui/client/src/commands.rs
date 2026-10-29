//! Composer menu helpers, mirroring the web UI's `chat-commands.ts`.
//! Everything comes from the gateway (`GET /commands`, `/skills`,
//! `/extensions`); the app never hardcodes a name.
//!
//! Two triggers, like opencode and Claude Code:
//! - `/` opens commands (only as the draft's first word) and skills
//!   (anywhere). The backend force-activates a `/skill` mention.
//! - `@` opens extensions; the backend pins a mentioned extension's tools
//!   for that turn.

use ironclaw_product_contracts::product_wire::RebornProductCommandInfo;

/// One entry in the composer menu.
#[derive(Debug, Clone, PartialEq)]
pub enum MenuItem {
    Command(RebornProductCommandInfo),
    Skill { name: String, description: String },
    Extension { name: String, description: String },
}

impl MenuItem {
    pub fn name(&self) -> &str {
        match self {
            Self::Command(c) => &c.name,
            Self::Skill { name, .. } | Self::Extension { name, .. } => name,
        }
    }

    /// The left column: usage for commands, the mention for the others.
    pub fn label(&self) -> String {
        match self {
            Self::Command(c) => c.usage.clone(),
            Self::Skill { name, .. } => format!("/{name}"),
            Self::Extension { name, .. } => format!("@{name}"),
        }
    }

    pub fn description(&self) -> &str {
        match self {
            Self::Command(c) => &c.description,
            Self::Skill { description, .. } | Self::Extension { description, .. } => description,
        }
    }

    /// Section heading shown above the first item of each kind.
    pub fn section(&self) -> &'static str {
        match self {
            Self::Command(_) => "Commands",
            Self::Skill { .. } => "Skills",
            Self::Extension { .. } => "Tools",
        }
    }
}

/// What the menu can offer, loaded once per chat view.
#[derive(Debug, Default)]
pub struct MenuSources {
    pub commands: Vec<RebornProductCommandInfo>,
    pub skills: Vec<MenuItem>,
    pub extensions: Vec<MenuItem>,
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
    let Some((start, token)) = trigger_token(text) else {
        return Vec::new();
    };
    let query = token[1..].to_lowercase();
    let hit = |item: &&MenuItem| item.name().to_lowercase().starts_with(&query);
    if token.starts_with('@') {
        return sources.extensions.iter().filter(hit).cloned().collect();
    }
    // Commands only run as the first word of the draft.
    let at_start = text[..start].trim().is_empty();
    let commands = sources
        .commands
        .iter()
        .filter(|_| at_start)
        .filter(|c| c.name.starts_with(&query))
        .map(|c| MenuItem::Command(c.clone()));
    let skills = sources.skills.iter().filter(hit).cloned();
    commands.chain(skills).collect()
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
        };
        let names = |v: Vec<MenuItem>| v.iter().map(|i| i.label()).collect::<Vec<_>>();
        assert_eq!(
            names(menu_matches("/s", &sources)),
            ["/status", "/stop", "/security-review", "/summarize"]
        );
        // Mid-sentence `/` offers skills only.
        assert_eq!(
            names(menu_matches("check this with /se", &sources)),
            ["/security-review"]
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
        assert_eq!(tab("/st", 1), "/stop");
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
}
