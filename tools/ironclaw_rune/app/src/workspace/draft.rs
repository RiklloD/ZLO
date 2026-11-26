//! The new-thread screen (t3code's, with a profile where t3code has a project):
//! "What should we work on?", the profile picker below it, and the composer
//! card. Nothing exists on the gateway until the first message is sent, so
//! abandoned drafts leave no empty threads behind.
//!
//! The composer shares the chat's pieces (area A): staged files are inline
//! cards at the caret (`chat::inline_files`), a paste of files or a picture
//! attaches them, and the `@` menu (`chat::menu`) offers files of the last
//! project folder; it has no commands, since those need a thread. A thread
//! dragged from the sidebar onto the card attaches its transcript
//! (`thread_drag`).

pub(super) mod runs_on;
mod snapshot;
mod submit;

use std::path::PathBuf;

use rune::{Cx, El, FileDialogOptions, KeyEvent, TextEdit, Tx, col, el, row};
use rune_kit::Kit;
use zlo_client::attachments::{self, StagedFile};
use zlo_client::commands::MenuItem;
use zlo_client::profiles::DEFAULT_PROFILE_ID;

pub use snapshot::stage_snapshot;
use submit::{Origin, Submission, send_with, submit};

use super::inbox::{self, picker};
use super::ui::{self, SEMIBOLD, SM, XS, t, tw};
use crate::chat::menu::{self, ComposerMenu, MenuKey, MenuKeyName};
use crate::chat::model_picker::ModelPickerState;
use crate::chat::{composer, inline_files};
use crate::drafts;
use crate::icons;
use crate::jobs::{self, Generation};
use crate::state::{Page, State};

/// The new-thread composer's width at rest.
const HERO_WIDTH: f32 = 720.;

pub struct DraftState {
    pub profile: String,
    pub input: TextEdit,
    /// The draft's model and effort pick (area H draws and owns it).
    pub picker: ModelPickerState,
    pub menu_open: bool,
    /// The "Runs on" computer picker menu is open.
    pub runs_on_open: bool,
    /// One-line note when the preferred default computer was not ready.
    pub computer_note: Option<String>,
    /// Files for the first message, handed to the thread on send; each has
    /// its card in `input`.
    pub staged: Vec<StagedFile>,
    /// The `/` and `@` menu: no commands, since those need a thread; `@`
    /// files come from the last project folder used.
    pub menu: ComposerMenu,
    pub sending: bool,
    pub error: Option<String>,
    /// The Local / New worktree switch as the person set it; `None` follows
    /// the default ([`in_worktree`]): Local, unless Settings → Worktrees
    /// starts git-folder profiles' threads in their own worktree.
    pub worktree: Option<bool>,
    /// Bumped when the draft is replaced, so a late attach lands nowhere.
    pub generation: Generation,
    /// Not drawn yet: the hero's greeting, profile switch and composer
    /// stagger in as this draft first mounts ([`hero_entrance`]); a switch
    /// back to it later shows it with the page's plain rise.
    pub(super) fresh: bool,
    /// Send's plate colours, which ease on a keystroke without animating
    /// its commit (`chat::composer::PlateTint`).
    pub(crate) send_tint: crate::chat::composer::PlateTint,
}

/// Where voice dictation types.
pub type ComposerTarget = crate::voice::Composer;

/// The composer voice types into: the quick composer when it is open, else
/// the open chat's or new-thread screen's while this window is in front.
/// Otherwise `open_quick` opens the quick composer for it.
pub fn voice_composer(s: &mut State, open_quick: bool) -> Option<ComposerTarget> {
    let front = match s.page {
        Page::Chat => super::active_id(s).map(|id| ComposerTarget::Chat(id.to_string())),
        Page::Draft if s.workspace.draft.is_some() => Some(ComposerTarget::Draft),
        _ => None,
    };
    match pick_voice_composer(
        s.workspace.quick.is_open(),
        s.workspace.window_active,
        front,
        open_quick,
    ) {
        VoicePick::Composer(target) => Some(target),
        VoicePick::OpenQuick => {
            super::quick::open(s);
            s.workspace.quick.is_open().then_some(ComposerTarget::Quick)
        }
        VoicePick::Nothing => None,
    }
}

