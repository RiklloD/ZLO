#![allow(dead_code)] // During the port: foundation items land before their users.
//! ZLO on Rune: a native window onto the local IronClaw gateway.
//!
//! A from-scratch port of the GPUI app (`tools/ironclaw_gpui/app`), which is
//! left untouched. Same gateway client, same preferences file, same
//! behaviour; see `../../PLAN.md` for the port plan and conventions.
//!
//! Opening the app starts the gateway's launchd job and quitting (⌘Q) stops
//! it (`service.rs`). Closing the window keeps the app and the gateway
//! running; clicking the Dock icon brings the window back. The app finds the
//! gateway and its token on its own; see
//! `ironclaw_gpui_client::Connection::discover`.

mod alerts;
mod chat;
mod design;
mod drafts;
mod icons;
mod jobs;
mod logs;
mod native_view;
mod pages;
mod run_clock;
mod scenes;
mod service;
mod snapshot;
mod state;
mod titles;
mod voice;
mod window_state;
mod workspace;

use rune::{App, AppSpec, Chrome, Cx, El, KeyEvent, Tx};

use crate::state::State;

fn view(s: &State, cx: &mut Cx<State>) -> El {
    workspace::view(s, cx)
}

fn poll(s: &mut State, _clock: f64, tx: &mut Tx) -> bool {
    let mut changed = workspace::poll(s, tx);
    changed |= chat::poll(s, tx);
    changed |= voice::poll(s, tx);
    changed |= snapshot::poll(s, tx);
    changed
}

fn on_key(s: &mut State, ev: &KeyEvent, tx: &mut Tx) -> bool {
    if ev.is_command('q') && !ev.mods.shift {
        tracing::info!("quit requested");
        workspace::on_close(s, tx);
        tx.quit();
        return true;
    }
    workspace::on_key(s, ev, tx)
}

fn main() {
    logs::init();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "ZLO (Rune) started");
    service::start();

    let spec = AppSpec::<State> {
        min_size: Some((720.0, 480.0)),
        fonts: design::FONTS,
        families: design::FAMILIES,
        poll: Some(poll),
        on_key: Some(on_key),
        scenes: scenes::SCENES,
        ..AppSpec::new("ZLO", (1280.0, 820.0), placeholder_init, view)
    };
    // Content runs under a transparent titlebar; the sidebar keeps its top
    // strip free (`.window_drag()`) so the window still drags from it.
    let mut app = App::new(spec)
        .chrome(Chrome {
            full_size_content: true,
            traffic_lights: Some((16.0, 17.0)),
            drag_regions: true,
        })
        .keep_running()
        // ⌘ held alone shows the sidebar's ⌘N badges.
        .modifier_keys()
        .on_focus(workspace::window_focus)
        .on_close(workspace::on_close)
        .on_window(window_state::created)
        .on_reopen(|_, has_visible, _| has_visible);
    for item in workspace::menu_items() {
        app = app.menu_item(item);
    }
    let (app, quick) = workspace::quick::install(app);
    let handle = app.handle();
    let app = app.init(move || {
        let mut s = State::new(jobs::Rt::new(handle));
        workspace::quick::attach(&mut s, quick);
        workspace::start(&mut s);
        s
    });
    app.run();
    service::stop();
}

/// `AppSpec::init` wants a plain fn; the real state comes from `App::init`.
/// Headless scenes use this one, with no window behind its handle.
fn placeholder_init() -> State {
    unreachable!("ZLO's state is built by App::init")
}
