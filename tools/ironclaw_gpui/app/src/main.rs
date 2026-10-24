//! IronClaw Desktop: a native GPUI window onto the local IronClaw gateway.
//!
//! Opening the app starts the gateway's launchd job and quitting (Cmd+Q)
//! stops it (`service.rs`). Closing the window keeps the app and the gateway
//! running; clicking the Dock icon opens a new window. The app finds the gateway and its token on its own; see
//! `ironclaw_gpui_client::Connection::discover`.

mod alerts;
mod assets;
mod chat;
mod design;
mod jobs;
mod login_item;
mod pages;
mod service;
mod titles;
mod ui;
mod workspace;

use gpui_kit::{
    App, AppContext, Bounds, KeyBinding, Menu, MenuItem, TitlebarOptions, WindowBounds,
    WindowOptions, point, px, size,
};

gpui_kit::actions!(
    ironclaw,
    [Quit, ToggleStatusBar, NextStyle, OpenCommandMenu]
);

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("IRONCLAW_DESKTOP_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let app = gpui_kit::application().with_assets(assets::AppAssets);
    app.on_reopen(|cx| {
        if cx.windows().is_empty() {
            open_main_window(cx);
        }
    });
    app.run(|cx: &mut App| {
        gpui_kit::init(cx);
        design::install(cx);
        service::start();
        cx.on_app_quit(|_| {
            service::stop();
            async {}
        })
        .detach();
        cx.on_action(|_: &Quit, cx| {
            tracing::info!("quit requested");
            cx.quit()
        });
        cx.on_action(|_: &ToggleStatusBar, cx| workspace::toggle_status_bar(cx));
        // Flip through the four design styles without opening Settings.
        cx.on_action(|_: &NextStyle, cx| {
            let all = design::styles::StyleId::ALL;
            let now = design::Style::of(cx).id;
            let ix = all.iter().position(|s| *s == now).unwrap_or(0);
            design::set_style(all[(ix + 1) % all.len()], cx);
        });
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-shift-s", ToggleStatusBar, None),
            KeyBinding::new("cmd-shift-y", NextStyle, None),
            KeyBinding::new("cmd-k", OpenCommandMenu, None),
        ]);
        cx.set_menus(vec![
            Menu {
                name: "IronClaw".into(),
                items: vec![MenuItem::action("Quit IronClaw", Quit)],
                disabled: false,
            },
            Menu {
                name: "View".into(),
                items: vec![
                    MenuItem::action("Command Menu…", OpenCommandMenu),
                    MenuItem::action("Toggle Status Bar", ToggleStatusBar),
                    MenuItem::action("Next Design Style", NextStyle),
                ],
                disabled: false,
            },
        ]);
        open_main_window(cx);
        cx.activate(true);
    });
}

fn open_main_window(cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(1120.), px(780.)), cx);
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        // Content runs under a transparent titlebar; the sidebar
        // keeps its top strip free so macOS still drags the window.
        titlebar: Some(TitlebarOptions {
            title: Some("IronClaw".into()),
            appears_transparent: true,
            traffic_light_position: Some(point(px(16.), px(17.))),
        }),
        ..Default::default()
    };
    if let Err(e) = gpui_kit::open_window(options, cx, |window, cx| {
        // The System color scheme follows macOS as it flips.
        window
            .observe_window_appearance(|_, cx| {
                if design::prefs::Prefs::of(cx).scheme == "system" {
                    design::reapply(cx);
                }
            })
            .detach();
        cx.new(|cx| workspace::Workspace::new(window, cx))
    }) {
        eprintln!("ironclaw-desktop: cannot open a window: {e}");
        cx.quit();
        return;
    }
    cx.activate(true);
}
