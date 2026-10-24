//! The Antigravity ACP agent (`agy_acp_server`) as an orchestrator
//! runtime, driven over the Agent Client Protocol like the Hermes t3code
//! driver: `initialize`, `authenticate` eagerly, `session/new` once, then
//! `session/resume` instead of replaying history.
//!
//! One agent process per turn (it starts in about a second). The ACP session
//! id is the agent's to mint, so the thread's `native_session_id` is a key of
//! ours and `<state_dir>/sessions/<key>` holds the agent's id. Transcripts are
//! IronClaw's own journal (`<state_dir>/transcripts/<key>.jsonl`): the agent's
//! conversation store is not a stable format.
//!
//! The `agy` CLI is not an ACP agent (1.4.0 has no ACP mode); the ACP server
//! is the separately shipped runtime T3 Code and Hermes install.
//!
//! Tools fail closed: the client offers no file system or terminal, the
//! session runs in the agent's `default` (ask first) mode, and every
//! `session/request_permission` goes through [`crate::permissions::decide`]:
//! an IronClaw approval in the thread when the host has a bridge, a denial
//! otherwise.

mod fold;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use ironclaw_host_api::ids::ThreadId;
use ironclaw_orchestrator::model::{RuntimeId, ThreadRecord, ThreadScope, now_ms};
use ironclaw_orchestrator::runtime::*;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, watch};

use crate::bridge::CliBridge;
use crate::journal;
use crate::permissions::{self, PermissionDecision, PermissionRequest};
use crate::rpc::{self, Inbound, RpcClient, RpcError};
use fold::PromptFold;

/// The ACP runtime release the driver was checked against.
pub const EXPECTED_RUNTIME_VERSION: &str = "1.3.0";

/// Model alias that keeps the agent's own choice.
const DEFAULT_MODEL: &str = "default";

const INIT_TIMEOUT: Duration = Duration::from_secs(90);
const SESSION_TIMEOUT: Duration = Duration::from_secs(120);
const CONFIG_TIMEOUT: Duration = Duration::from_secs(60);
/// A prompt runs as long as the agent works on it; Stop ends it sooner.
const PROMPT_TIMEOUT: Duration = Duration::from_secs(6 * 3600);

/// The only inherited variables the agent sees. Everything else (ambient
/// cloud credentials, API keys, agent knobs) is dropped: the agent signs in
/// through its own profile only.
const KEPT_ENV: [&str; 10] = [
    "HOME",
    "PATH",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "SSL_CERT_FILE",
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "NO_PROXY",
];

#[derive(Debug, Clone)]
pub struct AntigravityConfig {
    /// Runtime and provider id (`antigravity`).
    pub runtime_id: String,
    pub display_name: String,
    /// `agy_acp_server(.par)`.
    pub executable: PathBuf,
    /// `localharness_external`, shipped beside the server.
    pub harness: PathBuf,
    /// `GEMINI_HOME`: the agent's private profile and its account sign-in.
    pub profile_dir: PathBuf,
    /// ACP auth method id (`oauth-personal`, `oauth-business`, ...).
    pub auth_method: String,
    /// How long `authenticate` may take. A missing sign-in waits for a
    /// browser here, which a background turn cannot open, so keep it short.
    pub auth_timeout: Duration,
    /// Working directory of every session.
    pub work_dir: PathBuf,
    /// Session-id map, transcripts and the model list.
    pub state_dir: PathBuf,
}

impl AntigravityConfig {
    fn token_file(&self) -> PathBuf {
        self.profile_dir
            .join("antigravity-acp")
            .join("acp_token.json")
    }

    fn session_file(&self, key: &str) -> PathBuf {
        self.state_dir.join("sessions").join(key)
    }

    fn transcript(&self, key: &str) -> PathBuf {
        self.state_dir
            .join("transcripts")
            .join(format!("{key}.jsonl"))
    }

    fn models_file(&self) -> PathBuf {
        self.state_dir.join("models.json")
    }

