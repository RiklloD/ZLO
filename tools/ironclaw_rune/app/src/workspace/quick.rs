//! The quick composer (GPUI `workspace/quick.rs`): ⇧⌘Space from any app
//! opens a small floating prompt. Enter sends it as a new thread through the
//! new-thread screen's own path (last-used profile, the picked model) and
//! brings the main window forward on that thread. Escape, the shortcut
//! again, or clicking elsewhere closes it.
//!
//! In Rune it is a second window with its own small state ([`QuickState`]),
//! a launcher panel (non-activating, hidden until shown) added with
//! `App::window`; the shortcut is Rune's `App::global_shortcut`, so GPUI's
//! hand-written HIToolbox registration is gone.
//!
//! ## Voice
//!
//! Voice runs in the main window's `State`; the field lives in this window.
//! Two windows cannot share one state, so:
//!
//! - **The field stays here** ([`QuickState::input`]): keys, IME and Enter
//!   act on it directly, and it is what this window draws.
//! - **The main state keeps a mirror** ([`Quick::input`]). This window's
//!   `poll` copies the field there whenever it changed (a key press, a
//!   paste), so voice reads it synchronously like any other composer
//!   (`chat::voice_composer`: the caret for spacing, the text on Enter).
//! - **Voice edits go both ways as the same edit** ([`voice_edit`]): the
//!   closure runs on the mirror at once and is queued to this window with
//!   `AppHandle::update`, where it runs on the real field in order with the
//!   person's own key presses. Nothing typed is overwritten; a mirror that
//!   briefly lags catches up on this window's next `poll`.
//! - **The controls** are `voice::ui::draw` over a [`Controls`] copy that the
//!   main window's `poll` sends whenever it changes ([`poll`]); a press goes
//!   back as a `voice::ui::Press` through `AppHandle::update`.
//! - **Enter** (a key here, or talk mode's) ends in [`send`] on the main
//!   state, which takes talk mode's claim, refuses an empty send, and hands
//!   both to `draft::quick_send`.
//!
//! Voice resolves the quick composer first while it is open, and a voice
//! key pressed in another app opens it (`draft::voice_composer`).

use rune::{
    Align, App, AppHandle, AppSpec, Color, Cx, DisplayTarget, El, Key, KeyEvent, OverlayPlacement,
    ScreenEdge, TextEdit, Tx, WindowOptions, WindowSpec, col, el, row,
};

use super::ui::{self, XS, t};
use crate::design::{self, Palette};
use crate::state::State;
use crate::voice::Composer;
use crate::voice::ui::{Controls, OnPress, Press};

pub const SHORTCUT: &str = "cmd+shift+space";
const WIDTH: f32 = 640.;
const HEIGHT: f32 = 132.;
const PLACEHOLDER: &str = "Ask IronClaw anything…";

/// The main window's side: the quick window's handle, whether it shows,
/// and the voice mirror of its field.
#[derive(Default)]
pub struct Quick {
    handle: Option<AppHandle<QuickState>>,
    open: bool,
    /// The field as the quick window last reported it, plus voice's edits.
    pub input: TextEdit,
    /// The voice controls last sent to the quick window.
    sent_controls: Option<Controls>,
}

impl Quick {
    pub fn is_open(&self) -> bool {
        self.open
    }
}

/// The quick window's own state.
pub struct QuickState {
    input: TextEdit,
    /// The field as last copied to the main state's mirror.
    synced: Synced,
    palette: Palette,
    /// The voice controls, as the main window last sent them.
    voice: Option<Controls>,
    main: AppHandle<State>,
    /// Became the key window once; only then does losing focus close it.
    seen_active: bool,
}

/// The parts of the field the mirror needs.
#[derive(Clone, Default, PartialEq)]
struct Synced {
    text: String,
    caret: (usize, usize),
    preedit: Option<String>,
}

