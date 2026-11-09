//! What the chat does (GPUI `chat.rs`, the `ChatView` methods): send, stop,
//! retry, gates, slash commands, attachments, opening files. Every function
//! takes the thread id and finds its chat with [`chat_for`], and every async
//! reply checks the chat is still there before it writes.

use std::path::{Path, PathBuf};

use rune::Tx;
use zlo_client::attachments::{self, StagedFile};
use zlo_client::commands::{match_command, takes_arguments};
use zlo_client::mentions as mention;
use zlo_client::product_wire::RebornProductCommandInfo;
use zlo_client::transcript::{AttachmentChip, Row};
use zlo_client::wire::CommandEffect;
use zlo_client::{Client, ClientError, GateDecision};

use super::{Reserve, chat_for, chat_ref, goals, inline_files, menu, set_error};
use crate::jobs;
use crate::state::State;

/// Commands, skills and extensions for the `/` and `@` menu, loaded once per
/// chat. Without the inventory every `/` line is sent as a normal message.
pub(crate) fn load_commands(s: &mut State, id: &str) {
    if let Some(c) = chat_for(s, id) {
        c.menu.current_thread = Some(id.to_string());
    }
    let thread = id.to_string();
    jobs::run(
        s,
        "chat",
        |c| menu::load(c, true),
        move |s, result, _| {
            if let (Ok(sources), Some(c)) = (result, chat_for(s, &thread)) {
                c.menu.sources = std::rc::Rc::new(sources);
            }
        },
    );
}

/// The paperclip: pick a file to attach.
pub(crate) fn pick_files(id: &str, tx: &mut Tx) {
    let thread = id.to_string();
    let options = rune::FileDialogOptions {
        title: "Attach".into(),
        ..Default::default()
    };
    tx.open_file(options, move |s: &mut State, result, _tx| match result {
        Ok(Some(path)) => attach(s, &thread, vec![path]),
        Ok(None) => {}
        Err(e) => set_error(s, &thread, e.to_string()),
    });
}

/// Attach another thread's transcript (its fork source included) as a
/// Markdown file, so this thread can read it. The sidebar drops a dragged
/// thread here (area B).
pub fn attach_thread(s: &mut State, id: &str, other_id: String, title: String) {
    if other_id == id {
        return;
    }
    let Some(c) = chat_ref(s, id) else { return };
    let limits = c.limits.clone();
    let thread = id.to_string();
    jobs::run(
        s,
        "chat",
        move |c| async move { c.thread_attachment(&other_id, &title, &limits).await },
        move |s, result, _| {
            let Some(c) = chat_for(s, &thread) else {
                return;
            };
            let staged = result
                .map_err(|e| e.to_string())
                .and_then(|file| attachments::check(&file, &c.staged, &c.limits).map(|()| file));
            match staged {
                Ok(file) => {
                    c.error = None;
                    place(s, &thread, vec![file]);
                }
                Err(problem) => c.error = Some(problem),
            }
        },
    );
}

/// Stage `files` and put their pills at the composer's caret.
pub fn place(s: &mut State, id: &str, files: Vec<StagedFile>) {
    let Some(c) = chat_ref(s, id) else { return };
    let mut all = c.staged.clone();
    all.extend(files.iter().cloned());
    let mut composer = c.composer.clone();
    inline_files::insert(s, &mut composer, &files, &all);
    if let Some(c) = chat_for(s, id) {
        c.staged = all;
        c.composer = composer;
    }
    record_draft(s, id);
}

/// Files or a picture pasted into the composer.
pub(crate) fn paste(s: &mut State, id: &str, pasted: inline_files::Pasted) {
    match pasted {
        inline_files::Pasted::Paths(paths) => attach(s, id, paths),
        inline_files::Pasted::Image { bytes, mime } => {
            let Some(c) = chat_for(s, id) else { return };
            match inline_files::pasted_image(bytes, mime, &c.staged, &c.limits) {
                Ok(file) => place(s, id, vec![file]),
                Err(problem) => c.error = Some(problem),
            }
        }
    }
}