/// [`voice_composer`]'s choice.
#[derive(Debug, PartialEq, Eq)]
enum VoicePick {
    Composer(ComposerTarget),
    OpenQuick,
    Nothing,
}

/// The quick composer while it is open; else the main window's composer
/// (`front`) while that window is in front; else, for a key press from
/// another app (`open_quick`), the quick composer, opened for it.
fn pick_voice_composer(
    quick_open: bool,
    window_active: bool,
    front: Option<ComposerTarget>,
    open_quick: bool,
) -> VoicePick {
    if quick_open {
        return VoicePick::Composer(ComposerTarget::Quick);
    }
    if window_active && let Some(front) = front {
        return VoicePick::Composer(front);
    }
    if open_quick {
        VoicePick::OpenQuick
    } else {
        VoicePick::Nothing
    }
}

/// The draft's text field, for a voice target that is the draft. The chat
/// composer is area A's; the quick composer's is a mirror of its window's
/// field (`quick::voice_edit`).
pub fn composer_edit<'a>(s: &'a mut State, target: &ComposerTarget) -> Option<&'a mut TextEdit> {
    match target {
        ComposerTarget::Draft => s.workspace.draft.as_mut().map(|d| &mut d.input),
        _ => None,
    }
}

/// Show the new-thread screen; an unsent draft keeps its text.
pub(super) fn open(s: &mut State) {
    // A newer navigation than a thread click still waiting on a switch.
    s.remote.pending_thread = None;
    s.page = Page::Draft;
    if s.workspace.draft.is_none() {
        let computer_note = match runs_on::apply_default_on_open(s) {
            runs_on::DefaultOutcome::Switched => return,
            runs_on::DefaultOutcome::Stay(note) => note,
        };
        let list = &s.workspace.inbox.list;
        let saved = drafts::restore(s, drafts::NEW_THREAD);
        // A saved draft keeps its profile while that profile exists.
        let profile = saved
            .as_ref()
            .and_then(|d| d.profile.clone())
            .filter(|id| list.profiles.iter().any(|p| &p.profile_id == id))
            .or_else(|| list.last_used_profile_id.clone())
            .or_else(|| list.profiles.first().map(|p| p.profile_id.clone()))
            .unwrap_or_else(|| DEFAULT_PROFILE_ID.into());
        let mut draft = empty_draft(&profile);
        draft.computer_note = computer_note;
        if let Some(saved) = saved {
            draft.staged = saved.files;
            let pills: Vec<_> = saved.tokens.into_iter().map(|t| (t.range, t.id)).collect();
            inline_files::set_content(&mut draft.input, &saved.text, &pills);
        }
        let generation = s.workspace.draft_opens.bump();
        draft.generation = generation;
        s.workspace.draft = Some(draft);
        load_menu(s, generation);
        crate::design::look::reshuffle(s);
        // A saved draft whose files were staged by this Mac's path (it was
        // written on This Mac, or swapped here) is read for this computer.
        runs_on::restage_for_remote(s);
    }
    // A kept draft re-reads its profile: its default may have changed.
    let catalog = catalog_thread(s);
    if let Some(profile) = s.workspace.draft.as_ref().map(|d| d.profile.clone()) {
        crate::chat::model_picker::draft_set_profile(s, profile, catalog);
    }
    if let Some(draft) = s.workspace.draft.as_mut() {
        draft.input.focused = true;
    }
    inbox::reveal_active_subagent(s);
}

/// The thread the draft's model catalog is read through. The catalog is per
/// user, so any thread can read it; `None` until the inbox has a row.
fn catalog_thread(s: &State) -> Option<String> {
    s.workspace
        .inbox
        .list
        .rows
        .first()
        .map(|r| r.thread_id.clone())
}

/// The inbox landed: a draft opened before it had a thread to read the model
/// catalog through binds its picker now, so the model pill appears.
pub(super) fn bind_catalog(s: &mut State) {
    let Some(profile) = s
        .workspace
        .draft
        .as_ref()
        .filter(|d| d.picker.catalog_thread.is_none())
        .map(|d| d.profile.clone())
    else {
        return;
    };
    if let Some(catalog) = catalog_thread(s) {
        crate::chat::model_picker::draft_set_profile(s, profile, Some(catalog));
    }
}