impl Synced {
    fn of(edit: &TextEdit) -> Self {
        Self {
            text: edit.text.clone(),
            caret: (edit.anchor, edit.focus),
            preedit: edit.preedit.as_ref().map(|(p, _)| p.clone()),
        }
    }
}

/// Add the quick window and its global shortcut to the app. Call before
/// `App::init`; hand the result to [`attach`] from the init closure.
pub fn install(app: App<State>) -> (App<State>, AppHandle<QuickState>) {
    let main = app.handle();
    let spec = AppSpec {
        fonts: design::FONTS,
        families: design::FAMILIES,
        on_key: Some(quick_key),
        poll: Some(quick_poll),
        ..AppSpec::new("ZLO Quick", (WIDTH, HEIGHT), unreachable_init, view)
    };
    let quick = App::new(spec)
        .init(move || {
            let prefs = crate::design::prefs::Prefs::load();
            QuickState {
                input: TextEdit::new(""),
                synced: Synced::default(),
                palette: design::resolve(&prefs, true),
                voice: None,
                main,
                seen_active: false,
            }
        })
        .on_focus(|q, focused, tx| {
            // Clicking another app or window closes it, like Spotlight.
            if focused {
                q.seen_active = true;
            } else if q.seen_active {
                close_from_quick(q, tx);
            }
        });
    let mut app = app.global_shortcut(SHORTCUT, |s, _| toggle(s));
    let (handle, _window) = app.window(
        WindowSpec::from_app("quick", quick).configured_options(WindowOptions::launcher_panel()),
    );
    (app, handle)
}

fn unreachable_init() -> QuickState {
    unreachable!("the quick composer's state is built by App::init")
}

/// Keep the quick window's handle in the main state.
pub fn attach(s: &mut State, handle: AppHandle<QuickState>) {
    s.workspace.quick.handle = Some(handle);
}

/// Open the quick composer unless it is open (voice from another app).
pub fn open(s: &mut State) {
    if !s.workspace.quick.open {
        toggle(s);
    }
}

pub fn close(s: &mut State) {
    s.workspace.quick.open = false;
    if let Some(q) = &s.workspace.quick.handle {
        q.hide();
    }
}

/// ⇧⌘Space.
pub fn toggle(s: &mut State) {
    if s.workspace.quick.open {
        close(s);
        return;
    }
    let Some(q) = s.workspace.quick.handle.clone() else {
        return;
    };
    let palette = s.palette;
    q.update(move |q, _| {
        q.palette = palette;
        q.input.clear();
        q.input.focused = true;
        q.synced = Synced::of(&q.input);
        q.seen_active = false;
    });
    // Spotlight's spot: centred, a fifth of the way down the screen.
    q.place(
        DisplayTarget::Primary,
        OverlayPlacement::Edge {
            edge: ScreenEdge::Top,
            margin: top_margin(crate::window_state::primary_screen_height()),
            align: Align::Center,
            inset: 0.,
        },
    );
    q.open();
    let quick = &mut s.workspace.quick;
    quick.open = true;
    quick.input = TextEdit::new("");
    // Sent again by the next `poll`.
    quick.sent_controls = None;
}

/// The quick composer's distance from the top of a screen `height` points
/// tall: a fifth of the way down (GPUI `quick.rs`), or 180 pt when the
/// screen is unknown.
fn top_margin(height: Option<f32>) -> f32 {
    height
        .filter(|h| *h > 0.)
        .map_or(180., |h| (h / 5.).round())
}

/// A voice edit of the quick composer's field: run on the mirror now (its
/// result is voice's), and queued to the quick window as the same edit.
/// `None` when the quick composer is closed.
pub(crate) fn voice_edit<R>(
    s: &mut State,
    f: impl Fn(&mut TextEdit) -> R + Clone + Send + 'static,
) -> Option<R> {
    let quick = &mut s.workspace.quick;
    if !quick.open {
        return None;
    }
    let out = f(&mut quick.input);
    if let Some(q) = &quick.handle {
        let f = f.clone();
        q.update(move |q, _| {
            let _ = f(&mut q.input);
        });
    }
    Some(out)
}

