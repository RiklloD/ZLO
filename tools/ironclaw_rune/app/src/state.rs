//! The app's one state tree. Rune hands every handler `&mut State`; there are
//! no per-view entities. Each feature area owns its sub-state type in its own
//! module and a slot here, so areas plug in without touching each other:
//!
//! | Slot | Owner module | GPUI origin |
//! |---|---|---|
//! | `workspace` | `workspace` | `workspace.rs`, `workspace/*`, alerts, drafts, titles |
//! | `chat` | `chat` | `chat.rs`, `chat/*` (minus side) |
//! | `pages` | `pages` | `pages/*` |
//! | `voice` | `voice` | `voice/*` |
//! | `snapshot` | `snapshot` | `snapshot/*` |
//! | `design` | `design::look` | wallpaper, battery saver, motion prefs |
//!
//! Handlers reach their slot from the root (`s.chat.as_mut()`, `&mut s.pages`)
//! — Rune has no state lenses, every handler closure is `Fn(&mut State)`.

use ironclaw_gpui_client::wire::SessionInfo;

use crate::design::{self, Palette, prefs::Prefs};
use crate::jobs::Rt;

/// What the main column shows. The sidebar and the page router read it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Page {
    #[default]
    Chat,
    /// The new-thread screen.
    Draft,
    Notifications,
    Automations,
    Files,
    Projects,
    Profiles,
    Extensions,
    Skills,
    Settings,
    Appearance,
    Permissions,
    Traces,
    Voice,
    Providers,
    Operator,
    Usage,
    Memory,
    Hooks,
}

/// Gateway connection state.
#[derive(Clone, Debug, Default)]
pub enum Conn {
    #[default]
    Connecting,
    /// `retry_in` is set when the app retries on its own.
    Failed {
        message: String,
        retry_in: Option<u64>,
    },
    Ready,
}

pub struct State {
    pub rt: Rt,
    pub prefs: Prefs,
    /// Resolved from `prefs` and the system appearance; see `design::resolve`.
    pub palette: Palette,
    /// The system appearance last seen by the view (for "system" scheme).
    pub system_dark: bool,
    pub conn: Conn,
    pub session: Option<SessionInfo>,
    pub page: Page,
    pub launched: std::time::Instant,
    /// A headless scene's fixture state: async replies (gateway, timers)
    /// are dropped so they cannot overwrite it. Set by `scenes.rs`.
    pub fixture: bool,
    pub workspace: crate::workspace::WorkspaceState,
    /// The open thread, if any.
    pub chat: Option<crate::chat::ChatState>,
    pub pages: crate::pages::PagesState,
    pub voice: crate::voice::VoiceState,
    pub snapshot: crate::snapshot::SnapshotState,
    /// Wallpaper, battery saver and other look state (area G).
    pub look: crate::design::look::LookState,
    /// At most two native macOS views (area S).
    pub native: crate::native_view::NativeViews,
}

impl State {
    pub fn new(rt: Rt) -> Self {
        let prefs = Prefs::load();
        let palette = design::resolve(&prefs, true);
        Self {
            rt,
            prefs,
            palette,
            system_dark: true,
            conn: Conn::Connecting,
            session: None,
            page: Page::Chat,
            launched: std::time::Instant::now(),
            fixture: false,
            workspace: Default::default(),
            chat: None,
            pages: Default::default(),
            voice: Default::default(),
            snapshot: Default::default(),
            look: Default::default(),
            native: Default::default(),
        }
    }

    /// Edit preferences, save them and re-resolve the palette.
    pub fn update_prefs(&mut self, edit: impl FnOnce(&mut Prefs)) {
        edit(&mut self.prefs);
        self.prefs.save();
        self.palette = design::resolve(&self.prefs, self.system_dark);
    }
}

/// A state over an app that never runs: background replies are dropped, so
/// tests call the reply handlers themselves.
#[cfg(test)]
pub fn test_state() -> State {
    fn no_init() -> State {
        unreachable!("tests build the state themselves")
    }
    let app = rune::App::new(rune::AppSpec::new("ZLO test", (1280., 820.), no_init, crate::view));
    State::new(Rt::new(app.handle()))
}
