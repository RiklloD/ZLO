//! Compacting an idle Muse session now (`ThreadAction::Compact`): a host
//! resumes the session, sends `session/compact`, and waits for the
//! compaction item to settle. The host holds the thread's turn slot while it
//! runs, so a turn waits and Stop ends it like a turn.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use ironclaw_orchestrator::model::ThreadRecord;
use ironclaw_orchestrator::runtime::RuntimeError;
use ironclaw_product_contracts::orchestration::ThreadActionOutcome;
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};

use super::{
    ActiveTurn, COMMAND_TIMEOUT, MuseRuntime, PERMISSION_PROFILE, close_host, command_id, connect,
    handshake,
};
use crate::rpc::{Inbound, RpcClient};

/// How long a summary may take before the host is given up on.
const COMPACT_TIMEOUT: Duration = Duration::from_secs(300);

/// How a compaction ended.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Compacted {
    Done,
    /// Muse had nothing to compact.
    Noop,
    Failed(String),
}

impl MuseRuntime {
    pub(super) async fn compact(
        &self,
        thread: &ThreadRecord,
    ) -> Result<ThreadActionOutcome, RuntimeError> {
        // Claimed before any await: a turn or another compaction admitted
        // meanwhile gets `Busy`, and a dropped future frees the slot.
        let Some(claim) = self.claim(&thread.thread_id) else {
            return Ok(ThreadActionOutcome::Busy);
        };
        let session = thread
            .native_session_id
            .clone()
            .ok_or_else(|| RuntimeError::Rejected("thread has no muse session".into()))?;
        let started = tokio::fs::try_exists(self.config.transcript(&session))
            .await
            .unwrap_or(false); // silent-ok: an unreadable marker reads as a fresh session, which has nothing to compact
        if !started {
            return Ok(ThreadActionOutcome::Done {
                note: "Nothing to compact yet.".into(),
            });
        }
        let (mut child, pgid) = self.spawn_host(false)?;
        let mut group = crate::slot::GroupGuard::new(pgid, "muse: dropped host group");
        let (client, inbound) = connect(&mut child, pgid)?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let (done_tx, done_rx) = watch::channel(false);
        claim.fill(ActiveTurn {
            pgid,
            cancelled: Arc::clone(&cancelled),
            live: Arc::default(),
            done: done_rx,
        });
        let result = tokio::time::timeout(COMPACT_TIMEOUT, run(&client, inbound, &session))
            .await
            .unwrap_or_else(|_| Compacted::Failed("timed out".into()));
        drop(client);
        close_host(&mut child, pgid).await;
        group.disarm();
        drop(claim);
        #[expect(clippy::let_underscore_must_use, reason = "nobody waits on this slot")]
        let _ = done_tx.send(true);
        if cancelled.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(ThreadActionOutcome::Busy);
        }
        match result {
            Compacted::Done => Ok(ThreadActionOutcome::Done {
                note: "Compacted the thread's context.".into(),
            }),
            Compacted::Noop => Ok(ThreadActionOutcome::Done {
                note: "Nothing to compact yet.".into(),
            }),
            Compacted::Failed(reason) => Err(RuntimeError::Unavailable(format!(
                "muse compaction failed ({reason})"
            ))),
        }
    }
}

async fn run(
    client: &RpcClient,
    mut inbound: mpsc::UnboundedReceiver<Inbound>,
    session: &str,
) -> Compacted {
    let opened = async {
        handshake(client, false).await?;
        client
            .request(
                "session/resume",
                json!({"sessionId": session, "commandId": command_id(), "excludeItems": true,
                       "permissionProfile": PERMISSION_PROFILE}),
                COMMAND_TIMEOUT,
            )
            .await?;
        client
            .request(
                "session/compact",
                json!({"sessionId": session, "commandId": command_id()}),
                COMMAND_TIMEOUT,
            )
            .await
    }
    .await;
    let answer = match opened {
        Ok(answer) => answer,
        Err(error) => return Compacted::Failed(error.label()),
    };
    match answer.get("status").and_then(Value::as_str) {
        Some("accepted") => {}
        Some("noop") => return Compacted::Noop,
        other => return Compacted::Failed(format!("status {}", other.unwrap_or("missing"))),
    }
    while let Some(message) = inbound.recv().await {
        match message {
            Inbound::Notification { method, params } => {
                if let Some(end) = settled(&method, &params, session) {
                    return end;
                }
            }
            Inbound::Request { id, .. } => {
                if let Err(error) =
                    client.respond_error(&id, -32601, "method not supported by this client")
                {
                    tracing::debug!(error = %error.label(), "muse: reply not sent");
                }
            }
        }
    }
    Compacted::Failed("host exited".into())
}

/// The compaction item's end, once it settles.
pub(super) fn settled(method: &str, params: &Value, session: &str) -> Option<Compacted> {
    if !matches!(method, "item/updated" | "item/completed")
        || params.get("sessionId").and_then(Value::as_str) != Some(session)
    {
        return None;
    }
    let item = &params["item"];
    if item.get("kind").and_then(Value::as_str) != Some("compaction") {
        return None;
    }
    let outcome = item.get("outcome").and_then(Value::as_str);
    match item.get("status").and_then(Value::as_str) {
        Some("inProgress") | None => None,
        Some("completed") if outcome.is_none_or(|o| o == "compacted") => Some(Compacted::Done),
        Some("completed") => Some(Compacted::Noop),
        Some(other) => Some(Compacted::Failed(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_this_sessions_settled_compaction_item_ends_it() {
        let ev = |sid: &str, kind: &str, status: &str, outcome: Option<&str>| json!({"sessionId": sid, "item": {"itemId": "c", "kind": kind, "status": status, "outcome": outcome}});
        assert_eq!(
            settled(
                "item/completed",
                &ev("s", "compaction", "inProgress", None),
                "s"
            ),
            None
        );
        assert_eq!(
            settled(
                "item/completed",
                &ev("x", "compaction", "completed", None),
                "s"
            ),
            None
        );
        assert_eq!(
            settled(
                "item/completed",
                &ev("s", "agentMessage", "completed", None),
                "s"
            ),
            None
        );
        assert_eq!(
            settled(
                "item/completed",
                &ev("s", "compaction", "completed", Some("compacted")),
                "s"
            ),
            Some(Compacted::Done)
        );
        assert_eq!(
            settled(
                "item/completed",
                &ev("s", "compaction", "completed", Some("skipped")),
                "s"
            ),
            Some(Compacted::Noop)
        );
        assert_eq!(
            settled("item/updated", &ev("s", "compaction", "failed", None), "s"),
            Some(Compacted::Failed("failed".into()))
        );
    }
}
