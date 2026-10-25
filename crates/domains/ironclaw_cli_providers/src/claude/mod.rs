//! The Claude CLI (`claude -p`, stream-json) as an orchestrator runtime.
//!
//! One process per turn: the first turn of a thread starts the session with
//! `--session-id <native id>`, later turns continue it with `--resume`. The
//! session id is minted at thread creation and kept by the engine as the
//! thread's `native_session_id`, so a backend restart loses nothing but the
//! turn that was running.
//!
//! With a [`CliBridge`] the CLI has its tools, and every call waits for the
//! thread's owner: the driver registers a PreToolUse hook over the
//! stream-json control protocol before the prompt is sent, and answers each
//! callback with the person's decision (deny on timeout or any failure).
//! The orchestrator tools come from the loopback MCP route
//! (`--mcp-config`, this session's token only, `--strict-mcp-config`).
//! Without a bridge, tools stay off (`--tools ""`). Either way: no allow
//! rules, prompts answered "no", never `--dangerously-skip-permissions`.

mod control;
mod stream;
mod transcript;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use ironclaw_host_api::ids::ThreadId;
use ironclaw_orchestrator::model::{RuntimeId, ThreadRecord, ThreadScope, now_ms};
use ironclaw_orchestrator::runtime::*;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::watch;

use crate::bridge::{CliBridge, SESSION_TOKEN_TTL, SessionGrant, mcp_config_json};
use control::{BridgedTurn, SharedStdin, Step};
use stream::TurnFold;

/// The CLI release the wire handling was checked against (`claude --version`).
/// `claude-codes` in Cargo.toml is pinned to the same number.
pub const EXPECTED_CLI_VERSION: &str = "2.1.302";

/// Models offered in the catalog; the CLI resolves them itself.
/// `claude --effort` levels.
const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

const MODELS: [&str; 4] = [
    "claude-opus-5-5",
    "claude-sonnet-5-5",
    "claude-haiku-5-5",
    "claude-fable-5-1",
];

/// How long Stop waits after SIGTERM before SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(2);
/// How long the CLI may take to acknowledge the hook registration.
const INIT_TIMEOUT: Duration = Duration::from_secs(60);

/// One Claude CLI account.
#[derive(Debug, Clone)]
pub struct ClaudeCliConfig {
    /// Runtime and provider id (`claude-cli`, `claude-cli-2`).
    pub runtime_id: String,
    pub display_name: String,
    pub binary: PathBuf,
    /// `CLAUDE_CONFIG_DIR`; `None` is the CLI default (`~/.claude`).
    pub config_dir: Option<PathBuf>,
    /// Working directory of every session; transcripts are filed under it.
    pub work_dir: PathBuf,
    /// Where the account's login lives (`~/.claude.json` or
    /// `<config_dir>/.claude.json`); its presence is "authenticated".
    pub login_file: PathBuf,
    /// Where the account's transcripts live (`<config>/projects`).
    pub projects_dir: PathBuf,
    /// Where each turn's rate-limit windows are written for the Usage page
    /// (`~/.ironclaw/usage/<runtime_id>.json`); `None` writes nothing.
    pub usage_file: Option<PathBuf>,
}

struct ActiveTurn {
    pgid: i32,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    /// Flips to `true` once the turn has been reported finished.
    done: watch::Receiver<bool>,
}

pub struct ClaudeCliRuntime {
    id: RuntimeId,
    config: ClaudeCliConfig,
    sink: Weak<dyn TurnEventSink>,
    // arch-exempt: optional_arc, a host without the gateway runs the CLI tools-off, plan #v3-cli-bridge
    bridge: Option<Arc<CliBridge>>,
    active: Arc<Mutex<HashMap<ThreadId, ActiveTurn>>>,
    /// Each thread's last profile context, reused when Stop restarts a turn.
    profiles: Mutex<HashMap<ThreadId, TurnProfile>>,
}

/// What one turn's process is started with, besides the session.
struct TurnPlan<'a> {
    /// The session's working directory; `--resume` finds it only there.
    dir: PathBuf,
    goal: Option<&'a str>,
    effort: Option<&'a str>,
    profile: Option<&'a TurnProfile>,
}

