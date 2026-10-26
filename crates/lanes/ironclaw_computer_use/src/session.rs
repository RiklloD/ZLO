//! The session layer the Hermes plugin added over the bare server: one
//! process per thread (element indices live in it), Stop, take over / hand
//! back, idle reaping, the latest frame, and around every call what the live
//! view needs (activity, pointer, the target window, the agent's screenshot)
//! plus the trace line. Calls on one session are serialised; Stop aborts the
//! call in flight and kills its process.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use base64::Engine as _;
use ironclaw_host_api::ids::{TenantId, ThreadId};
use ironclaw_product_contracts::computer_use::{COMPUTER_USE_RESULT_FRAME, ComputerUseSession};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::capture::{Capture, Refusal};
use crate::catalog::computer_use_tool;
use crate::config::ComputerUseConfig;
use crate::cursor::{CursorWatch, pointer_before, pointer_kind};
use crate::error::ComputerUseError;
use crate::frame::{Frame, split_content};
use crate::live::{Desk, Live, QUIET, WindowTarget, describe, desk, now_ms};
use crate::stdio::StdioServer;
use crate::target::TargetState;
use crate::windows::find_window;

/// Why a call did not run, or did not finish. Model-visible codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalCode {
    /// Stop was pressed before the call.
    Cancelled,
    /// Stop was pressed while the call ran; its effect is unknown.
    #[serde(rename = "cancelled")]
    CancelledInFlight,
    /// The user took over the desktop.
    HumanHasControl,
    Timeout,
    Unavailable,
    UnknownTool,
    /// Too many live sessions.
    Busy,
}

impl RefusalCode {
    fn message(self) -> &'static str {
        match self {
            Self::Cancelled => {
                "The user pressed Stop in the computer-use viewer. Stop driving the desktop and tell them where you got to."
            }
            Self::CancelledInFlight => {
                "Stopped: the turn was interrupted or the user pressed Stop while this action was running. Whether it took effect is unknown; call get_app_state to check."
            }
            Self::HumanHasControl => {
                "The user has taken over the desktop. Wait until they hand it back; do not retry now."
            }
            Self::Timeout => {
                "Open Computer Use did not answer in time; the outcome is unknown. Call get_app_state to check before retrying."
            }
            Self::Unavailable => "Computer Use is unavailable right now.",
            Self::UnknownTool => "That computer-use tool does not exist.",
            Self::Busy => "Too many computer-use sessions are open; try again later.",
        }
    }

    /// The viewer's status for a call that ended this way.
    fn viewer_status(self) -> &'static str {
        match self {
            Self::CancelledInFlight | Self::Timeout => "unknown",
            Self::Cancelled | Self::HumanHasControl | Self::Busy => "refused",
            Self::Unavailable | Self::UnknownTool => "failed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComputerUseOutcome {
    /// The server answered. `is_error` is the tool's own failure flag.
    Completed {
        text: String,
        is_error: bool,
        frame_seq: Option<u64>,
    },
    Refused(RefusalCode),
}

impl ComputerUseOutcome {
    /// The model-visible document. A failed action says so in the server's
    /// own words; screenshots stay host-side unless the caller attaches them.
    pub fn to_json(&self) -> Value {
        match self {
            Self::Completed {
                text,
                is_error: false,
                frame_seq,
            } => match frame_seq {
                // The host strips the frame number before the model sees it.
                Some(seq) => json!({"ok": true, "result": text, COMPUTER_USE_RESULT_FRAME: seq}),
                None => json!({"ok": true, "result": text}),
            },
            Self::Completed {
                text,
                is_error: true,
                ..
            } => {
                let text = if text.is_empty() {
                    "The action failed."
                } else {
                    text
                };
                json!({"ok": false, "code": "action_failed", "error": text})
            }
            Self::Refused(code) => json!({"ok": false, "code": code, "error": code.message()}),
        }
    }

