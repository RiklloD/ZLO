//! The live view (the Hermes plugin's `live.py`): what each session is
//! doing, which windows it used, where its cursor is, the human's take-over
//! lease, and the agent's latest screenshot per window. Recording costs a few
//! map writes per action; pictures are made only while a viewer asks
//! (`capture.rs`), never inside an action.
//!
//! Take over is per desktop, as in Hermes: a person holding the desktop of a
//! (tenant, user) pauses every session of that user, and each hold bumps an
//! epoch so a picture captured across a takeover can be dropped.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ironclaw_host_api::ids::{TenantId, UserId};
use ironclaw_product_contracts::computer_use::ComputerUseSession;
use serde::Serialize;
use serde_json::{Value, json};

/// Forget a session this long after its last action.
const SESSION_TTL: Duration = Duration::from_secs(1800);
/// A session's windows can be watched this long after its last action...
const VIEW_TTL: Duration = Duration::from_secs(180);
/// ...and this long after the session ended.
const ENDED_GRACE: Duration = Duration::from_secs(5);
/// A viewer that has not polled for this long is gone.
const VIEWER_TTL: Duration = Duration::from_secs(10);
/// A takeover whose viewer is gone this long is released.
const TAKEOVER_TTL: Duration = Duration::from_secs(30);
/// Windows remembered per session (the viewer shows the newest few).
const MAX_APPS: usize = 6;
const MAX_VIEWERS: usize = 64;

/// Calls that never change or show a window: no activity in the viewer.
pub(crate) const QUIET: &[&str] = &["list_apps"];
/// Keyboard input has no pointer: the viewer shows a badge on the window.
const TYPING: &[&str] = &["type_text", "press_key", "set_value"];

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0) // silent-ok: a clock before 1970 only skews a display timestamp
}

pub(crate) type Desk = (TenantId, UserId);

pub(crate) fn desk(key: &ComputerUseSession) -> Desk {
    (key.tenant_id.clone(), key.user_id.clone())
}

#[derive(Debug, Clone, Serialize)]
struct AppWindow {
    pid: i64,
    window_id: u32,
    app: String,
    title: String,
    last_used: i64,
}

#[derive(Debug, Clone, Serialize)]
struct Pointer {
    kind: &'static str,
    x: f64,
    y: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    to_x: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    to_y: Option<f64>,
    pid: i64,
    window_id: u32,
    seq: u64,
    at: i64,
}

#[derive(Debug, Clone, Serialize)]
struct Typing {
    text: String,
    at: i64,
    window_id: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
struct Failure {
    label: String,
    status: &'static str,
    code: Option<String>,
    at: i64,
}

#[derive(Debug, Clone)]
struct ViewSession {
    apps: Vec<AppWindow>,
    inflight: u32,
    label: String,
    action: String,
    status: Option<&'static str>,
    ok: Option<bool>,
    code: Option<String>,
    pointer: Option<Pointer>,
    typing: Option<Typing>,
    last_activity: i64,
    last_seen: Instant,
    ended_at: Option<(i64, Instant)>,
    last_failure: Option<Failure>,
    /// The window the latest call acted on (never served).
    target: Option<(i64, u32, String)>,
}

impl ViewSession {
    fn new() -> Self {
        Self {
            apps: Vec::new(),
            inflight: 0,
            label: String::new(),
            action: String::new(),
            status: None,
            ok: None,
            code: None,
            pointer: None,
            typing: None,
            last_activity: now_ms(),
            last_seen: Instant::now(),
            ended_at: None,
            last_failure: None,
            target: None,
        }
    }

    fn touch(&mut self) {
        self.last_activity = now_ms();
        self.last_seen = Instant::now();
    }

    fn watchable(&self) -> bool {
        match self.ended_at {
            Some((_, at)) => at.elapsed() < ENDED_GRACE,
            None => self.last_seen.elapsed() < VIEW_TTL,
        }
    }