/// A turn's per-process MCP grant: the token and the 0600 config file.
struct McpSession {
    token: String,
    config_file: PathBuf,
}

impl ClaudeCliRuntime {
    /// `bridge: None` runs every turn with tools off.
    pub fn new(
        config: ClaudeCliConfig,
        sink: Weak<dyn TurnEventSink>,
        bridge: Option<Arc<CliBridge>>,
    ) -> Result<Self, ironclaw_orchestrator::error::OrchestratorError> {
        Ok(Self {
            id: RuntimeId::new(config.runtime_id.clone())?,
            config,
            sink,
            bridge,
            active: Arc::new(Mutex::new(HashMap::new())),
            profiles: Mutex::new(HashMap::new()),
        })
    }

    fn transcript_path(&self, dir: &Path, session_id: &str) -> PathBuf {
        self.config
            .projects_dir
            .join(claude_codes::transcript::encode_project_dir(dir))
            .join(format!("{session_id}.jsonl"))
    }

    /// Where a session runs: the shared work dir when it already started
    /// there, else the profile's folder, else the shared work dir.
    fn session_dir(&self, session_id: &str, profile: Option<&TurnProfile>) -> PathBuf {
        let shared = &self.config.work_dir;
        match profile.and_then(|p| p.folder.as_ref()) {
            Some(folder) if !self.transcript_path(shared, session_id).is_file() => folder.clone(),
            _ => shared.clone(),
        }
    }

    /// The session's transcript wherever it ran (session ids are unique
    /// across project folders).
    async fn find_transcript(&self, session_id: &str) -> Option<PathBuf> {
        let shared = self.transcript_path(&self.config.work_dir, session_id);
        if shared.is_file() {
            return Some(shared);
        }
        let file = format!("{session_id}.jsonl");
        let mut dirs = tokio::fs::read_dir(&self.config.projects_dir).await.ok()?; // silent-ok: no projects folder means no transcript yet
        while let Ok(Some(entry)) = dirs.next_entry().await {
            let path = entry.path().join(&file);
            if path.is_file() {
                return Some(path);
            }
        }
        None
    }