    fn code(&self) -> Option<&'static str> {
        match self {
            Self::Completed { .. } => None,
            Self::Refused(RefusalCode::Cancelled | RefusalCode::CancelledInFlight) => {
                Some("cancelled")
            }
            Self::Refused(RefusalCode::HumanHasControl) => Some("human_has_control"),
            Self::Refused(RefusalCode::Timeout) => Some("timeout"),
            Self::Refused(RefusalCode::Unavailable) => Some("unavailable"),
            Self::Refused(RefusalCode::UnknownTool) => Some("unknown_tool"),
            Self::Refused(RefusalCode::Busy) => Some("busy"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    None,
    Idle,
    Running,
    TakenOver,
    Stopped,
}

/// What the session route shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStatus {
    pub state: SessionState,
    pub process_running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_activity_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
}

impl SessionStatus {
    fn none(taken_over: bool) -> Self {
        Self {
            state: if taken_over {
                SessionState::TakenOver
            } else {
                SessionState::None
            },
            process_running: false,
            current_tool: None,
            last_tool: None,
            last_activity_ms: None,
            frame_seq: None,
            app: None,
            window: None,
        }
    }
}

#[derive(Default)]
struct Flags {
    stopped_at: Option<Instant>,
    last_used: Option<Instant>,
    last_activity_ms: Option<i64>,
    current_tool: Option<String>,
    last_tool: Option<String>,
    process_running: bool,
    target: TargetState,
    frame: Option<Arc<Frame>>,
    next_frame_seq: u64,
    /// The newest frames and the tool result each one belongs to, for the
    /// model (it keeps the last [`MODEL_FRAMES`] screenshots in context).
    recent: std::collections::VecDeque<(Arc<Frame>, Option<String>)>,
}

/// Screenshots a model keeps in context, newest first (Hermes keeps 3).
pub const MODEL_FRAMES: usize = 3;

struct Session {
    /// Held for the whole call: calls on one thread run one at a time.
    server: tokio::sync::Mutex<Option<StdioServer>>,
    flags: Mutex<Flags>,
    /// Bumped by Stop; a call in flight watches it.
    stop: watch::Sender<u64>,
    /// The native cursor's observation file for this session's process.
    cursor_file: PathBuf,
}

impl Session {
    fn new(cursor_file: PathBuf) -> Self {
        Self {
            server: tokio::sync::Mutex::new(None),
            flags: Mutex::new(Flags::default()),
            stop: watch::Sender::new(0),
            cursor_file,
        }
    }

    fn flags(&self) -> MutexGuard<'_, Flags> {
        self.flags.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn stopped(&self, config: &ComputerUseConfig) -> bool {
        self.flags()
            .stopped_at
            .is_some_and(|at| at.elapsed() < config.stop_hold)
    }

    fn status(&self, config: &ComputerUseConfig, taken_over: bool) -> SessionStatus {
        let stopped = self.stopped(config);
        let flags = self.flags();
        let state = if stopped {
            SessionState::Stopped
        } else if taken_over {
            SessionState::TakenOver
        } else if flags.current_tool.is_some() {
            SessionState::Running
        } else {
            SessionState::Idle
        };
        let non_empty = |text: &str| (!text.is_empty()).then(|| text.to_string());
        SessionStatus {
            state,
            process_running: flags.process_running,
            current_tool: flags.current_tool.clone(),
            last_tool: flags.last_tool.clone(),
            last_activity_ms: flags.last_activity_ms,
            frame_seq: flags.frame.as_ref().map(|frame| frame.seq),
            app: non_empty(&flags.target.bundle),
            window: non_empty(&flags.target.title),
        }
    }

    fn idle_for(&self, config: &ComputerUseConfig) -> bool {
        let flags = self.flags();
        flags.current_tool.is_none()
            && flags
                .last_used
                .is_none_or(|at| at.elapsed() >= config.idle_reap)
    }

