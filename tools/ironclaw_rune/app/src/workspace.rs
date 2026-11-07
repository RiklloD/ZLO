//! Window root (t3code's layout): connects to the gateway; the sidebar lists
//! threads, or the settings pages while one is open; the panel shows the
//! open chat, the new-thread screen or a settings page.
//!
//! FOUNDATION VERSION: connect/retry, the page router and a minimal sidebar.
//! The workspace port (PLAN.md, area B) replaces the sidebar with the inbox,
//! and adds the command menu, draft, quick composer, context meter, status
//! bar, message search, alerts and drafts, porting `workspace.rs` and
//! `workspace/*` from the GPUI app file by file.

use std::time::Duration;

use ironclaw_gpui_client::{Backend, Client, ClientError, Connection};
use rune::{Cx, El, KeyEvent, Tx, col, el, row, text};

use crate::design::{self, Palette};
use crate::jobs;
use crate::state::{Conn, Page, State};

/// Retry schedule while the gateway is unreachable (seconds, last repeats).
const CONNECT_RETRY: [u64; 5] = [1, 2, 5, 10, 30];
/// The app starts the gateway itself, so for this long after launch a
/// refused connect means "still booting".
const STARTUP_GRACE: Duration = Duration::from_secs(25);
const STARTUP_POLL: Duration = Duration::from_millis(150);

#[derive(Default)]
pub struct WorkspaceState {
    pub connect_attempt: usize,
    /// Inbox rows (thread id, title), newest first. The inbox port replaces
    /// this with the full `inbox::Inbox`.
    pub threads: Vec<(String, String)>,
}

/// Called once from `App::init`, before the first frame.
pub fn start(s: &mut State) {
    connect(s);
}

pub fn poll(_s: &mut State, _tx: &mut Tx) -> bool {
    false
}

pub fn on_key(_s: &mut State, _ev: &KeyEvent, _tx: &mut Tx) -> bool {
    false
}

fn connect(s: &mut State) {
    s.conn = Conn::Connecting;
    let backend = match Connection::discover()
        .and_then(Client::new)
        .and_then(Backend::new)
    {
        Ok(backend) => backend,
        Err(e) => {
            tracing::warn!(error = %e, "gateway connection settings unavailable");
            s.conn = Conn::Failed {
                message: e.to_string(),
                retry_in: None,
            };
            return;
        }
    };
    s.rt.backend = Some(backend);
    jobs::run(
        s,
        "workspace",
        |c| async move {
            let session = c.session().await?;
            let inbox = c.inbox().await?;
            Ok::<_, ClientError>((session, inbox))
        },
        |s, result, _tx| match result {
            Ok((session, inbox)) => {
                s.workspace.connect_attempt = 0;
                s.session = Some(session);
                s.workspace.threads = inbox
                    .rows
                    .iter()
                    .map(|i| (i.thread_id.clone(), i.title.clone().unwrap_or_default()))
                    .collect();
                s.conn = Conn::Ready;
                if let Some((id, _)) = s.workspace.threads.first().cloned() {
                    open_thread(s, id);
                }
            }
            Err(e) => connect_failed(s, &e),
        },
    );
}

fn connect_failed(s: &mut State, error: &ClientError) {
    let network = matches!(error, ClientError::Network(_));
    if network && s.launched.elapsed() < STARTUP_GRACE {
        s.conn = Conn::Connecting;
        jobs::after(s, STARTUP_POLL, |s, _| {
            if matches!(s.conn, Conn::Connecting) {
                connect(s);
            }
        });
        return;
    }
    tracing::warn!(error = %error, attempt = s.workspace.connect_attempt, "gateway connection failed");
    let attempt = s.workspace.connect_attempt;
    let retry_in = network.then(|| CONNECT_RETRY[attempt.min(CONNECT_RETRY.len() - 1)]);
    s.conn = Conn::Failed {
        message: error.to_string(),
        retry_in,
    };
    let Some(secs) = retry_in else { return };
    s.workspace.connect_attempt += 1;
    jobs::after(s, Duration::from_secs(secs), |s, _| {
        if matches!(s.conn, Conn::Failed { .. }) {
            connect(s);
        }
    });
}

/// Open a thread in the main column. Other areas call this.
pub fn open_thread(s: &mut State, thread_id: String) {
    s.page = Page::Chat;
    crate::chat::open(s, thread_id);
}