    fn command(
        &self,
        session_id: &str,
        model: Option<&str>,
        mcp: Option<&Path>,
        plan: &TurnPlan<'_>,
    ) -> Command {
        let mut cmd = Command::new(&self.config.binary);
        cmd.args([
            "-p",
            "--output-format",
            "stream-json",
            "--input-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
        ]);
        // Without the approval hook there is nobody to ask: no tools at all.
        if self.bridge.is_none() {
            cmd.args(["--tools", ""]);
        }
        if let Some(config) = mcp {
            cmd.arg("--mcp-config").arg(config);
        }
        // Only MCP servers we pass; anything that would prompt is denied (the
        // hook has already said allow to everything the person approved).
        cmd.args([
            "--strict-mcp-config",
            "--permission-mode",
            "default",
            "--permission-prompts",
            "none",
        ]);
        if let Some(model) = model {
            cmd.args(["--model", model]);
        }
        if let Some(effort) = plan.effort {
            cmd.args(["--effort", effort]);
        }
        if let Some(note) = crate::system_note(plan.profile, plan.goal) {
            cmd.arg("--append-system-prompt").arg(note);
        }
        // The profile's skills load as a plugin, so they are native skills.
        if let Some(skills) = plan.profile.and_then(|p| p.skills.as_ref()) {
            cmd.arg("--plugin-dir").arg(&skills.plugin_dir);
        }
        if self.transcript_path(&plan.dir, session_id).is_file() {
            cmd.args(["--resume", session_id]);
        } else {
            cmd.args(["--session-id", session_id]);
        }
        if let Some(dir) = &self.config.config_dir {
            cmd.env("CLAUDE_CONFIG_DIR", dir);
        }
        cmd.current_dir(&plan.dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        cmd
    }

    fn is_active(&self, thread: &ThreadId) -> bool {
        self.active
            .lock()
            .map(|a| a.contains_key(thread))
            .unwrap_or(false) // silent-ok: a poisoned map only happens after a panic elsewhere
    }

    /// Mint the turn's MCP token and write its config file (0600). `None`
    /// when no bridge or no gateway URL is known yet.
    async fn mcp_session(&self, thread: &ThreadId, run_id: &str) -> Option<McpSession> {
        let bridge = self.bridge.as_ref()?;
        let url = bridge.mcp_url()?;
        let token = bridge.mint_token(
            SessionGrant {
                thread_id: thread.clone(),
                run_id: run_id.to_string(),
            },
            SESSION_TOKEN_TTL,
        );
        let dir = std::env::temp_dir().join("ironclaw-cli-bridge");
        let config_file = dir.join(format!("{run_id}.json"));
        let body = mcp_config_json(url, &token);
        let written = async {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::create_dir_all(&dir).await?;
            tokio::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).await?;
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&config_file)
                .await?;
            file.write_all(body.as_bytes()).await?;
            file.flush().await
        }
        .await;
        match written {
            Ok(()) => Some(McpSession { token, config_file }),
            Err(error) => {
                tracing::debug!(%error, "claude cli: mcp config not written; orchestrator tools off");
                bridge.revoke_token(&token);
                None
            }
        }
    }

    async fn spawn_turn(
        &self,
        thread: &ThreadRecord,
        text: &str,
        goal: Option<&str>,
        effort: Option<&str>,
        profile: Option<&TurnProfile>,
    ) -> Result<String, RuntimeError> {
        let session_id = thread
            .native_session_id
            .clone()
            .ok_or_else(|| RuntimeError::Rejected("thread has no claude session".into()))?;
        let plan = TurnPlan {
            dir: self.session_dir(&session_id, profile),
            goal,
            effort,
            profile,
        };
        tokio::fs::create_dir_all(&plan.dir)
            .await
            .map_err(|e| RuntimeError::Unavailable(format!("work dir: {}", e.kind())))?;
        let run_id = uuid::Uuid::new_v4().to_string();
        let mcp = self.mcp_session(&thread.thread_id, &run_id).await;
        let end_mcp = {
            let bridge = self.bridge.clone();
            move |mcp: Option<McpSession>| async move {
                if let (Some(bridge), Some(mcp)) = (bridge, mcp) {
                    bridge.revoke_token(&mcp.token);
                    let _ = tokio::fs::remove_file(&mcp.config_file).await; // silent-ok: temp file cleanup
                }
            }
        };
        let spawned = self
            .command(
                &session_id,
                thread.model.as_deref(),
                mcp.as_ref().map(|m| m.config_file.as_path()),
                &plan,
            )
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(e) => {
                end_mcp(mcp).await;
                return Err(RuntimeError::Unavailable(format!(
                    "spawn claude: {}",
                    e.kind()
                )));
            }
        };
        let pgid = child.id().and_then(|id| i32::try_from(id).ok());
        let (Some(pgid), Some(stdin), Some(stdout), Some(stderr)) = (
            pgid,
            child.stdin.take(),
            child.stdout.take(),
            child.stderr.take(),
        ) else {
            end_mcp(mcp).await;
            return Err(RuntimeError::Unavailable("claude stdio unavailable".into()));
        };
        let stdin: SharedStdin = Arc::new(tokio::sync::Mutex::new(Some(stdin)));
        let prompt = control::prompt_message(&run_id, text);
        // Bridged: register the hook first; the prompt follows its ack and
        // stdin stays open for hook answers. Otherwise: prompt, then EOF.
        let mut bridged = self.bridge.as_ref().map(|bridge| {
            BridgedTurn::new(
                Arc::clone(bridge),
                Arc::clone(&stdin),
                thread.thread_id.clone(),
                run_id.clone(),
                prompt.clone(),
            )
        });
        let first = if bridged.is_some() {
            control::initialize_request()
        } else {
            prompt
        };
        if let Err(e) = control::write_line(&stdin, &first).await {
            let _ = killpg(Pid::from_raw(pgid), Signal::SIGKILL); // silent-ok: best-effort cleanup of a child that could not take input
            end_mcp(mcp).await;
            return Err(RuntimeError::Unavailable(format!(
                "claude stdin: {}",
                e.kind()
            )));
        }
        if bridged.is_none() {
            control::close(&stdin).await;
        }

        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (done_tx, done_rx) = watch::channel(false);
        if let Ok(mut active) = self.active.lock() {
            active.insert(
                thread.thread_id.clone(),
                ActiveTurn {
                    pgid,
                    cancelled: Arc::clone(&cancelled),
                    done: done_rx,
                },
            );
        }

        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "claude_cli", "stderr: {line}");
            }
        });

        let sink = self.sink.clone();
        let bridge = self.bridge.clone();
        let active = Arc::clone(&self.active);
        let thread_id = thread.thread_id.clone();
        let provider = self.config.runtime_id.clone();
        let usage_file = self.config.usage_file.clone();
        let run = run_id.clone();
        tokio::spawn(async move {
            if let Some(sink) = sink.upgrade() {
                sink.turn_started(&thread_id, Some(run.clone()), now_ms())
                    .await;
            }
            if let Some(bridge) = &bridge {
                bridge.turn_started(&thread_id, &run);
            }
            let mut fold = TurnFold::default();
            let mut refused = None;
            let mut lines = BufReader::new(stdout).lines();
            loop {
                let waiting = bridged.as_ref().is_some_and(BridgedTurn::awaiting_ack);
                let next = if waiting {
                    match tokio::time::timeout(INIT_TIMEOUT, lines.next_line()).await {
                        Ok(next) => next,
                        Err(_) => {
                            refused = Some("claude cli did not acknowledge the approval hook");
                            let _ = killpg(Pid::from_raw(pgid), Signal::SIGKILL); // silent-ok: the turn fails either way
                            break;
                        }
                    }
                } else {
                    lines.next_line().await
                };
                match next {
                    Ok(Some(line)) => {
                        fold.push_line(&line, Instant::now());
                        if let Some(turn) = bridged.as_mut()
                            && turn.on_line(&line).await == Step::InitRefused
                        {
                            refused = Some("claude cli refused the approval hook");
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        tracing::debug!(%error, "claude cli: stdout read failed");
                        break;
                    }
                }
            }
            let exit = child.wait().await.ok().and_then(|s| s.code()); // silent-ok: exit code only labels a failure the fold already detects
            let mut outcome =
                fold.outcome(cancelled.load(std::sync::atomic::Ordering::SeqCst), exit);
            if let Some(reason) = refused {
                outcome = TurnOutcome::ended(TurnEndStatus::Failed, Some(reason.to_string()));
            }
            if let Ok(mut map) = active.lock() {
                map.remove(&thread_id);
            }
            if let Some(bridge) = &bridge {
                bridge.turn_ended(&thread_id, &run, outcome.status.as_str());
            }
            end_mcp(mcp).await;
            if let (Some(path), Some(windows)) = (usage_file, fold.limit_windows()) {
                write_limit_windows(&path, windows, now_ms()).await;
            }
            if let Some(sink) = sink.upgrade() {
                if let Some(telemetry) = fold.telemetry(&provider) {
                    sink.turn_telemetry(&thread_id, telemetry, now_ms()).await;
                }
                sink.turn_finished(&thread_id, Some(run), outcome, now_ms())
                    .await;
            }
            let _ = done_tx.send(true); // silent-ok: nobody waiting on this turn
        });
        Ok(run_id)
    }

    /// Kill the turn's process group and wait until it is reported finished.
    async fn stop(&self, thread: &ThreadId) {
        let entry = self.active.lock().ok().and_then(|a| {
            a.get(thread)
                .map(|t| (t.pgid, Arc::clone(&t.cancelled), t.done.clone()))
        });
        let Some((pgid, cancelled, mut done)) = entry else {
            return;
        };
        cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
        let group = Pid::from_raw(pgid);
        if let Err(error) = killpg(group, Signal::SIGTERM) {
            tracing::debug!(%error, "claude cli: SIGTERM failed");
        }
        let finished = tokio::time::timeout(KILL_GRACE, done.wait_for(|d| *d))
            .await
            .is_ok();
        if !finished {
            if let Err(error) = killpg(group, Signal::SIGKILL) {
                tracing::debug!(%error, "claude cli: SIGKILL failed");
            }
            let _ = done.wait_for(|d| *d).await; // silent-ok: sender dropped means the turn task ended
        }
    }
}

