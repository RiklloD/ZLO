//! ZLO: a native GPUI window onto the local IronClaw gateway.
//!
//! Opening the app starts the gateway's launchd job and quitting (Cmd+Q)
//! stops it (`service.rs`). Closing the window keeps the app and the gateway
//! running; clicking the Dock icon opens a new window. The app finds the gateway and its token on its own; see
//! `ironclaw_gpui_client::Connection::discover`.

mod alerts;
mod assets;
mod chat;
mod design;
mod drafts;
mod jobs;
mod login_item;
mod logs;
mod native_view;
mod pages;
mod permissions;
mod run_clock;
mod service;
mod snapshot;
mod titles;
mod ui;
mod window_state;
mod workspace;

use gpui_kit::{
    App, AppContext, KeyBinding, Menu, MenuItem, TitlebarOptions, WindowOptions, point, px,
};

gpui_kit::actions!(
    ironclaw,
    [
        Quit,
        ToggleStatusBar,
        ToggleScheme,
        OpenCommandMenu,
        OpenModelPicker,
        NewThread,
        ToggleSidebar,
        ToggleRightPane,
        ToggleWorkPanel,
        NewSideChat
    ]
);

fn main() {
    logs::init();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "ZLO started");

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
        drafts::load(cx);
        cx.on_app_quit(|cx| {
            window_state::save(cx);
            drafts::save(cx, true);
            service::stop();
            async {}
        })
        .detach();
        cx.on_action(|_: &Quit, cx| {
            tracing::info!("quit requested");
            cx.quit()
        });
        cx.on_action(|_: &ToggleStatusBar, cx| workspace::toggle_status_bar(cx));
        // Flip between the light and dark theme without opening Settings.
        cx.on_action(|_: &ToggleScheme, cx| design::toggle_scheme(cx));
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-shift-s", ToggleStatusBar, None),
            KeyBinding::new("cmd-shift-y", ToggleScheme, None),
            KeyBinding::new("cmd-k", OpenCommandMenu, None),
            KeyBinding::new("cmd-shift-m", OpenModelPicker, None),
            KeyBinding::new("cmd-shift-o", NewThread, None),
            KeyBinding::new("cmd-b", ToggleSidebar, None),
            KeyBinding::new("alt-cmd-b", ToggleRightPane, None),
            KeyBinding::new("cmd-e", ToggleWorkPanel, None),
            KeyBinding::new("alt-cmd-n", NewSideChat, None),
        ]);
        cx.set_menus(vec![
            Menu {
                name: "ZLO".into(),
                items: vec![MenuItem::action("Quit ZLO", Quit)],
                disabled: false,
            },
            Menu {
                name: "View".into(),
                items: vec![
                    MenuItem::action("New Thread", NewThread),
                    MenuItem::action("Toggle Sidebar", ToggleSidebar),
                    MenuItem::action("Toggle Right Pane", ToggleRightPane),
                    MenuItem::action("Toggle Changes and Files", ToggleWorkPanel),
                    MenuItem::action("New Side Chat", NewSideChat),
                    MenuItem::action("Command Menu…", OpenCommandMenu),
                    MenuItem::action("Model Picker…", OpenModelPicker),
                    MenuItem::action("Toggle Status Bar", ToggleStatusBar),
                    MenuItem::action("Toggle Light and Dark", ToggleScheme),
                ],
                disabled: false,
            },
        ]);
        open_main_window(cx);
        cx.activate(true);
    });
}

fn open_main_window(cx: &mut App) {
    let options = WindowOptions {
        window_bounds: Some(window_state::initial(cx)),
        // Content runs under a transparent titlebar; the sidebar
        // keeps its top strip free so macOS still drags the window.
        titlebar: Some(TitlebarOptions {
            title: Some("ZLO".into()),
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
        let view = cx.new(|cx| workspace::Workspace::new(window, cx));
        view.update(cx, |_, cx| {
            cx.observe_window_bounds(window, |_, window, cx| window_state::record(window, cx))
                .detach()
        });
        view
    }) {
        eprintln!("zlo: cannot open a window: {e}");
        cx.quit();
        return;
    }
    cx.activate(true);
}
