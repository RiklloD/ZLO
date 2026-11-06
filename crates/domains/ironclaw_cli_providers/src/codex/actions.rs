//! Codex thread actions beyond turns, each on a short-lived app-server:
//!
//! - **Review** (`review/start`, inline delivery): runs as a turn of the
//!   thread (see `TurnKind::Review`); this file only shapes its request.
//! - **Revert** is not offered yet: pairing Codex's `thread/revert` with
//!   the app's file restore needs a crash-safe two-phase record (pending →
//!   confirmed, reconciled on reopen) that this runtime does not keep.
//! - **Compact** (`thread/compact/start`), now or when the thread idles
//!   (`compact.rs`). One visible "Context compacted." row marks it.

use std::path::Path;
use std::time::Duration;

use ironclaw_orchestrator::model::ThreadRecord;
use ironclaw_orchestrator::runtime::{HistoryMessage, HistoryRole, RuntimeError};
use ironclaw_product_contracts::orchestration::{
    ReviewTarget, ThreadActionOutcome, ThreadActionRefusal,
};
use serde_json::{Value, json};
use tokio::sync::watch;

use super::admission::Claim;
use super::session;
use super::{
    COMMAND_TIMEOUT, CodexConfig, CodexRuntime, connect, open_session, saved_thread,
    session_policy, spawn_app_server,
};
use crate::journal;
use crate::rpc::{self, Inbound, RpcClient};

/// The visible journal row a compaction leaves.
pub(super) const COMPACTED_NOTE: &str = "Context compacted.";
/// A compaction that has not finished by then is abandoned.
pub(super) const COMPACT_TIMEOUT: Duration = Duration::from_secs(240);

/// `review/start` params; Codex's target names are camelCase.
pub(super) fn review_params(codex_thread: &str, target: &ReviewTarget) -> Value {
    let target = match target {
        ReviewTarget::UncommittedChanges => json!({"type": "uncommittedChanges"}),
        ReviewTarget::BaseBranch { branch } => json!({"type": "baseBranch", "branch": branch}),
        ReviewTarget::Commit { sha, title } => {
            json!({"type": "commit", "sha": sha, "title": title})
        }
        ReviewTarget::Custom { instructions } => {
            json!({"type": "custom", "instructions": instructions})
        }
    };
    json!({"threadId": codex_thread, "target": target, "delivery": "inline"})
}

/// The user row a review turn journals.
pub(super) fn review_label(target: &ReviewTarget) -> String {
    match target {
        ReviewTarget::UncommittedChanges => "Review my uncommitted changes.".to_string(),
        ReviewTarget::BaseBranch { branch } => {
            format!("Review this branch's changes against {branch}.")
        }
        ReviewTarget::Commit { sha, title } => {
            let short: String = sha.chars().take(10).collect();
            match title.as_deref().filter(|t| !t.trim().is_empty()) {
                Some(title) => format!("Review commit {short} ({title})."),
                None => format!("Review commit {short}."),
            }
        }
        ReviewTarget::Custom { instructions } => format!("Review: {instructions}"),
    }
}

fn marker(text: &str, hidden: bool) -> HistoryMessage {
    HistoryMessage {
        hidden,
        ..journal::message(HistoryRole::System, text.to_string(), Vec::new())
    }
}

/// What a compaction run reported.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Compacted {
    Done,
    Cancelled,
    Failed(String),
}

/// One `thread/compact/start` on `session`'s Codex thread, waiting until it
/// is reported done, `cancelled` flips, or [`COMPACT_TIMEOUT`] passes. The
/// app-server's process group is gone when this returns.
pub(super) async fn run_compaction(
    config: &CodexConfig,
    session: &str,
    model: Option<&str>,
    bridged: bool,
    mut cancelled: watch::Receiver<bool>,
) -> Compacted {
    // An imported terminal session compacts in its own folder, or not at all.
    let cwd = match crate::workspace::resolve(&super::cwd_file_in(&config.state_dir, session)).await {
        Ok(Some(dir)) => dir,
        Ok(None) => config.work_dir.clone(),
        Err(_) => return Compacted::Failed("workspace_unavailable".into()),
    };
    let (mut child, pgid) = match spawn_app_server(config) {
        Ok(spawned) => spawned,
        Err(error) => return Compacted::Failed(error.code().into()),
    };
    let result = async {
        let (client, mut inbound) = match connect(&mut child, pgid) {
            Ok(io) => io,
            Err(error) => return Compacted::Failed(error.code().into()),
        };
        let deadline = tokio::time::sleep(COMPACT_TIMEOUT);
        tokio::pin!(deadline);
        let opened = tokio::select! {
            opened = open_session(&client, config, session, &cwd, model, session_policy(bridged)) => opened,
            _ = cancelled.wait_for(|c| *c) => return Compacted::Cancelled,
        };
        let codex_thread = match opened {
            Ok((id, _)) => id,
            Err(error) => return Compacted::Failed(error.label()),
        };
        let params = json!({"threadId": codex_thread});
        if let Err(error) = client
            .request("thread/compact/start", params, COMMAND_TIMEOUT)
            .await
        {
            return Compacted::Failed(error.label());
        }
        loop {
            let message = tokio::select! {
                message = inbound.recv() => message,
                _ = cancelled.wait_for(|c| *c) => return Compacted::Cancelled,
                _ = &mut deadline => return Compacted::Failed("timed out".into()),
            };
            let Some(message) = message else {
                return Compacted::Failed("app-server exited".into());
            };
            match message {
                Inbound::Notification { method, params } => {
                    if let Some(done) = compaction_done(&method, &params, &codex_thread) {
                        return done;
                    }
                }
                Inbound::Request { id, .. } => decline(&client, &id),
            }
        }
    }
    .await;
    rpc::end_group(&mut child, pgid, rpc::KILL_GRACE).await;
    result
}