#[async_trait]
impl TurnRuntime for ClaudeCliRuntime {
    fn id(&self) -> RuntimeId {
        self.id.clone()
    }

    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities {
            native_fork: false,
            // A new message restarts the turn; the session keeps the context.
            steer_in_turn: false,
            reports_usage: true,
            // The CLI child dies with the backend; live-work rows cover it.
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
        if let (Some(profile), Ok(mut profiles)) = (&input.profile, self.profiles.lock()) {
            profiles.insert(thread.thread_id.clone(), profile.clone());
        }
        let run_id = self
            .spawn_turn(
                &thread,
                &input.text,
                input.goal.as_deref(),
                input.effort.as_deref(),
                input.profile.as_ref(),
            )
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
        let profile = self
            .profiles
            .lock()
            .ok()
            .and_then(|p| p.get(&thread.thread_id).cloned()); // silent-ok: a poisoned map only follows a panic elsewhere
        self.spawn_turn(thread, text, None, None, profile.as_ref())
            .await?;
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
        let Some(session) = thread.native_session_id.as_deref() else {
            return Ok(Vec::new());
        };
        let Some(path) = self.find_transcript(session).await else {
            return Ok(Vec::new());
        };
        transcript::read(&path, limit).await
    }

    async fn catalog(&self, _scope: &ThreadScope) -> Result<Vec<CatalogProvider>, RuntimeError> {
        Ok(vec![CatalogProvider {
            provider: self.config.runtime_id.clone(),
            name: self.config.display_name.clone(),
            authenticated: self.config.login_file.is_file(),
            current: false,
            models: MODELS.iter().map(|m| m.to_string()).collect(),
            efforts: EFFORTS.iter().map(|e| e.to_string()).collect(),
            model_efforts: Default::default(),
            brand: None,
        }])
    }
}