    fn touch_app(
        &mut self,
        titles: &HashMap<(i64, u32), Geometry>,
        pid: i64,
        window: u32,
        app: &str,
    ) {
        let previous = self.apps.iter().position(|a| a.window_id == window);
        let previous = previous.map(|index| self.apps.remove(index));
        let title = titles
            .get(&(pid, window))
            .map(|geometry| geometry.title.clone())
            .filter(|title| !title.is_empty())
            .or_else(|| previous.as_ref().map(|a| a.title.clone()))
            .unwrap_or_default();
        let app = if app.is_empty() {
            previous.map(|a| a.app).unwrap_or_default()
        } else {
            app.to_string()
        };
        self.apps.insert(
            0,
            AppWindow {
                pid,
                window_id: window,
                app,
                title,
                last_used: now_ms(),
            },
        );
        self.apps.truncate(MAX_APPS);
    }
}

#[derive(Debug, Clone, Default)]
struct Geometry {
    size: Option<(u32, u32)>,
    /// x, y, width, height in screen points, origin top-left.
    bounds: Option<(f64, f64, f64, f64)>,
    title: String,
}

/// How the latest picture of a window was made.
#[derive(Debug, Clone, Default)]
pub(crate) struct Health {
    pub method: Option<&'static str>,
    pub error: Option<String>,
    pub at: Option<Instant>,
    /// macOS window capture is retried only after this.
    pub failed_until: Option<Instant>,
}

#[derive(Debug, Clone, Default)]
struct Lease {
    /// The viewer holding the desktop, `None` when the agent has it.
    holder: Option<String>,
    epoch: u64,
    since: i64,
}

/// The window a session's latest call acted on, from its state header and
/// the on-screen window list.
pub(crate) struct WindowTarget<'a> {
    pub pid: i64,
    pub window_id: u32,
    pub app: &'a str,
    pub title: &'a str,
    pub bounds: Option<(f64, f64, f64, f64)>,
    pub size: Option<(u32, u32)>,
}

/// A window a session used and may still be watched.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct WatchedWindow {
    pub session: ComputerUseSession,
    pub pid: i64,
    pub window_id: u32,
}

#[derive(Default)]
struct State {
    sessions: HashMap<ComputerUseSession, ViewSession>,
    geometry: HashMap<(i64, u32), Geometry>,
    snapshots: HashMap<u32, (Arc<Vec<u8>>, Instant)>,
    health: HashMap<u32, Health>,
    viewers: HashMap<String, Instant>,
    leases: HashMap<Desk, Lease>,
    /// viewer id → the desk it holds.
    takeovers: HashMap<String, Desk>,
}

pub(crate) struct Live {
    enabled: bool,
    state: Mutex<State>,
}

fn clamp(value: f64) -> f64 {
    (value.clamp(0.0, 1.0) * 10_000.0).round() / 10_000.0
}

impl Live {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            state: Mutex::new(State::default()),
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    // ── window geometry ──

    pub(crate) fn set_target(&self, key: &ComputerUseSession, target: WindowTarget<'_>) {
        let mut state = self.state();
        let geometry = state
            .geometry
            .entry((target.pid, target.window_id))
            .or_default();
        if target.size.is_some() {
            geometry.size = target.size;
        }
        if target.bounds.is_some_and(|(_, _, w, h)| w > 0.0 && h > 0.0) {
            geometry.bounds = target.bounds;
        }
        if !target.title.is_empty() {
            geometry.title = target.title.to_string();
        }
        let State {
            sessions, geometry, ..
        } = &mut *state;
        let session = sessions.entry(key.clone()).or_insert_with(ViewSession::new);
        session.target = Some((target.pid, target.window_id, target.app.to_string()));
        session.touch_app(geometry, target.pid, target.window_id, target.app);
    }

    pub(crate) fn window_bounds(&self, pid: i64, window_id: u32) -> Option<(f64, f64, f64, f64)> {
        self.state()
            .geometry
            .get(&(pid, window_id))
            .and_then(|geometry| geometry.bounds)
    }

    /// The screenshot from the agent's latest state of this window: the
    /// viewer's fallback picture.
    pub(crate) fn remember_snapshot(&self, window_id: u32, png: Vec<u8>) {
        if self.enabled && !png.is_empty() {
            self.state()
                .snapshots
                .insert(window_id, (Arc::new(png), Instant::now()));
        }
    }

