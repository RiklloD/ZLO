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
//! rules, prompts answered "no", never `--dangerously-skip-permissions`,
//! and the CLI's own subagent and agent-team tools
//! ([`crate::BUILTIN_SUBAGENT_TOOLS`]) are disallowed: subagents are
//! orchestrator threads (`t3_delegate`).
//!
//! Context stays small: the CLI's artifact tools are disallowed unless the
//! account keeps them ([`ClaudeCliConfig::artifacts`]), and `--settings`
//! lists only hot skills in full (`skills.rs`).

mod compact;
mod control;
pub(crate) mod import;
mod input;
mod login;
mod skills;
mod steer;
mod stream;
mod tasks;
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
use control::{BridgedTurn, SharedSteers, Step, TurnIo};
use input::{Input, InputState, SteerAdmit};
use stream::TurnFold;

pub use skills::SkillListing;

/// The CLI release the wire handling was checked against (`claude --version`).
/// `claude-codes` in Cargo.toml is pinned to the same number.
pub const EXPECTED_CLI_VERSION: &str = "2.1.328";

/// The task list the CLI's task tools write (2.1.328: this variable first,
/// then a team's list, then the session id).
const TASK_LIST_ENV: &str = "CLAUDE_CODE_TASK_LIST_ID";
/// Inherited variables that would route a session into another task list
/// or the CLI's own agent teams (ZLO never uses those: subagents are
/// orchestrator threads). Removed from every child.
const TASK_ROUTING_ENV: [&str; 5] = [
    "CLAUDE_INTERNAL_SESSION_TASK_LIST_ID",
    "CLAUDE_INTERNAL_ASSISTANT_TEAM_NAME",
    "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS",
    "CLAUDE_CODE_TEAMMATE_COMMAND",
    "CLAUDE_CODE_TEAM_TEARDOWN_PARK_TIMEOUT_MS",
];

/// `--autocompact` bounds the CLI accepts (tokens).
const AUTOCOMPACT_RANGE: std::ops::RangeInclusive<u64> = 100_000..=1_000_000;

/// Models offered in the catalog; the CLI resolves them itself.
/// `claude --effort` levels.
const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

const MODELS: [&str; 4] = [
    "claude-opus-5-5",
    "claude-sonnet-5-5",
    "claude-haiku-5-5",
    "claude-fable-5-2",
];

/// How long Stop waits after SIGTERM before SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(2);
/// How long the CLI may take to acknowledge the hook registration.
const INIT_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the CLI may run on once the turn's result is in and its input
/// is closed. The CLI does not exit while a background task of the session
/// lives (a `run_in_background` call, or a Bash call moved to the
/// background at its timeout); a hung one would hold the turn open forever.
/// A process-per-turn session cannot use what such a task brings later:
/// its wake-up turn finds input closed, so every tool call is denied. At
/// the deadline the process group is killed and the turn ends with its
/// result.
const EXIT_GRACE: Duration = Duration::from_secs(10);
/// How long a tool call whose result never comes counts as the CLI working.
const TOOL_BUSY_BOUND: Duration = Duration::from_secs(30 * 60);

/// One Claude CLI account.
#[derive(Debug, Clone)]
pub struct ClaudeCliConfig {
    /// Runtime and provider id (`claude-cli`, `claude-cli-2`).
    pub runtime_id: String,
    pub display_name: String,
    pub binary: PathBuf,
    /// `CLAUDE_CONFIG_DIR`; `None` is the CLI default (`~/.claude`), and an
    /// inherited `CLAUDE_CONFIG_DIR` is cleared so it matches `login_file`.
    pub config_dir: Option<PathBuf>,
    /// Working directory of every session; transcripts are filed under it.
    pub work_dir: PathBuf,
    /// Where the account's login lives (`~/.claude.json` or
    /// `<config_dir>/.claude.json`); "authenticated" when it holds a
    /// signed-in `oauthAccount` (the CLI writes the file signed in or not).
    pub login_file: PathBuf,
    /// Where the account's transcripts live (`<config>/projects`).
    pub projects_dir: PathBuf,
    /// Where each turn's rate-limit windows are written for the Usage page
    /// (`~/.ironclaw/usage/<runtime_id>.json`); `None` writes nothing.
    pub usage_file: Option<PathBuf>,
    /// Compact at about this many context tokens (`--autocompact`) instead
    /// of the CLI's default; it does not change the model's window.
    pub autocompact: Option<u64>,
    /// Whether the CLI's artifact tools (`Artifact*`, `DesignSync`) and
    /// their bundled skills stay on; off, they save about 12k tokens a turn.
    pub artifacts: bool,
    /// How the account's skills are listed (see `skills.rs`).
    pub skills: SkillListing,
}

struct ActiveTurn {
    pgid: i32,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
    /// Flips to `true` once the turn has been reported finished.
    done: watch::Receiver<bool>,
    /// The owner of the turn's stdin (see `input.rs`): a bridged turn keeps
    /// it open until the result, which is how a steer reaches the turn.
    input: Input,
    /// One steer at a time, from its gate answers to its admission.
    admission: Arc<tokio::sync::Mutex<()>>,
    run_id: String,
}