/// The `claude` binary: `PATH` first, then the installer's usual homes (a
/// desktop-launched backend often runs with a minimal `PATH`).
pub fn find_binary(home: &Path) -> Option<PathBuf> {
    let from_path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .unwrap_or_default();
    from_path
        .into_iter()
        .chain([
            home.join(".local/bin"),
            home.join(".claude/local"),
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/local/bin"),
        ])
        .map(|dir| dir.join("claude"))
        .find(|p| p.is_file())
}

/// The two accounts Lorik runs: the CLI default and `~/.claude-account2`.
/// Persist the CLI's own limit reading (atomic replace). Losing one is
/// harmless: the next turn writes another.
async fn write_limit_windows(path: &Path, windows: &serde_json::Value, at: i64) {
    let body = serde_json::json!({"observedAt": at, "windows": windows}).to_string();
    let tmp = path.with_extension("json.tmp");
    let written = async {
        if let Some(dir) = path.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        tokio::fs::write(&tmp, body).await?;
        tokio::fs::rename(&tmp, path).await
    }
    .await;
    if let Err(error) = written {
        tracing::debug!(%error, "claude cli: limit windows not saved");
    }
}

pub fn default_accounts(home: &Path, binary: &Path) -> Vec<ClaudeCliConfig> {
    let work_dir = home.join(".ironclaw").join("claude-cli");
    let usage_dir = home.join(".ironclaw").join("usage");
    let second = home.join(".claude-account2");
    vec![
        ClaudeCliConfig {
            runtime_id: "claude-cli".into(),
            display_name: "Claude CLI".into(),
            binary: binary.to_path_buf(),
            config_dir: None,
            work_dir: work_dir.clone(),
            login_file: home.join(".claude.json"),
            projects_dir: home.join(".claude").join("projects"),
            usage_file: Some(usage_dir.join("claude-cli.json")),
        },
        ClaudeCliConfig {
            runtime_id: "claude-cli-2".into(),
            display_name: "Claude CLI 2".into(),
            binary: binary.to_path_buf(),
            config_dir: Some(second.clone()),
            work_dir,
            login_file: second.join(".claude.json"),
            projects_dir: second.join("projects"),
            usage_file: Some(usage_dir.join("claude-cli-2.json")),
        },
    ]
}