    pub(crate) fn snapshot_png(&self, window_id: u32) -> Option<Arc<Vec<u8>>> {
        self.state()
            .snapshots
            .get(&window_id)
            .map(|(png, _)| Arc::clone(png))
    }

    pub(crate) fn health(&self, window_id: u32) -> Health {
        self.state()
            .health
            .get(&window_id)
            .cloned()
            .unwrap_or_default()
    }

    pub(crate) fn set_health(&self, window_id: u32, update: impl FnOnce(&mut Health)) {
        update(self.state().health.entry(window_id).or_default());
    }

    // ── activity and pointer ──

    pub(crate) fn activity_start(&self, key: &ComputerUseSession, action: &str, label: &str) {
        if !self.enabled {
            return;
        }
        let mut state = self.state();
        let State {
            sessions, geometry, ..
        } = &mut *state;
        let session = sessions.entry(key.clone()).or_insert_with(ViewSession::new);
        session.inflight += 1;
        session.label = label.to_string();
        session.action = action.to_string();
        session.status = None;
        session.ok = None;
        session.code = None;
        session.ended_at = None;
        session.touch();
        let target = session.target.clone();
        if TYPING.contains(&action) {
            session.typing = Some(Typing {
                text: label.to_string(),
                at: now_ms(),
                window_id: target.as_ref().map(|(_, window, _)| *window),
            });
        }
        if let Some((pid, window, app)) = target {
            session.touch_app(geometry, pid, window, &app);
        }
    }

    pub(crate) fn activity_end(
        &self,
        key: &ComputerUseSession,
        action: &str,
        label: &str,
        status: &'static str,
        ok: bool,
        code: Option<&str>,
    ) {
        if !self.enabled {
            return;
        }
        let mut state = self.state();
        let State {
            sessions, geometry, ..
        } = &mut *state;
        let session = sessions.entry(key.clone()).or_insert_with(ViewSession::new);
        session.inflight = session.inflight.saturating_sub(1);
        session.label = label.to_string();
        session.action = action.to_string();
        session.status = Some(status);
        session.ok = Some(ok);
        session.code = code.map(str::to_string);
        session.ended_at = None;
        session.touch();
        if !ok {
            session.last_failure = Some(Failure {
                label: label.to_string(),
                status,
                code: code.map(str::to_string),
                at: now_ms(),
            });
        }
        if let Some((pid, window, app)) = session.target.clone() {
            session.touch_app(geometry, pid, window, &app);
        }
    }

    /// Where input is about to land, as fractions of the window (0..1,
    /// origin top-left).
    pub(crate) fn pointer(
        &self,
        key: &ComputerUseSession,
        kind: &'static str,
        at: (f64, f64),
        to: Option<(f64, f64)>,
        pid: i64,
        window_id: u32,
    ) {
        if !self.enabled {
            return;
        }
        let mut state = self.state();
        let State {
            sessions, geometry, ..
        } = &mut *state;
        let session = sessions.entry(key.clone()).or_insert_with(ViewSession::new);
        let seq = session.pointer.as_ref().map_or(0, |p| p.seq) + 1;
        session.pointer = Some(Pointer {
            kind,
            x: clamp(at.0),
            y: clamp(at.1),
            to_x: to.map(|to| clamp(to.0)),
            to_y: to.map(|to| clamp(to.1)),
            pid,
            window_id,
            seq,
            at: now_ms(),
        });
        session.touch();
        let app = session
            .apps
            .iter()
            .find(|a| a.window_id == window_id)
            .map(|a| a.app.clone())
            .unwrap_or_default();
        session.touch_app(geometry, pid, window_id, &app);
    }

    pub(crate) fn ended(&self, key: &ComputerUseSession) {
        if let Some(session) = self.state().sessions.get_mut(key) {
            session.ended_at = Some((now_ms(), Instant::now()));
            session.inflight = 0;
        }
    }

    // ── viewers and take over ──