/// Read files off the main thread, then stage the ones within limits.
pub fn attach(s: &mut State, id: &str, paths: Vec<PathBuf>) {
    let Some(c) = chat_ref(s, id) else { return };
    let limits = c.limits.clone();
    let backend = crate::remote::attachment_backend(s);
    let thread = id.to_string();
    jobs::run(
        s,
        "chat",
        move |_| async move {
            let mut out = Vec::new();
            for path in paths {
                out.push(attachments::stage(&path, &limits, backend).await);
            }
            Ok::<_, ClientError>(out)
        },
        move |s, results, _| {
            let Ok(results) = results else { return };
            let Some(c) = chat_for(s, &thread) else {
                return;
            };
            let mut problems = Vec::new();
            let mut files: Vec<StagedFile> = Vec::new();
            for result in results {
                // Path files have no limit; only uploads are checked.
                let mut staged = c.staged.clone();
                staged.extend(files.iter().cloned());
                match result
                    .and_then(|file| attachments::check(&file, &staged, &c.limits).map(|()| file))
                {
                    Ok(file) => files.push(file),
                    Err(problem) => problems.push(problem),
                }
            }
            c.error = (!problems.is_empty()).then(|| problems.join(" "));
            place(s, &thread, files);
        },
    );
}

/// Save an attachment to a temp folder and open it in its default app.
pub(crate) fn open_attachment(s: &mut State, id: &str, chip: AttachmentChip) {
    let Some(message_id) = chip.message_id.clone() else {
        return;
    };
    let thread_id = id.to_string();
    // The ids come from the gateway: hashed, never joined as a path.
    let dir = super::downloads::dir_for("attachment", &[id, &message_id, &chip.id]);
    let attachment = chip.id.clone();
    open_fetched(s, id, dir, &chip.filename, move |c| async move {
        c.attachment_bytes(&thread_id, &message_id, &attachment)
            .await
    });
}

/// Fetch bytes, write them under the app's temp folder and hand the file
/// to macOS, which opens it in its default app.
pub(crate) fn open_fetched<Fut>(
    s: &mut State,
    id: &str,
    dir: PathBuf,
    name: &str,
    fetch: impl FnOnce(Client) -> Fut + Send + 'static,
) where
    Fut: std::future::Future<Output = Result<Vec<u8>, ClientError>> + Send + 'static,
{
    // The name comes from the gateway; keep only its last component.
    let name = super::downloads::file_name(name);
    let thread = id.to_string();
    jobs::run(
        s,
        "chat",
        move |c| {
            let fetched = fetch(c);
            async move {
                let bytes = fetched.await?;
                super::downloads::save_async(dir, name, bytes)
                    .await
                    .map_err(|e| ClientError::Decode(format!("Couldn't save the file: {e}")))
            }
        },
        move |s, result, _| match result {
            Ok(path) => open_with_system(&path),
            Err(e) => set_error(s, &thread, e.to_string()),
        },
    );
}

/// Hand `path` to macOS, which opens it in its default app. Only for files
/// on this Mac (downloads, this Mac's backend); a path the backend names
/// goes through [`open_backend_file`].
pub fn open_with_system(path: &Path) {
    if let Err(e) = std::process::Command::new("open").arg(path).spawn() {
        tracing::warn!(error = %e, path = %path.display(), "couldn't open the file");
    }
}

/// Open a file by the path the backend gave. On this Mac it opens in its
/// app; on a remote backend the path is on that computer, so it opens in
/// VS Code over SSH to it, never as a path on this Mac.
pub fn open_backend_file(s: &State, path: &Path) {
    if !crate::remote::backend_is_remote(s) {
        return open_with_system(path);
    }
    let Some(url) = crate::remote::ssh_host(s).and_then(|host| editor_over_ssh(&host, path)) else {
        tracing::debug!(path = %path.display(), "remote file has no editor route");
        return;
    };
    if let Err(e) = std::process::Command::new("open").arg(&url).spawn() {
        tracing::warn!(error = %e, "couldn't open the editor over SSH");
    }
}

/// `vscode://vscode-remote/ssh-remote+<host><path>` for an absolute path on
/// the remote computer, percent-encoding what a URL path can't hold.
pub(crate) fn editor_over_ssh(host: &str, path: &Path) -> Option<String> {
    let path = path.to_str().filter(|p| p.starts_with('/'))?;
    let host = Some(host).filter(|h| {
        !h.is_empty()
            && h.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '@'))
    })?;
    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    Some(format!("vscode://vscode-remote/ssh-remote+{host}{encoded}"))
}

