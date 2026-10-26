//! Idle compaction: a Claude CLI thread that sits idle for 4.5 minutes sends
//! `/compact` while Anthropic's 5-minute prompt cache is still warm, so the
//! compaction reads the context at the cached rate and the person's next
//! message starts from a short summary instead of the full history.
//!
//! The timer lives here, in the driver, because only the driver sees every
//! turn of its threads start and end, whatever the app shows. Rules:
//! - armed when a turn completes with at least [`MIN_CONTEXT_TOKENS`] of
//!   context (smaller contexts cost less to resend than to summarize);
//! - every turn start (a user message, a queue drain, a steer restart),
//!   Stop and thread deletion cancel the timer and kill a compaction already
//!   running, and wait until its process has exited, before anything else
//!   touches the session. Approvals only exist inside a turn, so none can be
//!   pending while the timer runs;
//! - fires at most once per idle period: the compaction run never re-arms;
//! - fires only when `Settings::idle_compact_claude` is on (read through the
//!   sink at fire time, so the switch applies to threads already idle).
//!
//! The compaction runs with the turn's exact command shape (model, effort,
//! system note, plugin, tools, MCP server) so it hits the cached prefix. It
//! is not reported to the engine as a turn; the transcript reader hides its
//! rows and shows one "Compacted" marker. Each run appends one line to
//! `<usage dir>/compactions.jsonl` for the Usage page.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use claude_codes::ClaudeOutput;
use ironclaw_host_api::ids::ThreadId;
use ironclaw_orchestrator::model::now_ms;
use ironclaw_orchestrator::runtime::{TurnEndStatus, TurnOutcome, TurnProfile};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::watch;

use super::{ClaudeCliRuntime, TurnPlan, control};
use crate::bridge::CliBridge;

/// Idle time before compacting: half a minute inside the 5-minute cache TTL.
pub(super) const IDLE_DELAY: Duration = Duration::from_secs(270);
/// Below this many context tokens a compaction does not pay for itself.
pub(super) const MIN_CONTEXT_TOKENS: u64 = 20_000;
/// A compaction that has not finished by then is killed.
const COMPACT_TIMEOUT: Duration = Duration::from_secs(240);
/// What the compaction run sends; also how the transcript reader spots it.
pub(super) const COMPACT_PROMPT: &str = "/compact";
const LOG_FILE: &str = "compactions.jsonl";

/// Whether a finished turn arms the idle timer.
pub(super) fn should_arm(outcome: &TurnOutcome, context_used: Option<u64>) -> bool {
    outcome.status == TurnEndStatus::Completed
        && context_used.is_some_and(|tokens| tokens >= MIN_CONTEXT_TOKENS)
}

/// One pending timer (or running compaction) per thread of one account.
#[derive(Default)]
pub(super) struct IdleCompactor {
    slots: Mutex<HashMap<ThreadId, Slot>>,
    next_id: AtomicU64,
}

/// The task owns its process: cancelling asks it to kill and reap the
/// child, and `done` flips only once that child has exited.
struct Slot {
    id: u64,
    cancel: watch::Sender<bool>,
    done: watch::Receiver<bool>,
}

impl IdleCompactor {
    /// Cancel the thread's timer and kill its running compaction, returning
    /// once the compaction process has exited. Called before every turn of
    /// the thread starts, on Stop and on thread deletion.
    pub(super) async fn preempt(&self, thread: &ThreadId) {
        // silent-ok: a poisoned map only follows a panic elsewhere; nothing to cancel then
        let slot = self.slots.lock().ok().and_then(|mut s| s.remove(thread));
        let Some(mut slot) = slot else {
            return;
        };
        let _ = slot.cancel.send(true); // silent-ok: the task already ended
        let _ = slot.done.wait_for(|d| *d).await; // silent-ok: sender dropped means the task ended
        tracing::debug!(thread = %thread, "idle compaction: preempted");
    }

