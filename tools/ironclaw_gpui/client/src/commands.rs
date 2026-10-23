//! Slash-command helpers for the composer, mirroring the web UI's
//! `chat-commands.ts`. Everything comes from the gateway's inventory
//! (`GET /commands`); the app never hardcodes a command name.

use ironclaw_product_contracts::product_wire::RebornProductCommandInfo;

/// The inventory entry the draft's first word names, or `None` when the text
/// should be sent as an ordinary message.
pub fn match_command<'a>(
    text: &str,
    commands: &'a [RebornProductCommandInfo],
) -> Option<&'a RebornProductCommandInfo> {
    let first = text.trim().strip_prefix('/')?.split_whitespace().next()?;
    let first = first.to_lowercase();
    commands.iter().find(|c| c.name == first)
}

/// Commands to offer while the draft is still a bare `/word` (no space yet).
pub fn menu_matches<'a>(
    text: &str,
    commands: &'a [RebornProductCommandInfo],
) -> Vec<&'a RebornProductCommandInfo> {
    let Some(token) = text.trim_start().strip_prefix('/') else {
        return Vec::new();
    };
    if token.chars().any(char::is_whitespace) {
        return Vec::new();
    }
    let token = token.to_lowercase();
    commands
        .iter()
        .filter(|c| c.name.starts_with(&token))
        .collect()
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

    #[test]
    fn menu_and_match_follow_the_draft() {
        let all = [
            cmd("status", "/status"),
            cmd("stop", "/stop"),
            cmd("model", "/model [use <model> | default]"),
        ];
        let names = |v: Vec<&RebornProductCommandInfo>| {
            v.into_iter().map(|c| c.name.clone()).collect::<Vec<_>>()
        };
        assert_eq!(names(menu_matches("/st", &all)), ["status", "stop"]);
        assert_eq!(names(menu_matches("/", &all)).len(), 3);
        assert!(menu_matches("/model use", &all).is_empty());
        assert!(menu_matches("hello", &all).is_empty());

        assert_eq!(
            match_command(" /Model use x", &all).map(|c| c.name.as_str()),
            Some("model")
        );
        assert!(match_command("/unknown", &all).is_none());
        assert!(match_command("status", &all).is_none());
        assert!(takes_arguments(&all[2]) && !takes_arguments(&all[0]));
    }
}