pub struct ClaudeCliRuntime {
    id: RuntimeId,
    config: ClaudeCliConfig,
    sink: Weak<dyn TurnEventSink>,
    // arch-exempt: optional_arc, a host without the gateway runs the CLI tools-off, plan #25
    bridge: Option<Arc<CliBridge>>,
    active: Arc<Mutex<HashMap<ThreadId, ActiveTurn>>>,
    /// Each thread's last profile context, reused when Stop restarts a turn.
    profiles: Mutex<HashMap<ThreadId, TurnProfile>>,
    /// Threads bound to their own git worktree (`TurnInput::work_dir`):
    /// their sessions run there, over the profile folder.
    work_dirs: Mutex<HashMap<ThreadId, PathBuf>>,
    /// Idle `/compact` timers, one per thread (see `compact.rs`).
    compactor: Arc<compact::IdleCompactor>,
    /// What each thread's CLI was sent, for the context meter.
    sent: Arc<crate::context_meter::SentContexts>,
    /// How long after the result an unacknowledged steer may hold stdin.
    steer_ack_timeout: Duration,
    /// How long the CLI may stay silent after the result while steers it
    /// acknowledged keep stdin open.
    steer_idle_timeout: Duration,
    /// How long an unanswered tool call counts as work after the result.
    tool_busy_bound: Duration,
    /// How long the CLI may outlive its closed input after the result.
    exit_grace: Duration,
    /// How long one frame may take to go onto the CLI's stdin.
    write_timeout: Duration,
}

/// What one turn's process is started with, besides the session.
struct TurnPlan<'a> {
    /// The session's working directory; `--resume` finds it only there.
    dir: PathBuf,
    goal: Option<&'a str>,
    /// Long-term memory recalled for this message.
    memory: Option<&'a str>,
    effort: Option<&'a str>,
    profile: Option<&'a TurnProfile>,
    /// `--settings` JSON (the skill listing); `None` passes none.
    settings: Option<&'a str>,
}

