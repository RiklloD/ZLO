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

use rune::{
    Align, App, AppHandle, AppSpec, Color, Cx, DisplayTarget, El, Key, KeyEvent, OverlayPlacement,
    ScreenEdge, TextEdit, Tx, WindowOptions, WindowSpec, col, el, row,
};

use super::ui::{self, XS, t};
use crate::design::{self, Palette};
use crate::state::State;

pub const SHORTCUT: &str = "cmd+shift+space";
const WIDTH: f32 = 640.;
const HEIGHT: f32 = 132.;

/// The main window's side: the quick window's handle and whether it shows.
#[derive(Default)]
pub struct Quick {
    handle: Option<AppHandle<QuickState>>,
    open: bool,
}

impl Quick {
    pub fn is_open(&self) -> bool {
        self.open
    }
}

/// The quick window's own state.
pub struct QuickState {
    input: TextEdit,
    palette: Palette,
    main: AppHandle<State>,
    /// Became the key window once; only then does losing focus close it.
    seen_active: bool,
}

/// Add the quick window and its global shortcut to the app. Call before
/// `App::init`; hand the result to [`attach`] from the init closure.
pub fn install(app: App<State>) -> (App<State>, AppHandle<QuickState>) {
    let main = app.handle();
    let spec = AppSpec {
        fonts: design::FONTS,
        families: design::FAMILIES,
        on_key: Some(quick_key),
        ..AppSpec::new("ZLO Quick", (WIDTH, HEIGHT), unreachable_init, view)
    };
    let quick = App::new(spec)
        .init(move || {
            let prefs = crate::design::prefs::Prefs::load();
            QuickState {
                input: TextEdit::new(""),
                palette: design::resolve(&prefs, true),
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
    let (handle, _window) =
        app.window(WindowSpec::from_app("quick", quick).configured_options(WindowOptions::launcher_panel()));
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
        q.seen_active = false;
    });
    // Spotlight's spot: centred, near the top of the screen.
    q.place(
        DisplayTarget::Primary,
        OverlayPlacement::Edge {
            edge: ScreenEdge::Top,
            margin: 180.,
            align: Align::Center,
            inset: 0.,
        },
    );
    q.open();
    s.workspace.quick.open = true;
}

/// Type `text` into the open quick composer (voice).
pub fn insert_text(s: &State, text: String) {
    if let Some(q) = &s.workspace.quick.handle {
        q.update(move |q, _| {
            let mut full = q.input.text.clone();
            full.push_str(&text);
            q.input.replace_text(full);
        });
    }
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

fn submit(q: &mut QuickState, tx: &mut Tx) {
    let text = q.input.text.trim().to_string();
    if text.is_empty() {
        return;
    }
    q.input.clear();
    q.main.update(move |s, _| {
        s.workspace.quick.open = false;
        super::draft::quick_send(s, text);
    });
    q.main.open();
    close_from_quick(q, tx);
}

fn view(q: &QuickState, _cx: &mut Cx<QuickState>) -> El {
    let p = &q.palette;
    let k = design::kit(p);
    let _ = &k;
    let input = ui::grow_box(
        ui::plain_area(&q.input, 16., "Ask IronClaw anything…", p)
            .on_edit(|q: &mut QuickState| &mut q.input)
            .on_submit(submit),
        1,
        4,
    );
    let card = col()
        .w_full()
        .h_full()
        .px(16.)
        .pt(12.)
        .pb(8.)
        .gap(8.)
        .child(el().grow(1.0).min_h(0.0).child(input))
        .child(
            row()
                .gap(12.)
                .center_y()
                .child(ui::dot(p.accent, 6.))
                .child(el().grow(1.0).child(t("New thread in IronClaw", XS, p.text_faint)))
                .child(t("↵ send", XS, p.text_faint))
                .child(t("esc close", XS, p.text_faint)),
        );
    el().w_full()
        .h_full()
        .padding(8.)
        .fill(Color::TRANSPARENT)
        .child(ui::floating(p, card, 16.))
}

/// The scene renders the panel in the main window's state (headless tools
/// draw only the primary window): a fixture copy of the card.
pub fn fixture_card(s: &State) -> El {
    let p = &s.palette;
    let card = col()
        .w(WIDTH - 16.)
        .h(HEIGHT - 16.)
        .px(16.)
        .pt(12.)
        .pb(8.)
        .gap(8.)
        .child(el().grow(1.0).child(t("Ask IronClaw anything…", 16., p.text_faint)))
        .child(
            row()
                .gap(12.)
                .center_y()
                .child(ui::dot(p.accent, 6.))
                .child(el().grow(1.0).child(t("New thread in IronClaw", XS, p.text_faint)))
                .child(t("↵ send", XS, p.text_faint))
                .child(t("esc close", XS, p.text_faint)),
        );
    ui::floating(p, card, 16.)
}