/// The main window's frame hook: send the voice controls to the open quick
/// composer when they changed. Never changes what the main window shows.
pub fn poll(s: &mut State) -> bool {
    if !s.workspace.quick.open {
        s.workspace.quick.sent_controls = None;
        return false;
    }
    let controls = Controls::of(s, &Composer::Quick);
    let quick = &mut s.workspace.quick;
    if quick.sent_controls.as_ref() == Some(&controls) {
        return false;
    }
    quick.sent_controls = Some(controls.clone());
    if let Some(q) = &quick.handle {
        q.update(move |q, _| q.voice = Some(controls));
    }
    false
}

/// Enter in the quick composer, on the main state: from its own field
/// (keyboard) or the mirror (talk mode's Enter, `chat::voice_press_enter`).
/// Takes talk mode's claim; an empty send is refused (the composer stays
/// open), anything else becomes a new thread with the claim riding along.
pub(crate) fn send(s: &mut State, text: String) {
    let voice = crate::voice::take_submission(s, &Composer::Quick);
    let text = text.trim().to_string();
    if text.is_empty() {
        if let Some(sub) = voice {
            crate::voice::submission_refused(s, sub, "There was nothing to send.");
        }
        return;
    }
    s.workspace.quick.input = TextEdit::new("");
    if let Some(q) = &s.workspace.quick.handle {
        q.update(|q, _| {
            q.input.clear();
            q.synced = Synced::of(&q.input);
        });
    }
    close(s);
    super::draft::quick_send(s, text, voice);
    if !s.fixture {
        s.rt.handle.open();
    }
}

/// Talk mode's Enter (`chat::voice_press_enter`): send what the mirror holds.
pub(crate) fn press_enter(s: &mut State) {
    let text = s.workspace.quick.input.text.clone();
    send(s, text);
}

fn close_from_quick(q: &mut QuickState, tx: &mut Tx) {
    q.seen_active = false;
    q.main.update(|s, _| s.workspace.quick.open = false);
    tx.close_window();
}

fn quick_key(q: &mut QuickState, ev: &KeyEvent, tx: &mut Tx) -> bool {
    if ev.key == Key::Escape {
        close_from_quick(q, tx);
        return true;
    }
    false
}

/// Before each of this window's frames: copy a changed field to the mirror.
fn quick_poll(q: &mut QuickState, _clock: f64, _tx: &mut Tx) -> bool {
    let now = Synced::of(&q.input);
    if now != q.synced {
        q.synced = now;
        let edit = q.input.clone();
        q.main.update(move |s, _| {
            if s.workspace.quick.open {
                s.workspace.quick.input = edit;
            }
        });
    }
    false
}

fn submit(q: &mut QuickState, _tx: &mut Tx) {
    let text = q.input.text.clone();
    q.main.update(move |s, _| send(s, text));
}

fn view(q: &QuickState, _cx: &mut Cx<QuickState>) -> El {
    let p = &q.palette;
    let input = ui::grow_box(
        ui::plain_area(&q.input, 16., PLACEHOLDER, p)
            .on_edit(|q: &mut QuickState| &mut q.input)
            .on_submit(submit),
        1,
        4,
    );
    let controls = q.voice.as_ref().map(|c| {
        let on = |press: Press| -> OnPress<QuickState> {
            Box::new(move |q: &mut QuickState| {
                q.main.update(move |s, _| {
                    crate::voice::ui::press(s, press, Composer::Quick, None);
                });
            })
        };
        crate::voice::ui::draw(c, p, on, |mic| mic)
    });
    let card = card(p, input, controls).w_full().h_full();
    el().w_full()
        .h_full()
        .padding(8.)
        .fill(Color::TRANSPARENT)
        .child(ui::floating(p, card, 16.))
}

