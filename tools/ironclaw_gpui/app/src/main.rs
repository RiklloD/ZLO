//! IronClaw Desktop: a native GPUI window onto the local IronClaw gateway.
//!
//! Opening the app starts the gateway's launchd job and quitting stops it
//! (`service.rs`). The app finds the gateway and its token on its own; see
//! `ironclaw_gpui_client::Connection::discover`.

mod alerts;
mod chat;
mod jobs;
mod login_item;
mod pages;
mod service;
mod titles;
mod ui;
mod workspace;

use gpui_kit::{
    App, AppContext, Bounds, KeyBinding, Menu, MenuItem, TitlebarOptions, WindowBounds,
    WindowOptions, px, size,
};

gpui_kit::actions!(ironclaw, [Quit]);

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("IRONCLAW_DESKTOP_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(|cx: &mut App| {
            gpui_kit::init(cx);
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
            cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
            cx.set_menus(vec![Menu {
                name: "IronClaw".into(),
                items: vec![MenuItem::action("Quit IronClaw", Quit)],
                disabled: false,
            }]);
            cx.on_window_closed(|cx, _| {
                if cx.windows().is_empty() {
                    tracing::info!("last window closed, quitting");
                    cx.quit();
                }
            })
            .detach();

            let bounds = Bounds::centered(None, size(px(1120.), px(780.)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("IronClaw".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            if let Err(e) = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| workspace::Workspace::new(window, cx))
            }) {
                eprintln!("ironclaw-desktop: cannot open a window: {e}");
                cx.quit();
                return;
            }
            cx.activate(true);
        });
}