/// Skills, extensions and threads for the `@` menu of the draft opened as
/// `generation`.
fn load_menu(s: &mut State, generation: Generation) {
    jobs::run(
        s,
        "draft menu",
        |c| menu::load(c, false),
        move |s, result, _| {
            let draft = s
                .workspace
                .draft
                .as_mut()
                .filter(|d| d.generation == generation);
            if let (Ok(sources), Some(draft)) = (result, draft) {
                draft.menu.sources = std::rc::Rc::new(sources);
            }
        },
    );
}

/// The draft's menu, found again from the root.
fn menu_of() -> menu::MenuOf {
    std::sync::Arc::new(|s: &mut State| s.workspace.draft.as_mut().map(|d| &mut d.menu))
}

/// The text changed: the menu's highlight goes back to the top, cards that
/// were deleted drop their files, an `@` query ranks the last project
/// folder's files, and the draft is noted.
fn on_change(s: &mut State) {
    let Some(draft) = s.workspace.draft.as_mut() else {
        return;
    };
    draft.menu.reset();
    inline_files::prune(&draft.input, &mut draft.staged);
    if let Some(query) = ComposerMenu::at_query(&draft.input.text) {
        // A new thread has no workspace on a remote backend yet, and this
        // Mac's folders are not that backend's.
        let source = (!crate::remote::backend_is_remote(s))
            .then(|| s.prefs.last_project.clone().map(menu::IndexKey::Local))
            .flatten();
        menu::sync_index(s, source, menu_of());
        menu::rank_in_background(s, query, menu_of());
    }
    record(s);
}

/// Menu keys for the draft composer.
fn on_key(s: &mut State, ev: &KeyEvent, tx: &mut Tx) -> bool {
    let Some(name) = MenuKeyName::of(ev) else {
        return false;
    };
    let Some(draft) = s.workspace.draft.as_mut() else {
        return false;
    };
    match draft.menu.key(name, &mut draft.input, tx) {
        MenuKey::Unused => false,
        // The draft's menu lists no commands.
        MenuKey::Handled | MenuKey::Command(_) => {
            on_change(s);
            true
        }
    }
}

fn pick_item(s: &mut State, item: &MenuItem) {
    if let Some(draft) = s.workspace.draft.as_mut() {
        menu::pick(item, &mut draft.input);
    }
    on_change(s);
}

/// A prompt from the quick composer: the new-thread screen's send, with its
/// profile and model choice. `voice` is talk mode's claim when it pressed
/// Enter there (`quick::send` took it and refused an empty send).
///
/// It is its own submission: the new-thread draft's text, cards and saved
/// copy are never touched, whether that draft is sending or not.
pub(super) fn quick_send(s: &mut State, text: String, voice: Option<crate::voice::Submission>) {
    open(s);
    let Some(draft) = s.workspace.draft.as_ref() else {
        if let Some(sub) = voice {
            crate::voice::submission_refused(s, sub, "There was nothing to send.");
        }
        return;
    };
    let submission = Submission::of(s, draft, text, Vec::new(), Vec::new(), Origin::Quick);
    submit(s, submission, voice);
}

/// A profile picked in either menu: open the draft screen in it.
pub fn start_in_profile(s: &mut State, profile: String) {
    picker::close(s);
    open(s);
    let catalog = catalog_thread(s);
    crate::chat::model_picker::draft_set_profile(s, profile.clone(), catalog);
    if let Some(draft) = s.workspace.draft.as_mut() {
        draft.profile = profile;
        draft.menu_open = false;
        // The switch follows the new profile's folder again.
        draft.worktree = None;
    }
    record(s);
}

/// Profile `id` was archived: a draft in it moves to the profile the
/// picker now leads with, so the screen never shows one profile and sends
/// another.
pub(super) fn profile_gone(s: &mut State, id: &str) {
    if s.workspace.draft.as_ref().is_none_or(|d| d.profile != id) {
        return;
    }
    let list = &s.workspace.inbox.list;
    let profile = list
        .last_used_profile_id
        .clone()
        .or_else(|| list.profiles.first().map(|p| p.profile_id.clone()))
        .unwrap_or_else(|| DEFAULT_PROFILE_ID.into());
    let catalog = catalog_thread(s);
    crate::chat::model_picker::draft_set_profile(s, profile.clone(), catalog);
    if let Some(draft) = s.workspace.draft.as_mut() {
        draft.profile = profile;
        draft.worktree = None;
    }
    record(s);
}

