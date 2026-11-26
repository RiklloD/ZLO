//! The window's drawing: the
//! loading and connection-error screens; then Zeron's shell of full-height
//! columns — the sidebar (inbox or settings nav, titlebar strip, footer), its
//! resize edge, and the main panel under the breadcrumb — with the status
//! bar below, the wallpaper layers over each area (`design::look` draws them), and
//! the ⌘K menu and dialogs above everything.

use rune::{Cx, Edge, El, Transition, col, el, row};
use rune_kit::{Dialog, Kit};

use super::ui::{SEMIBOLD, SM, XS, t, tw};
use super::{command, status};
use crate::design::{self, Palette};
use crate::icons;
use crate::state::{Conn, State};

mod frame;
mod main_area;
mod sidebar;
mod update_button;

use frame::ready;

pub(super) fn view(s: &State, cx: &mut Cx<State>) -> El {
    // Follow the system appearance. Only the view can read it (`Cx`), so a
    // change is queued back into the state: `s.system_dark` and
    // `s.palette` stay current for every reader (⇧⌘Y, pages, wallpaper).
    // The window's first frame paints with the system's side at once (the
    // state starts out guessing dark); after that a flip morphs the colours
    // from the palette on screen (`design::motion::color_morph`).
    let dark = cx.system_dark();
    let first = !s.workspace.painted.replace(true);
    if dark != s.system_dark {
        s.rt.handle.update(move |s, tx| {
            if !first {
                crate::design::motion::color_morph(tx);
            }
            apply_system_dark(s, dark);
        });
    }
    let p = if dark == s.system_dark || !first {
        s.palette
    } else {
        design::resolve(&s.prefs, dark)
    };
    note_field_focus(s, cx);
    let k = design::kit(&p);
    let mut layer: Vec<El> = Vec::new();
    let intro = s.intro.playing();
    if intro {
        s.intro.reduced.set(cx.is_reduced_motion());
    }
    // Loading, failed and the app crossfade where their commit animates (the
    // connect reply that brings the app or the failure asks for it). Under
    // the launch intro the stage's iris is the entrance: no crossfade too.
    let body = match &s.conn {
        // The intro draws the launch screen itself, over the shell; what
        // it opens onto mounts only once the reveal starts.
        _ if s.intro.covers() => el(),
        Conn::Connecting if intro => el(),
        Conn::Connecting => loading(s, &p),
        Conn::Failed { message, retry_in } => failed(s, &p, &k, message, *retry_in),
        Conn::Ready => ready(s, &p, cx, &k, &mut layer),
    };
    let body = if intro {
        body
    } else {
        body.transition(boot_swap())
    };
    // The shell: the surface at the style's shell alpha. Translucent, the
    // desktop shows through it, blurred by the WindowServer (`backdrop`).
    let st = design::styles::Style::of(&s.prefs);
    let shell = p.surface.alpha(st.shell_alpha);
    let mut root =
        col()
            .key("window")
            .w_full()
            .h_full()
            .fill(shell)
            .child(super::intro::settle_body(
                s,
                el().w_full().grow(1.0).min_h(0.0).child(body),
            ));
    // The intro's stage is the whole window: no bar under it until it opens.
    let staging = intro && matches!(s.conn, Conn::Connecting);
    if !s.prefs.status_bar_hidden && !staging {
        // Connected, on the panel's fill, so the bar is the same plane as
        // the panel above it rather than a darker strip of the shell; while
        // connecting or failed there is no panel, and it stays on the shell.
        // ⇧⌘S wipes it in from the window's edge and back out.
        let mut bar = el()
            .key("status-bar")
            .w_full()
            .transition(Transition::reveal(Edge::Bottom));
        if matches!(s.conn, Conn::Ready) {
            bar = bar.fill(p.bg.alpha(st.panel_alpha));
        }
        root = root.child(bar.child(status::bar(s, &k, &mut layer)));
    }
    if let Some(over) = crate::design::look::wallpaper_layer(s, "window") {
        root = root.child(over);
    }
    command::view(s, &k, &mut layer);
    if s.workspace.quick_fixture {
        layer.push(
            row()
                .pin(Some(0.), Some(120.), Some(0.), None)
                .justify_center()
                .child(super::quick::fixture_card(s)),
        );
    }
    confirm_delete(s, &k, &mut layer);
    update_button::confirm(s, &k, &mut layer);
    // First-run setup over the home screen, whatever the connection does
    // (it switches computers itself).
    if !s.intro.covers() {
        crate::onboarding::view(s, &k, &mut layer);
    }
    // The launch intro above everything, until it has irised open.
    if intro {
        layer.push(super::intro::stage(s, &p, shell));
    }
    // Native previews draw above Rune's view: while anything floats in the
    // overlay layer (⌘K, a menu or popover, a dialog) they hide.
    crate::native_view::cover(s, !layer.is_empty());
    // The thread-drag card floats too, but must not hide the previews.
    layer.extend(super::thread_drag::ghost(s));
    rune_kit::root(root, layer)
}

