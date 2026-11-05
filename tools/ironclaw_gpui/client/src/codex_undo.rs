//! Undo of a Codex turn as one coordinated operation, GPUI-free so its
//! decisions are unit-tested.
//!
//! The order is fixed: Codex reverts the conversation first (it checks the
//! repo and the turn the checkpoint was taken for, and refuses before
//! changing anything), and only a confirmed revert lets the app restore the
//! files from its checkpoint. So:
//! - a refusal, a busy session or a failed revert leaves the files alone;
//! - a failed restore after a confirmed revert is reported as such, with
//!   the checkpoint kept so the restore can be retried.

use crate::error::ClientError;
use ironclaw_product_contracts::orchestration::{ThreadActionOutcome, ThreadActionRefusal};

/// What to do after Codex answered the revert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AfterRevert {
    /// The conversation is reverted: restore the files now.
    Restore { note: String },
    /// Nothing changed; tell the person why.
    Stop(UndoOutcome),
}

/// How an undo ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UndoOutcome {
    /// Conversation reverted and `restored` files put back.
    Done { note: String, restored: usize },
    /// Codex refused before changing anything.
    Refused(ThreadActionRefusal),
    /// A turn or another action holds the session; nothing changed.
    Busy,
    /// The revert failed; nothing was restored.
    RevertFailed(String),
    /// The conversation is reverted but the files are not: the checkpoint
    /// is kept for a retry.
    RestoreFailed { note: String, error: String },
}

impl UndoOutcome {
    /// The checkpoint is still needed (nothing done, or files pending).
    pub fn keeps_checkpoint(&self) -> bool {
        !matches!(self, Self::Done { .. })
    }

    /// Only the file restore is left to retry.
    pub fn restore_pending(&self) -> bool {
        matches!(self, Self::RestoreFailed { .. })
    }

    /// One line for the person.
    pub fn message(&self) -> String {
        match self {
            Self::Done { restored, .. } => format!(
                "Reverted the turn in Codex and restored {restored} file{}.",
                if *restored == 1 { "" } else { "s" }
            ),
            Self::Refused(ThreadActionRefusal::WorkspaceMismatch { runtime_root }) => {
                format!("Codex works in {runtime_root}, not this folder: nothing was undone.")
            }
            Self::Refused(ThreadActionRefusal::UnknownTurn) => {
                "Codex has no record of that turn: nothing was undone.".into()
            }
            Self::Refused(ThreadActionRefusal::AlreadyReverted) => {
                "That turn was already reverted: nothing was undone.".into()
            }
            Self::Busy => "Codex is busy with this thread; try again when it is done.".into(),
            Self::RevertFailed(e) => {
                format!("Codex could not revert the turn ({e}); files untouched.")
            }
            Self::RestoreFailed { error, .. } => format!(
                "Codex reverted the turn, but the files were not restored ({error}). The checkpoint is kept: retry Restore files."
            ),
        }
    }
}

/// Decide from Codex's answer to `revert_turn`.
pub fn after_revert(answer: Result<ThreadActionOutcome, ClientError>) -> AfterRevert {
    match answer {
        Ok(ThreadActionOutcome::Done { note }) => AfterRevert::Restore { note },
        Ok(ThreadActionOutcome::Refused { refusal }) => {
            AfterRevert::Stop(UndoOutcome::Refused(refusal))
        }
        Ok(ThreadActionOutcome::Busy) => AfterRevert::Stop(UndoOutcome::Busy),
        Ok(ThreadActionOutcome::Started { .. }) => {
            AfterRevert::Stop(UndoOutcome::RevertFailed("unexpected answer".into()))
        }
        Err(e) => AfterRevert::Stop(UndoOutcome::RevertFailed(e.to_string())),
    }
}

/// Decide from the file restore that followed a confirmed revert.
pub fn after_restore(note: String, restored: Result<usize, String>) -> UndoOutcome {
    match restored {
        Ok(restored) => UndoOutcome::Done { note, restored },
        Err(error) => UndoOutcome::RestoreFailed { note, error },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_restored_unless_codex_confirmed_the_revert() {
        let refused = after_revert(Ok(ThreadActionOutcome::Refused {
            refusal: ThreadActionRefusal::WorkspaceMismatch {
                runtime_root: "/codex/work".into(),
            },
        }));
        let AfterRevert::Stop(outcome) = refused else {
            panic!("a refusal must not restore")
        };
        assert!(outcome.keeps_checkpoint() && !outcome.restore_pending());
        assert!(outcome.message().contains("/codex/work"));
        assert_eq!(
            after_revert(Ok(ThreadActionOutcome::Busy)),
            AfterRevert::Stop(UndoOutcome::Busy)
        );
        let failed = after_revert(Err(ClientError::Refused {
            code: "runtime_unavailable".into(),
            message: "codex resume: timeout".into(),
        }));
        assert!(matches!(
            failed,
            AfterRevert::Stop(UndoOutcome::RevertFailed(_))
        ));
        assert!(matches!(
            after_revert(Ok(ThreadActionOutcome::Done { note: "ok".into() })),
            AfterRevert::Restore { .. }
        ));
    }

    #[test]
    fn a_failed_restore_after_a_revert_keeps_the_checkpoint_for_a_retry() {
        let partial = after_restore("Reverted 1 turn".into(), Err("permission denied".into()));
        assert!(partial.keeps_checkpoint() && partial.restore_pending());
        assert!(partial.message().contains("retry Restore files"));
        let done = after_restore("Reverted 1 turn".into(), Ok(3));
        assert!(!done.keeps_checkpoint());
        assert_eq!(
            done.message(),
            "Reverted the turn in Codex and restored 3 files."
        );
    }
}