impl TurnPlan<'_> {
    /// `--append-system-prompt`: SOUL.md, the delegation and visuals notes
    /// and the goal.
    fn system_note(&self) -> crate::context_meter::Notes {
        crate::system_note(self.profile, self.goal, self.memory)
    }
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
            work_dirs: Mutex::new(HashMap::new()),
            compactor: Arc::default(),
            sent: Arc::default(),
            steer_ack_timeout: steer::ACK_TIMEOUT,
            steer_idle_timeout: steer::IDLE_TIMEOUT,
            tool_busy_bound: TOOL_BUSY_BOUND,
            write_timeout: input::DEFAULT_WRITE_TIMEOUT,
            exit_grace: EXIT_GRACE,
        })
    }

    /// Bound the post-result wait on unacknowledged steers (default
    /// [`steer::ACK_TIMEOUT`]); tests shorten it.
    pub fn with_steer_ack_timeout(mut self, timeout: Duration) -> Self {
        self.steer_ack_timeout = timeout;
        self
    }

    /// Bound the post-result silence while acknowledged steers keep stdin
    /// open (default [`steer::IDLE_TIMEOUT`]); tests shorten it.
    pub fn with_steer_idle_timeout(mut self, timeout: Duration) -> Self {
        self.steer_idle_timeout = timeout;
        self
    }

    /// Bound one write to the CLI's stdin (default 30 s); a write past it
    /// fails the turn's input and ends the turn. Tests shorten it.
    pub fn with_write_timeout(mut self, timeout: Duration) -> Self {
        self.write_timeout = timeout;
        self
    }

    /// Bound how long a tool call whose result never arrives counts as the
    /// CLI working (default 30 min); tests shorten it.
    pub fn with_tool_busy_bound(mut self, bound: Duration) -> Self {
        self.tool_busy_bound = bound;
        self
    }

    /// Bound how long the CLI may run on after the result once its input
    /// is closed (default [`EXIT_GRACE`]); tests shorten it.
    pub fn with_exit_grace(mut self, grace: Duration) -> Self {
        self.exit_grace = grace;
        self
    }

    /// The session's task store (`<config>/tasks/<session>`), beside the
    /// account's `projects` folder.
    fn task_store(&self, session_id: &str) -> Option<PathBuf> {
        self.config
            .projects_dir
            .parent()
            .map(|config| config.join("tasks").join(session_id))
    }

    fn transcript_path(&self, dir: &Path, session_id: &str) -> PathBuf {
        self.config
            .projects_dir
            .join(claude_codes::transcript::encode_project_dir(dir))
            .join(format!("{session_id}.jsonl"))
    }

    /// Where a session runs: the thread's worktree; else, for a terminal
    /// session imported from `~/.claude/projects`, its own folder (or not at
    /// all); else the shared work dir when it already started there; the
    /// folder it was started in when its transcript is elsewhere, as
    /// `--resume` finds it only from there; else the profile's folder, else
    /// the shared work dir.
    async fn session_dir(
        &self,
        thread: &ThreadId,
        session_id: &str,
        profile: Option<&TurnProfile>,
    ) -> Result<PathBuf, RuntimeError> {
        let bound = self
            .work_dirs
            .lock()
            .ok() // silent-ok: a poisoned map only follows a panic elsewhere
            .and_then(|m| m.get(thread).cloned());
        if let Some(dir) = bound {
            return Ok(dir);
        }
        // An imported session resumes in its own folder, or not at all.
        let record = imports_dir(&self.config.work_dir).join(format!("{session_id}.json"));
        if let Some(dir) = crate::workspace::resolve(&record).await? {
            return Ok(dir);
        }
        let shared = &self.config.work_dir;
        if self.transcript_path(shared, session_id).is_file() {
            return Ok(shared.clone());
        }
        if let Some(path) = self.find_transcript(session_id).await
            && let Some(dir) = transcript::cwd(&path).await
            && dir.is_dir()
            && self.transcript_path(&dir, session_id) == path
        {
            return Ok(dir);
        }
        Ok(match profile.and_then(|p| p.folder.as_ref()) {
            Some(folder) => folder.clone(),
            None => shared.clone(),
        })
    }

    /// A session that moves to another folder (into its thread's worktree,
    /// or back once that worktree is gone) takes its transcript along, so
    /// `--resume` finds it there. It is moved, not copied: one file per
    /// session, so history and the next resume always read every turn.
    async fn follow_session(&self, session_id: &str, dir: &Path) -> Result<(), RuntimeError> {
        let target = self.transcript_path(dir, session_id);
        let Some(source) = self.find_transcript(session_id).await else {
            return Ok(());
        };
        if source == target {
            return Ok(());
        }
        let moved = async {
            if let Some(parent) = target.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            // Both live under the account's projects folder: one filesystem.
            tokio::fs::rename(&source, &target).await
        };
        moved
            .await
            .map_err(|e| RuntimeError::Unavailable(format!("session transcript: {}", e.kind())))
    }

    /// The session's one authoritative transcript, wherever it ran
    /// (session ids are unique across project folders). An earlier build
    /// copied a moving session instead of moving it, which can leave
    /// several copies: the one with the most entries wins (an older copy is
    /// a prefix of the newer one), the newest on a tie, and every other is
    /// renamed `<session>.jsonl.superseded-<ms>` beside it — kept, never
    /// deleted, just no longer read.
    async fn find_transcript(&self, session_id: &str) -> Option<PathBuf> {
        let file = format!("{session_id}.jsonl");
        let mut copies = Vec::new();
        let mut dirs = tokio::fs::read_dir(&self.config.projects_dir).await.ok()?; // silent-ok: no projects folder means no transcript yet
        while let Ok(Some(entry)) = dirs.next_entry().await {
            let path = entry.path().join(&file);
            if path.is_file() {
                copies.push(path);
            }
        }
        if copies.len() <= 1 {
            return copies.pop();
        }
        let mut ranked = Vec::with_capacity(copies.len());
        for path in copies {
            let entries = match tokio::fs::read_to_string(&path).await {
                Ok(text) => text.lines().filter(|l| !l.trim().is_empty()).count(),
                Err(error) => {
                    tracing::debug!(%error, "claude cli: transcript copy unreadable; ranked last");
                    0
                }
            };
            let modified = tokio::fs::metadata(&path)
                .await
                .and_then(|m| m.modified())
                .ok(); // silent-ok: an unknown time only loses the tie-break
            ranked.push((entries, modified, path));
        }
        ranked.sort_by_key(|a| std::cmp::Reverse((a.0, a.1)));
        let mut ranked = ranked.into_iter();
        let (_, _, keep) = ranked.next()?;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0); // silent-ok: a clock before 1970 only changes the suffix
        for (_, _, stale) in ranked {
            let aside = stale.with_extension(format!("jsonl.superseded-{stamp}"));
            if let Err(error) = tokio::fs::rename(&stale, &aside).await {
                tracing::debug!(%error, "claude cli: stale transcript copy left in place");
            }
        }
        Some(keep)
    }

    fn command(
        &self,
        session_id: &str,
        model: Option<&str>,
        mcp: Option<&Path>,
        plan: &TurnPlan<'_>,
    ) -> Command {
        let mut cmd = Command::new(&self.config.binary);
        // Identify our headless runs instead of inheriting the SDK/host
        // entrypoint, which Anthropic meters differently on subscription plans.
        cmd.env("CLAUDE_CODE_ENTRYPOINT", "ironclaw");
        // The task-list tools (TaskCreate/TaskUpdate/...), which replace
        // TodoWrite in 2.1.3xx headless runs; they become the thread's plan.
        cmd.env("CLAUDE_CODE_ENABLE_TODO_TOOLS", "1");
        // They run without a gate because they touch only this session's
        // store, so the store is pinned here: the CLI's resolver prefers an
        // inherited list id or team over the session id.
        for var in TASK_ROUTING_ENV {
            cmd.env_remove(var);
        }
        cmd.env(TASK_LIST_ENV, session_id);
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
        // Subagents are orchestrator threads the person can see and steer
        // (`t3_delegate`), never the CLI's own hidden ones.
        cmd.arg("--disallowedTools").arg(self.disallowed_tools());
        if let Some(settings) = plan.settings {
            cmd.arg("--settings").arg(settings);
        }
        // Attached files are read from their landing folder (see `attachments`).
        if let Some(root) = crate::attachments::root() {
            cmd.arg("--add-dir").arg(root);
        }
        if let Some(model) = model {
            cmd.args(["--model", model]);
        }
        if let Some(effort) = plan.effort {
            cmd.args(["--effort", effort]);
        }
        if let Some(tokens) = self.config.autocompact {
            cmd.arg("--autocompact").arg(tokens.to_string());
        }
        if let Some(note) = plan.system_note().text() {
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
        match &self.config.config_dir {
            Some(dir) => cmd.env("CLAUDE_CONFIG_DIR", dir),
            None => cmd.env_remove("CLAUDE_CONFIG_DIR"),
        };
        cmd.current_dir(&plan.dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);
        cmd
    }

    /// The CLI's own subagent tools, and its artifact tools unless the
    /// account keeps them.
    fn disallowed_tools(&self) -> String {
        let artifacts: &[&str] = if self.config.artifacts {
            &[]
        } else {
            &skills::ARTIFACT_TOOLS
        };
        crate::BUILTIN_SUBAGENT_TOOLS
            .iter()
            .chain(artifacts)
            .copied()
            .collect::<Vec<_>>()
            .join(",")
    }

    /// The account's skill listing for its next turn.
    async fn skill_settings(
        &self,
        profile: Option<&TurnProfile>,
        text: &str,
    ) -> skills::LaunchSkills {
        skills::launch_settings(
            &self.config.skills,
            &self.config.runtime_id,
            &self.config.login_file,
            profile.and_then(|p| p.skills.as_ref()),
            self.config.artifacts,
            text,
        )
        .await
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
        (goal, memory): (Option<&str>, Option<&str>),
        effort: Option<&str>,
        profile: Option<&TurnProfile>,
    ) -> Result<String, RuntimeError> {
        let session_id = thread
            .native_session_id
            .clone()
            .ok_or_else(|| RuntimeError::Rejected("thread has no claude session".into()))?;
        // A new turn ends the idle period: no compaction may touch the session.
        self.compactor.preempt(&thread.thread_id).await;
        let launch = self.skill_settings(profile, text).await;
        let plan = TurnPlan {
            dir: self
                .session_dir(&thread.thread_id, &session_id, profile)
                .await?,
            goal,
            memory,
            effort,
            profile,
            settings: launch.settings.as_deref(),
        };
        tokio::fs::create_dir_all(&plan.dir)
            .await
            .map_err(|e| RuntimeError::Unavailable(format!("work dir: {}", e.kind())))?;
        self.follow_session(&session_id, &plan.dir).await?;
        let run_id = uuid::Uuid::new_v4().to_string();
        let mcp = self.mcp_session(&thread.thread_id, &run_id).await;
        // The system note, plus the profile skills as the CLI lists them
        // under this turn's `skillOverrides`; recorded once the CLI runs with
        // them and took its first input, so a failed launch keeps the
        // thread's earlier record.
        let delivery = self.sent.pending(
            &thread.thread_id,
            Some(&plan.system_note().instructions(launch.listing.clone())),
            &crate::context_meter::Notes::default(),
            mcp.is_some(),
        );
        let end_mcp = {
            let bridge = self.bridge.clone();
            move |mcp: Option<McpSession>| async move {
                if let (Some(bridge), Some(mcp)) = (bridge, mcp) {
                    bridge.revoke_token(&mcp.token);
                    if let Err(error) = tokio::fs::remove_file(&mcp.config_file).await {
                        tracing::debug!(%error, "claude cli: could not remove the MCP config temp file");
                    }
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
        let prompt = control::prompt_with_images(&run_id, text).await;
        let steers = SharedSteers::default();
        // From here on only the input owner writes to the CLI.
        let input = Input::spawn(stdin, Arc::clone(&steers), self.write_timeout);
        // Bridged: register the hook first; the prompt follows its ack and
        // stdin stays open for hook answers. Otherwise: prompt, then EOF.
        let task_store = self.task_store(&session_id);
        let mut bridged = self.bridge.as_ref().map(|bridge| {
            BridgedTurn::new(
                TurnIo {
                    bridge: Arc::clone(bridge),
                    input: input.clone(),
                    thread: thread.thread_id.clone(),
                    run_id: run_id.clone(),
                    steers: Arc::clone(&steers),
                    tool_bound: self.tool_busy_bound,
                },
                prompt.clone(),
                task_store,
            )
        });
        let first = if bridged.is_some() {
            control::initialize_request()
        } else {
            prompt
        };
        if !input.frame(first).await {
            crate::rpc::kill_group(pgid, "claude cli: child could not take input");
            end_mcp(mcp).await;
            return Err(RuntimeError::Unavailable(
                "claude stdin: the CLI did not take its first input".into(),
            ));
        }
        delivery.delivered();
        if bridged.is_none() {
            input.close(true);
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
                    input: input.clone(),
                    admission: Arc::default(),
                    run_id: run_id.clone(),
                },
            );
        }

        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "claude_cli", "stderr: {line}");
            }
        });

        let idle = compact::IdleArm::new(
            self,
            &thread.thread_id,
            &session_id,
            thread.model.as_deref(),
            &plan,
        );
        let sink = self.sink.clone();
        let bridge = self.bridge.clone();
        let active = Arc::clone(&self.active);
        let thread_id = thread.thread_id.clone();
        let provider = self.config.runtime_id.clone();
        let usage_file = self.config.usage_file.clone();
        let skill_usage = self.config.skills.usage.clone();
        let skill_names = launch.names;
        let skill_source = launch.source;
        let run = run_id.clone();
        let steer_ack_timeout = self.steer_ack_timeout;
        let steer_idle_timeout = self.steer_idle_timeout;
        let exit_grace = self.exit_grace;
        let mut input_state = input.watch();
        tokio::spawn(async move {
            if let Some(sink) = sink.upgrade() {
                sink.turn_started(&thread_id, Some(run.clone()), now_ms())
                    .await;
            }
            if let Some(bridge) = &bridge {
                bridge.turn_started(&thread_id, &run);
            }
            let mut fold = TurnFold::default();
            let mut skill_watch = skills::SkillWatch::default();
            let mut refused = None;
            let mut lines = BufReader::new(stdout).lines();
            // Absolute deadlines: unrelated output must not extend a wait.
            let init_deadline = tokio::time::Instant::now() + INIT_TIMEOUT;
            let mut steer_deadline: Option<tokio::time::Instant> = None;
            let mut exit_deadline: Option<tokio::time::Instant> = None;
            let mut quiet_from = tokio::time::Instant::now();
            loop {
                // Nothing more can be asked of the CLI, so its exit is waited
                // for only so long: the result is in and input is closed (or
                // the ledger says it closes now: the owner refuses any steer
                // from then on), or a write failed, result or not.
                if exit_deadline.is_none() {
                    let state = input.state();
                    let settled = fold.has_result()
                        && (state != InputState::Open
                            || control::with_ledger(&steers, |l| l.may_close()));
                    if settled || state == InputState::Failed {
                        if state == InputState::Failed && !fold.has_result() {
                            refused = Some("claude cli stopped taking input");
                        }
                        exit_deadline = Some(tokio::time::Instant::now() + exit_grace);
                    }
                }
                let waiting = bridged.as_ref().is_some_and(BridgedTurn::awaiting_ack);
                let steer_wait = bridged
                    .as_ref()
                    .is_some_and(BridgedTurn::waiting_for_steer_ack);
                if !steer_wait {
                    steer_deadline = None;
                }
                // Acknowledged steers still open after the result: Claude
                // works on them as its next native turn, and streams while
                // it does. Silence with nothing running is a CLI that will
                // not, so input closes and the steers it never started are
                // reported unread (t3code bounds the same wait).
                // Silence counts from the last frame, the last hook answer,
                // or the moment a tool call stopped counting as work.
                let idle = if steer_wait || exit_deadline.is_some() {
                    None
                } else {
                    bridged
                        .as_ref()
                        .filter(|t| t.holding_open())
                        .map(|t| (t.answers(), t.busy_until()))
                };
                let next = if let Some((answers, busy_until)) = idle {
                    let idle_deadline = quiet_from + steer_idle_timeout;
                    tokio::select! {
                        next = lines.next_line() => next,
                        // A hook answered: busy may have ended, look again.
                        () = answers.finished() => {
                            quiet_from = tokio::time::Instant::now();
                            continue;
                        }
                        () = tokio::time::sleep_until(busy_until.unwrap_or(idle_deadline)), if busy_until.is_some() => {
                            quiet_from = tokio::time::Instant::now();
                            continue;
                        }
                        // Input closed or failed: look again.
                        Ok(()) = input_state.changed() => continue,
                        () = tokio::time::sleep_until(idle_deadline), if busy_until.is_none() => {
                            tracing::debug!(
                                "claude cli: idle with steers open after the result; input closed"
                            );
                            if let Some(turn) = &bridged {
                                turn.give_up_on_steers().await;
                            }
                            continue;
                        }
                    }
                } else if steer_wait {
                    let deadline = *steer_deadline
                        .get_or_insert_with(|| tokio::time::Instant::now() + steer_ack_timeout);
                    match tokio::time::timeout_at(deadline, lines.next_line()).await {
                        Ok(next) => next,
                        Err(_) => {
                            tracing::debug!("claude cli: steer never acknowledged; input closed");
                            if let Some(turn) = &bridged {
                                turn.give_up_on_steers().await;
                            }
                            continue;
                        }
                    }
                } else if let Some(deadline) = exit_deadline {
                    match tokio::time::timeout_at(deadline, lines.next_line()).await {
                        Ok(next) => next,
                        // Ended below, with whatever it left running.
                        Err(_) => break,
                    }
                } else if waiting {
                    match tokio::time::timeout_at(init_deadline, lines.next_line()).await {
                        Ok(next) => next,
                        Err(_) => {
                            refused = Some("claude cli did not acknowledge the approval hook");
                            crate::rpc::kill_group(
                                pgid,
                                "claude cli: approval hook not acknowledged",
                            );
                            break;
                        }
                    }
                } else {
                    tokio::select! {
                        next = lines.next_line() => next,
                        // A failed write ends the turn even with no result.
                        Ok(()) = input_state.changed() => continue,
                    }
                };
                if matches!(next, Ok(Some(_))) {
                    quiet_from = tokio::time::Instant::now();
                }
                match next {
                    // Settled: what a background task's wake-up streams now
                    // cannot reach the CLI (its tool calls are denied) and
                    // must not change the outcome or open gates. Whether it
                    // started a steer still counts: one it read is not sent
                    // again.
                    Ok(Some(line)) if exit_deadline.is_some() => {
                        tracing::debug!("claude cli: frame after the settled result not acted on");
                        if let Some(turn) = &bridged {
                            turn.observe_only(&line);
                        }
                        report_reads(&sink, &thread_id, &run, &steers).await;
                    }
                    Ok(Some(line)) => {
                        fold.push_line(&line, Instant::now());
                        skill_watch.push_line(&line);
                        if let Some(turn) = bridged.as_mut()
                            && turn.on_line(&line).await == Step::InitRefused
                        {
                            refused = Some("claude cli refused the approval hook");
                        }
                        // What the CLI said about its steers (`started` or an
                        // echo: read; dropped): the timeline's marker follows.
                        report_reads(&sink, &thread_id, &run, &steers).await;
                    }
                    Ok(None) => break,
                    Err(error) => {
                        tracing::debug!(%error, "claude cli: stdout read failed");
                        break;
                    }
                }
            }
            // Bounded even after EOF: a CLI that closed its output can still
            // wait on a background task. What it leaves running goes too.
            let reap_by = exit_deadline.unwrap_or_else(|| tokio::time::Instant::now() + exit_grace);
            let exit = match tokio::time::timeout_at(reap_by, child.wait()).await {
                Ok(status) => status.ok().and_then(|s| s.code()), // silent-ok: exit code only labels a failure the fold already detects
                Err(_) => {
                    tracing::debug!("claude cli: still running after its turn; process tree ended");
                    crate::rpc::end_tree(&mut child, pgid, KILL_GRACE).await;
                    None
                }
            };
            let mut outcome =
                fold.outcome(cancelled.load(std::sync::atomic::Ordering::SeqCst), exit);
            if let Some(reason) = refused {
                outcome = TurnOutcome::ended(TurnEndStatus::Failed, Some(reason.to_string()));
            }
            // Settled from evidence once input is closed (every write it
            // took has finished: no steer can be written after the ledger is
            // read). Unread steers go back to the engine before the turn
            // ends, so no drain runs between the two; after Stop the engine
            // shows them as notices instead. One the engine could not keep
            // fails the turn, naming it.
            input.close_now().await;
            let stopped = cancelled.load(std::sync::atomic::Ordering::SeqCst);
            report_reads(&sink, &thread_id, &run, &steers).await;
            let settlement = control::with_ledger(&steers, |l| l.settle(stopped));
            // Unconfirmed ones (written, no word either way) become the
            // engine's "Steer delivery not confirmed" notices, never resent.
            outcome = crate::steer_door::report_settlement(
                &sink,
                &thread_id,
                &run,
                settlement.unread,
                settlement.unconfirmed,
                outcome,
            )
            .await;
            // Armed while the turn is still live: a Stop or delete either
            // waits on this turn (then preempts the timer) or comes after it
            // and finds the timer; it can never slip in between.
            idle.arm(
                &outcome,
                fold.telemetry(&provider).and_then(|t| t.context_used),
            );
            if let Ok(mut map) = active.lock() {
                map.remove(&thread_id);
            }
            if let Some(bridge) = &bridge {
                bridge.turn_ended(&thread_id, &run, outcome.status.as_str());
            }
            end_mcp(mcp).await;
            skills::record_turn(
                &skill_usage,
                skill_source.as_ref(),
                &skill_names,
                &skill_watch,
            )
            .await;
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
            #[expect(
                clippy::let_underscore_must_use,
                reason = "nobody waiting on this turn"
            )]
            let _ = done_tx.send(true);
        });
        Ok(run_id)
    }

    /// Fold `text` into the running turn (T3 Code's Claude steer):
    /// - a tool call waits for approval: it is declined with the person's
    ///   words as the hook's reason, which Claude hands the model as the
    ///   call's result, and nothing else is written (the model reads the
    ///   words once, not twice);
    /// - the prompt is not written yet: the steer is held and written right
    ///   after it, in order (never a separate later turn);
    /// - otherwise it goes on the turn's open stdin as a user message.
    ///
    /// `Rejected` when no turn runs or its stdin is already closed (the
    /// result is in); the engine then queues the text instead. A held steer
    /// the turn never writes is reported unread at its end.
    async fn steer_in_turn(&self, thread: &ThreadId, id: &str, text: &str) -> SteerOutcome {
        let entry = self.active.lock().ok().and_then(|a| {
            a.get(thread)
                .map(|t| (t.input.clone(), Arc::clone(&t.admission), t.run_id.clone()))
        }); // silent-ok: a poisoned map only follows a panic elsewhere
        let Some((input, admission, run_id)) = entry else {
            return SteerOutcome::TurnEnded;
        };
        // One steer at a time: a second one cannot overtake this one's gate
        // answers, nor see gates this one already took.
        let _admitted = admission.lock().await;
        // The words go as one denial's reason (or a question's answer);
        // only once that reply is on the CLI's stdin do they count as sent.
        // Otherwise (no gate, or that reply definitely failed or was
        // withdrawn) they go as a steer, after every decline.
        if input.state() == InputState::Open
            && let Some(bridge) = &self.bridge
        {
            let gates = bridge.steer_at_gates(thread, &run_id, text, true);
            if !gates.is_empty() && gates.settle().await {
                return SteerOutcome::Steered;
            }
        }
        // A fresh uuid: the steer belongs to the running run, it does not
        // start one in the journal. The ledger tracks it by this uuid until
        // the CLI starts it; one it never starts is queued at turn end.
        let uuid = id.to_string();
        let line = control::steer_message(&uuid, text).await;
        let steer = steer::HeldSteer {
            uuid,
            text: text.to_string(),
            line,
        };
        match input.steer(steer).await {
            SteerAdmit::Written | SteerAdmit::Held | SteerAdmit::Consumed => SteerOutcome::Steered,
            // Input closed, closing (the turn is settling) or failed: the
            // turn is gone; the engine starts the next one with the text.
            SteerAdmit::Refused => SteerOutcome::TurnEnded,
        }
    }

    /// Kill the turn's process group and wait until it is reported finished.
    async fn stop(&self, thread: &ThreadId) {
        let entry = self.active.lock().ok().and_then(|a| {
            a.get(thread).map(|t| {
                (
                    t.pgid,
                    Arc::clone(&t.cancelled),
                    t.done.clone(),
                    t.input.clone(),
                )
            })
        });
        let Some((pgid, cancelled, mut done, input)) = entry else {
            return;
        };
        cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
        // The Stop boundary, in the input's own order: from here on nothing
        // new is written (no prompt, no steer, no gate answer); what came
        // before it is. Bounded: a write in progress has its own bound.
        input.stop(KILL_GRACE).await;
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
            #[expect(
                clippy::let_underscore_must_use,
                reason = "sender dropped means the turn task ended"
            )]
            let _ = done.wait_for(|d| *d).await;
        }
    }
}