/// Put a draft carried from another computer back as the new-thread draft
/// (triage's switch, `super::triage::connected`) and show it. Its profile
/// falls back to the default when this computer has no such profile.
/// Answers the new draft's generation.
pub(super) fn restore(
    s: &mut State,
    profile: String,
    text: &str,
    pills: &[(std::ops::Range<usize>, String)],
    staged: Vec<StagedFile>,
    worktree: Option<bool>,
) -> Generation {
    let known = s
        .workspace
        .inbox
        .list
        .profiles
        .iter()
        .any(|p| p.profile_id == profile);
    let profile = if known {
        profile
    } else {
        DEFAULT_PROFILE_ID.into()
    };
    let mut draft = empty_draft(&profile);
    draft.staged = staged;
    draft.worktree = worktree;
    inline_files::set_content(&mut draft.input, text, pills);
    let generation = s.workspace.draft_opens.bump();
    draft.generation = generation;
    s.workspace.draft = Some(draft);
    load_menu(s, generation);
    open(s);
    // Saved once this computer's session is linked (`triage::prepared`).
    generation
}

/// Drop the unsent new-thread draft (text, files, profile). An open
/// new-thread screen starts over empty.
pub(super) fn discard_new_draft(s: &mut State) {
    drafts::remove(s, drafts::NEW_THREAD);
    s.workspace.draft = None;
    if s.page == Page::Draft {
        open(s);
    }
}

/// Note the new-thread composer's text, files and profile.
pub(super) fn record(s: &mut State) {
    let Some(draft) = s.workspace.draft.as_ref() else {
        return;
    };
    let text = draft.input.text.clone();
    let tokens = inline_files::pills(&draft.input)
        .into_iter()
        .filter_map(|(range, id)| {
            let shown = text.get(range.clone())?.to_string();
            Some(drafts::DraftToken {
                range,
                id,
                label: shown.clone(),
                text: shown,
            })
        })
        .collect();
    let saved = drafts::Draft {
        text,
        tokens,
        files: draft.staged.clone(),
        profile: Some(draft.profile.clone()),
    };
    drafts::record(s, drafts::NEW_THREAD, saved);
}

/// Enter on the new-thread screen (voice's Enter too: talk mode's claim on
/// it rides along into the new chat's first send).
pub fn send(s: &mut State) {
    let voice = crate::voice::take_submission(s, &ComposerTarget::Draft);
    send_with(s, voice);
}

fn pick_files(s: &mut State, tx: &mut Tx) {
    let _ = s;
    let options = FileDialogOptions {
        title: "Attach".into(),
        ..Default::default()
    };
    tx.open_file(options, |s: &mut State, result, _| match result {
        Ok(Some(path)) => attach(s, vec![path]),
        Ok(None) => {}
        Err(e) => {
            if let Some(d) = s.workspace.draft.as_mut() {
                d.error = Some(e.to_string());
            }
        }
    });
}

/// Read files off the main thread, then stage the ones within limits.
pub(super) fn attach(s: &mut State, paths: Vec<PathBuf>) {
    let (Some(session), Some(draft)) = (s.session.as_ref(), s.workspace.draft.as_ref()) else {
        return;
    };
    let limits = session.attachments.clone();
    let check = limits.clone();
    let generation = draft.generation;
    let backend = crate::remote::attachment_backend(s);
    jobs::run(
        s,
        "attach",
        move |_| async move {
            let mut out = Vec::new();
            for path in paths {
                out.push(attachments::stage(&path, &limits, backend).await);
            }
            Ok(out)
        },
        move |s, result, tx| {
            let Ok(results) = result else { return };
            // The person attached them: their cards enter rather than land.
            tx.animated();
            let Some(draft) = s
                .workspace
                .draft
                .as_mut()
                .filter(|d| d.generation == generation)
            else {
                return;
            };
            let mut problems = Vec::new();
            let mut files = Vec::new();
            for result in results {
                match result.and_then(|file| {
                    attachments::check(&file, &draft.staged, &check).map(|()| file)
                }) {
                    Ok(file) => files.push(file),
                    Err(problem) => problems.push(problem),
                }
            }
            draft.error = (!problems.is_empty()).then(|| problems.join(" "));
            place(s, files);
        },
    );
}