    fn finish(&self, thread: &ThreadId, id: u64) {
        if let Ok(mut slots) = self.slots.lock()
            && slots.get(thread).is_some_and(|s| s.id == id)
        {
            slots.remove(thread);
        }
    }
}

/// What a finished turn needs to compact its session later: the runtime
/// (for the command shape and MCP grant) and the turn's own plan.
pub(super) struct IdleArm {
    runtime: Arc<ClaudeCliRuntime>,
    thread_id: ThreadId,
    session_id: String,
    model: Option<String>,
    dir: PathBuf,
    goal: Option<String>,
    effort: Option<String>,
    profile: Option<TurnProfile>,
}

impl IdleArm {
    /// Captured in `spawn_turn`, before the turn's task starts.
    pub(super) fn new(
        rt: &ClaudeCliRuntime,
        thread_id: &ThreadId,
        session_id: &str,
        model: Option<&str>,
        plan: &TurnPlan<'_>,
    ) -> Self {
        // A handle on the same account: shared live-turn map, sink, bridge
        // and compactor; the profile cache is not needed for compaction.
        let runtime = Arc::new(ClaudeCliRuntime {
            id: rt.id.clone(),
            config: rt.config.clone(),
            sink: rt.sink.clone(),
            bridge: rt.bridge.clone(),
            active: Arc::clone(&rt.active),
            profiles: Mutex::new(HashMap::new()),
            compactor: Arc::clone(&rt.compactor),
        });
        Self {
            runtime,
            thread_id: thread_id.clone(),
            session_id: session_id.to_string(),
            model: model.map(str::to_string),
            dir: plan.dir.clone(),
            goal: plan.goal.map(str::to_string),
            effort: plan.effort.map(str::to_string),
            profile: plan.profile.cloned(),
        }
    }

    /// Called once the turn is reported finished and no longer live.
    pub(super) fn arm(self, outcome: &TurnOutcome, context_used: Option<u64>) {
        if !should_arm(outcome, context_used) {
            return;
        }
        let compactor = Arc::clone(&self.runtime.compactor);
        let id = compactor.next_id.fetch_add(1, Ordering::SeqCst);
        let thread = self.thread_id.clone();
        let (cancel, mut cancelled) = watch::channel(false);
        let (done_tx, done) = watch::channel(false);
        {
            let compactor = Arc::clone(&compactor);
            let thread = thread.clone();
            tokio::spawn(async move {
                let idle = tokio::select! {
                    _ = tokio::time::sleep(IDLE_DELAY) => true,
                    _ = cancelled.wait_for(|c| *c) => false,
                };
                if idle {
                    self.fire(cancelled).await;
                }
                compactor.finish(&thread, id);
                let _ = done_tx.send(true); // silent-ok: nobody waits on this slot
            });
        }
        let old = compactor
            .slots
            .lock()
            .ok()
            .and_then(|mut slots| slots.insert(thread, Slot { id, cancel, done }));
        // A turn start preempts first, so a replaced slot is only a timer.
        if let Some(old) = old {
            let _ = old.cancel.send(true); // silent-ok: the task already ended
        }
    }

    async fn fire(self, cancelled: watch::Receiver<bool>) {
        if self.runtime.is_active(&self.thread_id) {
            return;
        }
        let Some(sink) = self.runtime.sink.upgrade() else {
            return;
        };
        if !sink.idle_compact_enabled().await {
            return;
        }
        let run = self.compact(cancelled).await;
        // A run cut before its process started cost nothing: no row.
        if !run.started {
            return;
        }
        if let Some(dir) = self
            .runtime
            .config
            .usage_file
            .as_deref()
            .and_then(Path::parent)
        {
            append_record(&dir.join(LOG_FILE), &self.record(&run)).await;
        }
    }

