//! The new-thread screen (t3code's, with a profile where t3code has a
//! project): "What should we work on?", the profile picker below it, and
//! the composer card. Nothing exists on the gateway until the first message
//! is sent, so abandoned drafts leave no empty threads behind. (GPUI
//! `workspace/draft.rs`.)
//!
//! The composer shares the chat's pieces (area A): staged files are inline
//! cards at the caret (`chat::inline_files`), a paste of files or a picture
//! attaches them, and the `@` menu (`chat::menu`) offers files of the last
//! project folder; it has no commands, since those need a thread. A thread
//! dragged from the sidebar onto the card attaches its transcript
//! (`thread_drag`).

mod snapshot;
mod submit;

use std::path::PathBuf;

use zlo_client::attachments::{self, StagedFile};
use zlo_client::commands::MenuItem;
use zlo_client::profiles::DEFAULT_PROFILE_ID;
use rune::{Cx, El, FileDialogOptions, KeyEvent, TextEdit, Tx, col, el, row};
use rune_kit::{Kit, Placement};

pub use snapshot::stage_snapshot;
use submit::{Origin, Submission, send_with, submit};

use super::inbox::{self, view::profile_picker};
use super::ui::{self, SEMIBOLD, SM, XS, t, tw};
use crate::chat::menu::{self, ComposerMenu, MenuKey, MenuKeyName};
use crate::chat::model_picker::ModelPickerState;
use crate::chat::{composer, inline_files};
use crate::design::{motion, prefs::Wallpaper};
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
    /// Files for the first message, handed to the thread on send; each has
    /// its card in `input`.
    pub staged: Vec<StagedFile>,
    /// The `/` and `@` menu: no commands, since those need a thread; `@`
    /// files come from the last project folder used.
    pub menu: ComposerMenu,
    pub sending: bool,
    pub error: Option<String>,
    /// Start the thread in a new git worktree of the last project folder
    /// (the Local / New worktree switch) instead of working in place.
    pub worktree: bool,
    /// Bumped when the draft is replaced, so a late attach lands nowhere.
    pub generation: Generation,
}

/// Where voice dictation types (GPUI `Workspace::voice_composer`).
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
    s.page = Page::Draft;
    if s.workspace.draft.is_none() {
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
    }
    // A kept draft re-reads its profile: its default may have changed. The
    // model catalog is per user; any thread can read it.
    let catalog = s
        .workspace
        .inbox
        .list
        .rows
        .first()
        .map(|r| r.thread_id.clone());
    if let Some(profile) = s.workspace.draft.as_ref().map(|d| d.profile.clone()) {
        crate::chat::model_picker::draft_set_profile(s, profile, catalog);
    }
    if let Some(draft) = s.workspace.draft.as_mut() {
        draft.input.focused = true;
    }
    inbox::reveal_active_subagent(s);
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
        let root = s.prefs.last_project.clone();
        menu::sync_index(s, root.as_deref(), menu_of());
        menu::rank_in_background(s, query, menu_of());
    }
    record(s);
}