    fn uses_browser(&self) -> bool {
        matches!(
            self.auth_method.as_str(),
            "oauth-personal" | "oauth-business"
        )
    }
}

struct ActiveTurn {
    pgid: i32,
    cancelled: Arc<AtomicBool>,
    /// The live connection and ACP session, once the session is open.
    session: Arc<Mutex<Option<(RpcClient, String)>>>,
    done: watch::Receiver<bool>,
}

pub struct AntigravityRuntime {
    id: RuntimeId,
    config: Arc<AntigravityConfig>,
    sink: Weak<dyn TurnEventSink>,
    // arch-exempt: optional_arc, a host without the gateway denies every permission, plan #v3-cli-bridge
    bridge: Option<Arc<CliBridge>>,
    active: Arc<Mutex<HashMap<ThreadId, ActiveTurn>>>,
}

impl AntigravityRuntime {
    pub fn new(
        config: AntigravityConfig,
        sink: Weak<dyn TurnEventSink>,
        bridge: Option<Arc<CliBridge>>,
    ) -> Result<Self, ironclaw_orchestrator::error::OrchestratorError> {
        Ok(Self {
            id: RuntimeId::new(config.runtime_id.clone())?,
            config: Arc::new(config),
            sink,
            bridge,
            active: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    fn is_active(&self, thread: &ThreadId) -> bool {
        self.active
            .lock()
            .map(|a| a.contains_key(thread))
            .unwrap_or(false) // silent-ok: a poisoned map only happens after a panic elsewhere
    }

    fn command(&self) -> Command {
        let c = &self.config;
        let mut cmd = Command::new(&c.executable);
        cmd.env_clear();
        for key in KEPT_ENV {
            if let Some(value) = std::env::var_os(key) {
                cmd.env(key, value);
            }
        }
        // The agent is a one-file bundle that unpacks itself per launch;
        // keep that under its profile, where it can be reclaimed.
        let tmp = c.profile_dir.join("antigravity-acp").join("tmp");
        cmd.env("GEMINI_HOME", &c.profile_dir)
            .env("AGY_ACP_FORCE_FILE_STORAGE", "1")
            .env("PYTHONUNBUFFERED", "1")
            .env("ELECTRON_RUN_AS_NODE", "1")
            .env("ANTIGRAVITY_HARNESS_PATH", &c.harness)
            .env("TMPDIR", tmp)
            // A background turn never opens a browser on its own.
            .env("BROWSER", "/usr/bin/true")
            .current_dir(&c.work_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        cmd
    }

    async fn spawn_turn(
        &self,
        thread: &ThreadRecord,
        text: &str,
        goal: Option<&str>,
    ) -> Result<String, RuntimeError> {
        let key = thread
            .native_session_id
            .clone()
            .ok_or_else(|| RuntimeError::Rejected("thread has no antigravity session".into()))?;
        prepare_dirs(&self.config)
            .await
            .map_err(|e| RuntimeError::Unavailable(format!("antigravity dirs: {}", e.kind())))?;
        let mut child = self
            .command()
            .spawn()
            .map_err(|e| RuntimeError::Unavailable(format!("spawn antigravity: {}", e.kind())))?;
        let pgid = rpc::group_of(&child)
            .ok_or_else(|| RuntimeError::Unavailable("antigravity exited at spawn".into()))?;
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            let _ = killpg(Pid::from_raw(pgid), Signal::SIGKILL); // silent-ok: cleanup of a half-spawned child
            return Err(RuntimeError::Unavailable(
                "antigravity stdio unavailable".into(),
            ));
        };
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                // The sign-in link is printed here; it is never followed.
                tracing::debug!(target: "antigravity", "stderr: {line}");
            }
        });
        let (client, inbound) = RpcClient::start(stdin, stdout, "antigravity");

        let run_id = uuid::Uuid::new_v4().to_string();
        let cancelled = Arc::new(AtomicBool::new(false));
        let session = Arc::new(Mutex::new(None));
        let (done_tx, done_rx) = watch::channel(false);
        if let Ok(mut active) = self.active.lock() {
            active.insert(
                thread.thread_id.clone(),
                ActiveTurn {
                    pgid,
                    cancelled: Arc::clone(&cancelled),
                    session: Arc::clone(&session),
                    done: done_rx,
                },
            );
        }

        let turn = Turn {
            config: Arc::clone(&self.config),
            key,
            model: thread.model.clone(),
            text: text.to_string(),
            goal: goal.map(str::to_string),
            cancelled,
            session,
            bridge: self.bridge.clone(),
            thread_id: thread.thread_id.clone(),
            run_id: run_id.clone(),
        };
        let bridge = self.bridge.clone();
        let sink = self.sink.clone();
        let active = Arc::clone(&self.active);
        let thread_id = thread.thread_id.clone();
        let run = run_id.clone();
        tokio::spawn(async move {
            if let Some(sink) = sink.upgrade() {
                sink.turn_started(&thread_id, Some(run.clone()), now_ms())
                    .await;
            }
            if let Some(bridge) = &bridge {
                bridge.turn_started(&thread_id, &run);
            }
            let (outcome, telemetry) = turn.run(client, inbound).await;
            close_host(&mut child, pgid).await;
            if let Some(bridge) = &bridge {
                bridge.turn_ended(&thread_id, &run, outcome.status.as_str());
            }
            if let Ok(mut map) = active.lock() {
                map.remove(&thread_id);
            }
            if let Some(sink) = sink.upgrade() {
                if let Some(telemetry) = telemetry {
                    sink.turn_telemetry(&thread_id, telemetry, now_ms()).await;
                }
                sink.turn_finished(&thread_id, Some(run), outcome, now_ms())
                    .await;
            }
            let _ = done_tx.send(true); // silent-ok: nobody waiting on this turn
        });
        Ok(run_id)
    }

