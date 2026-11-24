//! "Triage an issue" (⌘K; t3code's in-app `t3 triage`): the backend writes
//! a triage session and starts fetching the source of its own build, then
//! the new-thread draft opens with the playbook prompt linked as a file
//! chip. The person writes what went wrong after it and picks any model;
//! the agent reads the logs and source there and asks before it fixes or
//! posts anything (`docs/using/triage.mdx`).
//!
//! The session lives on the computer the app is connected to (ZLO talks to
//! one computer at a time, so that is the triage machine). Switching to
//! another computer while the draft is still unsent moves it: that
//! computer prepares its own session and the chip's link is swapped.

use std::ops::Range;

use zlo_client::mentions::{self, MentionUri};
use zlo_client::triage_calls::{TRIAGE_CLI_COMMAND, TRIAGE_PROMPT_FILE_NAME, TriageClient};

use super::draft;
use crate::chat::inline_files;
use crate::jobs;
use crate::state::State;

/// The playbook chip in a composer, if it has one: its range and token id.
pub(super) fn playbook_pill(input: &rune::TextEdit) -> Option<(Range<usize>, String)> {
    inline_files::pills(input).into_iter().find(|(_, id)| {
        matches!(
            mentions::token_mention(id),
            Some(MentionUri::File { path, .. })
                if path.rsplit(['/', '\\']).next() == Some(TRIAGE_PROMPT_FILE_NAME)
        )
    })
}

/// Whether the new-thread draft is a triage draft (its chip is still there).
pub(super) fn is_triage_draft(s: &State) -> bool {
    s.workspace
        .draft
        .as_ref()
        .is_some_and(|d| playbook_pill(&d.input).is_some())
}

fn client() -> TriageClient {
    TriageClient {
        label: Some("ZLO desktop".into()),
        app_version: Some(env!("CARGO_PKG_VERSION").into()),
        os: Some(format!(
            "{} {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )),
    }
}

/// ⌘K → "Triage an issue": open the draft now (its hero says what is
/// happening) and fill it when the backend answers.
pub(super) fn start(s: &mut State) {
    draft::start_in_profile(s, zlo_client::profiles::DEFAULT_PROFILE_ID.into());
    prepare(s);
}

/// The draft moved to another computer: give it that computer's session.
pub(super) fn connected(s: &mut State) {
    if is_triage_draft(s) {
        prepare(s);
    }
}

fn prepare(s: &mut State) {
    let generation = s.workspace.draft.as_ref().map(|d| d.generation);
    jobs::run(
        s,
        "prepare triage",
        move |c| async move { c.prepare_triage(client()).await },
        move |s, result, _tx| {
            let Some(d) = s.workspace.draft.as_mut() else {
                return;
            };
            // A reset or send must not fill a different draft.
            if Some(d.generation) != generation {
                return;
            }
            match result {
                Ok(prepared) => {
                    d.error = None;
                    fill(&mut d.input, &prepared.prompt);
                    draft::record(s);
                }
                Err(error) => {
                    d.error = Some(failure_text(&error.to_string(), error.is_not_found()));
                }
            }
        },
    );
}

/// The draft's error line when the backend could not start triage.
fn failure_text(error: &str, too_old: bool) -> String {
    let why = if too_old {
        "That backend is too old for in-app triage.".to_string()
    } else {
        format!("Could not start triage: {error}.")
    };
    format!("{why} Run `{TRIAGE_CLI_COMMAND}` in a terminal on that computer instead.")
}

/// Put `prompt` (text with the playbook link) in front of what the
/// composer holds, or swap the link of a triage draft already there,
/// keeping everything the person wrote.
pub(super) fn fill(input: &mut rune::TextEdit, prompt: &str) {
    let (plain, chips) = inline_files::restored(prompt);
    let Some((_, new_id)) = chips.iter().find(|(_, id)| {
        mentions::token_mention(id).is_some_and(|uri| matches!(uri, MentionUri::File { .. }))
    }) else {
        tracing::debug!("triage: the prepared prompt links no playbook file");
        return;
    };
    if let Some((range, _)) = playbook_pill(input) {
        let label = input
            .text
            .get(range.clone())
            .unwrap_or_default()
            .to_string();
        input.replace_with_pill(range, new_id.clone(), &label);
        return;
    }
    let shift = plain.len();
    let mut pills = chips.clone();
    pills.extend(
        inline_files::pills(input)
            .into_iter()
            .map(|(range, id)| (range.start + shift..range.end + shift, id)),
    );
    let text = format!("{plain}{}", input.text);
    inline_files::set_content(input, &text, &pills);
    // The caret goes after the prompt, where the description starts.
    input.set_caret(shift);
    input.focused = true;
}

/// The hero's line under "What went wrong?": which computer, and the
/// terminal fallback.
pub(super) fn hero_line(s: &State) -> String {
    let machine = s
        .remote
        .descriptor
        .as_ref()
        .map(|d| format!("{} (v{})", d.label, d.server_version))
        .unwrap_or_else(|| "This computer".into());
    format!(
        "{machine}: the agent reads ZLO's logs and source there, and asks before it applies a \
         fix or posts anything. Or run `{TRIAGE_CLI_COMMAND}` in a terminal."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompt(path: &str) -> String {
        let link = MentionUri::File {
            path: path.into(),
            line_range: None,
        }
        .to_link(TRIAGE_PROMPT_FILE_NAME);
        format!("Read {link} and follow it. ")
    }

    #[test]
    fn an_empty_draft_becomes_the_prompt_with_the_caret_after_it() {
        let mut input = rune::TextEdit::new("");
        fill(
            &mut input,
            &prompt("/Users/a/.zlo/triage/1/triage-playbook.md"),
        );
        assert_eq!(input.text, "Read @triage-playbook.md and follow it. ");
        assert!(playbook_pill(&input).is_some());
        let (outgoing, _) = inline_files::outgoing(&input, &[]);
        assert!(
            zlo_client::triage_calls::is_triage_prompt(&outgoing),
            "sends the link: {outgoing}"
        );
    }

    #[test]
    fn what_the_person_wrote_stays_after_the_prompt() {
        let mut input = rune::TextEdit::new("");
        input.replace_text("agy will not sign in");
        fill(&mut input, &prompt("/a/triage-playbook.md"));
        assert_eq!(
            input.text,
            "Read @triage-playbook.md and follow it. agy will not sign in"
        );
    }

    #[test]
    fn another_computers_session_swaps_only_the_link() {
        let mut input = rune::TextEdit::new("");
        fill(
            &mut input,
            &prompt("/Users/a/.zlo/triage/1/triage-playbook.md"),
        );
        let end = input.text.len();
        input.set_caret(end);
        input.insert("it crashes");
        fill(
            &mut input,
            &prompt("/home/b/.zlo/triage/2/triage-playbook.md"),
        );
        assert_eq!(
            input.text,
            "Read @triage-playbook.md and follow it. it crashes"
        );
        let (outgoing, _) = inline_files::outgoing(&input, &[]);
        assert!(
            outgoing.contains("file:///home/b/.zlo/triage/2/triage-playbook.md"),
            "{outgoing}"
        );
        assert!(!outgoing.contains("/Users/a/"), "{outgoing}");
    }

    #[test]
    fn a_failure_points_at_the_terminal() {
        let text = failure_text("cannot reach the IronClaw gateway", false);
        assert!(text.contains("zlo triage"));
        assert!(failure_text("", true).contains("too old"));
    }
}
