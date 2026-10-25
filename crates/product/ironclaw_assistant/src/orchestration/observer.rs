//! Feeds IronClaw turn lifecycle into the orchestrator from the durable
//! process journal (cursor-tracked, so a restart replays what it missed),
//! including when a turn parks on an approval or sign-in the person answers
//! (the inbox's "Needs you").

use std::sync::Arc;

use async_trait::async_trait;
use ironclaw_host_api::turn::TurnRunId;
use ironclaw_orchestrator::engine::Engine;
use ironclaw_orchestrator::inbox::AwaitingKind;
use ironclaw_orchestrator::restart::MAX_INTENT_AGE_MS;
use ironclaw_orchestrator::runtime::{TurnEndStatus, TurnEventSink, TurnOutcome};
use ironclaw_processes::{
    ProcessJournalCommit, ProcessJournalCommitObserver, ProcessJournalKind, ProcessKind,
    ProcessLifecycleStatus, ProcessSuspensionKind,
};

pub struct OrchestratorProcessObserver {
    engine: Arc<Engine>,
}

impl OrchestratorProcessObserver {
    pub fn new(engine: Arc<Engine>) -> Self {
        Self { engine }
    }
}

fn millis(ts: &ironclaw_host_api::Timestamp) -> i64 {
    ts.timestamp_millis()
}

#[async_trait]
impl ProcessJournalCommitObserver for OrchestratorProcessObserver {
    fn process_observer_id(&self) -> &'static str {
        "orchestrator-turn-observer-v1"
    }

    async fn observe_process_commit(&self, commit: ProcessJournalCommit) -> Result<(), String> {
        let state = &commit.state;
        if state.process_kind != ProcessKind::AgentTurn {
            return Ok(());
        }
        let Some(thread_id) = state.scope.thread_id.clone() else {
            return Ok(());
        };
        let at = millis(&commit.occurred_at.unwrap_or(state.created_at));
        let run_id = Some(TurnRunId::from_uuid(state.process_id.as_uuid()).to_string());
        if state.status == ProcessLifecycleStatus::Suspended {
            let kind = match state.suspension.as_ref().map(|s| s.kind) {
                Some(ProcessSuspensionKind::Approval) => Some(AwaitingKind::Approval),
                Some(ProcessSuspensionKind::Authorization) => Some(AwaitingKind::Auth),
                _ => None,
            };
            if let Some(kind) = kind
                && let Err(error) = self
                    .engine
                    .store()
                    .set_awaiting(&thread_id, run_id.as_deref(), kind, at)
                    .await
            {
                return Err(format!("record awaiting input failed: {error}"));
            }
            return Ok(());
        }
        // Running again or finished: the gate is answered.
        let resumed = matches!(
            commit.kind,
            ProcessJournalKind::Claimed | ProcessJournalKind::Resumed
        );
        if (resumed || state.status.is_terminal())
            && let Err(error) = self.engine.store().clear_awaiting(&thread_id).await
        {
            return Err(format!("clear awaiting input failed: {error}"));
        }
        if matches!(
            commit.kind,
            ProcessJournalKind::Claimed | ProcessJournalKind::Resumed
        ) && state.status == ProcessLifecycleStatus::Running
        {
            self.engine.turn_started(&thread_id, run_id, at).await;
            return Ok(());
        }
        if !state.status.is_terminal() {
            return Ok(());
        }
        let error = state.failure.as_ref().map(|f| f.category().to_string());
        let outcome = match state.status {
            ProcessLifecycleStatus::Completed => TurnOutcome::completed(None),
            ProcessLifecycleStatus::Failed => TurnOutcome::ended(TurnEndStatus::Failed, error),
            ProcessLifecycleStatus::RecoveryRequired => {
                // The lease expired: the backend died under the turn. Old
                // history replayed on first subscription is not a live cut.
                let fresh = ironclaw_orchestrator::model::now_ms() - at <= MAX_INTENT_AGE_MS;
                TurnOutcome {
                    cut_by_restart: fresh,
                    ..TurnOutcome::ended(TurnEndStatus::Interrupted, error)
                }
            }
            _ => TurnOutcome::ended(TurnEndStatus::Cancelled, error),
        };
        self.engine
            .turn_finished(&thread_id, run_id, outcome, at)
            .await;
        Ok(())
    }
}