    /// Ask the agent to cancel, then kill its process group if it does not
    /// settle within the grace period. Waits until the turn is reported.
    async fn stop(&self, thread: &ThreadId) {
        let entry = self.active.lock().ok().and_then(|a| {
            a.get(thread).map(|t| {
                (
                    t.pgid,
                    Arc::clone(&t.cancelled),
                    Arc::clone(&t.session),
                    t.done.clone(),
                )
            })
        });
        let Some((pgid, cancelled, session, mut done)) = entry else {
            return;
        };
        cancelled.store(true, Ordering::SeqCst);
        let open = session.lock().ok().and_then(|s| s.clone());
        if let Some((client, session_id)) = open {
            let _ = client.notify("session/cancel", Some(json!({"sessionId": session_id}))); // silent-ok: the kill below still ends the turn
        }
        if tokio::time::timeout(rpc::KILL_GRACE, done.wait_for(|d| *d))
            .await
            .is_ok()
        {
            return;
        }
        let group = Pid::from_raw(pgid);
        if let Err(error) = killpg(group, Signal::SIGTERM) {
            tracing::debug!(%error, "antigravity: SIGTERM failed");
        }
        if tokio::time::timeout(rpc::KILL_GRACE, done.wait_for(|d| *d))
            .await
            .is_err()
        {
            if let Err(error) = killpg(group, Signal::SIGKILL) {
                tracing::debug!(%error, "antigravity: SIGKILL failed");
            }
            let _ = done.wait_for(|d| *d).await; // silent-ok: sender dropped means the turn task ended
        }
    }
}

/// One turn's protocol work, owned by its task.
struct Turn {
    config: Arc<AntigravityConfig>,
    key: String,
    model: Option<String>,
    text: String,
    /// The thread's goal, sent ahead of `text` but never journaled.
    goal: Option<String>,
    cancelled: Arc<AtomicBool>,
    session: Arc<Mutex<Option<(RpcClient, String)>>>,
    // arch-exempt: optional_arc, a host without the gateway denies every permission, plan #v3-cli-bridge
    bridge: Option<Arc<CliBridge>>,
    thread_id: ThreadId,
    run_id: String,
}