/// A command without arguments runs at once; others get `/name ` to finish.
pub(crate) fn pick_command(
    s: &mut State,
    id: &str,
    command: &RebornProductCommandInfo,
    tx: &mut Tx,
) {
    let Some(c) = chat_for(s, id) else { return };
    if takes_arguments(command) {
        c.composer.replace_text(format!("/{} ", command.name));
        c.composer.mark_edited();
        c.composer.focused = true;
    } else {
        c.composer.replace_text(format!("/{}", command.name));
        send(s, id, tx);
    }
}

fn run_command(s: &mut State, id: &str, text: String) {
    let Some(c) = chat_for(s, id) else { return };
    c.composer.clear();
    c.error = None;
    record_draft(s, id);
    let thread_id = id.to_string();
    let req = thread_id.clone();
    let line = text.clone();
    jobs::run(
        s,
        "chat",
        move |c| async move { c.execute_command(&req, &line).await },
        move |s, result, tx| {
            let Some(c) = chat_for(s, &thread_id) else {
                return;
            };
            let effect = result.as_ref().ok().and_then(|o| o.effect.clone());
            c.transcript
                .push_command(&text, result.map_err(|e| e.to_string()));
            super::refresh_rows(s, &thread_id, tx);
            tx.scroll_to_bottom(super::view::transcript_key(&thread_id));
            if let Some(CommandEffect::OpenThread { thread_id }) = effect {
                // `/new`: the gateway created the thread; switch to it.
                crate::workspace::open_thread(s, thread_id);
            }
        },
    );
}

/// The folder `@` files come from and file mentions are read in: the
/// thread's project folder, else the last one used. None while the backend
/// is remote: it can't read this Mac's folders, so `@` offers threads,
/// skills and commands only (no local walk, no local path sent).
pub(crate) fn mention_root(s: &State, id: &str) -> Option<PathBuf> {
    // A side chat reads its parent's folder: the pane is the parent's.
    let _ = id;
    if crate::remote::backend_is_remote(s) {
        return None;
    }
    super::side::folder(s)
        .map(Path::to_path_buf)
        .or_else(|| s.prefs.last_project.clone())
}

/// Where `id`'s `@` files come from: this Mac's project folder, or, with the
/// backend on another computer, the thread's workspace there, listed by
/// that backend (its mentions name `/workspace/...`, which the backend
/// resolves; this Mac is never walked for it).
pub(crate) fn mention_source(s: &State, id: &str) -> Option<menu::IndexKey> {
    if let crate::onboarding::gateway::Computer::Remote(remote) = &s.onboarding.computer {
        return (!id.is_empty()).then(|| menu::IndexKey::Remote {
            environment: remote.environment_id.clone(),
            thread: id.to_string(),
        });
    }
    mention_root(s, id).map(menu::IndexKey::Local)
}

/// The folder the backend may read `text`'s @-mentions from. A thread
/// without one of its own adopts the folder its mentions came from.
pub(crate) fn mention_roots(s: &mut State, id: &str, text: &str) -> Vec<String> {
    if !mention::needs_root(text) {
        return Vec::new();
    }
    let Some(root) = mention_root(s, id) else {
        return Vec::new();
    };
    if super::is_main(s, id) {
        super::side::adopt_folder(s, root.clone());
    }
    vec![root.to_string_lossy().into_owned()]
}