/// Show a page in the main column. Other areas call this.
pub fn show(s: &mut State, page: Page) {
    s.page = page;
    crate::pages::opened(s, page);
}

pub fn view(s: &State, cx: &mut Cx<State>) -> El {
    // Follow the system appearance for the "system" scheme.
    let p = if design::scheme_is_dark(&s.prefs, cx.system_dark()) == !s.palette.light {
        s.palette
    } else {
        design::resolve(&s.prefs, cx.system_dark())
    };
    let k = design::kit(&p);
    let mut layer: Vec<El> = Vec::new();
    let body = match &s.conn {
        Conn::Connecting => centered(&p, "Connecting to IronClaw…"),
        Conn::Failed { message, retry_in } => {
            let msg = match retry_in {
                Some(secs) => format!("{message}\nRetrying in {secs} s"),
                None => message.clone(),
            };
            centered(&p, &msg)
        }
        Conn::Ready => {
            let main = match s.page {
                Page::Chat | Page::Draft => crate::chat::view(s, cx, &k, &mut layer),
                page => crate::pages::view(s, page, cx, &k, &mut layer),
            };
            row()
                .w_full()
                .h_full()
                .fill(p.surface)
                .child(sidebar(s, &p, cx))
                .child(
                    col()
                        .key("main")
                        .grow(1.0)
                        .h_full()
                        .min_h(0.0)
                        .padding(8.0)
                        .child(
                            col()
                                .w_full()
                                .h_full()
                                .min_h(0.0)
                                .radius(design::PANEL_RADIUS)
                                .fill(p.bg)
                                .child(main),
                        ),
                )
        }
    };
    rune_kit::root(body, layer)
}

fn centered(p: &Palette, msg: &str) -> El {
    col()
        .w_full()
        .h_full()
        .center()
        .fill(p.bg)
        .child(text(msg.to_string()).color(p.text_muted))
}

/// Placeholder sidebar: one row per thread plus the page list. Replaced by
/// the inbox port.
fn sidebar(s: &State, p: &Palette, cx: &mut Cx<State>) -> El {
    let mut list = col()
        .key("sidebar")
        .w(design::SIDEBAR_WIDTH)
        .h_full()
        .padding(10.0)
        .gap(2.0)
        .child(el().h(cx.titlebar_inset().max(design::TITLEBAR_HEIGHT - 10.0)).window_drag());
    for (id, title) in &s.workspace.threads {
        let selected = s.chat.as_ref().is_some_and(|c| c.thread_id == *id) && s.page == Page::Chat;
        let id_for_press = id.clone();
        list = list.child(
            row()
                .key(format!("thread-{id}"))
                .h(design::ROW_HEIGHT)
                .px(10.0)
                .center_y()
                .radius(design::ROW_RADIUS)
                .fill(if selected { p.active } else { rune::Color::TRANSPARENT })
                .child(
                    text(if title.is_empty() { "Untitled".to_string() } else { title.clone() })
                        .color(p.text)
                        .truncate(),
                )
                .on_press(move |s: &mut State| open_thread(s, id_for_press.clone())),
        );
    }
    for (page, label) in [
        (Page::Settings, "Settings"),
        (Page::Providers, "Providers"),
        (Page::Usage, "Usage"),
    ] {
        list = list.child(
            row()
                .key(format!("nav-{label}"))
                .h(design::ROW_HEIGHT)
                .px(10.0)
                .center_y()
                .radius(design::ROW_RADIUS)
                .child(text(label).color(p.text_muted))
                .on_press(move |s: &mut State| show(s, page)),
        );
    }
    list
}

// PORT-STUB(area B): thread titles (GPUI `titles.rs`) and window activity,
// used by the chat extras (area H). Replace with the real title cache and
// the window's focus state; keep the names and signatures.

/// A thread's title, if known.
pub fn thread_title(s: &State, thread_id: &str) -> Option<String> {
    s.workspace
        .threads
        .iter()
        .find(|(id, _)| id == thread_id)
        .map(|(_, title)| title.clone())
        .filter(|t| !t.is_empty())
}

/// Fetch titles for these threads if they are not known yet (GPUI `titles::ensure`).
pub fn ensure_titles(_s: &mut State, _thread_ids: Vec<String>) {}

/// The app window is in front (GPUI `cx.active_window().is_some()`).
pub fn window_active(_s: &State) -> bool {
    true
}
