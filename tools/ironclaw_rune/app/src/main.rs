//! ZLO: a native Rune window onto the local IronClaw gateway.
//!
//! A from-scratch port of the earlier GPUI app, now archived on the
//! `archive/gpui` branch (tag `gpui-final`). Same preferences file, same
//! behaviour; see `../../PLAN.md` for the port plan and conventions.
//!
//! Opening the app starts this Mac's gateway and quitting (⌘Q) stops it
//! (`service.rs`), unless another computer is in use or "Run IronClaw on
//! this Mac" is off (`remote.rs`). Closing the window keeps the app and the gateway
//! running; clicking the Dock icon brings the window back. The app finds the
//! gateway and its token on its own; see
//! `zlo_client::Connection::discover`.

mod alerts;
mod backdrop;
mod chat;
mod design;
mod drafts;
mod energy;
mod icons;
mod jobs;
mod login_item;
mod logs;
mod native_view;
mod onboarding;
mod pages;
mod permissions;
mod persist;
mod remote;
mod run_clock;
mod scenes;
mod service;
mod snapshot;
mod state;
mod titles;
mod voice;
mod window_state;
mod workspace;

use rune::{App, AppSpec, Chrome, Cx, El, KeyEvent, Tx, WindowOptions};

use crate::state::State;

fn view(s: &State, cx: &mut Cx<State>) -> El {
    energy::hit(energy::C::ViewRun);
    // Appearance → UI font size scales every shared text style this build.
    design::styles::set_type_scale(&s.prefs);
    design::look::apply_motion(s, cx);
    if let Some(preview) = design::gallery::preview(s, cx) {
        return preview;
    }
    workspace::view(s, cx)
}

fn poll(s: &mut State, clock: f64, tx: &mut Tx) -> bool {
    use energy::{C, polled};
    energy::hit(C::PollCall);
    // The battery saver first: the pollers below read it, and resume in
    // this same poll when it lets go.
    let mut changed = polled(C::PollLook, design::look::poll(s, tx));
    changed |= polled(C::PollWorkspace, workspace::poll(s, tx));
    // The quick composer's voice controls live in its own window.
    changed |= polled(C::PollQuick, workspace::quick::poll(s));
    // The chat counts its own sources (pacer, clock, copy check, computer).
    changed |= chat::poll(s, tx);
    // A closed dock's column stays drawn until its clip has slid shut.
    changed |= polled(C::PollSide, chat::side::poll(s, clock, tx));
    changed |= polled(C::PollVoice, voice::poll(s, tx));
    changed |= polled(C::PollSnapshot, snapshot::poll(s, tx));
    // Show, place and hide the native previews (QuickLook, WebKit).
    changed |= polled(C::PollNativeView, native_view::poll(s, tx));
    changed
}

/// Window focus: the battery saver (reduced motion while unfocused) first,
/// so the workspace's refreshes on refocus see it released, then the
/// workspace (refreshes, composer focus).
fn window_focus(s: &mut State, focused: bool, tx: &mut Tx) {
    design::look::on_focus(s, focused, tx);
    workspace::window_focus(s, focused, tx);
}

fn on_key(s: &mut State, ev: &KeyEvent, tx: &mut Tx) -> bool {
    if ev.is_command('q') && !ev.mods.shift {
        workspace::quit(s, tx);
        return true;
    }
    workspace::on_key(s, ev, tx)
}

fn main() {
    logs::init();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "ZLO (Rune) started");
    // ZLO owns the local backend's lifetime: started at launch, stopped on
    // quit (`service.rs`). Headless scenes and scripts leave it be.
    let headless = rune::is_headless();
    // The computers list decides whether this Mac runs a backend at all
    // (Settings → Computers); it is read before anything starts.
    let launch = (!headless).then(remote::Launch::read);
    if let Some(launch) = &launch {
        service::set_allowed(launch.runs_local_backend());
        service::start();
        service::stop_on_terminate();
        energy::start();
    }
    // Motion nobody steers (a reply streaming in, the list following it)
    // paces at 60 Hz; any input lifts the cap, so scrolling and typing keep
    // the panel's full 120 Hz.
    rune::cap_ambient_motion(true);

    let spec = AppSpec::<State> {
        min_size: Some((720.0, 480.0)),
        fonts: design::FONTS,
        families: design::FAMILIES,
        poll: Some(poll),
        on_key: Some(on_key),
        scenes: scenes::SCENES,
        ..AppSpec::new("ZLO", (1280.0, 820.0), placeholder_init, view)
    };
    // Translucency is fixed when Rune creates the window, so the pref is
    // read here, at launch (`design::styles`).
    let translucent = !headless && design::prefs::Prefs::load().translucent;
    if !headless {
        design::styles::set_window_translucent(translucent);
    }
    // Content runs under a transparent titlebar; the sidebar keeps its top
    // strip free (`.window_drag()`) so the window still drags from it.
    let mut app = App::new(spec)
        .chrome(Chrome {
            full_size_content: true,
            traffic_lights: Some((16.0, 17.0)),
            drag_regions: true,
        })
        .window_options(WindowOptions {
            transparent: translucent,
            ..WindowOptions::default()
        })
        .keep_running()
        // ⌘ held alone shows the sidebar's ⌘N badges.
        .modifier_keys()
        .on_focus(window_focus)
        // The energy meter's frame counts: one atomic add per frame.
        .on_frame_encoded(energy::frame)
        .on_close(workspace::on_close)
        .on_window(move |window| {
            window_state::created(window);
            backdrop::on_window(window, translucent);
            // Native previews sit beside the window's content view.
            native_view::on_window(window);
        })
        .on_reopen(|_, has_visible, _| has_visible);
    for item in workspace::menu_items() {
        app = app.menu_item(item);
    }
    let (app, quick) = workspace::quick::install(app);
    let handle = app.handle();
    let app = app.init(move || {
        let mut s = State::new(jobs::Rt::new(handle));
        if let Some(launch) = launch {
            s.remote.launched(launch);
        }
        workspace::quick::attach(&mut s, quick);
        design::look::start(&mut s);
        workspace::start(&mut s);
        s
    });
    app.run();
    if !headless {
        service::stop();
        remote::tunnel::close();
    }
}

/// `AppSpec::init` wants a plain fn; the real state comes from `App::init`.
/// Headless scenes use this one, with no window behind its handle.
fn placeholder_init() -> State {
    unreachable!("ZLO's state is built by App::init")
}
