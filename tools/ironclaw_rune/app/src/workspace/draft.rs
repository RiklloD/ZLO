//! The new-thread screen (t3code's, with a profile where t3code has a
//! project): "What should we work on?", the profile picker below it, and
//! the composer card. Nothing exists on the gateway until the first message
//! is sent, so abandoned drafts leave no empty threads behind. (GPUI
//! `workspace/draft.rs`.)
//!
//! Staged files show as chips over the text here; GPUI's inline file cards
//! and the `/` `@` menu belong to the chat composer (area A) and join this
//! screen at integration.

mod snapshot;

use std::path::PathBuf;

use ironclaw_gpui_client::attachments::{self, StagedFile};
use ironclaw_gpui_client::profiles::DEFAULT_PROFILE_ID;
use rune::{Color, Cx, El, FileDialogOptions, TextEdit, Tx, col, el, row};
use rune_kit::{Kit, Placement};

pub use snapshot::stage_snapshot;

use super::inbox::{self, view::profile_picker};
use super::ui::{self, SEMIBOLD, SM, XS, t, tw};
use crate::chat::model_picker::ModelPickerState;
use crate::design::{self, prefs::Wallpaper};
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
    /// Files for the first message, handed to the thread on send.
    pub staged: Vec<StagedFile>,
    pub sending: bool,
    pub error: Option<String>,
    /// Start the thread in a new git worktree of the last project folder
    /// (the Local / New worktree switch) instead of working in place.
    pub worktree: bool,
    /// Bumped when the draft is replaced, so a late attach lands nowhere.
    pub generation: Generation,
}

/// Where voice dictation types (GPUI `Workspace::voice_composer`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ComposerTarget {
    /// The quick composer window.
    Quick,
    /// The open thread's composer (area A's `ChatState`).
    Chat(String),
    /// The new-thread screen.
    Draft,
}

/// The composer voice types into: the quick composer when it is open, else
/// the open chat's or new-thread screen's while this window is in front.
/// Otherwise `open_quick` opens the quick composer for it.
pub fn voice_composer(s: &mut State, open_quick: bool) -> Option<ComposerTarget> {
    if s.workspace.quick.is_open() {
        return Some(ComposerTarget::Quick);
    }
    if s.workspace.window_active {
        match s.page {
            Page::Chat => {
                if let Some(id) = super::active_id(s) {
                    return Some(ComposerTarget::Chat(id.to_string()));
                }
            }
            Page::Draft if s.workspace.draft.is_some() => return Some(ComposerTarget::Draft),
            _ => {}
        }
    }
    if !open_quick {
        return None;
    }
    super::quick::open(s);
    Some(ComposerTarget::Quick)
}

/// The draft's text field, for a voice target that is the draft. The chat
/// composer is area A's; the quick composer lives in its own window
/// (`quick::insert_text`).
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
        let mut staged = Vec::new();
        let mut text = String::new();
        if let Some(draft) = saved {
            staged = draft.files;
            text = without_tokens(&draft.text, &draft.tokens);
        }
        let generation = s.workspace.draft_opens.bump();
        let draft = DraftState {
            profile: profile.clone(),
            input: TextEdit::new(text),
            picker: ModelPickerState::default(),
            menu_open: false,
            staged,
            sending: false,
            error: None,
            worktree: false,
            generation,
        };
        s.workspace.draft = Some(draft);
        crate::design::look::reshuffle(s);
    }
    // A kept draft re-reads its profile: its default may have changed. The
    // model catalog is per user; any thread can read it.
    let catalog = s.workspace.inbox.list.rows.first().map(|r| r.thread_id.clone());
    if let Some(profile) = s.workspace.draft.as_ref().map(|d| d.profile.clone()) {
        crate::chat::model_picker::draft_set_profile(s, profile, catalog);
    }
    if let Some(draft) = s.workspace.draft.as_mut() {
        draft.input.focused = true;
    }
    inbox::reveal_active_subagent(s);
}

/// The draft's text with its inline card text taken out: the cards show as
/// chips here.
fn without_tokens(text: &str, tokens: &[drafts::DraftToken]) -> String {
    let mut out = String::new();
    let mut last = 0;
    for token in tokens {
        if token.range.start < last || token.range.end > text.len() {
            continue;
        }
        out.push_str(&text[last..token.range.start]);
        last = token.range.end;
    }
    out.push_str(&text[last..]);
    out
}

/// A prompt from the quick composer: the new-thread screen's send, with its
/// profile and model choice.
pub(super) fn quick_send(s: &mut State, text: String) {
    open(s);
    if let Some(draft) = s.workspace.draft.as_mut() {
        draft.input.replace_text(text);
    }
    send(s);
}

/// A profile picked in either menu: open the draft screen in it.
pub fn start_in_profile(s: &mut State, profile: String) {
    s.workspace.inbox.new_menu_open = false;
    open(s);
    let catalog = s.workspace.inbox.list.rows.first().map(|r| r.thread_id.clone());
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
    let saved = drafts::Draft {
        text: draft.input.text.clone(),
        tokens: Vec::new(),
        files: draft.staged.clone(),
        profile: Some(draft.profile.clone()),
    };
    drafts::record(s, drafts::NEW_THREAD, saved);
}