/// Whether a notification ends the compaction of `codex_thread`.
fn compaction_done(method: &str, params: &Value, codex_thread: &str) -> Option<Compacted> {
    if params
        .get("threadId")
        .and_then(Value::as_str)
        .is_some_and(|t| t != codex_thread)
    {
        return None;
    }
    match method {
        "thread/compacted" => Some(Compacted::Done),
        "item/completed" if params["item"]["type"] == "contextCompaction" => Some(Compacted::Done),
        "turn/completed" => Some(match params["turn"]["status"].as_str() {
            Some("completed") => Compacted::Done,
            Some("interrupted") => Compacted::Cancelled,
            _ => Compacted::Failed("compaction turn failed".into()),
        }),
        _ => None,
    }
}

/// Nobody answers questions during a thread action.
fn decline(client: &RpcClient, id: &Value) {
    if let Err(error) = client.respond_error(id, -32601, "not supported by this client") {
        tracing::debug!(error = %error.label(), "codex: reply not sent");
    }
}

impl CodexRuntime {
    fn session_of(thread: &ThreadRecord) -> Result<&str, RuntimeError> {
        thread
            .native_session_id
            .as_deref()
            .ok_or_else(|| RuntimeError::Rejected("thread has no codex session".into()))
    }

    /// The root this thread's session works in: its latest turn's, else the
    /// folder its next turn would run in.
    async fn session_root(&self, thread: &ThreadRecord) -> Result<String, RuntimeError> {
        let session = Self::session_of(thread)?;
        let log = session::load(&self.config.state_dir, session).await?;
        Ok(match log.root() {
            Some(root) => root.to_string(),
            None => session::workspace_root(&self.config.work_dir).await,
        })
    }

    /// `Some(refusal)` unless `repo` is the session's workspace root.
    pub(super) async fn workspace_refusal(
        &self,
        thread: &ThreadRecord,
        repo: &str,
    ) -> Result<Option<ThreadActionOutcome>, RuntimeError> {
        let root = self.session_root(thread).await?;
        if session::canonical(Path::new(repo)) == root {
            return Ok(None);
        }
        tracing::debug!(thread = %thread.thread_id, "codex: action refused, other workspace");
        Ok(Some(ThreadActionOutcome::Refused {
            refusal: ThreadActionRefusal::WorkspaceMismatch { runtime_root: root },
        }))
    }

    /// Compact now, holding the session; a new turn preempts it like an
    /// idle compaction.
    pub(super) async fn compact_now(
        &self,
        thread: &ThreadRecord,
        claim: Claim,
    ) -> Result<ThreadActionOutcome, RuntimeError> {
        let session = Self::session_of(thread)?.to_string();
        if saved_thread(&self.config, &session).await.is_none() {
            return Ok(ThreadActionOutcome::Done {
                note: "Nothing to compact yet.".into(),
            });
        }
        let (id, cancelled, done) = self.compactor.begin(&thread.thread_id);
        let result = run_compaction(
            &self.config,
            &session,
            thread.model.as_deref(),
            self.bridge.is_some(),
            cancelled,
        )
        .await;
        // The process is gone: free the session, then wake a preempting turn.
        drop(claim);
        self.compactor.finish(&thread.thread_id, id);
        let _ = done.send(true); // silent-ok: nobody waits on this slot
        match result {
            Compacted::Done => {
                note_compacted(&self.config, &session).await;
                Ok(ThreadActionOutcome::Done {
                    note: "Compacted the thread's context.".into(),
                })
            }
            Compacted::Cancelled => Ok(ThreadActionOutcome::Busy),
            Compacted::Failed(reason) => Err(RuntimeError::Unavailable(format!(
                "codex compaction failed ({reason})"
            ))),
        }
    }
}

pub(super) async fn note_compacted(config: &CodexConfig, session: &str) {
    journal::append(
        &config.transcript(session),
        &[marker(COMPACTED_NOTE, false)],
    )
    .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_targets_use_codex_names() {
        let p = review_params(
            "cx",
            &ReviewTarget::BaseBranch {
                branch: "main".into(),
            },
        );
        assert_eq!(
            p,
            json!({"threadId": "cx", "target": {"type": "baseBranch", "branch": "main"}, "delivery": "inline"})
        );
        assert_eq!(
            review_params("cx", &ReviewTarget::UncommittedChanges)["target"]["type"],
            "uncommittedChanges"
        );
    }

    #[test]
    fn compaction_ends_on_its_item_or_turn() {
        let item = json!({"threadId": "cx", "item": {"type": "contextCompaction", "id": "c"}});
        assert_eq!(
            compaction_done("item/completed", &item, "cx"),
            Some(Compacted::Done)
        );
        assert_eq!(compaction_done("item/completed", &item, "other"), None);
        let failed = json!({"threadId": "cx", "turn": {"id": "t", "status": "failed"}});
        assert!(matches!(
            compaction_done("turn/completed", &failed, "cx"),
            Some(Compacted::Failed(_))
        ));
    }
}
