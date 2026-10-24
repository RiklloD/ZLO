//! The session layer the Hermes plugin added over the bare server: one
//! process per thread (element indices live in it), Stop, take over / hand
//! back, idle reaping and the latest frame. Calls on one session are
//! serialised; Stop aborts the call in flight and kills its process.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ironclaw_host_api::ids::{TenantId, ThreadId};
use ironclaw_product_contracts::computer_use::ComputerUseSession;
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::watch;

use crate::catalog::computer_use_tool;
use crate::config::ComputerUseConfig;
use crate::error::ComputerUseError;
use crate::frame::{Frame, Target, parse_target, split_content};
use crate::stdio::StdioServer;

/// Why a call did not run, or did not finish. Model-visible codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalCode {
    /// Stop was pressed (before or during the call).
    Cancelled,
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
                "Stopped: the user pressed Stop. Whether a running action took effect is unknown. Stop driving the desktop and tell the user where you got to."
            }
            Self::HumanHasControl => {
                "The user has taken over the desktop. Wait until they hand it back; do not retry now."
            }
            Self::Timeout => {
                "Computer use did not answer in time; the outcome is unknown. Call get_app_state to check before retrying."
            }
            Self::Unavailable => "Computer use is unavailable right now.",
            Self::UnknownTool => "That computer-use tool does not exist.",
            Self::Busy => "Too many computer-use sessions are open; try again later.",
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
    /// The model-visible document.
    pub fn to_json(&self) -> Value {
        match self {
            Self::Completed {
                text,
                is_error,
                frame_seq,
            } => {
                let mut out = json!({"ok": !is_error, "result": text});
                if let Some(seq) = frame_seq {
                    // Screenshots stay host-side; the model reads the tree.
                    out["screenshot_frame"] = json!(seq);
                }
                out
            }
            Self::Refused(code) => json!({"ok": false, "code": code, "error": code.message()}),
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
    fn none() -> Self {
        Self {
            state: SessionState::None,
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
    taken_over: bool,
    stopped_at: Option<Instant>,
    last_used: Option<Instant>,
    last_activity_ms: Option<i64>,
    current_tool: Option<String>,
    last_tool: Option<String>,
    process_running: bool,
    target: Target,
    frame: Option<Arc<Frame>>,
    next_frame_seq: u64,
}

struct Session {
    /// Held for the whole call: calls on one thread run one at a time.
    server: tokio::sync::Mutex<Option<StdioServer>>,
    flags: Mutex<Flags>,
    /// Bumped by Stop; a call in flight watches it.
    stop: watch::Sender<u64>,
}

impl Session {
    fn new() -> Self {
        Self {
            server: tokio::sync::Mutex::new(None),
            flags: Mutex::new(Flags::default()),
            stop: watch::Sender::new(0),
        }
    }

    fn flags(&self) -> MutexGuard<'_, Flags> {
        self.flags.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn refusal(&self, config: &ComputerUseConfig) -> Option<RefusalCode> {
        let flags = self.flags();
        if flags
            .stopped_at
            .is_some_and(|at| at.elapsed() < config.stop_hold)
        {
            return Some(RefusalCode::Cancelled);
        }
        flags.taken_over.then_some(RefusalCode::HumanHasControl)
    }

    fn status(&self, config: &ComputerUseConfig) -> SessionStatus {
        let flags = self.flags();
        let state = if flags
            .stopped_at
            .is_some_and(|at| at.elapsed() < config.stop_hold)
        {
            SessionState::Stopped
        } else if flags.taken_over {
            SessionState::TakenOver
        } else if flags.current_tool.is_some() {
            SessionState::Running
        } else {
            SessionState::Idle
        };
        SessionStatus {
            state,
            process_running: flags.process_running,
            current_tool: flags.current_tool.clone(),
            last_tool: flags.last_tool.clone(),
            last_activity_ms: flags.last_activity_ms,
            frame_seq: flags.frame.as_ref().map(|frame| frame.seq),
            app: flags.target.app.clone(),
            window: flags.target.window.clone(),
        }
    }

    fn idle_for(&self, config: &ComputerUseConfig) -> bool {
        let flags = self.flags();
        !flags.taken_over
            && flags.current_tool.is_none()
            && flags
                .last_used
                .is_none_or(|at| at.elapsed() >= config.idle_reap)
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

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0) // silent-ok: a clock before 1970 only skews a display timestamp
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

/// Every computer-use session of this host.
pub struct ComputerUse {
    config: ComputerUseConfig,
    sessions: Mutex<HashMap<ComputerUseSession, Arc<Session>>>,
}

impl ComputerUse {
    pub fn new(config: ComputerUseConfig) -> Self {
        Self {
            config,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// `None` unless `COMPUTER_USE_ENABLED` turns it on.
    pub fn from_env() -> Result<Option<Arc<Self>>, ComputerUseError> {
        Ok(ComputerUseConfig::from_env()?.map(|config| Arc::new(Self::new(config))))
    }

    pub fn config(&self) -> &ComputerUseConfig {
        &self.config
    }

    fn sessions(&self) -> MutexGuard<'_, HashMap<ComputerUseSession, Arc<Session>>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn existing(&self, key: &ComputerUseSession) -> Option<Arc<Session>> {
        self.sessions().get(key).cloned()
    }

    /// The session for `key`, created if there is room. Idle sessions are
    /// reaped first; their processes die when the last handle drops.
    fn session(&self, key: &ComputerUseSession) -> Option<Arc<Session>> {
        let mut sessions = self.sessions();
        if let Some(session) = sessions.get(key) {
            return Some(Arc::clone(session));
        }
        sessions.retain(|_, session| !session.idle_for(&self.config));
        if sessions.len() >= self.config.max_sessions {
            return None;
        }
        let session = Arc::new(Session::new());
        sessions.insert(key.clone(), Arc::clone(&session));
        Some(session)
    }

    /// Run one tool. Never fails: refusals and server failures come back as
    /// a model-visible outcome. `max_text_bytes` bounds the returned text.
    pub async fn call(
        &self,
        key: &ComputerUseSession,
        tool: &str,
        arguments: Value,
        max_text_bytes: usize,
    ) -> ComputerUseOutcome {
        if computer_use_tool(tool).is_none() {
            return ComputerUseOutcome::Refused(RefusalCode::UnknownTool);
        }
        let Some(session) = self.session(key) else {
            return ComputerUseOutcome::Refused(RefusalCode::Busy);
        };
        // Subscribe before checking, so a Stop between the two is still seen.
        let mut stop = session.stop.subscribe();
        if let Some(code) = session.refusal(&self.config) {
            return ComputerUseOutcome::Refused(code);
        }
        let mut slot = session.server.lock().await;
        if let Some(code) = session.refusal(&self.config) {
            return ComputerUseOutcome::Refused(code);
        }
        session.flags().current_tool = Some(tool.to_string());
        let _running = RunningMark(&session);
        let reused = slot.take();
        let params = json!({"name": tool, "arguments": arguments});
        let result = tokio::select! {
            biased;
            _ = stop.changed() => Err(ComputerUseError::Exited),
            result = self.run(reused, params) => result,
        };
        let stopped = session.refusal(&self.config) == Some(RefusalCode::Cancelled);
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
        match result {
            Ok(result) => self.absorb(&session, tool, &result, max_text_bytes),
            Err(_) if stopped => ComputerUseOutcome::Refused(RefusalCode::Cancelled),
            Err(ComputerUseError::Timeout) => ComputerUseOutcome::Refused(RefusalCode::Timeout),
            Err(ComputerUseError::Server { message }) => ComputerUseOutcome::Completed {
                text: message,
                is_error: true,
                frame_seq: None,
            },
            Err(_) => ComputerUseOutcome::Refused(RefusalCode::Unavailable),
        }
    }

    /// Start (or reuse) the process and send one `tools/call`. A server
    /// error leaves the process usable; anything else drops (kills) it.
    async fn run(
        &self,
        reused: Option<StdioServer>,
        params: Value,
    ) -> Result<(StdioServer, Result<Value, ComputerUseError>), ComputerUseError> {
        let mut server = match reused {
            Some(mut server) => match server.alive() {
                true => server,
                false => StdioServer::start(&self.config).await?,
            },
            None => StdioServer::start(&self.config).await?,
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
        let mut flags = session.flags();
        flags.last_activity_ms = Some(now_ms());
        let target = parse_target(&text);
        if target.app.is_some() {
            flags.target = target;
        }
        let mut frame_seq = None;
        if let Some((mime_type, data)) = image
            && data.len() <= self.config.max_frame_bytes
        {
            flags.next_frame_seq += 1;
            let seq = flags.next_frame_seq;
            flags.frame = Some(Arc::new(Frame {
                seq,
                captured_at_ms: now_ms(),
                mime_type,
                data,
                tool: tool.to_string(),
                app: flags.target.app.clone(),
                window: flags.target.window.clone(),
            }));
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

    /// Stop: abort the call in flight, kill the process, refuse new actions
    /// for the hold time. Take over is cleared; the user ended the session.
    pub async fn stop(&self, key: &ComputerUseSession) -> SessionStatus {
        match self.existing(key) {
            Some(session) => self.stop_session(&session).await,
            None => SessionStatus::none(),
        }
    }

    /// The thread's Stop fired: end its session, whoever's run it was.
    pub async fn stop_thread(&self, tenant_id: &TenantId, thread_id: &ThreadId) {
        let matching: Vec<Arc<Session>> = self
            .sessions()
            .iter()
            .filter(|(key, _)| &key.tenant_id == tenant_id && &key.thread_id == thread_id)
            .map(|(_, session)| Arc::clone(session))
            .collect();
        for session in matching {
            self.stop_session(&session).await;
        }
    }

    async fn stop_session(&self, session: &Session) -> SessionStatus {
        {
            let mut flags = session.flags();
            flags.stopped_at = Some(Instant::now());
            flags.taken_over = false;
        }
        session.stop.send_modify(|epoch| *epoch += 1);
        // A call in flight owns the process and kills it itself.
        if let Ok(mut slot) = session.server.try_lock()
            && let Some(server) = slot.take()
        {
            server.shutdown().await;
            session.flags().process_running = false;
        }
        session.status(&self.config)
    }

    /// A turn on the thread ended: Codex's `notifications/turn-ended` (the
    /// server puts its cursor away). Skipped while a call is running.
    pub async fn turn_ended(&self, tenant_id: &TenantId, thread_id: &ThreadId) {
        let matching: Vec<Arc<Session>> = self
            .sessions()
            .iter()
            .filter(|(key, _)| &key.tenant_id == tenant_id && &key.thread_id == thread_id)
            .map(|(_, session)| Arc::clone(session))
            .collect();
        for session in matching {
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

    /// The user takes the desktop: new actions are refused until hand back.
    /// `None` when there is no room for a new session.
    pub fn take_over(&self, key: &ComputerUseSession) -> Option<SessionStatus> {
        let session = self.session(key)?;
        {
            let mut flags = session.flags();
            flags.taken_over = true;
            flags.last_used = Some(Instant::now());
        }
        Some(session.status(&self.config))
    }

    pub fn hand_back(&self, key: &ComputerUseSession) -> SessionStatus {
        match self.existing(key) {
            Some(session) => {
                {
                    let mut flags = session.flags();
                    flags.taken_over = false;
                    flags.last_used = Some(Instant::now());
                }
                session.status(&self.config)
            }
            None => SessionStatus::none(),
        }
    }

    pub fn status(&self, key: &ComputerUseSession) -> SessionStatus {
        self.existing(key)
            .map_or_else(SessionStatus::none, |session| session.status(&self.config))
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

    /// Kill every process (host shutdown).
    pub async fn shutdown(&self) {
        let all: Vec<Arc<Session>> = self.sessions().drain().map(|(_, s)| s).collect();
        for session in all {
            session.stop.send_modify(|epoch| *epoch += 1);
            if let Some(server) = session.server.lock().await.take() {
                server.shutdown().await;
            }
        }
    }
}