/// Enter in the composer: a goal or slash command runs, a message joins the
/// running turn (steer), else it starts a new turn.
pub fn send(s: &mut State, id: &str, tx: &mut Tx) {
    let Some(c) = chat_for(s, id) else { return };
    // Talk mode's claim on this send, if it pressed this Enter.
    let voice = c.voice_send.take();
    let key = crate::voice::Composer::Chat(id.to_string());
    let voice = voice.or_else(|| crate::voice::take_submission(s, &key));
    let refuse = |s: &mut State, why: &str, voice: Option<crate::voice::Submission>| {
        if let Some(sub) = voice {
            crate::voice::submission_refused(s, sub, why);
        }
    };
    let Some(c) = chat_ref(s, id) else { return };
    let (text, files) = inline_files::outgoing(&c.composer, &c.staged);
    if text.is_empty() && files.is_empty() {
        return refuse(s, "There was nothing to send.", voice);
    }
    if let Err(why) = crate::remote::check_outgoing(s, &files) {
        refuse(s, &why, voice);
        return set_error(s, id, why);
    }
    // Each chat, side chats too, has its own goal (area H).
    if files.is_empty()
        && let Some(command) = goals::goal_command(&text)
    {
        refuse(s, "That ran as a command, not a message.", voice);
        return goals::run_command(s, id, command);
    }
    if files.is_empty() && match_command(&text, &c.menu.sources.commands).is_some() {
        refuse(s, "That ran as a command, not a message.", voice);
        run_command(s, id, text);
        return;
    }
    if c.running_run().is_some() {
        return super::steer::steer(s, id, voice, tx);
    }
    let Some(channel) = c.channel.clone() else {
        refuse(s, "This gateway has no chat channel configured.", voice);
        set_error(s, id, "This gateway has no chat channel configured.");
        return;
    };
    let Some(c) = chat_for(s, id) else { return };
    let draft_text = c.composer.text.clone();
    let draft_pills = inline_files::pills(&c.composer);
    c.composer.clear();
    c.error = None;
    c.staged.clear();
    let names: Vec<String> = files.iter().map(|f| f.filename.clone()).collect();
    let pending = c.transcript.push_pending_user(&text, &names);
    // Zeron's turn: the message moves to the top and the answer streams
    // into the room kept below it.
    c.turn_heights.clear();
    c.reserve = Some(Reserve { held: true });
    c.pin_next = true;
    c.far_from_bottom = false;
    let running = c.running_run().is_some();
    record_draft(s, id);
    // MonoCode's prompt rise: the scroll that carries the sent prompt from
    // the composer to the top of the list (and the composer emptying) runs
    // on its 560 ms curve; the row itself only fades in (`view::prompt_rise`).
    tx.motion(super::view::PROMPT_RISE_SPRING);
    super::refresh_rows(s, id, tx);
    // A prompt sent here starts its run's clock now; a steer keeps it.
    if !running {
        crate::run_clock::end(s, id);
        crate::run_clock::start(s, id, None, None);
    }
    // The project's working tree as it stands now, for the turn's card.
    if super::is_main(s, id) {
        super::side::on_turn_started(s);
    }

    let thread_id = id.to_string();
    let req = thread_id.clone();
    let body = text.clone();
    let wire = files.iter().map(StagedFile::to_wire).collect();
    let roots = mention_roots(s, id, &text);
    jobs::run(
        s,
        "chat",
        move |c| async move {
            c.send_message_with_roots(&channel, &req, &body, wire, roots)
                .await
        },
        move |s, result, tx| {
            match result {
                Ok(ack) => {
                    if let Some(sub) = voice {
                        super::steer::report_voice(s, &thread_id, sub, &ack, None);
                    }
                    let Some(c) = chat_for(s, &thread_id) else {
                        return;
                    };
                    c.transcript.ack_pending_user(pending);
                    if let Some(notice) = ack.notice.filter(|n| !n.trim().is_empty()) {
                        c.error = Some(notice);
                    }
                    super::load_timeline(s, &thread_id);
                    // A CLI-runtime turn has no stream; the poll follows it.
                    super::refresh_extras(s, &thread_id);
                }
                Err(e) => {
                    if let Some(sub) = voice {
                        crate::voice::submission_refused(s, sub, &e.to_string());
                    }
                    let Some(c) = chat_for(s, &thread_id) else {
                        return;
                    };
                    c.transcript.drop_pending_user(pending);
                    if let Some(r) = c.reserve.as_mut() {
                        r.held = false;
                    }
                    let error = restore_failed(c, &draft_text, &draft_pills, files, e.to_string());
                    record_draft(s, &thread_id);
                    set_error(s, &thread_id, error);
                    super::refresh_rows(s, &thread_id, tx);
                }
            }
        },
    );
}

/// A send failed: put its text and cards back so a retry needs no
/// re-picking (GPUI's behaviour), but only into an empty composer with no
/// files staged. When the person has typed or attached since, that stays,
/// and the error names the unsent message instead. Returns the error to show.
pub(super) fn restore_failed(
    c: &mut super::ChatState,
    text: &str,
    pills: &[(std::ops::Range<usize>, String)],
    files: Vec<StagedFile>,
    error: String,
) -> String {
    if c.composer.text.is_empty() && c.staged.is_empty() {
        c.staged = files;
        inline_files::set_content(&mut c.composer, text, pills);
        error
    } else {
        format!("{error} Not sent: \u{201c}{}\u{201d}", text.trim())
    }
}