impl Turn {
    /// Run the turn, then drop every handle on the connection so the agent
    /// sees stdin close and exits.
    async fn run(
        self,
        client: RpcClient,
        inbound: mpsc::UnboundedReceiver<Inbound>,
    ) -> (
        TurnOutcome,
        Option<ironclaw_orchestrator::telemetry::TurnTelemetry>,
    ) {
        let result = self.drive(client, inbound).await;
        if let Ok(mut slot) = self.session.lock() {
            *slot = None;
        }
        result
    }

    async fn drive(
        &self,
        client: RpcClient,
        mut inbound: mpsc::UnboundedReceiver<Inbound>,
    ) -> (
        TurnOutcome,
        Option<ironclaw_orchestrator::telemetry::TurnTelemetry>,
    ) {
        let mut fold = PromptFold::default();
        let session_id = match self.open(&client, &mut fold).await {
            Ok(id) => id,
            Err(outcome) => return (outcome, None),
        };
        if self.cancelled.load(Ordering::SeqCst) {
            return (TurnOutcome::ended(TurnEndStatus::Cancelled, None), None);
        }
        journal::append(
            &self.config.transcript(&self.key),
            &[journal::message(
                HistoryRole::User,
                self.text.clone(),
                Vec::new(),
            )],
        )
        .await;

        let prompt = {
            let client = client.clone();
            let params = json!({
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": crate::with_goal(&self.text, self.goal.as_deref())}],
            });
            async move {
                client
                    .request("session/prompt", params, PROMPT_TIMEOUT)
                    .await
            }
        };
        tokio::pin!(prompt);
        let answer = loop {
            tokio::select! {
                answer = &mut prompt => break answer,
                next = inbound.recv() => match next {
                    Some(message) => self.handle(&client, &session_id, message, &mut fold),
                    // The agent is gone; the pending prompt fails as closed.
                    None => break prompt.await,
                },
            }
        };
        // Updates that raced the answer still belong to this turn.
        while let Ok(message) = inbound.try_recv() {
            self.handle(&client, &session_id, message, &mut fold);
        }
        let outcome = fold.outcome(self.cancelled.load(Ordering::SeqCst), &answer);
        if let Some(entry) = fold.assistant_message() {
            journal::append(&self.config.transcript(&self.key), &[entry]).await;
        }
        let telemetry = fold.telemetry(&self.config.runtime_id, answer.as_ref().ok());
        (outcome, telemetry)
    }