    /// One `/compact` process on the thread's session, folded to its figures.
    /// Nothing here is dropped mid-await: cancellation and the timeout are
    /// observed between steps, so the MCP grant is always cleaned up and the
    /// child is always killed and reaped.
    async fn compact(&self, mut cancelled: watch::Receiver<bool>) -> CompactRun {
        let rt = &self.runtime;
        let run_id = uuid::Uuid::new_v4().to_string();
        let mcp = rt
            .mcp_session(&self.thread_id, &run_id)
            .await
            .map(|m| McpGuard {
                bridge: rt.bridge.clone(),
                token: m.token,
                file: m.config_file,
            });
        if *cancelled.borrow() {
            return CompactRun::failed("cancelled");
        }
        let plan = TurnPlan {
            dir: self.dir.clone(),
            goal: self.goal.as_deref(),
            effort: self.effort.as_deref(),
            profile: self.profile.as_ref(),
        };
        let spawned = rt
            .command(
                &self.session_id,
                self.model.as_deref(),
                mcp.as_ref().map(|m| m.file.as_path()),
                &plan,
            )
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                tracing::debug!(%error, "idle compaction: spawn failed");
                return CompactRun::failed("spawn failed");
            }
        };
        let pgid = child.id().and_then(|id| i32::try_from(id).ok());
        let kill = |pgid: Option<i32>| {
            if let Some(group) = pgid {
                let _ = killpg(Pid::from_raw(group), Signal::SIGKILL); // silent-ok: the group may already be gone
            }
        };
        let mut run = CompactRun {
            started: true,
            ..CompactRun::default()
        };
        let (Some(mut stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            kill(pgid);
            let _ = child.wait().await; // silent-ok: reaping a child we killed
            run.error = Some("stdio unavailable".into());
            return run;
        };
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "claude_cli", "compact stderr: {line}");
            }
        });
        // Prompt, then EOF: the CLI compacts and exits.
        let prompt = control::prompt_message(&run_id, COMPACT_PROMPT);
        let written = async {
            stdin.write_all(format!("{prompt}\n").as_bytes()).await?;
            stdin.flush().await?;
            stdin.shutdown().await
        }
        .await;
        if let Err(error) = written {
            tracing::debug!(%error, "idle compaction: stdin write failed");
            kill(pgid);
            let _ = child.wait().await; // silent-ok: reaping a child we killed
            run.error = Some("stdin closed".into());
            return run;
        }
        let deadline = tokio::time::sleep(COMPACT_TIMEOUT);
        tokio::pin!(deadline);
        let mut lines = BufReader::new(stdout).lines();
        let cut = loop {
            tokio::select! {
                next = lines.next_line() => match next {
                    Ok(Some(line)) => run.push_line(&line),
                    _ => break None,
                },
                _ = cancelled.wait_for(|c| *c) => break Some("cancelled"),
                _ = &mut deadline => break Some("timed out"),
            }
        };
        // Stdout closed: the child should exit now, but a CLI that lingers is
        // still bound by the cancel and the deadline.
        let cut = match cut {
            Some(reason) => Some(reason),
            None => tokio::select! {
                _ = child.wait() => None,
                _ = cancelled.wait_for(|c| *c) => Some("cancelled"),
                _ = &mut deadline => Some("timed out"),
            },
        };
        if let Some(reason) = cut {
            kill(pgid);
            run.error = Some(reason.into());
        }
        // Reaped before returning: `preempt` waits on this.
        let _ = child.wait().await; // silent-ok: the folded result already says how it went
        if run.error.is_none() && !run.done {
            run.error = Some("no result".into());
        }
        run
    }

    /// The Usage page row (`ironclaw_product_contracts::usage::UsageCompaction`).
    fn record(&self, run: &CompactRun) -> serde_json::Value {
        serde_json::json!({
            "at": now_ms(),
            "provider": self.runtime.config.runtime_id,
            "threadId": self.thread_id.as_str(),
            "model": run.model.clone().or_else(|| self.model.clone()),
            "tokensBefore": run.tokens_before,
            "tokensAfter": run.tokens_after,
            "costUsd": run.cost_usd,
            "inputTokens": run.input_tokens,
            "cacheReadTokens": run.cache_read_tokens,
            "cacheCreationTokens": run.cache_creation_tokens,
            "outputTokens": run.output_tokens,
            "ok": run.error.is_none(),
            "error": run.error,
        })
    }
}