/// While the window is in front, note whether any text field (the
/// composer, an overlay's field, a side chat's) holds the caret; the
/// rune-kit root wraps the shell and its overlays. A refocus reads it
/// (`workspace::restore_composer`); while in the back it keeps the value
/// from before the window lost focus.
fn note_field_focus(s: &State, cx: &Cx<State>) {
    if s.workspace.window_active {
        s.workspace
            .field_focused
            .set(cx.caret("kit-root").is_some());
    }
}

/// The launch screens' swap (loading → the app or the failure): a fade
/// from a hair small, on the style's enter-like calm spring, out faster.
fn boot_swap() -> Transition {
    Transition::opacity()
        .combined(Transition::scale(0.98))
        .springs(rune::SpringPair::with_exit_ratio(
            rune::Spring::new(0.3, 0.0),
            0.6,
        ))
}

/// The loading caption's entrance: hidden 700 ms, then in over 300 ms.
const CAPTION_IN: &[rune::Beat] = &[
    rune::Beat::new(0.0).opacity(0.0),
    rune::Beat::new(0.7).opacity(0.0),
    rune::Beat::new(1.0),
];

/// Connecting again later in the session (the launch has its intro): the
/// wordmark still in the middle, its caption fading in late so a quick
/// reconnect shows no text at all.
fn loading(s: &State, p: &Palette) -> El {
    el().key("loading")
        .w_full()
        .h_full()
        .child(super::intro::still_mark(p, s.intro.size))
        .child(
            row()
                .pin(Some(0.), Some(s.intro.size[1] / 2. + 56.), Some(0.), None)
                .justify_center()
                .child(
                    t("Connecting to ZLO", SM, p.text_muted)
                        .key("loading-caption")
                        .sequence(rune::Sequence::new(CAPTION_IN)),
                ),
        )
}