    /// This session's own `OPEN_COMPUTER_USE_*` switches, so its cursor
    /// positions land in its own file.
    fn server_env(&self, config: &ComputerUseConfig, images: bool) -> Vec<(String, String)> {
        let mut env = vec![(
            "OPEN_COMPUTER_USE_VISUAL_CURSOR_OBSERVATION_FILE".to_string(),
            self.cursor_file.display().to_string(),
        )];
        if config.fork {
            // Action results: changed tree lines only, and no screenshot
            // capture or encoding for a model that never sees it.
            env.push((
                "OPEN_COMPUTER_USE_ACTION_RESULT".to_string(),
                "diff".to_string(),
            ));
            env.push((
                "OPEN_COMPUTER_USE_ACTION_SCREENSHOT".to_string(),
                if images { "1" } else { "0" }.to_string(),
            ));
        }
        env
    }
}

/// Clears `current_tool` however the call ends (including a dropped future).
struct RunningMark<'a>(&'a Session);

impl Drop for RunningMark<'_> {
    fn drop(&mut self) {
        let mut flags = self.0.flags();
        flags.last_tool = flags.current_tool.take();
        flags.last_used = Some(Instant::now());
    }
}

/// Cut `text` to at most `limit` bytes on a char boundary.
fn bounded(mut text: String, limit: usize) -> String {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit.saturating_sub(32);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str("\n[output truncated]");
    text
}

fn safe_file_name(key: &ComputerUseSession) -> String {
    let raw = format!("{}_{}_{}", key.tenant_id, key.user_id, key.thread_id);
    let safe: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let tail: String = safe
        .chars()
        .rev()
        .take(80)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("cursor-{tail}.json")
}

/// Every computer-use session of this host.
pub struct ComputerUse {
    config: ComputerUseConfig,
    sessions: Mutex<HashMap<ComputerUseSession, Arc<Session>>>,
    live: Arc<Live>,
    capture: Arc<Capture>,
}

impl ComputerUse {
    pub fn new(config: ComputerUseConfig) -> Self {
        let live = Arc::new(Live::new(config.live_preview));
        Self {
            config,
            sessions: Mutex::new(HashMap::new()),
            live,
            capture: Arc::new(Capture::new()),
        }
    }

    /// `None` unless `COMPUTER_USE_ENABLED` turns it on.
    pub fn from_env() -> Result<Option<Arc<Self>>, ComputerUseError> {
        Ok(ComputerUseConfig::from_env()?.map(|config| Arc::new(Self::new(config))))
    }

    pub fn config(&self) -> &ComputerUseConfig {
        &self.config
    }

    /// The system-prompt section for runs that have these tools.
    pub fn instructions(&self) -> String {
        crate::prompt::instructions(self.config.fork)
    }