/// The card's column: the field, then the hint row with the voice controls.
fn card(p: &Palette, input: El, controls: Option<El>) -> El {
    let mut hints = row()
        .gap(12.)
        .center_y()
        .child(ui::dot(p.accent, 6.))
        .child(
            el().grow(1.0)
                .child(t("New thread in IronClaw", XS, p.text_faint)),
        )
        .child(t("↵ send", XS, p.text_faint))
        .child(t("esc close", XS, p.text_faint));
    if let Some(controls) = controls {
        hints = hints.child(controls);
    }
    col()
        .px(16.)
        .pt(12.)
        .pb(8.)
        .gap(8.)
        .child(el().grow(1.0).min_h(0.0).child(input))
        .child(hints)
}

/// The scene renders the panel in the main window's state (headless tools
/// draw only the primary window): the same card, with the field's text as
/// plain text and the controls drawn over the main state.
pub fn fixture_card(s: &State) -> El {
    let p = &s.palette;
    let text = &s.workspace.quick.input.text;
    let input = if text.is_empty() {
        t(PLACEHOLDER, 16., p.text_faint)
    } else {
        t(text.clone(), 16., p.text)
    };
    let controls = crate::voice::ui::draw(
        &Controls::of(s, &Composer::Quick),
        p,
        |press: Press| -> OnPress<State> {
            Box::new(move |s: &mut State| {
                crate::voice::ui::press(s, press, Composer::Quick, None);
            })
        },
        |mic| mic,
    );
    ui::floating(
        p,
        card(p, input, Some(controls))
            .w(WIDTH - 16.)
            .h(HEIGHT - 16.),
        16.,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        let spec = AppSpec::new("test", (100., 100.), unreachable_main, |_: &State, _| el());
        let handle = App::new(spec).handle();
        State::new(crate::jobs::Rt::new(handle))
    }

    fn unreachable_main() -> State {
        unreachable!("tests build their own state")
    }

    #[test]
    fn the_quick_composer_sits_a_fifth_of_the_way_down() {
        assert_eq!(top_margin(Some(1117.)), 223.);
        assert_eq!(top_margin(Some(900.)), 180.);
        assert_eq!(top_margin(None), 180.);
    }

    #[test]
    fn an_empty_quick_send_refuses_talk_modes_claim_and_stays_open() {
        let mut s = state();
        s.fixture = true;
        s.workspace.quick.open = true;
        let sub = crate::voice::fixtures::claim(&mut s, Composer::Quick);
        send(&mut s, "   \n".into());
        assert!(
            !crate::voice::fixtures::waits_for(&s, sub),
            "the claim was refused"
        );
        assert!(crate::voice::take_submission(&mut s, &Composer::Quick).is_none());
        assert!(
            s.workspace.quick.is_open(),
            "nothing was sent, so it stays open"
        );
        assert!(s.workspace.draft.is_none(), "no new thread was started");
    }

    #[test]
    fn a_quick_send_takes_the_claim_into_the_new_thread() {
        let mut s = state();
        s.fixture = true;
        s.workspace.quick.open = true;
        let sub = crate::voice::fixtures::claim(&mut s, Composer::Quick);
        send(&mut s, "  hello  ".into());
        // Taken by this send: no one else can take it, and it still waits
        // for the gateway (no backend here, so the thread is still being
        // made).
        assert!(crate::voice::take_submission(&mut s, &Composer::Quick).is_none());
        assert!(crate::voice::fixtures::waits_for(&s, sub));
        assert!(!s.workspace.quick.is_open());
        // A submission of its own: the new-thread draft is not written over.
        assert_eq!(
            s.workspace.draft.as_ref().map(|d| d.input.text.as_str()),
            Some("")
        );
    }

    #[test]
    fn voice_edits_reach_the_mirror_only_while_open() {
        let mut s = state();
        assert_eq!(voice_edit(&mut s, |e: &mut TextEdit| e.insert("x")), None);
        s.workspace.quick.open = true;
        assert_eq!(
            voice_edit(&mut s, |e: &mut TextEdit| e.insert("hi")),
            Some(())
        );
        assert_eq!(s.workspace.quick.input.text, "hi");
    }
}
