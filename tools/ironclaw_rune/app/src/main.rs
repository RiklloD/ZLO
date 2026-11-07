#![allow(dead_code)] // During the port: foundation items land before their users.
//! ZLO on Rune: a native window onto the local IronClaw gateway.
//!
//! A from-scratch port of the GPUI app (`tools/ironclaw_gpui/app`), which is
//! left untouched. Same gateway client, same preferences file, same
//! behaviour; see `../../PLAN.md` for the port plan and conventions.

mod chat;
mod design;
mod icons;
mod jobs;
mod login_item;
mod native_view;
mod pages;
mod permissions;
mod scenes;
mod service;
mod snapshot;
mod state;
mod voice;
mod workspace;

use rune::{App, AppSpec, Cx, El, KeyEvent, Tx};

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
        tracing::debug!("quit requested");
        service::stop();
        tx.quit();
        return true;
    }
    workspace::on_key(s, ev, tx)
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ironclaw_rune=info".into()),
        )
        .init();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "ZLO (Rune) started");
    // ZLO owns the local backend's lifetime: started at launch, stopped on
    // quit (area F, `service.rs`). Headless scenes and scripts leave it be.
    let headless = rune::is_headless();
    if !headless {
        service::start();
    }

    let spec = AppSpec::<State> {
        min_size: Some((720.0, 480.0)),
        fonts: design::FONTS,
        families: design::FAMILIES,
        poll: Some(poll),
        on_key: Some(on_key),
        scenes: scenes::SCENES,
        ..AppSpec::new("ZLO", (1280.0, 820.0), placeholder_init, view)
    };
    let app = App::new(spec);
    let handle = app.handle();
    let app = app
        .init(move || {
            let mut s = State::new(jobs::Rt::new(handle));
            workspace::start(&mut s);
            s
        })
        .keep_running()
        .on_reopen(|_, has_visible, _| has_visible);
    app.run();
    if !headless {
        service::stop();
    }
}

/// `AppSpec::init` wants a plain fn; the real state comes from `App::init`.
/// Headless scenes use this one, with no window behind its handle.
fn placeholder_init() -> State {
    unreachable!("ZLO's state is built by App::init")
}