    /// `initialize`, `authenticate`, then resume or create the session and
    /// bring its mode and model in line. Failures come back as the outcome.
    async fn open(&self, client: &RpcClient, fold: &mut PromptFold) -> Result<String, TurnOutcome> {
        let init = json!({
            "protocolVersion": 1,
            "clientInfo": {"name": "ironclaw", "title": "IronClaw", "version": env!("CARGO_PKG_VERSION")},
            "clientCapabilities": {"fs": {"readTextFile": false, "writeTextFile": false}, "terminal": false},
        });
        client
            .request("initialize", init, INIT_TIMEOUT)
            .await
            .map_err(|e| fold::failed(&e))?;
        client
            .request(
                "authenticate",
                json!({"methodId": self.config.auth_method}),
                self.config.auth_timeout,
            )
            .await
            .map_err(|e| match e {
                RpcError::Timeout if self.config.uses_browser() => TurnOutcome::ended(
                    TurnEndStatus::Failed,
                    Some("antigravity: sign-in required (run the Antigravity login)".into()),
                ),
                other => fold::failed(&other),
            })?;

        let cwd = self.config.work_dir.display().to_string();
        let known = tokio::fs::read_to_string(self.config.session_file(&self.key))
            .await
            .ok() // silent-ok: no file means the first turn of the thread
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let mut opened = None;
        if let Some(id) = known {
            let resumed = client
                .request(
                    "session/resume",
                    json!({"sessionId": id, "cwd": cwd, "mcpServers": []}),
                    SESSION_TIMEOUT,
                )
                .await;
            match resumed {
                Ok(result) => opened = Some((id, result)),
                // The agent lost the session: start over rather than leave
                // the thread stuck. Context lives on in our transcript.
                Err(error) => tracing::debug!(error = %error.label(), "antigravity: resume failed"),
            }
        }
        let (session_id, result) = match opened {
            Some(found) => found,
            None => {
                let result = client
                    .request(
                        "session/new",
                        json!({"cwd": cwd, "mcpServers": []}),
                        SESSION_TIMEOUT,
                    )
                    .await
                    .map_err(|e| fold::failed(&e))?;
                let id = result
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        TurnOutcome::ended(
                            TurnEndStatus::Failed,
                            Some("antigravity: session/new returned no session".into()),
                        )
                    })?
                    .to_string();
                save_session_id(&self.config.session_file(&self.key), &id).await;
                (id, result)
            }
        };
        if let Ok(mut slot) = self.session.lock() {
            *slot = Some((client.clone(), session_id.clone()));
        }

        let mut options = result.get("configOptions").cloned().unwrap_or(Value::Null);
        save_models(&self.config.models_file(), &options).await;
        self.apply_mode(client, &session_id, &options, result.get("modes"))
            .await
            .map_err(|e| fold::failed(&e))?;
        if let Some(model) = self.model.as_deref().filter(|m| *m != DEFAULT_MODEL)
            && fold::current_model(&options).as_deref() != Some(model)
        {
            if !fold::model_options(&options).iter().any(|m| m == model) {
                return Err(TurnOutcome::ended(
                    TurnEndStatus::Failed,
                    Some(format!(
                        "antigravity: model {model} is not available for this account"
                    )),
                ));
            }
            let answer = client
                .request(
                    "session/set_config_option",
                    json!({"sessionId": session_id, "configId": "model", "value": model}),
                    CONFIG_TIMEOUT,
                )
                .await
                .map_err(|e| fold::failed(&e))?;
            if let Some(updated) = answer.get("configOptions") {
                options = updated.clone();
            }
        }
        fold.model = fold::current_model(&options).or_else(|| self.model.clone());
        Ok(session_id)
    }

    /// Keep the agent in its ask-first `default` mode.
    async fn apply_mode(
        &self,
        client: &RpcClient,
        session_id: &str,
        options: &Value,
        modes: Option<&Value>,
    ) -> Result<(), RpcError> {
        const MODE: &str = "default";
        let mode_option = options.as_array().and_then(|all| {
            all.iter().find(|o| {
                o.get("category").and_then(Value::as_str) == Some("mode")
                    && o.get("type").and_then(Value::as_str) == Some("select")
            })
        });
        if let Some(option) = mode_option {
            if option.get("currentValue").and_then(Value::as_str) != Some(MODE)
                && let Some(id) = option.get("id").and_then(Value::as_str)
            {
                client
                    .request(
                        "session/set_config_option",
                        json!({"sessionId": session_id, "configId": id, "value": MODE}),
                        CONFIG_TIMEOUT,
                    )
                    .await?;
            }
        } else if let Some(current) = modes
            .and_then(|m| m.get("currentModeId"))
            .and_then(Value::as_str)
            && current != MODE
        {
            client
                .request(
                    "session/set_mode",
                    json!({"sessionId": session_id, "modeId": MODE}),
                    CONFIG_TIMEOUT,
                )
                .await?;
        }
        Ok(())
    }

    fn handle(
        &self,
        client: &RpcClient,
        session_id: &str,
        message: Inbound,
        fold: &mut PromptFold,
    ) {
        match message {
            Inbound::Notification { method, params } => {
                if method == "session/update"
                    && params.get("sessionId").and_then(Value::as_str) == Some(session_id)
                {
                    fold.push_update(&params["update"], Instant::now());
                }
            }
            Inbound::Request { id, method, params } => {
                if method == "session/request_permission" {
                    // The answer waits for a person; updates keep flowing meanwhile.
                    let client = client.clone();
                    let bridge = self.bridge.clone();
                    let (thread, run) = (self.thread_id.clone(), self.run_id.clone());
                    let (runtime, session) =
                        (self.config.runtime_id.clone(), session_id.to_string());
                    tokio::spawn(async move {
                        let answer = permission_answer(
                            bridge.as_deref(),
                            &thread,
                            &run,
                            &runtime,
                            &session,
                            &params,
                        )
                        .await;
                        if let Err(error) = client.respond(&id, answer) {
                            tracing::debug!(error = %error.label(), "antigravity: reply not sent");
                        }
                    });
                    return;
                }
                // No file system, no terminal: the client offered neither.
                if let Err(error) =
                    client.respond_error(&id, -32601, "method not supported by this client")
                {
                    tracing::debug!(error = %error.label(), "antigravity: reply not sent");
                }
            }
        }
    }
}