/// Stop the running turn. The row flips to "Stopping" on the click itself;
/// the backend confirms later. Computer use's Stop (area H) calls this too.
pub fn stop(s: &mut State, id: &str, tx: &mut Tx) {
    let Some(c) = chat_for(s, id) else { return };
    let Some(run) = c.running_run() else {
        return;
    };
    c.transcript.mark_stop_requested(&run);
    super::refresh_rows(s, id, tx);
    let thread_id = id.to_string();
    let req = thread_id.clone();
    jobs::run(
        s,
        "chat",
        move |c| async move { c.cancel_run(&req, &run).await },
        move |s, result, tx| {
            if let Err(e) = result
                && let Some(c) = chat_for(s, &thread_id)
            {
                c.transcript.clear_stop_requested();
                super::refresh_rows(s, &thread_id, tx);
                set_error(s, &thread_id, e.to_string());
            }
        },
    );
}

/// A failed run's Retry.
pub(crate) fn retry(s: &mut State, id: &str, run: String) {
    let req = id.to_string();
    run_and_report(s, id, move |c| async move { c.retry_run(&req, &run).await });
}

/// Approve or decline a gate. The card shows "waiting for IronClaw to
/// confirm" until the stream closes it; a failure gives it its buttons back.
pub(crate) fn resolve_gate(
    s: &mut State,
    id: &str,
    run: String,
    gate_ref: String,
    decision: GateDecision,
    tx: &mut Tx,
) {
    let Some(c) = chat_for(s, id) else { return };
    if c.reject
        .as_ref()
        .is_some_and(|editor| editor.gate_ref == gate_ref)
    {
        c.reject = None;
    }
    c.transcript.mark_gate_resolving(&gate_ref, decision);
    super::refresh_rows(s, id, tx);
    let thread_id = id.to_string();
    let req = thread_id.clone();
    let gate = gate_ref.clone();
    jobs::run(
        s,
        "chat",
        move |c| async move { c.resolve_gate(&req, &run, &gate, decision).await },
        move |s, result, tx| {
            if let Err(e) = result
                && let Some(c) = chat_for(s, &thread_id)
            {
                c.transcript.clear_gate_resolving(&gate_ref);
                set_error(s, &thread_id, e.to_string());
                super::refresh_rows(s, &thread_id, tx);
            }
        },
    );
}

/// Fire a mutation; the stream reports its effect, so only errors matter.
fn run_and_report<F, Fut>(s: &mut State, id: &str, f: F)
where
    F: FnOnce(Client) -> Fut,
    Fut: std::future::Future<Output = Result<serde_json::Value, ClientError>> + Send + 'static,
{
    let thread = id.to_string();
    jobs::run(s, "chat", f, move |s, result, _| {
        if let Err(e) = result {
            set_error(s, &thread, e.to_string());
        }
    });
}

/// A link in a reply: the right pane previews it (area S); with ⌘ held it
/// opens outside the app.
pub(crate) fn open_link(s: &mut State, id: &str, href: &str, external: bool, tx: &mut Tx) {
    // A side chat's links open in its parent's pane too.
    let _ = id;
    super::side::open_link(s, href, external, tx);
}

/// ↑ in an empty composer brings back the last message to edit and resend
/// (MonoCode's edit-last-turn recall, without the rewind).
pub(crate) fn recall_last(s: &mut State, id: &str) -> bool {
    let Some(c) = chat_for(s, id) else {
        return false;
    };
    if !c.composer.text.is_empty() {
        return false;
    }
    let Some(last) = c.rows.iter().rev().find_map(|r| match r {
        Row::User {
            text, attachments, ..
        } => Some(zlo_client::transcript::readable(text, attachments)),
        _ => None,
    }) else {
        return false;
    };
    let (text, pills) = inline_files::restored(&last);
    inline_files::set_content(&mut c.composer, &text, &pills);
    record_draft(s, id);
    true
}