fn failed(s: &State, p: &Palette, k: &Kit, message: &str, retry_in: Option<u64>) -> El {
    // A Light bundle with no computer yet: nothing failed, there is
    // nothing to connect to.
    if crate::remote::needs_computer(s) {
        return crate::pages::computers::first_computer(s, k);
    }
    // Another computer, or a computers list ZLO could not read: offer the
    // way back to this Mac (never taken on its own; a Light bundle has no
    // backend of its own to go back to).
    let remote = (crate::remote::backend_is_remote(s) || s.remote.registry_error.is_some())
        && !s.remote.app_only;
    // Triage in Terminal looks at this Mac's own backend: only when that
    // is what failed (never for a paired computer, a Light bundle, which
    // has no backend of its own, or an `IRONCLAW_DESKTOP_URL` gateway).
    let local_triage = !crate::remote::backend_is_remote(s)
        && !s.remote.app_only
        && !crate::remote::env_override();
    // This Mac's own backend was switched off (Settings → Computers, which
    // this screen covers): the way back is here.
    let local_off = local_triage && !s.remote.registry.run_local_backend;
    // Computers already connected in the background: one press away.
    let ready_peers: Vec<(String, String)> = crate::remote::peers::list(s)
        .into_iter()
        .filter(|peer| peer.conn == crate::remote::peers::PeerConn::Ready)
        .map(|peer| (peer.computer.key(), peer.computer.label()))
        .collect();
    let mut c = col()
        .key("failed")
        .w_full()
        .h_full()
        .center()
        .gap(12.)
        .padding(32.)
        .child(tw("Can't connect to ZLO", 16., SEMIBOLD, p.text))
        .child(
            el().w(520.).child(
                t(message.to_string(), SM, p.text_muted)
                    .wrap()
                    .align(rune::Align::Center),
            ),
        );
    if let Some(secs) = retry_in {
        c = c.child(t(format!("Trying again in {secs} s"), XS, p.text_faint));
    }
    c.child(
        row()
            .gap(8.)
            .child(
                rune_kit::Button::new("Try again")
                    .variant(rune_kit::ButtonVariant::Contrast)
                    .on_press(super::retry_connect)
                    .build(k),
            )
            .children(local_off.then(|| {
                rune_kit::Button::new("Run IronClaw on this Mac")
                    .icon(&icons::MONITOR)
                    .help("Turn this Mac's backend back on")
                    .on_press(|s: &mut State| crate::remote::pairing::set_run_local(s, true))
                    .build(k)
                    .key("failed-run-local")
            }))
            .children(ready_peers.into_iter().map(|(key, label)| {
                rune_kit::Button::new(format!("Use {label}"))
                    .icon(&icons::MONITOR)
                    .help(format!("Work on {label}, which is connected"))
                    .on_press({
                        let key = key.clone();
                        move |s: &mut State| crate::remote::peers::focus(s, &key)
                    })
                    .build(k)
                    .key(format!("failed-use-{key}"))
            }))
            .children(remote.then(|| {
                // A paired computer that does not answer must not strand
                // the app: back to this Mac's own gateway.
                rune_kit::Button::new("Use this Mac")
                    .icon(&icons::MONITOR)
                    .help(format!(
                        "Work on this Mac instead of {} (it stays saved)",
                        s.onboarding.computer.label()
                    ))
                    .on_press(crate::remote::use_this_mac)
                    .build(k)
            }))
            .children(local_triage.then(|| {
                // This Mac's backend is down, so in-app triage (which runs
                // on it) is out of reach: `zlo triage` in Terminal needs no
                // backend and no PATH (the bundled CLI, by full path).
                rune_kit::Button::new("Triage")
                    .icon(&icons::SQUARE_TERMINAL)
                    .help("Open Terminal with a coding agent that looks into what went wrong")
                    .on_press(super::triage_in_terminal)
                    .build(k)
            }))
            .child(
                // The gateway is down, so its in-app log ring is out of
                // reach: open the files instead.
                rune_kit::Button::new("Logs")
                    .icon(&icons::FILE)
                    .help("Open the app and gateway logs in Console")
                    .on_press(|s: &mut State| crate::logs::open_files(s))
                    .build(k),
            ),
    )
}

/// Deleting from the menu asks first: a right-click misfires more than the
/// settled row's trash button.
fn confirm_delete(s: &State, k: &Kit, layer: &mut Vec<El>) {
    let Some(id) = s.workspace.confirm_delete.clone() else {
        return;
    };
    let title = super::inbox::merged::find_row(s, &id)
        .map_or("this conversation", super::row_title)
        .to_string();
    let delete_id = id.clone();
    Dialog::new("confirm-delete", format!("Delete \"{title}\"?"))
        .open(true)
        .fit()
        .width(400.)
        .description("The conversation leaves the inbox and can't be opened again.")
        .child(
            row()
                .gap(8.)
                .justify_end()
                .child(
                    rune_kit::Button::new("Cancel")
                        .on_press(|s: &mut State| s.workspace.confirm_delete = None)
                        .build(k),
                )
                .child(
                    rune_kit::Button::new("Delete")
                        .variant(rune_kit::ButtonVariant::Danger)
                        .on_press(move |s: &mut State| {
                            s.workspace.confirm_delete = None;
                            super::inbox::delete_row(s, delete_id.clone());
                        })
                        .build(k),
                ),
        )
        .on_close(|s: &mut State| s.workspace.confirm_delete = None)
        .build(k, layer);
}

