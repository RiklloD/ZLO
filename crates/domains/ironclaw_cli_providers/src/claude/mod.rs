//! The Claude CLI (`claude -p`, stream-json) as an orchestrator runtime.
//!
//! One process per turn: the first turn of a thread starts the session with
//! `--session-id <native id>`, later turns continue it with `--resume`. The
//! session id is minted at thread creation and kept by the engine as the
//! thread's `native_session_id`, so a backend restart loses nothing but the
//! turn that was running.
//!
//! Tools are off (`--tools ""`, no MCP servers, prompts answered "no").
//! The user keeps every tool call behind IronClaw's approval queue, and the
//! CLI's own permission path cannot reach that queue yet; until a
//! `--permission-prompt-tool` bridge into `ironclaw_approvals` lands, the CLI
//! runs as a plain model. No allow rules, never
//! `--dangerously-skip-permissions`.

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

use stream::TurnFold;

/// The CLI release the wire handling was checked against (`claude --version`).
/// `claude-codes` in Cargo.toml is pinned to the same number.
pub const EXPECTED_CLI_VERSION: &str = "2.1.302";

/// Models offered in the catalog; the CLI resolves them itself.
const MODELS: [&str; 4] = [
    "claude-opus-5-5",
    "claude-sonnet-5-5",
    "claude-haiku-5-5",
    "claude-fable-5-1",
];

/// How long Stop waits after SIGTERM before SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(2);

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
    active: Arc<Mutex<HashMap<ThreadId, ActiveTurn>>>,
}

impl ClaudeCliRuntime {
    pub fn new(
        config: ClaudeCliConfig,
        sink: Weak<dyn TurnEventSink>,
    ) -> Result<Self, ironclaw_orchestrator::error::OrchestratorError> {
        Ok(Self {
            id: RuntimeId::new(config.runtime_id.clone())?,
            config,
            sink,
            active: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    fn transcript_path(&self, session_id: &str) -> PathBuf {
        self.config
            .projects_dir
            .join(claude_codes::transcript::encode_project_dir(
                &self.config.work_dir,
            ))
            .join(format!("{session_id}.jsonl"))
    }

    fn command(&self, session_id: &str, model: Option<&str>) -> Command {
        let mut cmd = Command::new(&self.config.binary);
        cmd.args([
            "-p",
            "--output-format",
            "stream-json",
            "--input-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
            // Fail closed: no tools, no MCP servers, nobody to say yes.
            "--tools",
            "",
            "--strict-mcp-config",
            "--permission-mode",
            "default",
            "--permission-prompts",
            "none",
        ]);
        if let Some(model) = model {
            cmd.args(["--model", model]);
        }
        if self.transcript_path(session_id).is_file() {
            cmd.args(["--resume", session_id]);
        } else {
            cmd.args(["--session-id", session_id]);
        }
        if let Some(dir) = &self.config.config_dir {
            cmd.env("CLAUDE_CONFIG_DIR", dir);
        }
        cmd.current_dir(&self.config.work_dir)
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

    async fn spawn_turn(&self, thread: &ThreadRecord, text: &str) -> Result<String, RuntimeError> {
        let session_id = thread
            .native_session_id
            .clone()
            .ok_or_else(|| RuntimeError::Rejected("thread has no claude session".into()))?;
        tokio::fs::create_dir_all(&self.config.work_dir)
            .await
            .map_err(|e| RuntimeError::Unavailable(format!("work dir: {}", e.kind())))?;
        let mut child = self
            .command(&session_id, thread.model.as_deref())
            .spawn()
            .map_err(|e| RuntimeError::Unavailable(format!("spawn claude: {}", e.kind())))?;
        let pgid = child
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .ok_or_else(|| RuntimeError::Unavailable("claude exited at spawn".into()))?;
        let (Some(mut stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(RuntimeError::Unavailable("claude stdio unavailable".into()));
        };
        let message = serde_json::json!({
            "type": "user",
            "message": {"role": "user", "content": text},
        });
        // One message, then EOF: the CLI answers and exits.
        let write = async {
            stdin.write_all(format!("{message}\n").as_bytes()).await?;
            stdin.shutdown().await
        };
        if let Err(e) = write.await {
            let _ = killpg(Pid::from_raw(pgid), Signal::SIGKILL); // silent-ok: best-effort cleanup of a child that could not take input
            return Err(RuntimeError::Unavailable(format!(
                "claude stdin: {}",
                e.kind()
            )));
        }

        let run_id = uuid::Uuid::new_v4().to_string();
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
            let mut fold = TurnFold::default();
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => fold.push_line(&line, Instant::now()),
                    Ok(None) => break,
                    Err(error) => {
                        tracing::debug!(%error, "claude cli: stdout read failed");
                        break;
                    }
                }
            }
            let exit = child.wait().await.ok().and_then(|s| s.code()); // silent-ok: exit code only labels a failure the fold already detects
            let outcome = fold.outcome(cancelled.load(std::sync::atomic::Ordering::SeqCst), exit);
            if let Ok(mut map) = active.lock() {
                map.remove(&thread_id);
            }
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
        let run_id = self.spawn_turn(&thread, &input.text).await?;
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
        self.spawn_turn(thread, text).await?;
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
        transcript::read(&self.transcript_path(session), limit).await
    }

    async fn catalog(&self, _scope: &ThreadScope) -> Result<Vec<CatalogProvider>, RuntimeError> {
        Ok(vec![CatalogProvider {
            provider: self.config.runtime_id.clone(),
            name: self.config.display_name.clone(),
            authenticated: self.config.login_file.is_file(),
            current: false,
            models: MODELS.iter().map(|m| m.to_string()).collect(),
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