    fn sessions(&self) -> MutexGuard<'_, HashMap<ComputerUseSession, Arc<Session>>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn existing(&self, key: &ComputerUseSession) -> Option<Arc<Session>> {
        self.sessions().get(key).cloned()
    }

    fn taken_over(&self, key: &ComputerUseSession) -> bool {
        self.live.lease(&desk(key)).0
    }

    /// The session for `key`, created if there is room. Idle sessions are
    /// reaped first; their processes die when the last handle drops.
    fn session(&self, key: &ComputerUseSession) -> Option<Arc<Session>> {
        let mut sessions = self.sessions();
        if let Some(session) = sessions.get(key) {
            return Some(Arc::clone(session));
        }
        let live = &self.live;
        sessions.retain(|key, session| {
            let keep = !session.idle_for(&self.config);
            if !keep {
                live.ended(key);
                // silent-ok: the file may never have been written.
                let _ = std::fs::remove_file(&session.cursor_file);
            }
            keep
        });
        if sessions.len() >= self.config.max_sessions {
            return None;
        }
        if let Err(error) = std::fs::create_dir_all(&self.config.cursor_dir) {
            tracing::debug!(%error, "computer-use cursor directory unavailable");
        }
        let session = Arc::new(Session::new(
            self.config.cursor_dir.join(safe_file_name(key)),
        ));
        sessions.insert(key.clone(), Arc::clone(&session));
        Some(session)
    }

    /// Run one tool for a model that does not take images. See
    /// [`Self::call_with_images`].
    pub async fn call(
        &self,
        key: &ComputerUseSession,
        tool: &str,
        arguments: Value,
        max_text_bytes: usize,
    ) -> ComputerUseOutcome {
        self.call_with_images(key, tool, arguments, max_text_bytes, false)
            .await
    }

    /// Run one tool. Never fails: refusals and server failures come back as
    /// a model-visible outcome. `max_text_bytes` bounds the returned text;
    /// `images` says whether the model sees screenshots (it decides whether
    /// the server captures one per action).
    pub async fn call_with_images(
        &self,
        key: &ComputerUseSession,
        tool: &str,
        arguments: Value,
        max_text_bytes: usize,
        images: bool,
    ) -> ComputerUseOutcome {
        let started = Instant::now();
        let outcome = self
            .run_call(key, tool, arguments, max_text_bytes, images)
            .await;
        if let Some(path) = &self.config.trace_path {
            let ok = matches!(
                outcome,
                ComputerUseOutcome::Completed {
                    is_error: false,
                    ..
                }
            );
            crate::trace::record(
                path,
                key.thread_id.as_str(),
                tool,
                ok,
                started.elapsed(),
                outcome.code(),
            )
            .await;
        }
        outcome
    }

    async fn run_call(
        &self,
        key: &ComputerUseSession,
        tool: &str,
        mut arguments: Value,
        max_text_bytes: usize,
        images: bool,
    ) -> ComputerUseOutcome {
        if let Some(object) = arguments.as_object_mut() {
            object.retain(|_, value| !value.is_null());
        }
        let Some(spec) = computer_use_tool(tool) else {
            return ComputerUseOutcome::Refused(RefusalCode::UnknownTool);
        };
        crate::args::coerce(&spec.input_schema, &mut arguments);
        let Some(session) = self.session(key) else {
            return ComputerUseOutcome::Refused(RefusalCode::Busy);
        };
        // Subscribe before checking, so a Stop between the two is still seen.
        let mut stop = session.stop.subscribe();
        if let Some(code) = self.refusal(key, &session, tool) {
            return ComputerUseOutcome::Refused(code);
        }
        self.live.release_abandoned_takeovers();
        let mut slot = session.server.lock().await;
        if let Some(code) = self.refusal(key, &session, tool) {
            return ComputerUseOutcome::Refused(code);
        }
        let target = session.flags().target.clone();
        let watching = self.live.enabled() && !QUIET.contains(&tool);
        let label = describe(tool, &arguments, target.element_for(&arguments));
        let mut watch = None;
        if watching {
            self.live.activity_start(key, tool, &label);
            pointer_before(&self.live, key, &target, tool, &arguments);
            let by_element = arguments
                .get("element_index")
                .is_some_and(|index| !matches!(index, Value::String(text) if text.is_empty()));
            if by_element && let Some(kind) = pointer_kind(tool, &arguments) {
                watch = CursorWatch::start(
                    Arc::clone(&self.live),
                    key.clone(),
                    &target,
                    kind,
                    session.cursor_file.clone(),
                )
                .await;
            }
        }
        session.flags().current_tool = Some(tool.to_string());
        let running = RunningMark(&session);
        let reused = slot.take();
        let params = json!({"name": tool, "arguments": self.prepare_args(tool, arguments, images)});
        let env = session.server_env(&self.config, images);
        let result = tokio::select! {
            biased;
            _ = stop.changed() => Err(ComputerUseError::Exited),
            result = self.run(reused, params, &env) => result,
        };
        let stopped = session.stopped(&self.config);
        let (server, result) = match result {
            Ok((server, result)) => (Some(server), result),
            Err(error) => {
                tracing::debug!(%error, "computer-use call ended without a reply");
                (None, Err(error))
            }
        };
        session.flags().process_running = server.is_some();
        *slot = server;
        drop(slot);
        drop(running);
        let outcome = match result {
            Ok(result) => self.absorb(key, &session, tool, &result, max_text_bytes),
            Err(_) if stopped => ComputerUseOutcome::Refused(RefusalCode::CancelledInFlight),
            Err(ComputerUseError::Timeout) => ComputerUseOutcome::Refused(RefusalCode::Timeout),
            Err(ComputerUseError::Server { message }) => ComputerUseOutcome::Completed {
                text: message,
                is_error: true,
                frame_seq: None,
            },
            Err(_) => ComputerUseOutcome::Refused(RefusalCode::Unavailable),
        };
        if let Some(watch) = watch {
            watch.finish().await;
        }
        if watching {
            let (status, ok) = match &outcome {
                ComputerUseOutcome::Completed { is_error, .. } => {
                    if *is_error {
                        ("failed", false)
                    } else {
                        ("ok", true)
                    }
                }
                ComputerUseOutcome::Refused(code) => (code.viewer_status(), false),
            };
            self.live
                .activity_end(key, tool, &label, status, ok, outcome.code());
        }
        outcome
    }

    fn refusal(
        &self,
        key: &ComputerUseSession,
        session: &Session,
        tool: &str,
    ) -> Option<RefusalCode> {
        if !QUIET.contains(&tool) && session.stopped(&self.config) {
            return Some(RefusalCode::Cancelled);
        }
        self.taken_over(key).then_some(RefusalCode::HumanHasControl)
    }

    /// A model that cannot see images gets no `get_app_state` screenshot
    /// unless it asks: capture and encoding are the slowest part of a state
    /// read. Kept while the live view is on, which uses it as its fallback
    /// picture.
    fn prepare_args(&self, tool: &str, mut arguments: Value, images: bool) -> Value {
        if tool == "get_app_state"
            && self.config.fork
            && !images
            && !self.live.enabled()
            && let Some(object) = arguments.as_object_mut()
        {
            object
                .entry("include_screenshot")
                .or_insert(Value::Bool(false));
        }
        arguments
    }

    /// Start (or reuse) the process and send one `tools/call`. A server
    /// error leaves the process usable; anything else drops (kills) it.
    async fn run(
        &self,
        reused: Option<StdioServer>,
        params: Value,
        env: &[(String, String)],
    ) -> Result<(StdioServer, Result<Value, ComputerUseError>), ComputerUseError> {
        let mut server = match reused {
            Some(mut server) => match server.alive() {
                true => server,
                false => StdioServer::start(&self.config, env).await?,
            },
            None => StdioServer::start(&self.config, env).await?,
        };
        match server
            .request("tools/call", params, self.config.call_timeout)
            .await
        {
            Ok(result) => Ok((server, Ok(result))),
            Err(error @ ComputerUseError::Server { .. }) => Ok((server, Err(error))),
            Err(error) => Err(error),
        }
    }

    fn absorb(
        &self,
        key: &ComputerUseSession,
        session: &Session,
        tool: &str,
        result: &Value,
        max_text_bytes: usize,
    ) -> ComputerUseOutcome {
        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let (text, image) = split_content(result);
        let png = image
            .as_ref()
            .filter(|(mime, _)| mime == "image/png")
            .and_then(|(_, data)| base64::engine::general_purpose::STANDARD.decode(data).ok()); // silent-ok: an unreadable screenshot only loses the viewer's fallback picture
        let mut flags = session.flags();
        flags.last_activity_ms = Some(now_ms());
        if tool != "list_apps" && flags.target.absorb(&text, png.as_deref()) {
            let target = flags.target.clone();
            drop(flags);
            self.learn_window(key, &target, png);
            flags = session.flags();
        }
        let mut frame_seq = None;
        if let Some((mime_type, data)) = image
            && data.len() <= self.config.max_frame_bytes
        {
            flags.next_frame_seq += 1;
            let seq = flags.next_frame_seq;
            let non_empty = |text: &str| (!text.is_empty()).then(|| text.to_string());
            let frame = Arc::new(Frame {
                seq,
                captured_at_ms: now_ms(),
                mime_type,
                data,
                tool: tool.to_string(),
                app: non_empty(&flags.target.bundle),
                window: non_empty(&flags.target.title),
            });
            if flags.recent.len() == MODEL_FRAMES {
                flags.recent.pop_front();
            }
            flags.recent.push_back((Arc::clone(&frame), None));
            flags.frame = Some(frame);
            frame_seq = Some(seq);
        }
        let text = if text.is_empty() && !is_error {
            "OK".to_string()
        } else {
            text
        };
        ComputerUseOutcome::Completed {
            text: bounded(text, max_text_bytes),
            is_error,
            frame_seq,
        }
    }

    /// Which on-screen window the state belongs to; tell the viewer.
    fn learn_window(&self, key: &ComputerUseSession, target: &TargetState, png: Option<Vec<u8>>) {
        let Some(pid) = target.pid else {
            return;
        };
        let Some(window) = find_window(pid, Some(target.title.as_str())) else {
            return;
        };
        if let Some(session) = self.existing(key) {
            session.flags().target.window_id = Some(window.window_id);
        }
        let title = if target.title.is_empty() {
            window.title.as_str()
        } else {
            target.title.as_str()
        };
        self.live.set_target(
            key,
            WindowTarget {
                pid,
                window_id: window.window_id,
                app: &target.app,
                title,
                bounds: window.bounds,
                size: target.shot_size,
            },
        );
        if let Some(png) = png {
            self.live.remember_snapshot(window.window_id, png);
        }
    }

    /// Stop: abort the call in flight, kill the process, refuse new actions
    /// for the hold time. Take over is cleared; the user ended the session.
    pub async fn stop(&self, key: &ComputerUseSession) -> SessionStatus {
        self.live.hand_back(&desk(key));
        match self.existing(key) {
            Some(session) => self.stop_session(key, &session).await,
            None => SessionStatus::none(false),
        }
    }

    /// The thread's Stop fired: end its session, whoever's run it was.
    pub async fn stop_thread(&self, tenant_id: &TenantId, thread_id: &ThreadId) {
        for (key, session) in self.matching(tenant_id, thread_id) {
            self.stop_session(&key, &session).await;
        }
    }

    fn matching(
        &self,
        tenant_id: &TenantId,
        thread_id: &ThreadId,
    ) -> Vec<(ComputerUseSession, Arc<Session>)> {
        self.sessions()
            .iter()
            .filter(|(key, _)| &key.tenant_id == tenant_id && &key.thread_id == thread_id)
            .map(|(key, session)| (key.clone(), Arc::clone(session)))
            .collect()
    }

    async fn stop_session(&self, key: &ComputerUseSession, session: &Session) -> SessionStatus {
        session.flags().stopped_at = Some(Instant::now());
        session.stop.send_modify(|epoch| *epoch += 1);
        // A call in flight owns the process and kills it itself.
        if let Ok(mut slot) = session.server.try_lock()
            && let Some(server) = slot.take()
        {
            server.shutdown().await;
            session.flags().process_running = false;
        }
        session.status(&self.config, self.taken_over(key))
    }

    /// A turn on the thread ended: Codex's `notifications/turn-ended` (the
    /// server puts its cursor away). Skipped while a call is running.
    pub async fn turn_ended(&self, tenant_id: &TenantId, thread_id: &ThreadId) {
        for (_, session) in self.matching(tenant_id, thread_id) {
            if let Ok(mut slot) = session.server.try_lock()
                && let Some(server) = slot.as_mut()
                && server.alive()
                && let Err(error) = server.notify("notifications/turn-ended").await
            {
                tracing::debug!(%error, "computer-use turn-ended notification failed");
                *slot = None;
                session.flags().process_running = false;
            }
        }
    }

    /// The user takes the desktop: every session of theirs is refused until
    /// hand back. `viewer` names the viewer holding it (released if that
    /// viewer stops polling).
    pub fn take_over_as(&self, key: &ComputerUseSession, viewer: &str) -> SessionStatus {
        self.live.take_over(&desk(key), viewer);
        self.status(key)
    }

    /// [`Self::take_over_as`] from a caller that names no viewer. `None`
    /// is kept for the port's busy answer; a take over always fits.
    pub fn take_over(&self, key: &ComputerUseSession) -> Option<SessionStatus> {
        Some(self.take_over_as(key, "route"))
    }

    pub fn hand_back(&self, key: &ComputerUseSession) -> SessionStatus {
        self.live.hand_back(&desk(key));
        self.status(key)
    }

    pub fn status(&self, key: &ComputerUseSession) -> SessionStatus {
        let taken_over = self.taken_over(key);
        self.existing(key).map_or_else(
            || SessionStatus::none(taken_over),
            |session| session.status(&self.config, taken_over),
        )
    }

    /// Tie frame `seq` to the tool result that reported it.
    pub fn bind_result_frame(&self, key: &ComputerUseSession, seq: u64, result_ref: &str) {
        let Some(session) = self.existing(key) else {
            return;
        };
        let mut flags = session.flags();
        if let Some(entry) = flags.recent.iter_mut().find(|(frame, _)| frame.seq == seq) {
            entry.1 = Some(result_ref.to_string());
        }
    }

    /// The screenshot of tool result `result_ref`, while it is among the
    /// session's newest [`MODEL_FRAMES`].
    pub fn result_frame(&self, key: &ComputerUseSession, result_ref: &str) -> Option<Arc<Frame>> {
        let session = self.existing(key)?;
        let flags = session.flags();
        flags
            .recent
            .iter()
            .find(|(_, bound)| bound.as_deref() == Some(result_ref))
            .map(|(frame, _)| Arc::clone(frame))
    }

    /// The latest frame, if it is newer than `after_seq`.
    pub fn latest_frame(
        &self,
        key: &ComputerUseSession,
        after_seq: Option<u64>,
    ) -> Option<Arc<Frame>> {
        let session = self.existing(key)?;
        let frame = session.flags().frame.clone()?;
        (after_seq.is_none_or(|after| frame.seq > after)).then_some(frame)
    }

    /// The live view of every session of `desk`'s user. `viewer` marks a
    /// polling viewer alive (it keeps its take over).
    pub(crate) fn viewer_state(&self, desk: &Desk, viewer: Option<&str>) -> Value {
        self.live.snapshot(desk, viewer, |key| {
            self.existing(key)
                .is_some_and(|session| session.stopped(&self.config))
        })
    }

    /// The newest picture of one window `key`'s session used, after
    /// `after_seq` (waits up to a second for one).
    pub(crate) async fn window_frame(
        &self,
        key: &ComputerUseSession,
        window_id: u32,
        width: u32,
        fps: f64,
        after_seq: Option<u64>,
    ) -> Value {
        let Some(window) = self.live.watched(key, window_id) else {
            return json!({"ok": false, "code": Refusal::Ended.as_str()});
        };
        let width = width.clamp(160, 1600);
        match self
            .capture
            .frame(&self.live, window, width, fps, after_seq)
            .await
        {
            Ok(Some(shot)) => json!({"ok": true, "frame": shot.to_json()}),
            Ok(None) => json!({"ok": true, "frame": null}),
            Err(refusal) => {
                let detail = self.live.health(window_id).error;
                json!({"ok": false, "code": refusal.as_str(), "detail": detail})
            }
        }
    }

    /// Kill every process (host shutdown).
    pub async fn shutdown(&self) {
        let all: Vec<(ComputerUseSession, Arc<Session>)> = self.sessions().drain().collect();
        for (key, session) in all {
            self.live.ended(&key);
            session.stop.send_modify(|epoch| *epoch += 1);
            if let Some(server) = session.server.lock().await.take() {
                server.shutdown().await;
            }
        }
    }
}