/// Menu keys for the draft composer (GPUI `draft_menu_key`).
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
    s.workspace.inbox.new_menu_open = false;
    open(s);
    let catalog = s
        .workspace
        .inbox
        .list
        .rows
        .first()
        .map(|r| r.thread_id.clone());
    crate::chat::model_picker::draft_set_profile(s, profile.clone(), catalog);
    if let Some(draft) = s.workspace.draft.as_mut() {
        draft.profile = profile;
        draft.menu_open = false;
    }
    record(s);
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
    jobs::run(
        s,
        "attach",
        move |_| async move {
            let mut out = Vec::new();
            for path in paths {
                out.push(attachments::stage(&path, &limits).await);
            }
            Ok(out)
        },
        move |s, result, _| {
            let Ok(results) = result else { return };
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

/// A sidebar thread dropped on the card: attach its transcript as a file
/// (GPUI `attach_thread_to_draft`).
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
        move |s, result, _| {
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

pub(super) fn view(s: &State, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let Some(draft) = s.workspace.draft.as_ref() else {
        return el();
    };
    let name = profile_name(s);
    let ready = !draft.staged.is_empty() || !draft.input.text.trim().is_empty();
    // The chat composer's own controls, so the card that flies to the
    // chat's dock on send is one component.
    let send_button = composer::round_button(s, "draft-send", &icons::ARROW_UP, ready, "Send", k)
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
    let in_worktree = draft.worktree;
    let place = row()
        .key("draft-worktree")
        .role("button")
        .px(8.)
        .h(28.)
        .gap(4.)
        .center_y()
        .radius(14.)
        .cursor(rune::Cursor::Pointer)
        .hover_group()
        .help(if in_worktree {
            "Runs in its own git worktree and branch of the last project folder (committed work only)"
        } else {
            "Works in the project folder itself"
        })
        .child(ui::glyph(
            &icons::GIT_BRANCH,
            12.,
            if in_worktree { p.text } else { p.text_faint },
        ))
        .child(
            t(
                if in_worktree { "New worktree" } else { "Local" },
                XS,
                if in_worktree { p.text } else { p.text_faint },
            )
            .group_color(p.text),
        )
        .on_press(|s: &mut State| {
            if let Some(d) = s.workspace.draft.as_mut() {
                d.worktree = !d.worktree;
            }
        });
    // No resting underline (T3's pass): a chevron says it opens.
    let profile_switch = row()
        .key("draft-profile")
        .role("button")
        .label(format!("Profile: {name}"))
        .gap(4.)
        .center_y()
        .cursor(rune::Cursor::Pointer)
        .hover_group()
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
            .group_color(p.text),
        )
        .child(ui::glyph(&icons::CHEVRON_DOWN, 12., p.text_faint))
        .on_press(|s: &mut State| {
            if let Some(d) = s.workspace.draft.as_mut() {
                d.menu_open = !d.menu_open;
            }
        });
    if draft.menu_open {
        layer.push(rune_kit::popover::anchored(
            k,
            col()
                .w(260.)
                .modal(true, |s: &mut State| {
                    if let Some(d) = s.workspace.draft.as_mut() {
                        d.menu_open = false;
                    }
                })
                .child(profile_picker(s, k)),
            "draft-profile",
            Placement::Bottom,
            6.,
        ));
    }
    let snapshots = inline_files::snapshot_keys(s, &draft.staged);
    let input = ui::grow_box(
        // The chat composer's field, type and all.
        rune::text_input(
            &draft.input,
            crate::chat::flow::type_at(s, 14., 22.75, 400.),
        )
        .key("composer-input")
        .w_full()
        .color(p.text)
        .placeholder("Do anything…", p.text_faint)
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
        .on_input_change(|s: &mut State, _| on_change(s))
        .on_submit(|s: &mut State, _| send(s)),
        1,
        8,
    );
    let thread_over = super::thread_drag::over_draft(s);
    let actions = row()
        .key("composer-actions")
        .gap(6.)
        .center_y()
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
    // The chat's composer card in its hero form (GPUI `composer_card` with
    // `compact` false), hero-wide; the chat's docked card is the same one.
    let mut card = composer::composer_card(
        s,
        "draft-composer",
        draft.input.focused,
        false,
        None,
        input,
        crate::chat::model_picker::draft_pill(s, cx, k, layer),
        actions,
    )
    .matched(motion::COMPOSER_MATCH)
    .max_w(HERO_WIDTH)
    .on_drawn_layout_tx(super::thread_drag::saw_draft)
    .on_drop_files(|s: &mut State, files, _| attach(s, files.to_vec()));
    if thread_over {
        card = card.border(1., p.accent);
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
                .child(tw("What should we work on?", 32., SEMIBOLD, p.text))
                .child(profile_switch),
        )
        .child(card);
    if let Some(error) = draft.error.clone() {
        body = body.child(el().max_w(720.).child(t(error, SM, p.danger).wrap()));
    }
    // Room under the card so it sits just above the middle, like t3code.
    body = body.child(el().h(80.));
    // Without a wallpaper, a soft accent glow from the top stands in.
    let bare = s.prefs.wallpaper == Wallpaper::None;
    let mut root = el()
        .key("draft")
        .w_full()
        .h_full()
        .on_drop_files(|s: &mut State, files, _| attach(s, files.to_vec()));
    if bare {
        root = root.child(
            el().pin(Some(0.), Some(0.), Some(0.), None)
                .h(320.)
                .gradient(p.accent_wash, p.accent_wash.alpha(0.)),
        );
    }
    root.child(body)
}

pub(super) fn empty_draft(profile: &str) -> DraftState {
    DraftState {
        profile: profile.into(),
        input: TextEdit::new(""),
        picker: ModelPickerState::default(),
        menu_open: false,
        staged: Vec::new(),
        menu: ComposerMenu::default(),
        sending: false,
        error: None,
        worktree: false,
        generation: Generation::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