    pub(crate) fn seen_viewer(&self, viewer: &str) {
        let mut state = self.state();
        if state.viewers.len() >= MAX_VIEWERS && !state.viewers.contains_key(viewer) {
            state.viewers.retain(|_, at| at.elapsed() < VIEWER_TTL);
        }
        state
            .viewers
            .insert(viewer.chars().take(64).collect(), Instant::now());
    }

    /// (a person holds the desktop, lease epoch).
    pub(crate) fn lease(&self, desk: &Desk) -> (bool, u64) {
        self.state()
            .leases
            .get(desk)
            .map_or((false, 0), |lease| (lease.holder.is_some(), lease.epoch))
    }

    pub(crate) fn take_over(&self, desk: &Desk, viewer: &str) {
        let mut state = self.state();
        let lease = state.leases.entry(desk.clone()).or_default();
        if lease.holder.is_none() {
            lease.epoch += 1;
            lease.since = now_ms();
        }
        lease.holder = Some(viewer.to_string());
        state.takeovers.insert(viewer.to_string(), desk.clone());
        state.viewers.insert(viewer.to_string(), Instant::now());
    }

    /// Release the desk's lease (whoever took it: hand back comes from the
    /// person, on any of their threads).
    pub(crate) fn hand_back(&self, desk: &Desk) {
        let mut state = self.state();
        if let Some(lease) = state.leases.get_mut(desk)
            && lease.holder.take().is_some()
        {
            lease.epoch += 1;
        }
        state.takeovers.retain(|_, held| held != desk);
    }

    /// A take over whose own viewer stopped polling (window closed, app
    /// quit) must not keep the agent paused.
    pub(crate) fn release_abandoned_takeovers(&self) {
        let mut state = self.state();
        let gone: Vec<(String, Desk)> = state
            .takeovers
            .iter()
            .filter(|(viewer, _)| {
                state
                    .viewers
                    .get(*viewer)
                    .is_none_or(|at| at.elapsed() > TAKEOVER_TTL)
            })
            .map(|(viewer, desk)| (viewer.clone(), desk.clone()))
            .collect();
        for (viewer, desk) in gone {
            state.takeovers.remove(&viewer);
            if let Some(lease) = state.leases.get_mut(&desk)
                && lease.holder.as_deref() == Some(viewer.as_str())
            {
                lease.holder = None;
                lease.epoch += 1;
            }
        }
    }

    // ── what the viewer reads ──

    /// The window, if `key`'s session used it and may still be watched.
    pub(crate) fn watched(
        &self,
        key: &ComputerUseSession,
        window_id: u32,
    ) -> Option<WatchedWindow> {
        if !self.enabled {
            return None;
        }
        let state = self.state();
        let session = state.sessions.get(key).filter(|s| s.watchable())?;
        let app = session.apps.iter().find(|a| a.window_id == window_id)?;
        Some(WatchedWindow {
            session: key.clone(),
            pid: app.pid,
            window_id,
        })
    }

