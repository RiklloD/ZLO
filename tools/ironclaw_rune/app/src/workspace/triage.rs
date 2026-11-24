//! "Triage an issue" (⌘K; t3code's in-app `t3 triage`): the backend writes
//! a triage session and starts fetching the source of its own build, then
//! the new-thread draft opens with the playbook prompt linked as a file
//! chip. The person writes what went wrong after it and picks any model;
//! the agent reads the logs and source there and asks before it fixes or
//! posts anything (`docs/using/triage.mdx`).
//!
//! The session lives on the computer the app is connected to (ZLO talks to
//! one computer at a time, so that is the triage machine). The draft's
//! triage state ([`TriageDraft`]) is explicit and kept beside the draft, not
//! read back from its chip: a switch to another computer clears the draft
//! and saves drafts per computer, so the triage draft is carried across
//! ([`carry`]), put back on the new computer ([`connected`]), and that
//! computer prepares its own session, whose link replaces the chip's. Send
//! waits until the chip links a session on the computer in use
//! ([`send_block`]).

use std::ops::Range;

use zlo_client::ClientError;
use zlo_client::attachments::StagedFile;
use zlo_client::mentions::{self, MentionUri};
use zlo_client::triage_calls::{
    PrepareTriageResponse, TRIAGE_CLI_COMMAND, TRIAGE_PROMPT_FILE_NAME, TriageClient,
};

use super::draft;
use crate::chat::inline_files;
use crate::jobs::{self, Generation};
use crate::state::State;

/// A triage draft's own state.
#[derive(Clone)]
pub(crate) struct TriageDraft {
    /// The new-thread draft this belongs to; any other draft is not one.
    generation: Generation,
    link: Link,
    /// The draft as it was when the app left its computer, for the next.
    carried: Option<Carried>,
}

/// Whether the chip links a session the computer in use prepared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Link {
    /// The computer in use is preparing its session.
    Preparing,
    /// The chip links a session on this computer (its key).
    Ready { computer: String },
    /// That computer could not prepare one; the line says what to do.
    Failed(String),
}

#[derive(Clone)]
struct Carried {
    profile: String,
    text: String,
    pills: Vec<(Range<usize>, String)>,
    staged: Vec<StagedFile>,
    worktree: Option<bool>,
}

/// The triage state of the open new-thread draft, if it is one.
fn current(s: &State) -> Option<&TriageDraft> {
    let generation = s.workspace.draft.as_ref()?.generation;
    s.workspace
        .triage
        .as_ref()
        .filter(|t| t.generation == generation)
}

#[cfg(test)]
pub(super) fn link(s: &State) -> Option<Link> {
    current(s).map(|t| t.link.clone())
}

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

/// Whether the new-thread draft is a triage draft: started as one, and
/// its chip is still there (or not there yet).
pub(super) fn is_triage_draft(s: &State) -> bool {
    let (Some(t), Some(d)) = (current(s), s.workspace.draft.as_ref()) else {
        return false;
    };
    playbook_pill(&d.input).is_some() || !matches!(t.link, Link::Ready { .. })
}

/// Why Send waits, if it does: the chip must link a session the computer
/// in use prepared.
pub(super) fn send_block(s: &State) -> Option<String> {
    let t = current(s)?;
    if !is_triage_draft(s) {
        return None;
    }
    match &t.link {
        Link::Ready { computer } if *computer == s.onboarding.computer.key() => None,
        Link::Ready { .. } | Link::Preparing => Some(format!(
            "{} is still preparing the triage session; Send works once it is ready.",
            machine(s)
        )),
        // Only a chip that links no session here blocks: without one the
        // person's words send as an ordinary message (the error line still
        // points at `zlo triage`), never stranded.
        Link::Failed(why) => {
            let chip = s
                .workspace
                .draft
                .as_ref()
                .is_some_and(|d| playbook_pill(&d.input).is_some());
            chip.then(|| why.clone())
        }
    }
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
    let Some(generation) = s.workspace.draft.as_ref().map(|d| d.generation) else {
        return;
    };
    s.workspace.triage = Some(TriageDraft {
        generation,
        link: Link::Preparing,
        carried: None,
    });
    prepare(s);
}

/// The app is leaving its computer (the draft is about to be cleared):
/// keep an unsent triage draft to put back on the next one.
pub(super) fn carry(s: &mut State) {
    if !is_triage_draft(s) {
        s.workspace.triage = None;
        return;
    }
    let (Some(d), Some(t)) = (s.workspace.draft.as_ref(), s.workspace.triage.as_mut()) else {
        return;
    };
    t.carried = Some(Carried {
        profile: d.profile.clone(),
        text: d.input.text.clone(),
        pills: inline_files::pills(&d.input),
        staged: d.staged.clone(),
        worktree: d.worktree,
    });
    // The chip links the old computer's session: not sendable here.
    t.link = Link::Preparing;
}