#[async_trait]
impl TurnRuntime for ClaudeCliRuntime {
    fn id(&self) -> RuntimeId {
        self.id.clone()
    }

    /// `/compact` sent as a turn is the CLI's own compaction (the idle timer
    /// sends the same); the CLI also compacts by itself near the limit.
    fn compaction(&self) -> RuntimeCompaction {
        RuntimeCompaction {
            manual_prompt: Some(compact::COMPACT_PROMPT),
            automatic: true,
        }
    }

    fn injected_context(
        &self,
        thread: &ThreadId,
        profile: Option<&TurnProfile>,
        goal: Option<&str>,
    ) -> ironclaw_orchestrator::context_report::ContextSample {
        self.sent.sample(thread, || {
            let listing = skills::unmanaged_listing(profile.and_then(|p| p.skills.as_ref()));
            let mcp = self.bridge.as_ref().is_some_and(|b| b.mcp_url().is_some());
            (
                crate::system_note(profile, goal, None).instructions(listing),
                mcp,
            )
        })
    }

    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities {
            native_fork: false,
            // A bridged turn keeps stdin open until every command written
            // is done, and the CLI takes a user message sent mid-turn at its
            // next step (checked against 2.1.328, see `steer.rs`). Without
            // the bridge stdin closes at once.
            steer_in_turn: self.bridge.is_some(),
            reports_usage: true,
            // The CLI child dies with the backend; live-work rows cover it.
            reports_restart_cuts: false,
            host_files: true,
        }
    }

    fn supports_work_dir(&self) -> bool {
        true
    }

    fn default_work_dir(&self) -> Option<PathBuf> {
        Some(self.config.work_dir.clone())
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
        if let Ok(mut dirs) = self.work_dirs.lock() {
            match &input.work_dir {
                Some(dir) => dirs.insert(thread.thread_id.clone(), dir.clone()),
                None => dirs.remove(&thread.thread_id),
            };
        }
        let run_id = self
            .spawn_turn(
                &thread,
                &input.text,
                (input.goal.as_deref(), input.memory.as_deref()),
                input.effort.as_deref(),
                input.profile.as_ref(),
            )
            .await?;
        Ok(TurnAdmission::Started {
            run_id: Some(run_id),
        })
    }

    /// The engine's steer: its id is the message uuid written to the CLI,
    /// so `command_lifecycle`, echoes, the journal's `source_uuid` and every
    /// report about it carry the engine's id.
    async fn steer_message(
        &self,
        thread: &ThreadRecord,
        request: &SteerRequest,
    ) -> Result<SteerOutcome, RuntimeError> {
        if request.how == SteerHow::Steer {
            return Ok(self
                .steer_in_turn(&thread.thread_id, request.id.as_str(), &request.text)
                .await);
        }
        self.steer(thread, &request.text, request.how).await
    }

    /// The CLI says when it starts a steer (`started`, or an echo) and
    /// when it drops one: the engine shows "waiting to be read" until then.
    fn reports_steer_reads(&self) -> bool {
        self.bridge.is_some()
    }

    /// `priority: "next"` waits for the tool calls in flight, but the CLI's
    /// own queue may cut them short (t3code: Claude steering interrupts
    /// tools), so automatic input never steers it.
    fn steer_interrupts_tools(&self) -> bool {
        true
    }

    async fn steer(
        &self,
        thread: &ThreadRecord,
        text: &str,
        how: SteerHow,
    ) -> Result<SteerOutcome, RuntimeError> {
        // A steer never kills the turn; only an explicit Restart does.
        if how == SteerHow::Steer {
            let id = uuid::Uuid::new_v4().to_string();
            return Ok(self.steer_in_turn(&thread.thread_id, &id, text).await);
        }
        self.stop(&thread.thread_id).await;
        let profile = self
            .profiles
            .lock()
            .ok()
            .and_then(|p| p.get(&thread.thread_id).cloned()); // silent-ok: a poisoned map only follows a panic elsewhere
        self.spawn_turn(thread, text, (None, None), None, profile.as_ref())
            .await?;
        Ok(SteerOutcome::Restarted)
    }

    /// Also reached for idle threads (Stop, deletion): a pending or running
    /// idle compaction ends here too. The turn is stopped first, so a timer
    /// it armed while finishing is the one the preempt then cancels.
    async fn cancel(&self, thread: &ThreadRecord, _reason: &str) -> Result<(), RuntimeError> {
        self.stop(&thread.thread_id).await;
        self.compactor.preempt(&thread.thread_id).await;
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
            authenticated: login::signed_in(&self.config.login_file).await,
            current: false,
            models: MODELS.iter().map(|m| m.to_string()).collect(),
            efforts: EFFORTS.iter().map(|e| e.to_string()).collect(),
            model_efforts: Default::default(),
            brand: None,
            launch: None,
        }])
    }
}

