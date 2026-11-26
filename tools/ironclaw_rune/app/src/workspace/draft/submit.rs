//! Sending from the new-thread screen and the quick composer: each send is a
//! [`Submission`] fixed at Enter, and its reply only clears the draft it came
//! from while that draft is unedited.

use zlo_client::ClientError;
use zlo_client::attachments::StagedFile;

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
        // A profile with its own git folder branches from that folder (the
        // backend's default); others from the last project folder used.
        let profile_is_git = s
            .workspace
            .inbox
            .profile(&draft.profile)
            .is_some_and(|p| p.git_branch.is_some());
        let worktree_dir = if profile_is_git {
            None
        } else {
            s.prefs
                .last_project
                .clone()
                .map(|p| p.to_string_lossy().into_owned())
        };
        Self {
            origin,
            content,
            pills,
            files,
            profile: draft.profile.clone(),
            pick: crate::chat::model_picker::pending(&draft.picker),
            effort: crate::chat::model_picker::pending_effort(&draft.picker),
            worktree: super::in_worktree(s, draft).then_some(worktree_dir),
        }
    }

    /// Whether `draft` is still the one sent, unedited since.
    fn is_unedited(&self, draft: &DraftState) -> bool {
        self.origin
            == (Origin::Draft {
                generation: draft.generation,
            })
            && draft.input.text == self.content
            && inline_files::pills(&draft.input) == self.pills
            && draft
                .staged
                .iter()
                .map(|f| f.key)
                .eq(self.files.iter().map(|f| f.key))
    }
}