/// Connected (to a new computer, or again): a carried triage draft comes
/// back and that computer prepares its session. Answers whether the draft
/// was put back, so the connect does not open another screen over it. A
/// draft still waiting on a reply the reconnect dropped asks again.
pub(super) fn connected(s: &mut State) -> bool {
    let Some(carried) = s.workspace.triage.as_mut().and_then(|t| t.carried.take()) else {
        // Still waiting on a reply the reconnect dropped, or failed (a
        // gateway blip, the rate limit): ask this computer again.
        let unready = current(s).is_some_and(|t| !matches!(t.link, Link::Ready { .. }));
        if unready && is_triage_draft(s) {
            if let Some(t) = s.workspace.triage.as_mut() {
                t.link = Link::Preparing;
            }
            prepare(s);
        }
        return false;
    };
    let generation = draft::restore(
        s,
        carried.profile,
        &carried.text,
        &carried.pills,
        carried.staged,
        carried.worktree,
    );
    if let Some(t) = s.workspace.triage.as_mut() {
        t.generation = generation;
        t.link = Link::Preparing;
    }
    prepare(s);
    true
}

fn prepare(s: &mut State) {
    let Some(generation) = current(s).map(|t| t.generation) else {
        return;
    };
    let computer = s.onboarding.computer.key();
    jobs::run(
        s,
        "prepare triage",
        move |c| async move { c.prepare_triage(client()).await },
        move |s, result, _tx| prepared(s, generation, &computer, result),
    );
}

/// The backend answered: fill the draft and allow Send, or say what to do.
pub(super) fn prepared(
    s: &mut State,
    generation: Generation,
    computer: &str,
    result: Result<PrepareTriageResponse, ClientError>,
) {
    // Asked of another computer, or for another draft: not this draft's.
    if s.onboarding.computer.key() != computer {
        return;
    }
    let ws = &mut s.workspace;
    let (Some(t), Some(d)) = (
        ws.triage.as_mut().filter(|t| t.generation == generation),
        ws.draft.as_mut().filter(|d| d.generation == generation),
    ) else {
        return;
    };
    let failure = match result {
        Ok(prepared) if fill(&mut d.input, &prepared.prompt) => None,
        // A link the composer cannot make a chip of (another platform's
        // path): the terminal still works.
        Ok(_) => Some(failure_text(
            "that computer's playbook link cannot be opened here",
            false,
        )),
        Err(error) => Some(failure_text(&error.to_string(), error.is_not_found())),
    };
    match failure {
        None => {
            t.link = Link::Ready {
                computer: computer.to_string(),
            };
            d.error = None;
            draft::record(s);
        }
        Some(why) => {
            t.link = Link::Failed(why.clone());
            d.error = Some(why);
        }
    }
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
/// keeping everything the person wrote. `false` (nothing changed) when the
/// prompt links no playbook file the composer can show as a chip.
pub(super) fn fill(input: &mut rune::TextEdit, prompt: &str) -> bool {
    let (plain, chips) = inline_files::restored(prompt);
    let Some((_, new_id)) = chips.iter().find(|(_, id)| {
        matches!(
            mentions::token_mention(id),
            Some(MentionUri::File { path, .. })
                if path.rsplit(['/', '\\']).next() == Some(TRIAGE_PROMPT_FILE_NAME)
        )
    }) else {
        tracing::debug!("triage: the prepared prompt links no playbook file");
        return false;
    };
    if let Some((range, _)) = playbook_pill(input) {
        let label = input
            .text
            .get(range.clone())
            .unwrap_or_default()
            .to_string();
        input.replace_with_pill(range, new_id.clone(), &label);
        return true;
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
    true
}

/// The computer in use, as the hero names it.
fn machine(s: &State) -> String {
    s.remote
        .descriptor
        .as_ref()
        .map(|d| format!("{} (v{})", d.label, d.server_version))
        .unwrap_or_else(|| "This computer".into())
}

/// The hero's line under "What went wrong?": which computer, and the
/// terminal fallback.
pub(super) fn hero_line(s: &State) -> String {
    let machine = machine(s);
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
        assert!(fill(
            &mut input,
            &prompt("/Users/a/.zlo/triage/1/triage-playbook.md"),
        ));
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
        assert!(fill(&mut input, &prompt("/a/triage-playbook.md")));
        assert_eq!(
            input.text,
            "Read @triage-playbook.md and follow it. agy will not sign in"
        );
    }

    #[test]
    fn another_computers_session_swaps_only_the_link() {
        let mut input = rune::TextEdit::new("");
        assert!(fill(
            &mut input,
            &prompt("/Users/a/.zlo/triage/1/triage-playbook.md"),
        ));
        let end = input.text.len();
        input.set_caret(end);
        input.insert("it crashes");
        assert!(fill(
            &mut input,
            &prompt("/home/b/.zlo/triage/2/triage-playbook.md"),
        ));
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

    /// A prompt with no playbook chip (another platform's link the
    /// composer cannot read) changes nothing and is reported.
    #[test]
    fn a_prompt_without_a_playbook_chip_is_refused() {
        let mut input = rune::TextEdit::new("");
        input.replace_text("it crashes");
        assert!(!fill(
            &mut input,
            "Read C:\\Users\\a\\triage-playbook.md and follow it. "
        ));
        assert_eq!(input.text, "it crashes");
    }

    #[test]
    fn a_failure_points_at_the_terminal() {
        let text = failure_text("cannot reach the IronClaw gateway", false);
        assert!(text.contains("zlo triage"));
        assert!(failure_text("", true).contains("too old"));
    }
}