/// Create the thread in the chosen profile, apply the model pick, open it
/// and send the first message there.
fn send(s: &mut State) {
    let worktree_dir = s
        .prefs
        .last_project
        .clone()
        .map(|p| p.to_string_lossy().into_owned());
    let Some(draft) = s.workspace.draft.as_mut() else {
        return;
    };
    let text = draft.input.text.trim().to_string();
    if text.is_empty() && draft.staged.is_empty() {
        return;
    }
    if draft.sending {
        return;
    }
    let content = draft.input.text.clone();
    draft.sending = true;
    draft.error = None;
    let profile = draft.profile.clone();
    let pick = crate::chat::model_picker::pending(&draft.picker);
    let effort = crate::chat::model_picker::pending_effort(&draft.picker);
    let worktree = draft.worktree.then_some(worktree_dir);
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
        move |s, result, _| match result {
            Ok(id) => {
                let files = s
                    .workspace
                    .draft
                    .take()
                    .map(|d| d.staged)
                    .unwrap_or_default();
                drafts::remove(s, drafts::NEW_THREAD);
                inbox::refresh(s);
                super::open_thread(s, id.clone());
                if super::active_id(s) == Some(id.as_str()) {
                    crate::chat::send_text(s, content, files);
                }
            }
            Err(e) => {
                if let Some(draft) = s.workspace.draft.as_mut() {
                    draft.sending = false;
                    draft.error = Some(e.to_string());
                }
            }
        },
    );
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

/// Stage `files` in the draft.
pub(super) fn place(s: &mut State, files: Vec<StagedFile>) {
    let Some(draft) = s.workspace.draft.as_mut() else {
        return;
    };
    draft.staged.extend(files);
    record(s);
}

pub(super) fn profile_name(s: &State) -> String {
    let id = s.workspace.draft.as_ref().map(|d| d.profile.as_str());
    id.and_then(|id| s.workspace.inbox.profile(id).map(|p| p.display_name.clone()))
        .unwrap_or_else(|| "Default".into())
}

/// A round composer button (GPUI `chat::round_button`): filled with the
/// accent when `ready`.
fn round_button(
    p: &design::Palette,
    key: &str,
    icon: &'static rune::Icon,
    ready: bool,
    label: &str,
) -> El {
    row()
        .key(key.to_string())
        .role("button")
        .label(label.to_string())
        .help(label.to_string())
        .size(32., 32.)
        .radius(16.)
        .center()
        .cursor(rune::Cursor::Pointer)
        .fill(if ready { p.accent } else { Color::TRANSPARENT })
        .hover(if p.light {
            Color::BLACK.alpha(0.08)
        } else {
            Color::WHITE.alpha(0.10)
        })
        .child(ui::glyph(
            icon,
            16.,
            if ready { p.on_accent() } else { p.text_muted },
        ))
}

pub(super) fn view(s: &State, cx: &mut Cx<State>, k: &Kit, layer: &mut Vec<El>) -> El {
    let p = &s.palette;
    let Some(draft) = s.workspace.draft.as_ref() else {
        return el();
    };
    let name = profile_name(s);
    let ready = !draft.staged.is_empty() || !draft.input.text.trim().is_empty();
    let send_button = round_button(p, "draft-send", &icons::ARROW_UP, ready, "Send")
        .opacity(if draft.sending { 0.35 } else { 1. })
        .on_press(send);
    let attach_button = round_button(p, "draft-attach", &icons::PAPERCLIP, false, "Attach files")
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
            t(name, SM, if draft.menu_open { p.text } else { p.text_muted }).group_color(p.text),
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
    let chips: Vec<El> = draft
        .staged
        .iter()
        .map(|f| {
            let key = f.key;
            row()
                .key(format!("draft-file-{key}"))
                .gap(4.)
                .px(8.)
                .h(24.)
                .center_y()
                .radius(12.)
                .fill(p.surface_raised)
                .child(ui::glyph(&icons::FILE, 12., p.text_muted))
                .child(t(f.filename.clone(), XS, p.text).truncate())
                .child(ui::ghost_icon(
                    k,
                    &icons::CLOSE,
                    "Remove",
                    rune_kit::ControlSize::Mini,
                    move |s| {
                        if let Some(d) = s.workspace.draft.as_mut() {
                            d.staged.retain(|f| f.key != key);
                        }
                        record(s);
                    },
                ))
        })
        .collect();
    let input = ui::grow_box(
        ui::plain_area(&draft.input, 15., "Do anything…", p)
            .on_edit(|s: &mut State| {
                &mut s
                    .workspace
                    .draft
                    .get_or_insert_with(|| empty_draft(DEFAULT_PROFILE_ID))
                    .input
            })
            .on_input_change(|s: &mut State, _| record(s))
            .on_submit(|s: &mut State, _| send(s)),
        2,
        10,
    );
    let mut card = col()
        .key("draft-composer")
        .w_full()
        .max_w(HERO_WIDTH)
        .padding(12.)
        .gap(8.)
        .radius(design::BUBBLE_RADIUS)
        .fill(p.surface_card)
        .border(
            1.,
            if draft.input.focused {
                p.border_strong
            } else {
                p.border
            },
        )
        .on_drop_files(|s: &mut State, files, _| attach(s, files.to_vec()));
    if !chips.is_empty() {
        card = card.child(row().flex_wrap().gap(4.).children(chips));
    }
    card = card.child(input).child(
        row()
            .gap(6.)
            .center_y()
            .child(crate::chat::model_picker::draft_pill(s, cx, k, layer))
            .child(el().grow(1.0))
            .child(place)
            .child(attach_button)
            .child(crate::voice::composer_controls(s, k, ComposerTarget::Draft))
            .child(send_button),
    );
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

fn empty_draft(profile: &str) -> DraftState {
    DraftState {
        profile: profile.into(),
        input: TextEdit::new(""),
        picker: ModelPickerState::default(),
        menu_open: false,
        staged: Vec::new(),
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
    fn restored_text_drops_inline_card_text() {
        let tokens = vec![drafts::DraftToken {
            range: 4..10,
            id: "1".into(),
            text: "[a.md]".into(),
            label: "a.md".into(),
        }];
        assert_eq!(without_tokens("see [a.md] now", &tokens), "see  now");
    }
}