/// Hand the CLI's read and drop reports to the engine, in order.
async fn report_reads(
    sink: &Weak<dyn TurnEventSink>,
    thread: &ThreadId,
    run: &str,
    steers: &SharedSteers,
) {
    let events = control::with_ledger(steers, |l| l.take_events());
    if events.is_empty() {
        return;
    }
    let Some(sink) = sink.upgrade() else {
        return;
    };
    for (id, state) in events {
        match SteerId::new(id) {
            Ok(id) => sink.steer_updated(thread, run, &id, state, now_ms()).await,
            Err(error) => tracing::debug!(%error, "claude cli: steer id not reportable"),
        }
    }
}

/// Where imported terminal sessions' folder records live, beside the
/// shared work dir (`<work dir>-imports/<session>.json`).
pub fn imports_dir(work_dir: &Path) -> PathBuf {
    let mut name = work_dir
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push("-imports");
    work_dir.with_file_name(name)
}

/// The `claude` binary: `PATH` first, then the installer's usual homes (a
/// desktop-launched backend often runs with a minimal `PATH`).
pub fn find_binary(home: &Path) -> Option<PathBuf> {
    crate::binaries::find(home, "claude")
}

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

/// `ZLO_CLAUDE_AUTOCOMPACT` (tokens, 100000 to 1000000): when Claude CLI
/// sessions compact. Anything else keeps the CLI's default.
pub fn autocompact_setting(raw: Option<&str>) -> Option<u64> {
    let tokens = raw?.trim().replace('_', "").parse::<u64>().ok()?; // silent-ok: a malformed value keeps the CLI default
    if AUTOCOMPACT_RANGE.contains(&tokens) {
        Some(tokens)
    } else {
        tracing::debug!(
            tokens,
            "claude cli: ZLO_CLAUDE_AUTOCOMPACT out of range; ignored"
        );
        None
    }
}