/// Stage `files` in the draft and put their cards at its caret.
pub(super) fn place(s: &mut State, files: Vec<StagedFile>) {
    let Some(draft) = s.workspace.draft.as_ref() else {
        return;
    };
    let mut all = draft.staged.clone();
    all.extend(files.iter().cloned());
    let mut input = draft.input.clone();
    inline_files::insert(s, &mut input, &files, &all);
    if let Some(draft) = s.workspace.draft.as_mut() {
        draft.staged = all;
        draft.input = input;
    }
    record(s);
}

/// Files or a picture pasted into the draft's composer.
fn paste(s: &mut State, pasted: inline_files::Pasted) {
    match pasted {
        inline_files::Pasted::Paths(paths) => attach(s, paths),
        inline_files::Pasted::Image { bytes, mime } => {
            let (Some(draft), Some(session)) = (s.workspace.draft.as_mut(), s.session.as_ref())
            else {
                return;
            };
            match inline_files::pasted_image(bytes, mime, &draft.staged, &session.attachments) {
                Ok(file) => place(s, vec![file]),
                Err(problem) => draft.error = Some(problem),
            }
        }
    }
}

/// A sidebar thread dropped on the card: attach its transcript as a file.
pub(super) fn attach_thread(s: &mut State, id: String, title: String) {
    let Some(draft) = s.workspace.draft.as_mut() else {
        return;
    };
    draft.input.focused = true;
    let generation = draft.generation;
    let Some(session) = s.session.as_ref() else {
        return;
    };
    let limits = session.attachments.clone();
    jobs::run(
        s,
        "attach thread",
        move |c| async move { c.thread_attachment(&id, &title, &limits).await },
        move |s, result, tx| {
            // Dropped there by the person: its card enters.
            tx.animated();
            let (Some(draft), Some(session)) = (s.workspace.draft.as_mut(), s.session.as_ref())
            else {
                return;
            };
            // A reset or send must not attach this to a different draft.
            if draft.generation != generation {
                return;
            }
            let result = result.map_err(|e| e.to_string()).and_then(|file| {
                attachments::check(&file, &draft.staged, &session.attachments).map(|()| file)
            });
            match result {
                Ok(file) => {
                    draft.error = None;
                    place(s, vec![file]);
                }
                Err(error) => draft.error = Some(error),
            }
        },
    );
}

pub(super) fn profile_name(s: &State) -> String {
    let id = s.workspace.draft.as_ref().map(|d| d.profile.as_str());
    id.and_then(|id| {
        s.workspace
            .inbox
            .profile(id)
            .map(|p| p.display_name.clone())
    })
    .unwrap_or_else(|| "Default".into())
}

/// The draft has something to send.
fn send_ready(draft: &DraftState) -> bool {
    !draft.staged.is_empty() || !draft.input.text.trim().is_empty()
}

