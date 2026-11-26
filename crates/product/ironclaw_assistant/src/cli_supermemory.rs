//! Supermemory for CLI-agent threads (Claude Code, Codex, Antigravity,
//! Muse), as IronClaw's own loop has it: recall before each of the person's
//! turns ([`TurnMemory::recall`], rendered by
//! [`ironclaw_supermemory::cli_recall_note`]), capture of each completed
//! exchange, and the `supermemory_search` / `supermemory_write` tools on the
//! bridge's MCP route (`memory_bridge.rs`).
//!
//! Everything runs as the thread's owner and through the thread's own
//! binding (its profile's container, else the default), so a thread never
//! reads or writes another profile's container than the native loop would.
//! With no key, or no container for the thread, all of it is off and the
//! turn runs as before. IronClaw's own loop is skipped here: its turns
//! recall and capture inside the loop.

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_orchestrator::model::ThreadRecord;
use ironclaw_orchestrator::turn_memory::TurnMemory;
use ironclaw_supermemory::{Supermemory, SupermemoryCaller};

use crate::orchestration::IRONCLAW_RUNTIME_ID;

/// The thread's owner, scoped to the thread (its binding resolves through
/// the thread's profile).
pub(crate) fn thread_caller(thread: &ThreadRecord) -> SupermemoryCaller {
    SupermemoryCaller {
        tenant_id: thread.scope.tenant_id.clone(),
        user_id: thread.scope.user_id.clone(),
        agent_id: thread.scope.agent_id.clone(),
        project_id: thread.scope.project_id.clone(),
        thread_id: Some(thread.thread_id.clone()),
    }
}

pub(crate) struct CliSupermemory {
    supermemory: Arc<Supermemory>,
}

impl CliSupermemory {
    pub(crate) fn new(supermemory: Arc<Supermemory>) -> Self {
        Self { supermemory }
    }
}

fn native(thread: &ThreadRecord) -> bool {
    thread.runtime_id.as_str() == IRONCLAW_RUNTIME_ID
}

#[async_trait]
impl TurnMemory for CliSupermemory {
    async fn recall(&self, thread: &ThreadRecord, text: &str) -> Option<String> {
        if native(thread) {
            return None;
        }
        match self.supermemory.recall(&thread_caller(thread), text).await {
            Ok(Some(recall)) => Some(ironclaw_supermemory::cli_recall_note(&recall)),
            Ok(None) => None,
            Err(error) => {
                tracing::debug!(
                    kind = error.kind(),
                    "cli supermemory recall failed; turn runs without it"
                );
                None // silent-ok: recall is additive context, the turn still runs
            }
        }
    }

    async fn capture(&self, thread: &ThreadRecord, asked: &str, answer: &str) {
        if native(thread) {
            return;
        }
        if let Err(error) = self
            .supermemory
            .capture(
                &thread_caller(thread),
                thread.thread_id.as_str(),
                asked,
                answer,
            )
            .await
        {
            tracing::debug!(kind = error.kind(), "cli supermemory capture failed");
        }
    }
}