/// The CLI default login, plus `~/.claude-account2` as [`second_account`]
/// decides.
pub fn default_accounts(
    home: &Path,
    binary: &Path,
    usage: &crate::skill_listing::CliSkillUsage,
) -> Vec<ClaudeCliConfig> {
    let autocompact = autocompact_setting(std::env::var("ZLO_CLAUDE_AUTOCOMPACT").ok().as_deref());
    let work_dir = home.join(".ironclaw").join("claude-cli");
    let usage_dir = home.join(".ironclaw").join("usage");
    let skills_dir = home.join(".ironclaw").join("claude-skills");
    let listing = |id: &str| SkillListing {
        usage: usage.clone(),
        legacy_state_file: Some(skills_dir.join(format!("{id}.json"))),
    };
    let mut accounts = vec![ClaudeCliConfig {
        runtime_id: "claude-cli".into(),
        display_name: "Claude CLI".into(),
        binary: binary.to_path_buf(),
        config_dir: None,
        work_dir: work_dir.clone(),
        login_file: home.join(".claude.json"),
        projects_dir: home.join(".claude").join("projects"),
        usage_file: Some(usage_dir.join("claude-cli.json")),
        autocompact,
        artifacts: false,
        skills: listing("claude-cli"),
    }];
    if let Some(second) = second_account(home) {
        accounts.push(ClaudeCliConfig {
            runtime_id: "claude-cli-2".into(),
            display_name: "Claude CLI 2".into(),
            binary: binary.to_path_buf(),
            config_dir: Some(second.clone()),
            work_dir,
            login_file: second.join(".claude.json"),
            projects_dir: second.join("projects"),
            usage_file: Some(usage_dir.join("claude-cli-2.json")),
            autocompact,
            artifacts: false,
            skills: listing("claude-cli-2"),
        });
    }
    accounts
}