pub(super) fn view(s: &State, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let Some(draft) = s.workspace.draft.as_ref() else {
        return el();
    };
    let name = profile_name(s);
    let triage = super::triage::is_triage_draft(s);
    // A triage draft sends only once its chip links this computer's session.
    let ready = send_ready(draft) && super::triage::send_block(s).is_none();
    let fresh = draft.fresh;
    // The chat composer's own controls, so the card that flies to the
    // chat's dock on send is one component.
    let reduced =
        crate::design::battery::reduced(&s.prefs, &s.look.battery, cx.system_reduced_motion());
    let send_button = composer::round_button_tinted(
        s,
        "draft-send",
        &icons::ARROW_UP,
        ready,
        "Send",
        Some((&draft.send_tint, reduced)),
        k,
    )
    .help("Send")
    .opacity(if draft.sending { 0.35 } else { 1. })
    .on_press(send);
    let attach_button = composer::round_button(
        s,
        "draft-attach",
        &icons::PAPERCLIP,
        false,
        "Attach files",
        k,
    )
    .help("Attach files")
    .on_press_tx(pick_files);
    let worktree_on = in_worktree(s, draft);
    let place = row()
        .key("draft-worktree")
        .role("button")
        .px(8.)
        .h(28.)
        .gap(4.)
        .center_y()
        .radius(crate::design::CONTROL_RADIUS)
        .hover(p.hover)
        .cursor(rune::Cursor::Pointer)
        .hover_group()
        .help(if worktree_on {
            "Runs in its own git worktree, on a new branch made from main (committed work only)"
        } else {
            "Works in the project folder itself, on whatever branch it has checked out"
        })
        .child(ui::glyph(
            &icons::GIT_BRANCH,
            12.,
            if worktree_on { p.text } else { p.text_faint },
        ))
        .child(
            t(
                if worktree_on { "New worktree" } else { "Local" },
                XS,
                if worktree_on { p.text } else { p.text_faint },
            )
            .group_color(p.text),
        )
        .on_press(|s: &mut State| {
            let Some(on) = s.workspace.draft.as_ref().map(|d| in_worktree(s, d)) else {
                return;
            };
            if let Some(d) = s.workspace.draft.as_mut() {
                d.worktree = Some(!on);
            }
        });
    // Under the heading, no resting underline (T3's pass): a chevron says
    // it opens. A long name truncates instead of widening the column.
    let mut profile_switch = row()
        .key("draft-profile")
        .role("button")
        .label(format!("Profile: {name}"))
        .gap(4.)
        .center_y()
        .cursor(rune::Cursor::Pointer)
        .hover_group()
        .max_w(HERO_WIDTH)
        .min_w(0.)
        .child(
            t(
                name,
                SM,
                if draft.menu_open {
                    p.text
                } else {
                    p.text_muted
                },
            )
            .group_color(p.text)
            .min_w(0.)
            .truncate(),
        )
        .child(crate::design::kit::chevron(
            "draft-profile-chevron",
            draft.menu_open,
            crate::design::kit::Turn::Flip,
            12.,
            p.text_faint,
        ))
        .on_press(picker::toggle_draft_menu);
    if draft.menu_open {
        layer.push(picker::anchored(
            k,
            col()
                .w(picker::WIDTH)
                .min_h(0.0)
                .modal(true, |s: &mut State| {
                    picker::close(s);
                })
                .child(picker::profile_picker(s, k)),
            "draft-profile",
            6.,
        ));
    }
    let snapshots = inline_files::snapshot_keys(s, &draft.staged);
    let field_type = crate::chat::flow::type_at(s, 14., 22.75, 400.);
    let input = ui::grow_box(
        // The chat composer's field, type and all.
        rune::text_input(&draft.input, field_type)
            .key("composer-input")
            .w_full()
            .color(p.text)
            .placeholder("Ask anything…", p.text_faint)
            .caret(p.text, k.selection())
            .label("Message")
            .pill_colors(p.hover, Some(p.text))
            .pill_icon(move |pill| inline_files::pill_icon_for(&snapshots, pill))
            .on_input_key(on_key)
            .on_input_paste(|s: &mut State, content, _tx| {
                let Some(pasted) = inline_files::pasted(content) else {
                    return false;
                };
                paste(s, pasted);
                true
            })
            .on_edit(|s: &mut State| {
                &mut s
                    .workspace
                    .draft
                    .get_or_insert_with(|| empty_draft(DEFAULT_PROFILE_ID))
                    .input
            })
            // Typing lands at once, layout and all (`ui::grow_box`); Send's
            // plate eases on its live colours (`send_tint`), as the chat's.
            .on_input_change(|s: &mut State, _tx: &mut Tx| on_change(s))
            .on_submit(|s: &mut State, _| send(s)),
        field_type.line_px(),
        1,
        8,
    );
    let thread_over = super::thread_drag::over_draft(s);
    let actions = row()
        .key("composer-actions")
        .gap(6.)
        .center_y()
        .children(runs_on::control(s, k, layer))
        .child(place)
        .child(attach_button)
        .child(crate::voice::ui::composer_controls(
            s,
            k,
            layer,
            &ComposerTarget::Draft,
            None,
        ))
        .child(send_button);
    // The chat's composer card in its hero form, hero-wide; the chat's docked
    // card is the same one.
    let card = composer::composer_card(
        s,
        "draft-composer",
        draft.input.focused,
        false,
        None,
        input,
        crate::chat::model_picker::draft_pill(s, cx, k, layer),
        actions,
    );
    // The card a dock flies back to (a new thread from a chat) lands on the
    // dock's spring too: a matched flight takes its destination's
    // `motion` when its commit names none (`motion::COMPOSER_MATCH`).
    let mut card = composer::dock_matched(s, card)
        .motion(crate::design::motion::DOCK)
        .max_w(HERO_WIDTH)
        .on_drawn_layout_tx(super::thread_drag::saw_draft)
        .on_drop_files(|s: &mut State, files, _| attach(s, files.to_vec()));
    if thread_over {
        card = card.border(1., p.accent);
    }
    // A composer flying in from a chat's dock is its own entrance; with no
    // dock to come from (or a wallpaper, which swaps it in place) it rises
    // in last.
    let flies_in = s.chat.is_some() && !crate::design::wallpaper::covers_composer(&s.prefs);
    if fresh && !flies_in {
        card = card
            .transition(hero_entrance(s))
            .enter_delay(2. * HERO_STAGGER);
    }
    // The hero card sits mid-screen, so the menu opens below it, as wide.
    let items = draft.menu.items(&draft.input.text);
    if !items.is_empty() {
        let selected = draft.menu.selected(items.len());
        let on_pick: menu::OnPick = std::rc::Rc::new(|s: &mut State, item, _tx| pick_item(s, item));
        let width = cx
            .layout_size("draft-composer")
            .map_or(HERO_WIDTH, |[w, _]| w);
        menu::popup(
            s,
            k,
            layer,
            menu::Placing {
                key: "command-menu",
                anchor: "draft-composer".into(),
                side: rune::Side::Bottom,
                width,
                menu_of: menu_of(),
            },
            &items,
            selected,
            on_pick,
        );
    }
    let mut greeting = el().key("draft-greeting");
    if fresh {
        greeting = greeting.transition(hero_entrance(s));
        profile_switch = profile_switch
            .transition(hero_entrance(s))
            .enter_delay(HERO_STAGGER);
    }
    // The centred hero: the big question, the project switcher under it,
    // then the card.
    let mut body = col()
        .key("draft-body")
        .w_full()
        .h_full()
        .items_center()
        .justify_center()
        .px(24.)
        .gap(24.)
        .child(
            col()
                .items_center()
                .gap(8.)
                .child(greeting.child(tw(
                    if triage {
                        "What went wrong?"
                    } else {
                        "What should we work on?"
                    },
                    32.,
                    SEMIBOLD,
                    p.text,
                )))
                .children(triage.then(|| {
                    el().max_w(HERO_WIDTH)
                        .child(t(super::triage::hero_line(s), SM, p.text_muted).wrap())
                }))
                .child(profile_switch),
        )
        .child(card);
    if let Some(error) = draft.error.clone() {
        body = body.child(el().max_w(HERO_WIDTH).child(t(error, SM, p.danger).wrap()));
    }
    if let Some(note) = draft.computer_note.clone() {
        body = body.child(
            el().key("draft-computer-note")
                .max_w(HERO_WIDTH)
                .child(t(note, XS, p.text_muted).wrap()),
        );
    }
    if let Some(cu_note) = runs_on::computer_use_note(s, k) {
        body = body.child(cu_note);
    }
    // Room under the card so it sits just above the middle, like t3code.
    let body = body.child(el().h(80.));
    // A flat panel: no glow, no band; a wallpaper, when one is picked, is
    // laid over the panel by the frame.
    let mut root = el()
        .key("draft")
        .w_full()
        .h_full()
        .on_drop_files(|s: &mut State, files, _| attach(s, files.to_vec()));
    if fresh {
        // Drawn once: from here on it is a kept draft, and coming back to
        // it does not replay the stagger. No commit: nothing on screen
        // changes.
        root = root.on_drawn_layout_tx(|s: &mut State, _, tx| {
            if let Some(d) = s.workspace.draft.as_mut() {
                d.fresh = false;
            }
            tx.live_only();
        });
    }
    root.child(body)
}