/// The ACP answer to one `session/request_permission`, from the shared seam.
/// Native questions (`interaction_*` ids) are not approvals and are
/// cancelled; a decision without a matching option is cancelled too.
async fn permission_answer(
    bridge: Option<&CliBridge>,
    thread: &ThreadId,
    run_id: &str,
    runtime: &str,
    session_id: &str,
    params: &Value,
) -> Value {
    let call = &params["toolCall"];
    let call_id = call
        .get("toolCallId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let cancelled = json!({"outcome": {"outcome": "cancelled"}});
    if call_id.starts_with("interaction_") {
        return cancelled;
    }
    let request = PermissionRequest {
        runtime: runtime.to_string(),
        session_id: session_id.to_string(),
        tool: call
            .get("kind")
            .or_else(|| call.get("title"))
            .and_then(Value::as_str)
            .unwrap_or("tool")
            .to_string(),
        subject: call
            .get("title")
            .and_then(Value::as_str)
            .map(str::to_string),
    };
    let kind = match permissions::decide(bridge, thread, run_id, &request).await {
        PermissionDecision::AllowOnce => "allow_once",
        PermissionDecision::Deny => "reject_once",
    };
    params["options"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|o| o.get("kind").and_then(Value::as_str) == Some(kind))
        .and_then(|o| o.get("optionId").and_then(Value::as_str))
        .map(|option| json!({"outcome": {"outcome": "selected", "optionId": option}}))
        .unwrap_or(cancelled)
}

async fn prepare_dirs(config: &AntigravityConfig) -> std::io::Result<()> {
    let acp = config.profile_dir.join("antigravity-acp");
    tokio::fs::create_dir_all(acp.join("tmp")).await?;
    tokio::fs::create_dir_all(&config.work_dir).await?;
    tokio::fs::create_dir_all(config.state_dir.join("sessions")).await?;
    // The agent reads its auth method here; never a credential. An existing
    // file (a shared profile) is left alone.
    let settings = acp.join("settings.json");
    if !tokio::fs::try_exists(&settings).await.unwrap_or(false) {
        // silent-ok: a failed probe rewrites the same small file
        let body = json!({"auth": {"type": config.auth_method}}).to_string();
        tokio::fs::write(&settings, format!("{body}\n")).await?;
    }
    Ok(())
}

async fn save_session_id(path: &Path, id: &str) {
    let written = async {
        if let Some(dir) = path.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        tokio::fs::write(path, id).await
    }
    .await;
    if let Err(error) = written {
        tracing::debug!(%error, "antigravity: session id not saved");
    }
}

/// Keep the account's model list for the catalog.
async fn save_models(path: &Path, options: &Value) {
    let models = fold::model_options(options);
    if models.is_empty() {
        return;
    }
    let body = json!({"current": fold::current_model(options), "models": models}).to_string();
    if let Err(error) = tokio::fs::write(path, body).await {
        tracing::debug!(%error, "antigravity: model list not saved");
    }
}

/// After the answer: let the agent exit on stdin EOF, then make sure the
/// whole group is gone.
async fn close_host(child: &mut Child, pgid: i32) {
    if tokio::time::timeout(Duration::from_secs(3), child.wait())
        .await
        .is_err()
    {
        rpc::end_group(child, pgid, rpc::KILL_GRACE).await;
    } else {
        let _ = killpg(Pid::from_raw(pgid), Signal::SIGKILL); // silent-ok: usually nothing is left
    }
}

#[async_trait]
impl TurnRuntime for AntigravityRuntime {
    fn id(&self) -> RuntimeId {
        self.id.clone()
    }

    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities {
            native_fork: false,
            // ACP has no in-turn input; a new message restarts the turn.
            steer_in_turn: false,
            reports_usage: true,
            reports_restart_cuts: false,
        }
    }

    async fn create_thread(&self, request: CreateThread) -> Result<CreatedThread, RuntimeError> {
        let thread_id = ThreadId::new(uuid::Uuid::new_v4().to_string())
            .map_err(|e| RuntimeError::Rejected(e.to_string()))?;
        Ok(CreatedThread {
            thread_id,
            native_session_id: Some(uuid::Uuid::new_v4().to_string()),
            provider: Some(self.config.runtime_id.clone()),
            model: request.model,
        })
    }

    async fn start_turn(
        &self,
        thread: &ThreadRecord,
        input: TurnInput,
    ) -> Result<TurnAdmission, RuntimeError> {
        if self.is_active(&thread.thread_id) {
            return Ok(TurnAdmission::Busy);
        }
        let mut thread = thread.clone();
        if input.model.is_some() {
            thread.model = input.model;
        }
        let run_id = self
            .spawn_turn(&thread, &input.text, input.goal.as_deref())
            .await?;
        Ok(TurnAdmission::Started {
            run_id: Some(run_id),
        })
    }

    async fn steer(
        &self,
        thread: &ThreadRecord,
        text: &str,
        _how: SteerHow,
    ) -> Result<SteerOutcome, RuntimeError> {
        self.stop(&thread.thread_id).await;
        self.spawn_turn(thread, text, None).await?;
        Ok(SteerOutcome::Restarted)
    }

    async fn cancel(&self, thread: &ThreadRecord, _reason: &str) -> Result<(), RuntimeError> {
        self.stop(&thread.thread_id).await;
        Ok(())
    }

    async fn history(
        &self,
        thread: &ThreadRecord,
        limit: usize,
    ) -> Result<Vec<HistoryMessage>, RuntimeError> {
        let Some(key) = thread.native_session_id.as_deref() else {
            return Ok(Vec::new());
        };
        journal::read(&self.config.transcript(key), limit).await
    }

    async fn catalog(&self, _scope: &ThreadScope) -> Result<Vec<CatalogProvider>, RuntimeError> {
        Ok(vec![CatalogProvider {
            provider: self.config.runtime_id.clone(),
            name: self.config.display_name.clone(),
            authenticated: tokio::fs::try_exists(self.config.token_file())
                .await
                .unwrap_or(false), // silent-ok: an unreadable token reads as signed out
            current: false,
            models: cached_models(&self.config).await,
        }])
    }
}

