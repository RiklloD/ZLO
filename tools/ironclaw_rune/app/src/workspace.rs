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

// ---- Called by the chat (area A). PORT-STUB(area B): replace the bodies
// with the ports of `drafts.rs`, `run_clock.rs`, `workspace/draft.rs` and the
// workspace's chat-event handlers; keep the signatures.

/// An unsent message of one thread: its text, its pills (byte range and id;
/// each label is the text under its range) and its staged files (GPUI
/// `drafts::Draft`).
#[derive(Clone, Default)]
pub struct Draft {
    pub text: String,
    pub pills: Vec<(std::ops::Range<usize>, String)>,
    pub files: Vec<ironclaw_gpui_client::attachments::StagedFile>,
}

/// PORT-STUB(area B): `drafts::restore`, the thread's unsent draft.
pub fn draft_restore(_s: &State, _thread_id: &str) -> Option<Draft> {
    None
}

/// PORT-STUB(area B): `drafts::record`, called on every composer edit and
/// after a send clears it.
pub fn draft_record(_s: &mut State, _thread_id: &str, _draft: Draft) {}

/// PORT-STUB(area B): GPUI `ThreadUpdated`: a timeline load landed, so the
/// sidebar picks up server-set titles (refresh inbox and unread).
pub fn thread_updated(_s: &mut State, _thread_id: &str) {}

/// PORT-STUB(area B): GPUI `RunEnded`: a run watched live ended on its own
/// (telemetry, title poll, inbox visit, system alert when not focused).
pub fn run_ended(_s: &mut State, _thread_id: &str, _failed: bool) {}

/// PORT-STUB(area B): GPUI `OpenRunLogs`: a failed run's Logs button shows
/// the operator page's gateway log lines for `run_id`.
pub fn open_run_logs(s: &mut State, _run_id: String) {
    show(s, Page::Operator);
}

/// PORT-STUB(area B): `run_clock::start`, when `thread_id`'s run began (kept
/// across views, from the backend's time when known).
pub fn run_clock_start(
    _s: &mut State,
    _thread_id: &str,
    _run: Option<&str>,
    sent_ms: Option<i64>,
) -> std::time::Instant {
    let now = std::time::Instant::now();
    let since = sent_ms
        .and_then(|ms| chrono::DateTime::from_timestamp_millis(ms))
        .and_then(|at| (chrono::Utc::now() - at).to_std().ok());
    since.and_then(|d| now.checked_sub(d)).unwrap_or(now)
}

/// PORT-STUB(area B): `run_clock::end`.
pub fn run_clock_end(_s: &mut State, _thread_id: &str) {}

/// PORT-STUB(area B): the new-thread screen (GPUI `workspace/draft.rs`),
/// shown for `Page::Draft`. It sends its first message with
/// `chat::open` + `chat::send_text`.
pub fn draft_view(
    s: &State,
    _cx: &mut rune::Cx<State>,
    _k: &rune_kit::Kit,
    _layer: &mut Vec<El>,
) -> El {
    centered(&s.palette, "New thread")
}
