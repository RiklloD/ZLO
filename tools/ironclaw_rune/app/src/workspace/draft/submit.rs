//! Sending from the new-thread screen and the quick composer: each send is
//! a [`Submission`] fixed at Enter, and its reply only clears the draft it
//! came from while that draft is unedited (GPUI `send_draft_as`).

use ironclaw_gpui_client::attachments::StagedFile;

use super::{DraftState, inbox, record};
use crate::chat::inline_files;
use crate::drafts;
use crate::jobs::{self, Generation};
use crate::state::State;

/// Where a new-thread submission came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Origin {
    /// The new-thread screen's draft, as opened at `generation`.
    Draft { generation: Generation },
    /// The quick composer: never clears or fills the draft.
    Quick,
}

/// Everything one new-thread send carries, fixed at Enter: edits made while
/// the thread is being created belong to the draft, not to this message.
pub(super) struct Submission {
    origin: Origin,
    /// The composer's text and cards as sent (`chat::send_text` re-derives
    /// the outgoing message from them).
    content: String,
    pills: Vec<(std::ops::Range<usize>, String)>,
    files: Vec<StagedFile>,
    profile: String,
    pick: Option<(String, String)>,
    effort: Option<String>,
    worktree: Option<Option<String>>,
}

impl Submission {
    pub(super) fn of(
        s: &State,
        draft: &DraftState,
        content: String,
        pills: Vec<(std::ops::Range<usize>, String)>,
        files: Vec<StagedFile>,
        origin: Origin,
    ) -> Self {
        let worktree_dir = s
            .prefs
            .last_project
            .clone()
            .map(|p| p.to_string_lossy().into_owned());
        Self {
            origin,
            content,
            pills,
            files,
            profile: draft.profile.clone(),
            pick: crate::chat::model_picker::pending(&draft.picker),
            effort: crate::chat::model_picker::pending_effort(&draft.picker),
            worktree: draft.worktree.then_some(worktree_dir),
        }
    }

    /// Whether `draft` is still the one sent, unedited since.
    fn is_unedited(&self, draft: &DraftState) -> bool {
        self.origin == (Origin::Draft { generation: draft.generation })
            && draft.input.text == self.content
            && inline_files::pills(&draft.input) == self.pills
            && draft.staged.iter().map(|f| f.key).eq(self.files.iter().map(|f| f.key))
    }
}

/// Why the new-thread screen cannot send now (GPUI `send_draft_as`'s copy),
/// or `None` when it can.
fn refusal(draft: Option<&DraftState>) -> Option<&'static str> {
    let Some(draft) = draft else {
        return Some("There is no new-thread screen to send from.");
    };
    let (text, files) = inline_files::outgoing(&draft.input, &draft.staged);
    if text.is_empty() && files.is_empty() {
        return Some("There was nothing to send.");
    }
    draft.sending.then_some("A new thread is still being created; try again.")
}

/// [`send`] with talk mode's claim already taken.
pub(super) fn send_with(s: &mut State, voice: Option<crate::voice::Submission>) {
    let refuse = |s: &mut State, why: &str| {
        if let Some(sub) = voice {
            crate::voice::submission_refused(s, sub, why);
        }
    };
    if let Some(why) = refusal(s.workspace.draft.as_ref()) {
        return refuse(s, why);
    }
    let Some(draft) = s.workspace.draft.as_ref() else { return };
    let origin = Origin::Draft { generation: draft.generation };
    let submission = Submission::of(
        s,
        draft,
        draft.input.text.clone(),
        inline_files::pills(&draft.input),
        draft.staged.clone(),
        origin,
    );
    if let Some(draft) = s.workspace.draft.as_mut() {
        draft.sending = true;
        draft.error = None;
    }
    submit(s, submission, voice);
}

/// Create the thread in the submission's profile, apply its model pick,
/// open it and send the first message there.
pub(super) fn submit(s: &mut State, sub: Submission, voice: Option<crate::voice::Submission>) {
    let profile = sub.profile.clone();
    let pick = sub.pick.clone();
    let effort = sub.effort.clone();
    let worktree = sub.worktree.clone();
    jobs::run(
        s,
        "new thread",
        move |c| async move {
            let id = match c.create_profile_thread(&profile).await {
                Ok(created) => created.thread_id,
                // A gateway without profiles creates a plain thread.
                Err(e) if e.is_not_found() => c.create_thread().await?.thread.thread_id,
                Err(e) => return Err(e),
            };
            match (pick, effort) {
                (Some((provider, model)), effort) => {
                    c.set_thread_route(&id, &provider, &model, effort.as_deref())
                        .await?;
                }
                (None, Some(effort)) => {
                    c.set_thread_effort(&id, &effort).await?;
                }
                (None, None) => {}
            }
            // Made before the first message, so every turn runs there.
            if let Some(source) = worktree {
                c.start_thread_worktree(&id, source.as_deref()).await?;
            }
            Ok(id)
        },
        move |s, result, tx| match result {
            Ok(id) => {
                settle(s, &sub, None);
                inbox::refresh(s);
                crate::workspace::open_thread(s, id.clone());
                if crate::workspace::active_id(s) == Some(id.as_str()) {
                    crate::chat::send_text(s, sub.content, sub.pills, sub.files, voice, tx);
                } else if let Some(sub) = voice {
                    crate::voice::submission_refused(s, sub, "The new thread did not open.");
                }
            }
            Err(e) => {
                if let Some(v) = voice {
                    crate::voice::submission_refused(s, v, &e.to_string());
                }
                settle(s, &sub, Some(e.to_string()));
            }
        },
    );
}