/// Whether `draft` starts its thread in a new worktree: as switched, else
/// when the Worktrees setting is on and its profile's folder is a git
/// checkout (the profile reports a branch).
pub(super) fn in_worktree(s: &State, draft: &DraftState) -> bool {
    draft.worktree.unwrap_or_else(|| {
        s.prefs.thread_worktrees
            && s.workspace
                .inbox
                .profile(&draft.profile)
                .is_some_and(|p| p.git_branch.is_some())
    })
}

pub(super) fn empty_draft(profile: &str) -> DraftState {
    DraftState {
        profile: profile.into(),
        input: TextEdit::new(""),
        picker: ModelPickerState::default(),
        menu_open: false,
        runs_on_open: false,
        computer_note: None,
        staged: Vec::new(),
        menu: ComposerMenu::default(),
        sending: false,
        error: None,
        worktree: None,
        generation: Generation::default(),
        fresh: true,
        send_tint: Default::default(),
    }
}

/// Whether the draft screen shows its first entrance now: a new draft,
/// not yet drawn. The main column then gives the page no entrance of its
/// own, so the hero's parts carry theirs.
pub(super) fn entering(s: &State) -> bool {
    s.page == Page::Draft && s.workspace.draft.as_ref().is_some_and(|d| d.fresh)
}