/// The system appearance flipped: re-resolve the palette (it matters only
/// for the "system" scheme, but `system_dark` is always kept true to macOS)
/// and repaint the wallpaper for the new side.
fn apply_system_dark(s: &mut State, dark: bool) {
    if s.system_dark == dark {
        return;
    }
    s.system_dark = dark;
    let palette = design::resolve(&s.prefs, dark);
    let flipped = palette.light != s.palette.light;
    s.palette = palette;
    if flipped {
        crate::design::look::reload(s);
    }
}

#[cfg(test)]
mod tests {
    use rune::{App, AppSpec, Env, Runtime};

    use crate::state::State;

    fn no_init() -> State {
        unreachable!("tests build the state themselves")
    }

    /// The app's real view over a fixture state; the handle goes to an app
    /// that never runs, so background replies are dropped.
    fn runtime(fill: fn(&mut State)) -> Runtime<State> {
        let app = App::new(AppSpec::new(
            "ZLO test",
            (1280., 820.),
            no_init,
            crate::view,
        ));
        let mut s = State::new(crate::jobs::Rt::new(app.handle()));
        fill(&mut s);
        let mut rt = Runtime::new(s, crate::view, (1280., 820.), Env::new(true, 1.));
        rt.frame(0.);
        rt
    }

    #[test]
    fn overlays_cover_the_native_previews() {
        let mut rt = runtime(crate::workspace::fixtures::fixture_command_menu);
        assert!(rt.state.native.covered(), "⌘K hides native previews");

        rt.update(|s, _| s.workspace.command = None);
        rt.frame(1. / 60.);
        assert!(
            !rt.state.native.covered(),
            "and they come back when it closes"
        );

        rt.update(|s, _| {
            s.workspace.confirm_delete = Some(crate::workspace::RowId::focused(s, "t-pin"))
        });
        rt.frame(1. / 60.);
        assert!(rt.state.native.covered(), "a dialog covers them too");
    }

    #[test]
    fn a_refocused_window_moves_focus_to_the_composer_only_when_no_field_had_it() {
        let mut rt = runtime(crate::chat::fixtures::chat);
        let composer =
            |rt: &Runtime<State>| rt.state.chat.as_ref().is_some_and(|c| c.composer.focused);

        // The composer holds the caret while the window is in front.
        rt.update(|s, _| {
            if let Some(c) = s.chat.as_mut() {
                c.composer.focused = true;
            }
        });
        rt.frame(1. / 60.);
        rt.frame(1. / 60.);
        assert!(
            rt.state.workspace.field_focused.get(),
            "the view notes a focused field"
        );

        // Nothing holds focus: back in front, the composer takes it.
        rt.update(|s, tx| {
            if let Some(c) = s.chat.as_mut() {
                c.composer.focused = false;
            }
            tx.blur();
        });
        rt.frame(1. / 60.);
        rt.frame(1. / 60.);
        assert!(!rt.state.workspace.field_focused.get());
        rt.update(|s, tx| crate::workspace::window_focus(s, false, tx));
        rt.update(|s, tx| crate::workspace::window_focus(s, true, tx));
        assert!(composer(&rt), "an unfocused window refocuses the composer");

        // Another field had focus (the URL bar, say): it keeps it.
        rt.update(|s, tx| {
            if let Some(c) = s.chat.as_mut() {
                c.composer.focused = false;
            }
            s.workspace.field_focused.set(true);
            crate::workspace::window_focus(s, false, tx);
        });
        rt.frame(1. / 60.);
        rt.update(|s, tx| crate::workspace::window_focus(s, true, tx));
        assert!(!composer(&rt), "the other field keeps focus");

        // The command menu keeps it too.
        rt.update(|s, tx| {
            s.workspace.field_focused.set(false);
            crate::workspace::window_focus(s, false, tx);
            s.workspace.command = Some(crate::workspace::command::CommandMenu {
                input: rune::TextEdit::new(""),
                selected: 0,
                opened: 1,
            });
        });
        rt.update(|s, tx| crate::workspace::window_focus(s, true, tx));
        assert!(!composer(&rt));
    }
}