/// The account's models: our own list from the last session, else the one
/// Hermes saved in a shared profile, else the agent's default alias.
async fn cached_models(config: &AntigravityConfig) -> Vec<String> {
    let hermes = config
        .profile_dir
        .join("antigravity-acp")
        .join("hermes-models.json");
    for path in [config.models_file(), hermes] {
        let Ok(raw) = tokio::fs::read_to_string(&path).await else {
            continue; // silent-ok: a missing cache falls through to the next source
        };
        let Ok(doc) = serde_json::from_str::<Value>(&raw) else {
            continue; // silent-ok: a torn cache falls through
        };
        let models: Vec<String> = doc["models"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m.as_str().or_else(|| m.get("id").and_then(Value::as_str)))
            .map(str::to_string)
            .collect();
        if !models.is_empty() {
            return models;
        }
    }
    vec![DEFAULT_MODEL.to_string()]
}

/// The installed ACP runtime: Hermes's managed copy, then T3 Code's. Returns
/// `(agy_acp_server, localharness_external)`.
pub fn find_runtime(home: &Path) -> Option<(PathBuf, PathBuf)> {
    let platform = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-arm64",
        ("macos", "x86_64") => "darwin-x64",
        ("linux", "aarch64") => "linux-arm64",
        ("linux", "x86_64") => "linux-x64",
        _ => return None,
    };
    [
        home.join(".hermes/plugin-data/t3code/antigravity-acp"),
        home.join(".t3/tools/antigravity-acp"),
    ]
    .into_iter()
    .find_map(|dir| {
        let base = dir.join(platform);
        let active = std::fs::read_to_string(base.join("active.json")).ok()?; // silent-ok: no install here
        let release: Value = serde_json::from_str(&active).ok()?; // silent-ok: a torn marker is no install
        let version = base.join("versions").join(release["releaseId"].as_str()?);
        let executable = version.join("agy_acp_server.par");
        let harness = version.join("localharness_external");
        (executable.is_file() && harness.is_file()).then_some((executable, harness))
    })
}