/// Why the new-thread screen cannot send now, or `None` when it can.
fn refusal(draft: Option<&DraftState>) -> Option<&'static str> {
    let Some(draft) = draft else {
        return Some("There is no new-thread screen to send from.");
    };
    let (text, files) = inline_files::outgoing(&draft.input, &draft.staged);
    if text.is_empty() && files.is_empty() {
        return Some("There was nothing to send.");
    }
    draft
        .sending
        .then_some("A new thread is still being created; try again.")
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
    // A triage draft waits for the computer in use to prepare its session.
    if let Some(why) = super::super::triage::send_block(s) {
        if let Some(draft) = s.workspace.draft.as_mut() {
            draft.error = Some(why.clone());
        }
        return refuse(s, &why);
    }
    if let Some(Err(why)) = s
        .workspace
        .draft
        .as_ref()
        .map(|d| crate::remote::check_outgoing(s, &d.staged))
    {
        if let Some(draft) = s.workspace.draft.as_mut() {
            draft.error = Some(why.clone());
        }
        return refuse(s, &why);
    }
    let Some(draft) = s.workspace.draft.as_ref() else {
        return;
    };
    let origin = Origin::Draft {
        generation: draft.generation,
    };
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
        move |c| async move { create(&c, &profile, pick, effort, worktree).await },
        move |s, result, tx| match result {
            Ok(id) => {
                // The person sent it: this reply is a queued update, which
                // Rune lands at once (no flight, no card entering) unless it
                // asks for motion.
                tx.animated();
                settle(s, &sub, None);
                s.workspace.started = Some((id.clone(), sub.profile.clone()));
                inbox::refresh_shown(s);
                crate::workspace::open_thread(s, id.clone());
                if crate::workspace::active_id(s) == Some(id.as_str()) {
                    crate::chat::send_text(s, sub.content, sub.pills, sub.files, voice, tx);
                } else if let Some(sub) = voice {
                    crate::voice::submission_refused(s, sub, "The new thread did not open.");
                }
                // The hero composer flies to the chat's dock on its own spring;
                // only this commit uses it. Set last: a matched flight takes
                // the commit's spring over its element's, and the send asked
                // for the prompt rise's. Nothing here needs that one: the new
                // chat's transcript mounts in this same commit, so the prompt
                // has no earlier place to climb from (the rise is the fade
                // `prompt_rise` draws by time, which no commit spring touches).
                tx.motion(crate::design::motion::DOCK);
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

/// Create the thread on the gateway: in `profile`, on the picked route, in
/// its own worktree when `worktree` asks for one (`Some(source)`, `None`
/// source: the profile's folder). Answers its id.
///
/// A worktree the backend refuses (the model's runtime cannot run in one,
/// the folder is not a git checkout) fails the send with a plain reason;
/// the draft keeps its text to send again. The thread made for it stays:
/// another client may already be using it, so it is never deleted here
/// (the draft screen offers a worktree only where it can be made).
async fn create(
    c: &zlo_client::Client,
    profile: &str,
    pick: Option<(String, String)>,
    effort: Option<String>,
    worktree: Option<Option<String>>,
) -> Result<String, ClientError> {
    let id = match c.create_profile_thread(profile).await {
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
    // Made before the first message, so every turn runs there; only on a
    // runtime that can work there. The thread's own route decides, not the
    // screen's pill, which may still show the previous profile's default.
    let worktree = match worktree {
        Some(_) if runtime_refuses_worktrees(c, &id).await => {
            tracing::debug!("new thread's runtime cannot run in a worktree; it works Local");
            None
        }
        other => other,
    };
    if let Some(source) = worktree
        && let Err(e) = c.start_thread_worktree(&id, source.as_deref()).await
    {
        return Err(match e {
            ClientError::Refused { code, .. } if code == "worktree_unavailable" => {
                ClientError::Refused {
                    code,
                    message: "This model can't run in its own worktree. Switch to Local, \
                              or pick a model that can."
                        .into(),
                }
            }
            e => e,
        });
    }
    Ok(id)
}

/// The gateway says thread `id`'s runtime cannot run in a worktree. An
/// unreadable route, or a gateway that does not say, is not a refusal: the
/// worktree is asked for and the gateway answers.
async fn runtime_refuses_worktrees(c: &zlo_client::Client, id: &str) -> bool {
    match c.thread_route(id).await {
        Ok(route) => {
            route
                .providers
                .iter()
                .find(|p| p.runtime == route.runtime)
                .and_then(|p| p.worktrees)
                == Some(false)
        }
        Err(e) => {
            tracing::debug!(error = %e, "route unreadable before the worktree");
            false
        }
    }
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
    let Some(draft) = s.workspace.draft.as_mut() else {
        return;
    };
    let mine = sub.origin
        == (Origin::Draft {
            generation: draft.generation,
        });
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
                draft.error = Some(format!(
                    "{e} The quick message was not sent: \u{201c}{}\u{201d}",
                    sub.content
                ));
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
        let Some(d) = s.workspace.draft.as_ref() else {
            panic!("no draft")
        };
        let origin = Origin::Draft {
            generation: d.generation,
        };
        Submission::of(
            s,
            d,
            d.input.text.clone(),
            inline_files::pills(&d.input),
            d.staged.clone(),
            origin,
        )
    }

    /// The Local / New worktree menu under the card decides where the new
    /// thread works: Local sends no worktree; New worktree branches from the
    /// profile's own git folder (the backend's default source), or, for a
    /// profile without one, from the last project folder used.
    #[test]
    fn the_worktree_menu_decides_where_the_thread_works() {
        use crate::workspace::draft::set_worktree;
        let mut s = with_draft("hello");
        crate::workspace::fixtures::fixture_inbox(&mut s);
        s.workspace.draft = Some({
            let mut d = empty_draft("work");
            d.input.replace_text("hello");
            d
        });
        assert_eq!(sent(&s).worktree, None, "Local by default");
        set_worktree(&mut s, true);
        assert_eq!(sent(&s).worktree, Some(None), "from the profile's folder");
        set_worktree(&mut s, false);
        assert_eq!(sent(&s).worktree, None, "back to Local");

        // A model whose runtime the gateway says cannot run in a worktree
        // sends Local, whatever the switch says; Claude CLI can.
        set_worktree(&mut s, true);
        if let Some(d) = s.workspace.draft.as_mut() {
            d.picker = crate::chat::model_picker::fixture_matrix();
            d.picker.current = Some(("codex".into(), "gpt-6.1-sol".into()));
        }
        assert_eq!(sent(&s).worktree, None, "Codex: Local");
        if let Some(d) = s.workspace.draft.as_mut() {
            d.picker.current = Some(("claude-cli".into(), "claude-opus-5-5".into()));
        }
        assert_eq!(sent(&s).worktree, Some(None), "Claude CLI: its worktree");
        if let Some(d) = s.workspace.draft.as_mut() {
            d.picker = Default::default();
        }

        // A profile with no git folder and no project folder used (a draft
        // carried from another computer can land there): nothing to branch
        // from, so Local, whatever the switch says.
        s.prefs.last_project = None;
        s.workspace.draft = Some({
            let mut d = empty_draft("default");
            d.input.replace_text("hello");
            d.worktree = Some(true);
            d
        });
        assert_eq!(sent(&s).worktree, None, "nothing to branch from");

        s.prefs.last_project = Some("/Users/you/code/site".into());
        s.workspace.draft = Some({
            let mut d = empty_draft("default");
            d.input.replace_text("hello");
            d
        });
        set_worktree(&mut s, true);
        assert_eq!(
            sent(&s).worktree,
            Some(Some("/Users/you/code/site".into())),
            "no git folder of its own: the last project folder"
        );
    }

    /// A worktree the backend refuses (the model's runtime cannot run in
    /// one) fails the send with a plain reason, and nothing is deleted (the
    /// thread may already be in use elsewhere); a Local send never asks for
    /// a worktree.
    #[test]
    fn a_refused_worktree_says_why_and_deletes_nothing() {
        use crate::test_gateway::{Answer, Gateway};
        let gateway = Gateway::start(vec![Answer::ok("{}")]);
        gateway.route(
            "/profiles/work/threads",
            Answer::ok(r#"{"profile_id":"work","thread_id":"t9"}"#),
        );
        gateway.route(
            "/threads/t9/worktree",
            Answer::ok(
                r#"{"ok":false,"error":"worktree_unavailable","message":"Runtime ironclaw cannot run a thread in a worktree (t3_capabilities)."}"#,
            ),
        );
        // The backend owns a runtime of its own: it must drop outside this one.
        let backend = gateway.backend();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let c = backend.client();

        let refused = rt.block_on(create(c, "work", None, None, Some(None)));
        let Err(ClientError::Refused { code, message }) = refused else {
            panic!("refused: {refused:?}")
        };
        assert_eq!(code, "worktree_unavailable");
        assert!(message.starts_with("This model can't run in its own worktree"));
        assert!(!message.contains("t3_capabilities"), "{message}");
        assert_eq!(gateway.requests_to("/threads/t9/worktree"), 1);
        assert_eq!(gateway.requests_to("/threads/t9"), 0, "nothing is deleted");

        let local = rt.block_on(create(c, "work", None, None, None));
        assert_eq!(local.ok().as_deref(), Some("t9"));
        assert_eq!(
            gateway.requests_to("/threads/t9/worktree"),
            1,
            "Local asks none"
        );
        assert_eq!(gateway.requests_to("/threads/t9"), 0, "nor deleted");

        // The new thread's own route decides: created on a runtime that
        // cannot run in a worktree (the pill still showed another profile's
        // default), it works Local and no worktree is asked for.
        gateway.route(
            "/threads/t9/route",
            Answer::ok(
                r#"{"threadId":"t9","runtime":"codex","provider":"codex","providers":[{"provider":"codex","name":"Codex","runtime":"codex","worktrees":false},{"provider":"claude-cli","name":"Claude CLI","runtime":"claude-cli","worktrees":true}]}"#,
            ),
        );
        let switched = rt.block_on(create(c, "work", None, None, Some(None)));
        assert_eq!(switched.ok().as_deref(), Some("t9"));
        assert_eq!(
            gateway.requests_to("/threads/t9/worktree"),
            1,
            "no worktree asked on Codex"
        );
        // On a runtime that can, it is asked for.
        gateway.route(
            "/threads/t9/route",
            Answer::ok(
                r#"{"threadId":"t9","runtime":"claude-cli","providers":[{"provider":"claude-cli","name":"Claude CLI","runtime":"claude-cli","worktrees":true}]}"#,
            ),
        );
        gateway.route(
            "/threads/t9/worktree",
            Answer::ok(r#"{"path":"/wt/t9","branch":"zlo/t9","state":"active","exists":true}"#),
        );
        let made = rt.block_on(create(c, "work", None, None, Some(None)));
        assert_eq!(made.ok().as_deref(), Some("t9"));
        assert_eq!(gateway.requests_to("/threads/t9/worktree"), 2);
    }

    #[test]
    fn a_refused_send_says_why() {
        assert_eq!(
            refusal(None),
            Some("There is no new-thread screen to send from.")
        );
        let s = with_draft("  ");
        assert_eq!(
            refusal(s.workspace.draft.as_ref()),
            Some("There was nothing to send.")
        );
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
        assert_eq!(
            (d.input.text.as_str(), d.sending, d.error.as_deref()),
            ("hello", false, Some("boom"))
        );
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
        assert!(
            d.error
                .as_deref()
                .is_some_and(|e| e.contains("\u{201c}q\u{201d}"))
        );

        // An empty draft takes a failed quick message back.
        let mut s = with_draft("");
        let d = s.workspace.draft.as_ref().expect("draft");
        let quick = Submission::of(&s, d, "q".into(), Vec::new(), Vec::new(), Origin::Quick);
        settle(&mut s, &quick, Some("boom".into()));
        assert_eq!(
            s.workspace.draft.as_ref().map(|d| d.input.text.as_str()),
            Some("q")
        );
    }
}