/// What one compaction run reported on stdout.
#[derive(Debug, Default)]
struct CompactRun {
    /// A process was spawned, so tokens may have been spent.
    started: bool,
    done: bool,
    model: Option<String>,
    tokens_before: Option<u64>,
    tokens_after: Option<u64>,
    cost_usd: f64,
    input_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    output_tokens: u64,
    error: Option<String>,
}

impl CompactRun {
    fn failed(reason: &str) -> Self {
        Self {
            error: Some(reason.to_string()),
            ..Self::default()
        }
    }

    fn push_line(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        // silent-ok: frames the wire types do not know carry nothing we count
        let Ok(frame) = ClaudeOutput::parse_json_tolerant(line) else {
            return;
        };
        match frame {
            ClaudeOutput::System(sys) => {
                if let Some(boundary) = sys.as_compact_boundary() {
                    self.tokens_before = Some(boundary.compact_metadata.pre_tokens);
                    self.tokens_after = boundary.compact_metadata.post_tokens;
                }
            }
            ClaudeOutput::Assistant(msg) => self.model = Some(msg.message.model.clone()),
            ClaudeOutput::Result(r) => {
                self.done = true;
                self.cost_usd = r.total_cost_usd;
                if let Some(u) = &r.usage {
                    self.input_tokens = u64::from(u.input_tokens);
                    self.cache_read_tokens = u64::from(u.cache_read_input_tokens);
                    self.cache_creation_tokens = u64::from(u.cache_creation_input_tokens);
                    self.output_tokens = u64::from(u.output_tokens);
                }
                if r.is_error {
                    // Raw CLI text can carry paths; keep a code.
                    self.error = Some("claude cli error".into());
                }
            }
            _ => {}
        }
    }
}

/// Revokes the run's MCP token and removes its config file even when a new
/// turn aborts the compaction mid-run.
struct McpGuard {
    // arch-exempt: optional_arc, mirrors ClaudeCliRuntime::bridge (tools-off hosts), plan #v3-cli-bridge
    bridge: Option<Arc<CliBridge>>,
    token: String,
    file: PathBuf,
}

impl Drop for McpGuard {
    fn drop(&mut self) {
        if let Some(bridge) = &self.bridge {
            bridge.revoke_token(&self.token);
        }
        let _ = std::fs::remove_file(&self.file); // silent-ok: temp file cleanup
    }
}

/// Append one JSON line; losing one only drops a Usage page row. Appends
/// are serialized so two threads' rows never interleave.
async fn append_record(path: &Path, record: &serde_json::Value) {
    static APPEND: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _turn = APPEND.lock().await;
    let written = async {
        if let Some(dir) = path.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .await?;
        file.write_all(format!("{record}\n").as_bytes()).await?;
        file.flush().await
    }
    .await;
    if let Err(error) = written {
        tracing::debug!(%error, "idle compaction: usage record not saved");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_completed_turns_with_a_large_context_arm_the_timer() {
        let done = TurnOutcome::completed(None);
        assert!(should_arm(&done, Some(MIN_CONTEXT_TOKENS)));
        assert!(should_arm(&done, Some(150_000)));
        // Tiny or unknown context: resending is cheaper than summarizing.
        assert!(!should_arm(&done, Some(MIN_CONTEXT_TOKENS - 1)));
        assert!(!should_arm(&done, None));
        // A failed, stopped or cut turn is followed by a retry, not idling.
        for status in [
            TurnEndStatus::Failed,
            TurnEndStatus::Cancelled,
            TurnEndStatus::Interrupted,
        ] {
            assert!(!should_arm(
                &TurnOutcome::ended(status, None),
                Some(150_000)
            ));
        }
    }
}