/// Note the composer as the thread's draft, so an unsent message comes back
/// (every edit, and the clear after a send).
pub(crate) fn record_draft(s: &mut State, id: &str) {
    let Some(c) = chat_ref(s, id) else { return };
    let text = c.composer.text.clone();
    let tokens = inline_files::pills(&c.composer)
        .into_iter()
        .filter_map(|(range, id)| {
            let shown = text.get(range.clone())?.to_string();
            Some(crate::drafts::DraftToken {
                range,
                id,
                label: shown.clone(),
                text: shown,
            })
        })
        .collect();
    let draft = crate::drafts::Draft {
        text,
        tokens,
        files: c.staged.clone(),
        profile: None,
    };
    crate::drafts::record(s, id, draft);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_mentions_never_walk_this_mac() {
        let mut s = crate::chat::fixtures::test_state();
        s.prefs.last_project = Some(PathBuf::from("/Users/you/code/zlo"));
        assert_eq!(
            mention_root(&s, "t1").as_deref(),
            Some(Path::new("/Users/you/code/zlo"))
        );
        s.onboarding.computer =
            crate::onboarding::gateway::Computer::remote("env-studio", "Studio");
        assert_eq!(mention_root(&s, "t1"), None, "no local folder for a remote");
        assert!(mention_roots(&mut s, "t1", "see @file:src/a.rs").is_empty());
    }

    /// `@` files come from this Mac's folder locally, and from the backend's
    /// index of that thread's workspace when the backend is remote.
    #[test]
    fn remote_mentions_list_the_backends_thread_workspace() {
        let mut s = crate::chat::fixtures::test_state();
        s.prefs.last_project = Some(PathBuf::from("/Users/you/code/zlo"));
        assert_eq!(
            mention_source(&s, "t1"),
            Some(menu::IndexKey::Local(PathBuf::from("/Users/you/code/zlo")))
        );
        s.onboarding.computer =
            crate::onboarding::gateway::Computer::remote("env-studio", "Studio");
        assert_eq!(
            mention_source(&s, "t1"),
            Some(menu::IndexKey::Remote {
                environment: "env-studio".into(),
                thread: "t1".into(),
            })
        );
        assert_eq!(mention_source(&s, ""), None, "no thread, no workspace");
    }

    #[test]
    fn remote_files_open_in_the_editor_over_ssh() {
        assert_eq!(
            editor_over_ssh("studio.tail1.ts.net", Path::new("/home/me/My notes.md")).as_deref(),
            Some("vscode://vscode-remote/ssh-remote+studio.tail1.ts.net/home/me/My%20notes.md")
        );
        assert_eq!(editor_over_ssh("studio", Path::new("relative.txt")), None);
        assert_eq!(editor_over_ssh("bad host/x", Path::new("/a")), None);
    }

    #[test]
    fn a_remote_send_refuses_files_staged_by_path() {
        let mut s = crate::chat::fixtures::test_state();
        let by_path = StagedFile {
            key: attachments::next_key(),
            filename: "a.txt".into(),
            mime_type: "text/plain".into(),
            size: 1,
            body: attachments::Body::Path("/tmp/a.txt".into()),
        };
        let files = vec![by_path];
        assert!(crate::remote::check_outgoing(&s, &files).is_ok());
        s.onboarding.computer =
            crate::onboarding::gateway::Computer::remote("env-studio", "Studio");
        assert_eq!(
            crate::remote::check_outgoing(&s, &files).err().as_deref(),
            Some("Attach a.txt again: it was added for this Mac's backend.")
        );
    }

    #[test]
    fn a_failed_send_restores_only_into_an_empty_composer() {
        let mut c = super::super::ChatState::new("t1".into());
        let error = restore_failed(&mut c, "first try", &[], Vec::new(), "boom".into());
        assert_eq!(
            (c.composer.text.as_str(), error.as_str()),
            ("first try", "boom")
        );

        let mut c = super::super::ChatState::new("t1".into());
        c.composer.replace_text("typed since");
        let error = restore_failed(&mut c, "first try", &[], Vec::new(), "boom".into());
        assert_eq!(
            c.composer.text, "typed since",
            "what was typed since survives"
        );
        assert!(error.contains("\u{201c}first try\u{201d}"), "{error}");
    }

    #[test]
    fn a_restored_failed_send_is_recorded_as_the_draft() {
        let mut s = crate::chat::fixtures::test_state();
        s.chat = Some(super::super::ChatState::new("t1".into()));
        let c = chat_for(&mut s, "t1").expect("chat");
        let _ = restore_failed(c, "first try", &[], Vec::new(), "boom".into());
        record_draft(&mut s, "t1");
        assert_eq!(
            crate::drafts::restore(&s, "t1").map(|d| d.text),
            Some("first try".into())
        );
    }
}