/// The default account: the installed ACP runtime, the profile Hermes already
/// signed in when there is one (the plan shares CLI logins across apps),
/// else IronClaw's own.
pub fn default_config(home: &Path) -> Option<AntigravityConfig> {
    let (executable, harness) = find_runtime(home)?;
    let hermes_profile = home.join(".hermes/plugin-data/t3code/antigravity/profile");
    let state = home.join(".ironclaw").join("antigravity");
    let profile_dir = if hermes_profile.is_dir() {
        hermes_profile
    } else {
        state.join("profile")
    };
    Some(AntigravityConfig {
        runtime_id: "antigravity".into(),
        display_name: "Antigravity".into(),
        executable,
        harness,
        profile_dir,
        auth_method: "oauth-personal".into(),
        auth_timeout: Duration::from_secs(60),
        work_dir: state.join("work"),
        state_dir: state,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn permission_requests_wait_for_the_owner_and_questions_are_cancelled() {
        let request = json!({
            "sessionId": "s",
            "toolCall": {"toolCallId": "call_1", "kind": "execute", "title": "rm -rf"},
            "options": [
                {"optionId": "yes", "kind": "allow_once", "name": "Allow"},
                {"optionId": "always", "kind": "allow_always", "name": "Always"},
                {"optionId": "no", "kind": "reject_once", "name": "Reject"},
            ],
        });
        let thread = ThreadId::new("t1").unwrap();
        // No bridge: nobody to ask, so rejected.
        assert_eq!(
            permission_answer(None, &thread, "r1", "antigravity", "s", &request).await,
            json!({"outcome": {"outcome": "selected", "optionId": "no"}})
        );
        let question = json!({"toolCall": {"toolCallId": "interaction_1"}, "options": []});
        assert_eq!(
            permission_answer(None, &thread, "r1", "antigravity", "s", &question).await["outcome"]
                ["outcome"],
            "cancelled"
        );

        // With a bridge the call waits as a gate; the owner's approval
        // picks the once-scoped allow, never "always".
        let bridge = CliBridge::new(crate::bridge::DEFAULT_APPROVAL_TIMEOUT);
        bridge.turn_started(&thread, "r1");
        let waiter = {
            let (bridge, thread) = (Arc::clone(&bridge), thread.clone());
            tokio::spawn(async move {
                permission_answer(Some(&bridge), &thread, "r1", "antigravity", "s", &request).await
            })
        };
        let gate = loop {
            if let Some(g) = bridge
                .snapshot(&thread)
                .and_then(|s| s.gates.first().cloned())
            {
                break g;
            }
            tokio::task::yield_now().await;
        };
        assert_eq!(gate.body.as_deref(), Some("rm -rf"));
        assert!(bridge.resolve_gate(&thread, &gate.gate_ref, true));
        assert_eq!(
            waiter.await.unwrap(),
            json!({"outcome": {"outcome": "selected", "optionId": "yes"}})
        );
    }
}