/// The draft once `sub`'s thread was created (`error` `None`) or failed.
///
/// Created: the draft goes (and its saved copy) only when it is still the
/// one sent, unedited; a draft edited meanwhile, or any draft for a quick
/// submission, stays as it is. Failed: the draft that sent gets its Send
/// back and the error; a quick submission's text goes into the draft only
/// when the draft is empty, else the error names it, so nothing is lost and
/// nothing typed is overwritten.
pub(super) fn settle(s: &mut State, sub: &Submission, error: Option<String>) {
    let Some(draft) = s.workspace.draft.as_mut() else { return };
    let mine = sub.origin == (Origin::Draft { generation: draft.generation });
    match error {
        None if sub.is_unedited(draft) => {
            s.workspace.draft = None;
            drafts::remove(s, drafts::NEW_THREAD);
        }
        None => {
            if mine {
                draft.sending = false;
            }
        }
        Some(e) if mine => {
            draft.sending = false;
            draft.error = Some(e);
        }
        Some(e) if sub.origin == Origin::Quick => {
            if draft.input.text.trim().is_empty() && draft.staged.is_empty() {
                draft.input.replace_text(sub.content.clone());
                draft.error = Some(e);
                record(s);
            } else {
                draft.error = Some(format!("{e} The quick message was not sent: \u{201c}{}\u{201d}", sub.content));
            }
        }
        // A draft replaced since: its own state is not this send's.
        Some(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::draft::{empty_draft, quick_send};

    fn with_draft(text: &str) -> State {
        let mut s = crate::chat::fixtures::test_state();
        let mut draft = empty_draft("p");
        draft.generation = s.workspace.draft_opens.bump();
        draft.input.replace_text(text);
        s.workspace.draft = Some(draft);
        s
    }

    fn sent(s: &State) -> Submission {
        let Some(d) = s.workspace.draft.as_ref() else { panic!("no draft") };
        let origin = Origin::Draft { generation: d.generation };
        Submission::of(s, d, d.input.text.clone(), inline_files::pills(&d.input), d.staged.clone(), origin)
    }

    #[test]
    fn a_refused_send_says_why_in_gpuis_words() {
        assert_eq!(refusal(None), Some("There is no new-thread screen to send from."));
        let s = with_draft("  ");
        assert_eq!(refusal(s.workspace.draft.as_ref()), Some("There was nothing to send."));
        let mut s = with_draft("hello");
        assert_eq!(refusal(s.workspace.draft.as_ref()), None);
        if let Some(d) = s.workspace.draft.as_mut() {
            d.sending = true;
        }
        assert_eq!(
            refusal(s.workspace.draft.as_ref()),
            Some("A new thread is still being created; try again.")
        );
    }

    #[test]
    fn a_created_thread_clears_only_the_unedited_draft_that_sent() {
        let mut s = with_draft("hello");
        let sub = sent(&s);
        settle(&mut s, &sub, None);
        assert!(s.workspace.draft.is_none(), "the sent draft goes");

        let mut s = with_draft("hello");
        let sub = sent(&s);
        if let Some(d) = s.workspace.draft.as_mut() {
            d.sending = true;
            d.input.replace_text("hello, and more typed while sending");
        }
        settle(&mut s, &sub, None);
        let d = s.workspace.draft.as_ref().expect("edited draft kept");
        assert_eq!(d.input.text, "hello, and more typed while sending");
        assert!(!d.sending);

        // A draft replaced since (new generation) is not the one sent.
        let mut s = with_draft("hello");
        let sub = sent(&s);
        if let Some(d) = s.workspace.draft.as_mut() {
            d.generation = Generation(d.generation.0 + 1);
        }
        settle(&mut s, &sub, None);
        assert!(s.workspace.draft.is_some());
    }

    #[test]
    fn a_failed_send_keeps_the_draft_and_gives_back_send() {
        let mut s = with_draft("hello");
        let sub = sent(&s);
        if let Some(d) = s.workspace.draft.as_mut() {
            d.sending = true;
        }
        settle(&mut s, &sub, Some("boom".into()));
        let d = s.workspace.draft.as_ref().expect("draft kept");
        assert_eq!((d.input.text.as_str(), d.sending, d.error.as_deref()), ("hello", false, Some("boom")));
    }

    #[test]
    fn a_quick_send_never_touches_the_saved_draft() {
        let mut s = with_draft("my unsent draft");
        if let Some(d) = s.workspace.draft.as_mut() {
            d.sending = true;
        }
        quick_send(&mut s, "from the quick composer".into(), None);
        let d = s.workspace.draft.as_ref().expect("draft kept");
        assert_eq!(d.input.text, "my unsent draft");
        assert!(d.sending, "the draft's own pending send is untouched");

        // Its reply, either way, leaves the draft alone.
        let quick = Submission::of(&s, d, "q".into(), Vec::new(), Vec::new(), Origin::Quick);
        settle(&mut s, &quick, None);
        settle(&mut s, &quick, Some("boom".into()));
        let d = s.workspace.draft.as_ref().expect("draft kept");
        assert_eq!(d.input.text, "my unsent draft");
        assert!(d.error.as_deref().is_some_and(|e| e.contains("\u{201c}q\u{201d}")));

        // An empty draft takes a failed quick message back.
        let mut s = with_draft("");
        let d = s.workspace.draft.as_ref().expect("draft");
        let quick = Submission::of(&s, d, "q".into(), Vec::new(), Vec::new(), Origin::Quick);
        settle(&mut s, &quick, Some("boom".into()));
        assert_eq!(s.workspace.draft.as_ref().map(|d| d.input.text.as_str()), Some("q"));
    }
}