    /// Every session of `desk`, newest first, for the viewer.
    /// `stopping(key)` reports the session layer's Stop hold.
    pub(crate) fn snapshot(
        &self,
        desk: &Desk,
        viewer: Option<&str>,
        stopping: impl Fn(&ComputerUseSession) -> bool,
    ) -> Value {
        if let Some(viewer) = viewer {
            self.seen_viewer(viewer);
        }
        let mut state = self.state();
        state
            .sessions
            .retain(|_, session| session.last_seen.elapsed() < SESSION_TTL);
        let held = state
            .leases
            .get(desk)
            .is_some_and(|lease| lease.holder.is_some());
        let mut sessions: Vec<(i64, Value)> = state
            .sessions
            .iter()
            .filter(|(key, _)| key.tenant_id == desk.0 && key.user_id == desk.1)
            .map(|(key, session)| {
                let apps: Vec<Value> = session
                    .apps
                    .iter()
                    .map(|app| {
                        let mut item = json!(app);
                        if let Some((width, height)) = state
                            .geometry
                            .get(&(app.pid, app.window_id))
                            .and_then(|geometry| geometry.size)
                        {
                            item["width"] = json!(width);
                            item["height"] = json!(height);
                        }
                        let health = state.health.get(&app.window_id).cloned().unwrap_or_default();
                        item["capture"] = json!({
                            "method": health.method,
                            "error": health.error,
                            "frame_age": health.at.map(|at| (at.elapsed().as_secs_f64() * 10.0).round() / 10.0),
                        });
                        item
                    })
                    .collect();
                let item = json!({
                    "session_id": key.thread_id.as_str(),
                    "thread_id": key.thread_id.as_str(),
                    "apps": apps,
                    "inflight": session.inflight,
                    "label": session.label,
                    "action": session.action,
                    "status": session.status,
                    "ok": session.ok,
                    "code": session.code,
                    "pointer": session.pointer,
                    "typing": session.typing,
                    "last_activity": session.last_activity,
                    "ended": session.ended_at.is_some(),
                    "ended_at": session.ended_at.map(|(at, _)| at),
                    "last_failure": session.last_failure,
                    "human_has_control": held,
                    "stopping": stopping(key),
                    "watchable": self.enabled && session.watchable(),
                });
                (session.last_activity, item)
            })
            .collect();
        drop(state);
        sessions.sort_by_key(|item| std::cmp::Reverse(item.0));
        json!({
            "now": now_ms(),
            "enabled": self.enabled,
            "human_has_control": held,
            "sessions": sessions.into_iter().map(|(_, item)| item).collect::<Vec<_>>(),
        })
    }
}

/// One line for the viewer. `element` is the element's line from the latest
/// state ("button Save").
pub(crate) fn describe(action: &str, args: &Value, element: Option<&str>) -> String {
    let text_arg = |name: &str| match args.get(name) {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    };
    let target = match element {
        Some(element) => format!("\u{201c}{element}\u{201d}"),
        None => match args.get("element_index") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(index)) if index.is_empty() => String::new(),
            Some(_) => format!("#{}", text_arg("element_index")),
        },
    };
    let text = text_arg("text");
    let short = if text.chars().count() > 40 {
        format!("{}\u{2026}", text.chars().take(40).collect::<String>())
    } else {
        text
    };
    let app = text_arg("app");
    let button = text_arg("mouse_button");
    let clicks = args
        .get("click_count")
        .and_then(|count| count.as_u64().or_else(|| count.as_str()?.parse().ok()))
        .unwrap_or(1);
    let verb = match (button.as_str(), clicks) {
        ("right", _) => "Right-clicking",
        ("middle", _) => "Middle-clicking",
        (_, 2) => "Double-clicking",
        (_, 3) => "Triple-clicking",
        _ => "Clicking",
    };
    let joined = |head: &str, tail: &str| format!("{head} {tail}").trim().to_string();
    match action {
        "list_apps" => "Listing apps".to_string(),
        "get_app_state" => format!(
            "Looking at {}",
            if app.is_empty() { "the app" } else { &app }
        ),
        "click" if !target.is_empty() => joined(verb, &target),
        "click" => joined(verb, &format!("in {app}")),
        "perform_secondary_action" => {
            let name = text_arg("action");
            joined(if name.is_empty() { "Action" } else { &name }, &target)
        }
        "scroll" => {
            let direction = text_arg("direction");
            joined(
                &format!(
                    "Scrolling {}",
                    if direction.is_empty() {
                        "down"
                    } else {
                        &direction
                    }
                ),
                &target,
            )
        }
        "drag" => "Dragging".to_string(),
        "type_text" => format!("Typing \u{201c}{short}\u{201d}"),
        "press_key" => format!("Pressing {}", text_arg("key")),
        "set_value" => format!(
            "Setting {target} to \u{201c}{}\u{201d}",
            text_arg("value").chars().take(40).collect::<String>()
        )
        .replace("  ", " "),
        other => {
            let words = other.replace('_', " ");
            let mut chars = words.chars();
            chars.next().map_or(String::new(), |first| {
                first.to_uppercase().collect::<String>() + chars.as_str()
            })
        }
    }
}