/// The second account's config dir, `~/.claude-account2`, when it gets a
/// runtime. Most people have one Claude login, so the second account is
/// opt-in: sign it in with
/// `CLAUDE_CONFIG_DIR=~/.claude-account2 claude /login` and restart. It is
/// used when a different account than the CLI default is signed in there,
/// or when it already has transcripts (`projects/`): threads, profiles and
/// queued messages may name `claude-cli-2`, and a missing runtime breaks
/// them. A folder older builds created by probing it, never used, is not
/// enough, so a fresh install has no second runtime.
pub fn second_account(home: &Path) -> Option<PathBuf> {
    let dir = home.join(SECOND_ACCOUNT_DIR);
    let different = login::account_at(&dir.join(".claude.json")).is_some_and(|second| {
        login::account_at(&home.join(".claude.json")).as_deref() != Some(second.as_str())
    });
    if different || has_entries(&dir.join("projects")) {
        Some(dir)
    } else {
        tracing::debug!("claude cli: no distinct or used second login; not registered");
        None
    }
}

/// Whether `dir` exists and holds anything.
fn has_entries(dir: &Path) -> bool {
    match std::fs::read_dir(dir) {
        Ok(mut entries) => entries.next().is_some(),
        Err(error) => {
            tracing::debug!(%error, "claude cli: no second-login transcripts");
            false
        }
    }
}