/// One part of the hero rising in on the page's entrance, `delay` seconds
/// after the screen mounts (greeting, profile switch, composer: 0, 40,
/// 80 ms), leaving faster with the page.
fn hero_entrance(s: &State) -> rune::Transition {
    let st = crate::design::styles::Style::of(&s.prefs);
    rune::Transition::opacity()
        .combined(rune::Transition::offset(0., st.enter_rise))
        .springs(rune::SpringPair::with_exit_ratio(st.enter, 0.6))
}

/// The hero's stagger, in seconds per part.
const HERO_STAGGER: f32 = 0.04;

#[cfg(test)]
mod tests {
    use super::*;

    /// New threads work in place by default. With the Worktrees setting
    /// on, a git profile starts each one in its own worktree (from `main`)
    /// unless switched to Local; a profile without a git folder works in
    /// place; switching profile follows the new one again.
    #[test]
    fn a_git_profile_starts_new_threads_in_their_own_worktree() {
        let mut s = crate::chat::fixtures::test_state();
        crate::workspace::fixtures::fixture_inbox(&mut s);
        assert!(!s.prefs.thread_worktrees, "off by default");
        assert!(!in_worktree(&s, &empty_draft("work")), "Local by default");
        s.prefs.thread_worktrees = true;
        let mut draft = empty_draft("work");
        assert!(
            in_worktree(&s, &draft),
            "IronClaw's folder is a git checkout"
        );
        draft.worktree = Some(false);
        assert!(!in_worktree(&s, &draft), "switched to Local");
        let plain = empty_draft("default");
        assert!(!in_worktree(&s, &plain), "no git folder, nothing to branch");
        s.workspace.draft = Some(draft);
        start_in_profile(&mut s, "work".into());
        let draft = s.workspace.draft.as_ref().unwrap();
        assert!(in_worktree(&s, draft), "a new profile resets the switch");
    }

    #[test]
    fn voice_picks_the_open_quick_composer_first() {
        let chat = || Some(ComposerTarget::Chat("t1".into()));
        for (window_active, open_quick) in
            [(true, true), (true, false), (false, true), (false, false)]
        {
            assert_eq!(
                pick_voice_composer(true, window_active, chat(), open_quick),
                VoicePick::Composer(ComposerTarget::Quick)
            );
        }
    }

    #[test]
    fn voice_types_into_the_front_composer_while_the_window_is_in_front() {
        assert_eq!(
            pick_voice_composer(false, true, Some(ComposerTarget::Chat("t1".into())), true),
            VoicePick::Composer(ComposerTarget::Chat("t1".into()))
        );
        assert_eq!(
            pick_voice_composer(false, true, Some(ComposerTarget::Draft), false),
            VoicePick::Composer(ComposerTarget::Draft)
        );
    }

    #[test]
    fn a_key_press_from_another_app_opens_the_quick_composer() {
        // The window is behind another app, or shows a page with no composer.
        let chat = Some(ComposerTarget::Chat("t1".into()));
        assert_eq!(
            pick_voice_composer(false, false, chat.clone(), true),
            VoicePick::OpenQuick
        );
        assert_eq!(
            pick_voice_composer(false, true, None, true),
            VoicePick::OpenQuick
        );
        // A mic click or a barge-in never opens it.
        assert_eq!(
            pick_voice_composer(false, false, chat, false),
            VoicePick::Nothing
        );
        assert_eq!(
            pick_voice_composer(false, true, None, false),
            VoicePick::Nothing
        );
    }
}