/// Where the optional second Claude login lives, under `$HOME`.
pub const SECOND_ACCOUNT_DIR: &str = ".claude-account2";

#[cfg(test)]
mod tests {
    use super::{SECOND_ACCOUNT_DIR, autocompact_setting, default_accounts};
    use crate::skill_listing::CliSkillUsage;

    fn login(path: &std::path::Path, uuid: Option<&str>) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body = match uuid {
            Some(uuid) => serde_json::json!({"oauthAccount": {"accountUuid": uuid}}),
            None => serde_json::json!({"numStartups": 1}),
        };
        std::fs::write(path, body.to_string()).unwrap();
    }

    fn runtimes(home: &std::path::Path) -> Vec<String> {
        default_accounts(home, &home.join("claude"), &CliSkillUsage::default())
            .into_iter()
            .map(|c| c.runtime_id)
            .collect()
    }

    /// The bug: a fresh install listed "Claude CLI 2" beside "Claude CLI",
    /// both Ready, both the same login. The second account runs only for a
    /// different signed-in login, or one that already has transcripts (its
    /// threads and profiles must keep resolving).
    #[test]
    fn the_second_account_runs_only_when_distinct_or_already_used() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let first = home.join(".claude.json");
        let dir = home.join(SECOND_ACCOUNT_DIR);
        let second = dir.join(".claude.json");
        login(&first, Some("acct-a"));
        assert_eq!(runtimes(home), ["claude-cli"], "no second login");

        login(&second, None);
        assert_eq!(
            runtimes(home),
            ["claude-cli"],
            "a login file that never signed in"
        );

        login(&second, Some(" acct-a "));
        assert_eq!(runtimes(home), ["claude-cli"], "the same account");

        login(&second, Some("acct-b"));
        assert_eq!(runtimes(home), ["claude-cli", "claude-cli-2"]);

        // Used before: kept even as a duplicate, or with its login gone.
        std::fs::remove_file(&second).unwrap();
        std::fs::create_dir_all(dir.join("projects").join("-work")).unwrap();
        assert_eq!(runtimes(home), ["claude-cli", "claude-cli-2"]);
    }

    #[test]
    fn autocompact_takes_only_what_the_cli_accepts() {
        assert_eq!(autocompact_setting(Some("300000")), Some(300_000));
        assert_eq!(autocompact_setting(Some(" 1_000_000 ")), Some(1_000_000));
        assert_eq!(autocompact_setting(Some("99999")), None);
        assert_eq!(autocompact_setting(Some("2000000")), None);
        assert_eq!(autocompact_setting(Some("auto")), None);
        assert_eq!(autocompact_setting(None), None);
    }
}
