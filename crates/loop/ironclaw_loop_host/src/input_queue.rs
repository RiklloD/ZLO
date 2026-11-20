//! Host-owned input queue contract for Reborn loop input ports, plus the ONE
//! queue model both backends share.
//!
//! The cursor/ack semantics live in [`RunQueueModel`], a pure serde-able value
//! type: the in-memory backend keeps it in a per-run map under a mutex, the
//! durable backend persists it verbatim as the per-run JSON document behind a
//! CAS loop. Fixing a queue-semantics bug therefore happens in exactly one
//! place, and the two backends cannot drift.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use ironclaw_host_api::ids::ThreadId;
use ironclaw_loop_contracts::{
    LoopInput, LoopInputAckEffect, LoopInputAckToken, LoopInputCursorToken,
};
use ironclaw_threads::{MessageStatus, SessionThreadService, ThreadMessageId, ThreadScope};
use ironclaw_turns::{TurnId, TurnRunId, TurnScope};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Host-owned input queue surface.
///
/// The host runtime exposes one implementation backed by its actual
/// user-input, steering, and followup substrate. `HostQueueLoopInputPort`
/// adapts this surface to the `LoopInputPort` contract the loop calls.
///
/// Cursor semantics:
///
/// - Tokens are opaque to the loop. Implementations may use a monotonic
///   sequence, generation token, or compound key. `next_after` must return the
///   first input strictly after `after`, or an equivalent origin point for a
///   run-start cursor. Implementations must reject malformed, foreign, or
///   unissued future cursor tokens for the bound run instead of treating them
///   as empty positions.
/// - Cursors are read positions, not ack identities. Acking is by exact
///   per-input token so control inputs cannot be skipped by cursor-through ack.
/// - `ack_consumed` is at-most-once. Acking the same token twice is a no-op.
/// - Polled but unacked inputs are redeliverable when the caller polls again
///   from the same prior cursor.
///
/// Implementations are per host process. Each adapter binds to one run at host
/// build time; cross-run polls are rejected by the adapter before reaching the
/// queue.
#[async_trait]
pub trait HostInputQueue: Send + Sync {
    async fn next_after(
        &self,
        run_id: TurnRunId,
        after: LoopInputCursorToken,
        limit: usize,
    ) -> Result<HostInputBatch, HostInputQueueError>;

    async fn ack_consumed(
        &self,
        run_id: TurnRunId,
        tokens: Vec<LoopInputAckToken>,
    ) -> Result<(), HostInputQueueError>;

    /// A model call carried the drained inputs behind `tokens`: the agent
    /// has read them. Until then a drained user steer is only consumed —
    /// terminal reconciliation hands it off as unread if the run ends first
    /// (cancel, budget exit, prompt failure after the drain). Idempotent.
    async fn confirm_read(
        &self,
        run_id: TurnRunId,
        tokens: Vec<LoopInputAckToken>,
    ) -> Result<(), HostInputQueueError> {
        let _ = (run_id, tokens);
        Ok(())
    }
}

/// Host-side terminal reconciliation surface, separate from the loop-facing
/// drain contract: its only callers are the runner's terminal reconcilers, so
/// the loop port never gains settle authority and drain-side test doubles need
/// no dead stubs.
#[async_trait]
pub trait HostInputQueueReconcile: Send + Sync {
    /// Terminal reconciliation: close the run's queue and settle every input
    /// the run never read — still queued, or drained by the loop but never
    /// carried by a model call (see [`HostInputQueue::confirm_read`]).
    ///
    /// With a [`crate::SteerHandoffStore`] (production), every user steer is
    /// recorded as a durable [`crate::SteerHandoff`] BEFORE the queue is
    /// closed, and the thread's dispatcher decides each one exactly once: a
    /// run that [`RunQueueEnding::Ended`] by itself has them start the next
    /// turn (oldest first, ahead of newer messages); a
    /// [`RunQueueEnding::Stopped`] run (a Stop, a declined gate, a restart)
    /// has them settled "not delivered" — the person decides, as after a
    /// Stop in t3code. A Stop that lands after the end still holds the
    /// handoff's unread steers.
    ///
    /// Without a store, and for background-result inputs, claimed rows flip
    /// `Queued` → `RejectedBusy`. Entries are claimed before any write, so a
    /// still-running loop that races this call can no longer drain them.
    /// Idempotent: a repeated call retries whatever did not settle.
    ///
    /// Returns the message ids whose rows were flipped `RejectedBusy` here.
    async fn settle_unconsumed(
        &self,
        run_id: TurnRunId,
        ending: RunQueueEnding,
    ) -> Result<Vec<ThreadMessageId>, HostInputQueueError>;
}

/// How the run whose queue is being settled ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunQueueEnding {
    /// The run was cancelled: unread steers are not redelivered.
    Stopped,
    /// The run completed or failed by itself: unread steers start the next
    /// turn.
    Ended,
}

/// Where a user steer stood when its sender took it back from a run's queue
/// ([`HostInputEnqueuePort::withdraw_user_steer`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SteerWithdrawal {
    /// Still unread in an open queue: removed, the caller owns it again.
    Withdrawn,
    /// The loop already drained it.
    Consumed,
    /// Terminal reconciliation already claimed it (or the queue is closed).
    Settled,
    /// The queue does not know it (never enqueued, or the queue document
    /// was reclaimed after settling): the row's delivery state decides.
    Unknown,
}

/// Raw queue batch returned by a host queue implementation.
///
/// The adapter wraps `next_cursor` into a `LoopInputCursor` scoped to the
/// bound run context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostInputBatch {
    pub inputs: Vec<HostInputEnvelope>,
    pub next_cursor: LoopInputCursorToken,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostInputEnvelope {
    pub input: LoopInput,
    pub cursor: LoopInputCursorToken,
    pub ack_token: LoopInputAckToken,
}

#[derive(Debug, Error)]
pub enum HostInputQueueError {
    #[error("input queue unavailable: {reason}")]
    Unavailable { reason: String },
    #[error("cursor invalid for run: {reason}")]
    InvalidCursor { reason: String },
    /// Steering is deliberately not wired for this runtime (the
    /// [`RejectingInputEnqueue`] null port). Distinct from [`Self::Unavailable`]
    /// so callers can fall back to the rejected-busy outcome ONLY for the
    /// disabled mode, while genuine durable-I/O failures surface as retryable
    /// errors instead of masquerading as successful rejections.
    #[error("input queue is not wired for this runtime")]
    Disabled,
    /// The run's queue was closed by terminal reconciliation: the run will
    /// never drain again, so accepting the input would strand it. Callers
    /// submit the message again once the run releases the thread (it starts
    /// the next turn) — unlike [`Self::Unavailable`], which stays a retryable
    /// operational failure.
    #[error("input queue for the run is closed")]
    RunClosed,
    /// The run's queue is at [`MAX_QUEUED_INPUTS_PER_RUN`]. A run can sit
    /// busy indefinitely (e.g. parked on an approval gate), so an unbounded
    /// queue would grow durable state and rewrite amplification without
    /// limit. Callers settle the message as rejected-busy (resend
    /// affordance).
    #[error("input queue for the run is full")]
    CapacityExhausted,
    #[error("input queue internal error")]
    Internal,
}

/// Ceiling on tracked queued inputs per run — live entries PLUS consumed
/// entries whose `Submitted` flip is still pending retry, so the whole
/// persisted state is bounded even while the thread store keeps failing.
/// Steering inputs are small fixed-shape refs
/// (`LoopInput::Steering { message_ref }`), so a count bound also bounds
/// serialized size; 32 is far beyond any interactive use while keeping the
/// durable document's rewrite-per-enqueue cost trivial.
pub const MAX_QUEUED_INPUTS_PER_RUN: usize = 32;
/// Independent replay-dedup window for inputs already consumed successfully.
/// Keeping this separate from live capacity avoids exhausting a long-running
/// run merely because it has processed inputs over time.
const RECENTLY_CONSUMED_DEDUP_LIMIT: usize = MAX_QUEUED_INPUTS_PER_RUN;

#[async_trait]
pub trait HostInputEnqueuePort: Send + Sync {
    /// Enqueue a user message as steering/followup input for an active run.
    ///
    /// The request carries the originating thread message identity so the queue
    /// can transition that message to `submitted` once the input is consumed.
    /// That transition is BEST-EFFORT per attempt: input consumption/ack is
    /// never rolled back when the status write fails (a stale `Queued` badge
    /// is recoverable; a dead run is not) — the failure is logged at debug
    /// level and the binding is RETAINED as a pending flip, retried by later
    /// queue operations and by terminal reconciliation, so the row converges
    /// to `Submitted` by run end rather than being silently dropped. There is
    /// deliberately no metadata-free variant: every enqueued input is backed
    /// by a thread message, so the transition attempt can never be omitted.
    ///
    /// An enqueue can be refused with [`HostInputQueueError::RunClosed`]
    /// (terminal reconciliation already closed the run's queue: the run is
    /// ending, and callers submit the message again as the next turn) or
    /// [`HostInputQueueError::CapacityExhausted`]
    /// ([`MAX_QUEUED_INPUTS_PER_RUN`]; callers settle it as rejected-busy,
    /// exactly like [`HostInputQueueError::Disabled`]).
    async fn enqueue_queued_message(
        &self,
        request: EnqueueQueuedMessageRequest,
    ) -> Result<HostInputEnvelope, HostInputQueueError>;

    /// [`Self::enqueue_queued_message`] for a user steer, recording the turn
    /// scope it was admitted under so terminal reconciliation can start the
    /// next turn with it if the run ends before reading it. Queues without a
    /// redelivery path keep the plain enqueue (the steer then settles
    /// `RejectedBusy` if stranded).
    async fn enqueue_user_steer(
        &self,
        request: EnqueueQueuedMessageRequest,
        turn_scope: TurnScope,
    ) -> Result<HostInputEnvelope, HostInputQueueError> {
        let _ = turn_scope;
        self.enqueue_queued_message(request).await
    }

    /// Take a user steer back from `run_id`'s queue before the run reads it:
    /// the admission fence for a run that turned out to be ending after the
    /// enqueue (its reconciliation may already have run and reclaimed the
    /// queue, so a fresh entry would never be read). Queues without
    /// redelivery answer [`SteerWithdrawal::Unknown`].
    async fn withdraw_user_steer(
        &self,
        run_id: TurnRunId,
        message_id: ThreadMessageId,
    ) -> Result<SteerWithdrawal, HostInputQueueError> {
        let _ = (run_id, message_id);
        Ok(SteerWithdrawal::Unknown)
    }
}

/// Host-owned callback for durable queue acknowledgment effects.
///
/// Queue implementations call this only after the corresponding input ack is
/// durably recorded. A failed callback leaves the effect in the queue's
/// pending state so later queue operations or terminal reconciliation retry it.
#[async_trait]
pub trait HostInputAckEffectHandler: Send + Sync {
    async fn handle_ack_effect(
        &self,
        effect: LoopInputAckEffect,
    ) -> Result<(), HostInputQueueError>;
}

/// Null-object enqueue port used as the default when a host has not wired a
/// real input queue. Every enqueue fails closed with the distinct
/// [`HostInputQueueError::Disabled`] rather than silently dropping the
/// message (or conflating "steering off" with an operational failure).
#[derive(Debug, Default, Clone, Copy)]
pub struct RejectingInputEnqueue;

#[async_trait]
impl HostInputEnqueuePort for RejectingInputEnqueue {
    async fn enqueue_queued_message(
        &self,
        _request: EnqueueQueuedMessageRequest,
    ) -> Result<HostInputEnvelope, HostInputQueueError> {
        Err(HostInputQueueError::Disabled)
    }
}

#[derive(Debug, Clone)]
pub struct EnqueueQueuedMessageRequest {
    pub run_id: TurnRunId,
    pub turn_id: TurnId,
    pub scope: ThreadScope,
    pub thread_id: ThreadId,
    pub message_id: ThreadMessageId,
    pub input: LoopInput,
    /// Optional durable callback effect. Only background subagent result
    /// inputs populate this field; ordinary steering/followup input leaves it
    /// `None`.
    pub ack_effect: Option<LoopInputAckEffect>,
}

// ---------------------------------------------------------------------------
// The shared queue model
// ---------------------------------------------------------------------------

/// The transcript message bound to a queued input, used to flip its status
/// once the input is consumed (`Submitted`) or terminally reconciled
/// (`RejectedBusy`). One type for both backends; the in-memory backend simply
/// never serializes it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct QueuedMessageStatusUpdate {
    pub(crate) turn_id: TurnId,
    pub(crate) scope: ThreadScope,
    pub(crate) thread_id: ThreadId,
    pub(crate) message_id: ThreadMessageId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct QueueEntry {
    pub(crate) sequence: u64,
    pub(crate) input: LoopInput,
    pub(crate) status: QueuedMessageStatusUpdate,
    #[serde(default)]
    pub(crate) ack_effect: Option<LoopInputAckEffect>,
    /// The turn scope a user steer was admitted under; `None` for
    /// background-result inputs and rows queued by older builds. Only an
    /// entry carrying it can be redelivered when its run ends unread.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) turn_scope: Option<TurnScope>,
}

/// A consumed (acked) entry whose `Queued` → `Submitted` transcript flip has
/// not been confirmed yet. Retained so the flip can be retried by later queue
/// operations — and so an idempotent re-enqueue of the same message repairs
/// the stale row instead of minting a new sequence (duplicate delivery).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PendingSubmitFlip {
    pub(crate) sequence: u64,
    pub(crate) status: QueuedMessageStatusUpdate,
}

/// A consumed entry's deferred callback effect. Retained until the handler
/// reports success, including across durable queue rehydration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PendingAckEffect {
    pub(crate) sequence: u64,
    pub(crate) effect: LoopInputAckEffect,
}

/// Bounded dedup identity for an input the loop already consumed. Successful
/// transcript flips remove [`PendingSubmitFlip`], so they cannot double as the
/// replay tombstone: a request that classified the row as `Queued` before the
/// flip may reach enqueue afterward. Retaining the message id with its original
/// sequence lets that racing replay return the same envelope without becoming
/// deliverable again.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ConsumedMessage {
    pub(crate) sequence: u64,
    pub(crate) message_id: ThreadMessageId,
}

fn first_sequence() -> u64 {
    1
}

/// One run's queue state — the single implementation of the cursor/ack
/// semantics documented on [`HostInputQueue`]. Serialized verbatim as the
/// durable backend's per-run JSON document; held in a per-run map by the
/// in-memory backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RunQueueModel {
    /// Next sequence to issue. Starts at 1 so the origin cursor (sequence 0)
    /// stays the unique run-start position and every real input is strictly
    /// after it.
    #[serde(default = "first_sequence")]
    next_sequence: u64,
    entries: Vec<QueueEntry>,
    /// Compact ack state: every sequence `<= acked_watermark` is acked, plus
    /// the sparse out-of-order set above it. Keeps duplicate/redelivered acks
    /// idempotent without an unbounded tombstone per input. `BTreeSet`
    /// serializes as a sorted array.
    #[serde(default)]
    acked_watermark: u64,
    #[serde(default)]
    acked_above: BTreeSet<u64>,
    /// Consumed entries whose `Submitted` transcript flip failed and is
    /// awaiting retry. Bounded by the flips that actually fail; drained by
    /// every later ack, re-enqueue, and the terminal reconciliation.
    #[serde(default)]
    pending_submit_flips: Vec<PendingSubmitFlip>,
    /// Callback effects moved out of consumed entries. These count toward the
    /// queue capacity and keep the queue document alive until acknowledged.
    #[serde(default)]
    pending_ack_effects: Vec<PendingAckEffect>,
    /// Recent successful or pending consumptions, retained independently from
    /// live queue capacity. The ring is bounded to cover concurrent/retried
    /// admissions without making a long-lived run permanently full after the
    /// same number of successful inputs.
    #[serde(default)]
    recently_consumed: Vec<ConsumedMessage>,
    /// Terminal tombstone: set by [`Self::close_and_claim`]. A closed queue
    /// rejects enqueues ([`HostInputQueueError::RunClosed`]) and treats late
    /// acks as no-ops — terminal reconciliation owns settlement from then on.
    #[serde(default)]
    closed: bool,
    /// Entries claimed at close whose `RejectedBusy` transcript flip has not
    /// succeeded yet. Retained (durably: the document itself is retained)
    /// until every flip lands, so a transient thread-store failure during
    /// cancellation keeps a durable retry source instead of stranding the
    /// rows `Queued` forever.
    #[serde(default)]
    pending_reject_flips: Vec<QueuedMessageStatusUpdate>,
    /// User steers the loop drained but no model call has carried yet
    /// ([`HostInputQueue::confirm_read`]). Kept whole so terminal settlement
    /// can hand them off as unread.
    /// Outside the live-capacity count: each one was a live entry until the
    /// drain, and the next model response confirms the whole drain.
    #[serde(default)]
    awaiting_read: Vec<QueueEntry>,
}

impl Default for RunQueueModel {
    fn default() -> Self {
        Self {
            next_sequence: first_sequence(),
            entries: Vec::new(),
            acked_watermark: 0,
            acked_above: BTreeSet::new(),
            pending_submit_flips: Vec::new(),
            pending_ack_effects: Vec::new(),
            recently_consumed: Vec::new(),
            closed: false,
            pending_reject_flips: Vec::new(),
            awaiting_read: Vec::new(),
        }
    }
}

/// What [`RunQueueModel::close_and_claim`] took for a durable handoff.
#[derive(Debug, Default)]
pub(crate) struct ClaimedInputs {
    pub(crate) handoff: Vec<crate::HandoffSteer>,
    pub(crate) turn_scope: Option<TurnScope>,
    pub(crate) thread: Option<(ThreadScope, ThreadId)>,
}

impl ClaimedInputs {
    /// The durable handoff for these claims, if any user steer was taken.
    pub(crate) fn into_handoff(
        self,
        run_id: TurnRunId,
        ending: RunQueueEnding,
    ) -> Option<crate::SteerHandoff> {
        let (thread_scope, thread_id) = self.thread?;
        if self.handoff.is_empty() {
            return None;
        }
        Some(crate::SteerHandoff {
            ended_run_id: run_id,
            thread_scope,
            thread_id,
            turn_scope: self.turn_scope,
            stopped: ending == RunQueueEnding::Stopped,
            recorded_at_ms: crate::steer_handoff::now_ms(),
            steers: self.handoff,
        })
    }
}

/// Outcome of [`RunQueueModel::enqueue_dedup`]: callers only persist when the
/// model actually changed (`Inserted`).
pub(crate) enum EnqueueDisposition {
    /// A new entry was appended at `sequence`.
    Inserted { sequence: u64 },
    /// The identical input is already live at `sequence` (retried enqueue).
    Duplicate { sequence: u64 },
    /// The message was already consumed but its `Submitted` flip is still
    /// pending: the caller retries the flip (repair) instead of re-minting a
    /// sequence, which would deliver the same message twice.
    AlreadyConsumed { flip: PendingSubmitFlip },
}

/// Result of [`RunQueueModel::validate_and_ack`].
pub(crate) struct AckOutcome {
    /// Whether any token was newly acked (the model changed and must be
    /// persisted before the flips run).
    pub(crate) newly_acked: bool,
    /// Every `Submitted` flip now due — the newly acked bindings plus any
    /// retained retries from earlier failed flips.
    pub(crate) due_flips: Vec<PendingSubmitFlip>,
    /// Every callback effect now due, including retained retries.
    pub(crate) due_ack_effects: Vec<PendingAckEffect>,
}

impl RunQueueModel {
    fn is_acked(&self, sequence: u64) -> bool {
        sequence <= self.acked_watermark || self.acked_above.contains(&sequence)
    }

    fn record_ack(&mut self, sequence: u64) {
        if sequence <= self.acked_watermark {
            return;
        }
        self.acked_above.insert(sequence);
        while self.acked_above.remove(&(self.acked_watermark + 1)) {
            self.acked_watermark += 1;
        }
    }

    /// Enqueue `input`, deduplicating a retried enqueue of the same message
    /// (steering refs derive from unique message ids, so distinct messages can
    /// never collide). The first status binding for an entry wins.
    ///
    /// Rejects a closed queue ([`HostInputQueueError::RunClosed`]) and a full
    /// queue ([`HostInputQueueError::CapacityExhausted`]); dedup and repair
    /// are checked first so retries of an already-accepted message never
    /// bounce off the capacity ceiling. Records the steer's turn scope (see
    /// [`QueueEntry::turn_scope`]).
    pub(crate) fn enqueue_dedup_scoped(
        &mut self,
        input: LoopInput,
        status: QueuedMessageStatusUpdate,
        turn_scope: Option<TurnScope>,
    ) -> Result<EnqueueDisposition, HostInputQueueError> {
        if self.closed {
            return Err(HostInputQueueError::RunClosed);
        }
        if let Some(existing) = self.entries.iter().find(|entry| entry.input == input) {
            return Ok(EnqueueDisposition::Duplicate {
                sequence: existing.sequence,
            });
        }
        if let Some(pending) = self
            .pending_submit_flips
            .iter()
            .find(|pending| pending.status.message_id == status.message_id)
        {
            return Ok(EnqueueDisposition::AlreadyConsumed {
                flip: pending.clone(),
            });
        }
        if let Some(consumed) = self
            .recently_consumed
            .iter()
            .find(|consumed| consumed.message_id == status.message_id)
        {
            return Ok(EnqueueDisposition::Duplicate {
                sequence: consumed.sequence,
            });
        }
        // Bound distinct tracked inputs, not outstanding operations: one
        // consumed input can own both a transcript flip and an ack effect.
        // The independently bounded replay ring does not consume capacity.
        let tracked_sequences: BTreeSet<u64> = self
            .entries
            .iter()
            .map(|entry| entry.sequence)
            .chain(self.pending_submit_flips.iter().map(|flip| flip.sequence))
            .chain(
                self.pending_ack_effects
                    .iter()
                    .map(|effect| effect.sequence),
            )
            .collect();
        if tracked_sequences.len() >= MAX_QUEUED_INPUTS_PER_RUN {
            return Err(HostInputQueueError::CapacityExhausted);
        }
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.saturating_add(1);
        // Entries stay sequence-ordered by construction: sequences are issued
        // monotonically and only ever appended.
        self.entries.push(QueueEntry {
            sequence,
            input,
            status,
            ack_effect: None,
            turn_scope,
        });
        Ok(EnqueueDisposition::Inserted { sequence })
    }

    pub(crate) fn attach_ack_effect(
        &mut self,
        sequence: u64,
        effect: Option<LoopInputAckEffect>,
    ) -> Result<(), HostInputQueueError> {
        let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.sequence == sequence)
        else {
            return Err(HostInputQueueError::Internal);
        };
        entry.ack_effect = effect;
        Ok(())
    }

    /// Strictly-after scan: an entry's own cursor is its sequence, so polling
    /// from that cursor never returns the entry again. `next_sequence` is the
    /// next-to-issue value, so the max issued cursor is `next_sequence - 1`;
    /// anything at or past `next_sequence` is an unissued FUTURE cursor and is
    /// rejected instead of read as an empty position.
    pub(crate) fn scan_after(
        &self,
        after_sequence: u64,
        limit: usize,
    ) -> Result<HostInputBatch, HostInputQueueError> {
        if after_sequence >= self.next_sequence {
            return Err(HostInputQueueError::InvalidCursor {
                reason: "input cursor is ahead of the run input queue".to_string(),
            });
        }
        let mut inputs = Vec::new();
        let mut cursor_sequence_out = after_sequence;
        for entry in self
            .entries
            .iter()
            .filter(|entry| entry.sequence > after_sequence)
        {
            if self.is_acked(entry.sequence) {
                cursor_sequence_out = entry.sequence;
                continue;
            }
            if inputs.len() >= limit {
                break;
            }
            inputs.push(envelope_for(entry.sequence, entry.input.clone())?);
            cursor_sequence_out = entry.sequence;
        }
        let next_cursor = if cursor_sequence_out == 0 {
            LoopInputCursorToken::origin()
        } else {
            cursor_token(cursor_sequence_out)?
        };
        Ok(HostInputBatch {
            inputs,
            next_cursor,
        })
    }

    /// Validate `tokens` and record their acks, pruning the consumed entries'
    /// payloads to bound state size. Each newly acked binding moves to
    /// [`Self::pending_submit_flips`] until the caller confirms its
    /// transcript flip via [`Self::confirm_submit_flips`].
    ///
    /// A closed queue treats every ack as a no-op: terminal reconciliation
    /// already claimed the remaining entries, and whichever side claimed a
    /// sequence first owns its transcript settlement.
    ///
    /// Fails loud on a token that matches no live entry and is not already
    /// acked: committing an unknown sequence would poison state — a later
    /// entry minted for that sequence would be skipped forever by the scan. A
    /// redelivered ack lands in `is_acked`; anything else is a genuine fault
    /// (stale/forged token), not a no-op.
    pub(crate) fn validate_and_ack(
        &mut self,
        tokens: &[LoopInputAckToken],
    ) -> Result<AckOutcome, HostInputQueueError> {
        if self.closed {
            return Ok(AckOutcome {
                newly_acked: false,
                due_flips: Vec::new(),
                due_ack_effects: Vec::new(),
            });
        }
        let mut newly_acked = Vec::new();
        for token in tokens {
            let sequence = ack_sequence(token)?;
            if self.is_acked(sequence) {
                continue;
            }
            let Some(entry) = self.entries.iter().find(|entry| entry.sequence == sequence) else {
                return Err(HostInputQueueError::InvalidCursor {
                    reason: format!(
                        "ack token references sequence {sequence} that is neither live nor \
                         already acked for this run"
                    ),
                });
            };
            self.pending_submit_flips.push(PendingSubmitFlip {
                sequence,
                status: entry.status.clone(),
            });
            if let Some(effect) = entry.ack_effect.clone() {
                self.pending_ack_effects
                    .push(PendingAckEffect { sequence, effect });
            }
            if matches!(entry.input, LoopInput::Steering { .. }) && entry.ack_effect.is_none() {
                // Drained, not yet read: a model call must carry it first.
                self.awaiting_read.push(entry.clone());
            }
            self.recently_consumed.push(ConsumedMessage {
                sequence,
                message_id: entry.status.message_id,
            });
            if self.recently_consumed.len() > RECENTLY_CONSUMED_DEDUP_LIMIT {
                self.recently_consumed.remove(0);
            }
            newly_acked.push(sequence);
        }
        for sequence in &newly_acked {
            self.record_ack(*sequence);
        }
        self.entries
            .retain(|entry| !newly_acked.contains(&entry.sequence));
        Ok(AckOutcome {
            newly_acked: !newly_acked.is_empty(),
            due_flips: self.pending_submit_flips.clone(),
            due_ack_effects: self.pending_ack_effects.clone(),
        })
    }

    /// Drop the pending `Submitted` flips whose transcript writes succeeded.
    pub(crate) fn confirm_submit_flips(&mut self, flipped: &[u64]) {
        self.pending_submit_flips
            .retain(|pending| !flipped.contains(&pending.sequence));
    }

    /// Drop callback effects whose handlers completed successfully.
    pub(crate) fn confirm_ack_effects(&mut self, confirmed: &[u64]) {
        self.pending_ack_effects
            .retain(|pending| !confirmed.contains(&pending.sequence));
    }

    /// Terminal claim: close the queue (rejecting further enqueues) and take
    /// every input the run never read. With `handoff` (a handoff store is
    /// wired), user steers — unread entries and drained-but-unread ones —
    /// come back as [`crate::HandoffSteer`]s, in queue order, for the
    /// caller to record durably; everything else (and every user steer
    /// without a store) moves to [`Self::pending_reject_flips`].
    /// Idempotent — a second call claims nothing new. The record is
    /// disposable once [`Self::is_settled`].
    pub(crate) fn close_and_claim(&mut self, handoff: bool) -> ClaimedInputs {
        self.closed = true;
        let mut claimed = ClaimedInputs::default();
        let entries: Vec<QueueEntry> = std::mem::take(&mut self.entries)
            .into_iter()
            .filter(|entry| !self.is_acked(entry.sequence))
            .collect();
        let unread = entries
            .into_iter()
            .map(|entry| (entry, crate::HandoffSteerKind::Unread));
        let read = std::mem::take(&mut self.awaiting_read)
            .into_iter()
            .map(|entry| (entry, crate::HandoffSteerKind::ReadUnconfirmed));
        let mut all: Vec<_> = unread.chain(read).collect();
        all.sort_by_key(|(entry, _)| entry.sequence);
        for (entry, kind) in all {
            match (&entry.input, &entry.ack_effect, handoff) {
                (LoopInput::Steering { message_ref }, None, true) => {
                    if claimed.turn_scope.is_none() {
                        claimed.turn_scope = entry.turn_scope.clone();
                    }
                    if claimed.thread.is_none() {
                        claimed.thread =
                            Some((entry.status.scope.clone(), entry.status.thread_id.clone()));
                    }
                    claimed.handoff.push(crate::HandoffSteer {
                        message_id: entry.status.message_id,
                        message_ref: message_ref.clone(),
                        kind,
                    });
                }
                // A consumed input without a handoff path keeps its
                // `Submitted` row: only queued rows flip.
                _ if kind == crate::HandoffSteerKind::ReadUnconfirmed => {}
                _ => self.pending_reject_flips.push(entry.status),
            }
        }
        // `closed` is now the authoritative tombstone, so replay identities
        // are unnecessary while a terminal document awaits flip settlement.
        self.recently_consumed.clear();
        claimed
    }

    /// Take a user steer back before the loop reads it.
    pub(crate) fn withdraw(&mut self, message_id: ThreadMessageId) -> SteerWithdrawal {
        if self.closed {
            return SteerWithdrawal::Settled;
        }
        if let Some(index) = self.entries.iter().position(|entry| {
            entry.status.message_id == message_id && !self.is_acked(entry.sequence)
        }) {
            self.entries.remove(index);
            return SteerWithdrawal::Withdrawn;
        }
        let consumed = self
            .recently_consumed
            .iter()
            .any(|consumed| consumed.message_id == message_id)
            || self
                .pending_submit_flips
                .iter()
                .any(|flip| flip.status.message_id == message_id)
            || self
                .awaiting_read
                .iter()
                .any(|entry| entry.status.message_id == message_id);
        if consumed {
            SteerWithdrawal::Consumed
        } else {
            SteerWithdrawal::Unknown
        }
    }

    /// A model call carried these drained inputs. `true` when the model
    /// changed.
    pub(crate) fn confirm_read(&mut self, tokens: &[LoopInputAckToken]) -> bool {
        if self.closed {
            return false;
        }
        let sequences: Vec<u64> = tokens
            .iter()
            .filter_map(|token| ack_sequence(token).ok())
            .collect();
        let before = self.awaiting_read.len();
        self.awaiting_read
            .retain(|entry| !sequences.contains(&entry.sequence));
        before != self.awaiting_read.len()
    }

    /// Drop the pending `RejectedBusy` flips whose transcript writes
    /// succeeded.
    pub(crate) fn confirm_reject_flips(&mut self, flipped: &[ThreadMessageId]) {
        self.pending_reject_flips
            .retain(|pending| !flipped.contains(&pending.message_id));
    }

    /// The `Submitted` flips currently awaiting (re)try.
    pub(crate) fn due_submit_flips(&self) -> Vec<PendingSubmitFlip> {
        self.pending_submit_flips.clone()
    }

    /// Callback effects currently awaiting a successful handler retry.
    pub(crate) fn due_ack_effects(&self) -> Vec<PendingAckEffect> {
        self.pending_ack_effects.clone()
    }

    /// The `RejectedBusy` flips currently awaiting (re)try.
    pub(crate) fn due_reject_flips(&self) -> Vec<QueuedMessageStatusUpdate> {
        self.pending_reject_flips.clone()
    }

    /// Closed with nothing left to flip: the per-run record can be reclaimed.
    pub(crate) fn is_settled(&self) -> bool {
        self.closed
            && self.entries.is_empty()
            && self.pending_submit_flips.is_empty()
            && self.pending_ack_effects.is_empty()
            && self.pending_reject_flips.is_empty()
            && self.awaiting_read.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Shared transcript-flip helpers (best-effort by contract; see
// `HostInputEnqueuePort::enqueue_queued_message`)
// ---------------------------------------------------------------------------

/// Flip each consumed row `Queued` → `Submitted`. Best-effort per call: the
/// inputs are already durably acked by the time this runs, and failing the
/// ack here would kill the whole run for a transcript status write — a stale
/// "queued" badge is recoverable; a dead run is not. A failed flip is NOT
/// dropped, though: its binding stays in the model's pending-submit-flip
/// state, retried by every later ack, idempotent re-enqueue, and the terminal
/// reconciliation, so the row converges to `Submitted` by run end.
///
/// Returns the sequences whose flips succeeded, for
/// [`RunQueueModel::confirm_submit_flips`].
pub(crate) async fn flip_submitted(
    thread_service: &dyn SessionThreadService,
    run_id: TurnRunId,
    flips: Vec<PendingSubmitFlip>,
) -> Vec<u64> {
    let mut flipped = Vec::new();
    for flip in flips {
        let update = flip.status;
        match thread_service
            .mark_message_submitted(
                &update.scope,
                &update.thread_id,
                update.message_id,
                update.turn_id.to_string(),
                run_id.to_string(),
            )
            .await
        {
            Ok(_) => flipped.push(flip.sequence),
            Err(error) => {
                tracing::debug!(
                    component = "host_input_queue",
                    operation = "mark_message_submitted",
                    %run_id,
                    thread_id = %update.thread_id,
                    message_id = %update.message_id,
                    %error,
                    "queued-message status flip failed after the input was consumed; \
                     already acked so the run continues (flip retried by later queue \
                     operations and terminal reconciliation)"
                );
            }
        }
    }
    flipped
}

/// Outcome of [`flip_rejected_busy`]: `flipped` rows actually transitioned to
/// `RejectedBusy` (reported to the reconciler's caller); `settled` rows were
/// found already settled by someone else (or gone) and can never be flipped —
/// both sets are confirmed off the pending-retry state so a row settled
/// elsewhere cannot pin the queue record forever.
pub(crate) struct RejectFlipOutcome {
    pub(crate) flipped: Vec<ThreadMessageId>,
    pub(crate) settled: Vec<ThreadMessageId>,
}

impl RejectFlipOutcome {
    pub(crate) fn confirmable(&self) -> Vec<ThreadMessageId> {
        let mut ids = self.flipped.clone();
        ids.extend(self.settled.iter().copied());
        ids
    }
}

/// Flip each claimed row `Queued` → `RejectedBusy` during terminal
/// reconciliation. Best-effort: any failure must not fail the caller's
/// terminal transition. A failed flip is re-examined with a point read — a
/// row that is no longer `Queued` (settled by an admission rollback, a
/// duplicate settle, or a racing consumer) is classified `settled` so its
/// pending-retry entry is confirmed instead of retried forever; a genuinely
/// transient failure stays pending for retry.
pub(crate) async fn flip_rejected_busy(
    thread_service: &dyn SessionThreadService,
    run_id: TurnRunId,
    updates: Vec<QueuedMessageStatusUpdate>,
) -> RejectFlipOutcome {
    let mut outcome = RejectFlipOutcome {
        flipped: Vec::new(),
        settled: Vec::new(),
    };
    for update in updates {
        match thread_service
            .mark_message_rejected_busy(&update.scope, &update.thread_id, update.message_id)
            .await
        {
            Ok(_) => outcome.flipped.push(update.message_id),
            Err(error) => {
                let already_settled = matches!(
                    thread_service
                        .read_thread_message(&update.scope, &update.thread_id, update.message_id)
                        .await,
                    Ok(Some(row)) if row.status != MessageStatus::Queued
                );
                if already_settled {
                    outcome.settled.push(update.message_id);
                }
                tracing::debug!(
                    component = "host_input_queue",
                    operation = "reject_unconsumed",
                    %run_id,
                    thread_id = %update.thread_id,
                    message_id = %update.message_id,
                    already_settled,
                    %error,
                    "queued-message reject skipped during terminal reconciliation"
                );
            }
        }
    }
    outcome
}

fn poisoned_lock(operation: &'static str) -> HostInputQueueError {
    // Carry the diagnosis to the log before collapsing to the sanitized
    // variant (`.claude/rules/error-handling.md`: never a bare `map_err(|_|)`).
    tracing::debug!(
        component = "host_input_queue",
        operation,
        "input queue state lock poisoned"
    );
    HostInputQueueError::Internal
}

// ---------------------------------------------------------------------------
// In-memory backend: the shared model in a per-run map under a mutex
// ---------------------------------------------------------------------------

pub struct InMemoryHostInputQueue {
    state: Arc<Mutex<HashMap<TurnRunId, RunQueueModel>>>,
    thread_service: Arc<dyn SessionThreadService>,
    ack_effect_handler: OnceLock<Arc<dyn HostInputAckEffectHandler>>,
    handoff: SteerHandoffWiring,
}

impl std::fmt::Debug for InMemoryHostInputQueue {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InMemoryHostInputQueue")
            .finish_non_exhaustive()
    }
}

impl InMemoryHostInputQueue {
    pub fn new(thread_service: Arc<dyn SessionThreadService>) -> Self {
        Self {
            state: Arc::new(Mutex::new(HashMap::new())),
            thread_service,
            ack_effect_handler: OnceLock::new(),
            handoff: SteerHandoffWiring::default(),
        }
    }

    /// Record unread steers of ended runs in `store` (see
    /// [`HostInputQueueReconcile::settle_unconsumed`]) instead of settling
    /// them `RejectedBusy`.
    pub fn with_steer_handoff(mut self, store: Arc<dyn crate::SteerHandoffStore>) -> Self {
        self.handoff.store = Some(store);
        self
    }

    /// Bind the callback used for deferred queue acknowledgment effects.
    /// Composition performs this once after both the queue and resolver exist.
    pub fn bind_ack_effect_handler(
        &self,
        handler: Arc<dyn HostInputAckEffectHandler>,
    ) -> Result<(), HostInputQueueError> {
        self.ack_effect_handler
            .set(handler)
            .map_err(|rejected_handler| {
                tracing::debug!(
                    handler_type = std::any::type_name_of_val(rejected_handler.as_ref()),
                    "input queue ack effect handler already bound"
                );
                HostInputQueueError::Unavailable {
                    reason: "input queue ack effect handler already bound".to_string(),
                }
            })
    }

    /// Bind the dispatcher told about each recorded handoff. Composition
    /// binds it once, after the coordinator exists; a handoff recorded
    /// before that waits durably for the dispatcher's startup sweep.
    pub fn bind_steer_handoff_dispatch(
        &self,
        dispatch: Arc<dyn crate::SteerHandoffDispatch>,
    ) -> Result<(), HostInputQueueError> {
        self.handoff.bind_dispatch(dispatch)
    }

    async fn enqueue_scoped(
        &self,
        request: EnqueueQueuedMessageRequest,
        turn_scope: Option<TurnScope>,
    ) -> Result<HostInputEnvelope, HostInputQueueError> {
        let disposition = {
            let mut state = self.state.lock().map_err(|_| poisoned_lock("enqueue"))?;
            let model = state.entry(request.run_id).or_default();
            let disposition = model.enqueue_dedup_scoped(
                request.input.clone(),
                QueuedMessageStatusUpdate {
                    turn_id: request.turn_id,
                    scope: request.scope,
                    thread_id: request.thread_id,
                    message_id: request.message_id,
                },
                turn_scope,
            )?;
            if let EnqueueDisposition::Inserted { sequence } = &disposition {
                model.attach_ack_effect(*sequence, request.ack_effect.clone())?;
            }
            disposition
        };
        let envelope = match disposition {
            EnqueueDisposition::Inserted { sequence }
            | EnqueueDisposition::Duplicate { sequence } => envelope_for(sequence, request.input),
            EnqueueDisposition::AlreadyConsumed { flip } => {
                // Idempotent replay of a consumed message whose `Submitted`
                // flip is still pending: repair the stale row instead of
                // re-minting a sequence (which would deliver it twice).
                let sequence = flip.sequence;
                let flipped =
                    flip_submitted(self.thread_service.as_ref(), request.run_id, vec![flip]).await;
                if !flipped.is_empty() {
                    let mut state = self.state.lock().map_err(|_| poisoned_lock("enqueue"))?;
                    if let Some(model) = state.get_mut(&request.run_id) {
                        model.confirm_submit_flips(&flipped);
                    }
                }
                envelope_for(sequence, request.input)
            }
        }?;
        self.retry_pending_ack_effects(request.run_id, &[]).await?;
        Ok(envelope)
    }

    async fn retry_pending_ack_effects(
        &self,
        run_id: TurnRunId,
        due: &[PendingAckEffect],
    ) -> Result<(), HostInputQueueError> {
        let Some(handler) = self.ack_effect_handler.get().cloned() else {
            return Ok(());
        };
        let pending = if due.is_empty() {
            let state = self
                .state
                .lock()
                .map_err(|_| poisoned_lock("retry_ack_effects"))?;
            state
                .get(&run_id)
                .map(|model| model.pending_ack_effects.clone())
                .unwrap_or_default()
        } else {
            due.to_vec()
        };
        let mut confirmed = Vec::new();
        for pending in pending {
            match handler.handle_ack_effect(pending.effect.clone()).await {
                Ok(()) => confirmed.push(pending.sequence),
                Err(error) => tracing::debug!(
                    component = "host_input_queue",
                    operation = "retry_ack_effects",
                    %run_id,
                    %error,
                    "input acknowledgment effect failed; retaining it for retry"
                ),
            }
        }
        if !confirmed.is_empty() {
            let mut state = self
                .state
                .lock()
                .map_err(|_| poisoned_lock("confirm_ack_effects"))?;
            if let Some(model) = state.get_mut(&run_id) {
                model.confirm_ack_effects(&confirmed);
                if model.is_settled() {
                    state.remove(&run_id);
                }
            }
        }
        Ok(())
    }
}

#[async_trait]
impl HostInputEnqueuePort for InMemoryHostInputQueue {
    async fn enqueue_queued_message(
        &self,
        request: EnqueueQueuedMessageRequest,
    ) -> Result<HostInputEnvelope, HostInputQueueError> {
        self.enqueue_scoped(request, None).await
    }

    async fn enqueue_user_steer(
        &self,
        request: EnqueueQueuedMessageRequest,
        turn_scope: TurnScope,
    ) -> Result<HostInputEnvelope, HostInputQueueError> {
        self.enqueue_scoped(request, Some(turn_scope)).await
    }

    async fn withdraw_user_steer(
        &self,
        run_id: TurnRunId,
        message_id: ThreadMessageId,
    ) -> Result<SteerWithdrawal, HostInputQueueError> {
        let mut state = self.state.lock().map_err(|_| poisoned_lock("withdraw"))?;
        Ok(match state.get_mut(&run_id) {
            Some(model) => model.withdraw(message_id),
            None => SteerWithdrawal::Unknown,
        })
    }
}

#[async_trait]
impl HostInputQueue for InMemoryHostInputQueue {
    async fn confirm_read(
        &self,
        run_id: TurnRunId,
        tokens: Vec<LoopInputAckToken>,
    ) -> Result<(), HostInputQueueError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| poisoned_lock("confirm_read"))?;
        if let Some(model) = state.get_mut(&run_id) {
            model.confirm_read(&tokens);
        }
        Ok(())
    }

    async fn next_after(
        &self,
        run_id: TurnRunId,
        after: LoopInputCursorToken,
        limit: usize,
    ) -> Result<HostInputBatch, HostInputQueueError> {
        let after_sequence = cursor_sequence(&after)?;
        let state = self.state.lock().map_err(|_| poisoned_lock("next_after"))?;
        let Some(model) = state.get(&run_id) else {
            return Ok(HostInputBatch {
                inputs: Vec::new(),
                next_cursor: after,
            });
        };
        model.scan_after(after_sequence, limit)
    }

    async fn ack_consumed(
        &self,
        run_id: TurnRunId,
        tokens: Vec<LoopInputAckToken>,
    ) -> Result<(), HostInputQueueError> {
        // Record acks and prune consumed entries UNDER the lock, before any
        // await: a concurrent `next_after` must never observe a consumed input
        // as unacked and redeliver it while the (async) transcript flip below
        // is still in flight.
        let (due_flips, due_ack_effects) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| poisoned_lock("ack_consumed"))?;
            let Some(model) = state.get_mut(&run_id) else {
                return Ok(());
            };
            let outcome = model.validate_and_ack(&tokens)?;
            (outcome.due_flips, outcome.due_ack_effects)
        };
        let flipped = flip_submitted(self.thread_service.as_ref(), run_id, due_flips).await;
        if !flipped.is_empty() {
            let mut state = self
                .state
                .lock()
                .map_err(|_| poisoned_lock("ack_consumed"))?;
            if let Some(model) = state.get_mut(&run_id) {
                model.confirm_submit_flips(&flipped);
            }
        }
        self.retry_pending_ack_effects(run_id, &due_ack_effects)
            .await?;
        Ok(())
    }
}

#[async_trait]
impl HostInputQueueReconcile for InMemoryHostInputQueue {
    async fn settle_unconsumed(
        &self,
        run_id: TurnRunId,
        ending: RunQueueEnding,
    ) -> Result<Vec<ThreadMessageId>, HostInputQueueError> {
        // The run is terminal: close the queue (rejecting late enqueues,
        // no-oping late duplicate acks) and claim every unacked entry. The
        // map entry is removed only once every claimed row settled — a
        // transient failure keeps the record so a repeated reconciliation
        // retries it — which is also the in-memory lifetime bound: settled
        // queues do not accumulate for the daemon's lifetime.
        let (submit_flips, reject_flips, handoff) = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| poisoned_lock("settle_unconsumed"))?;
            match state.get_mut(&run_id) {
                Some(model) => {
                    let claimed = model.close_and_claim(self.handoff.enabled());
                    (
                        model.due_submit_flips(),
                        model.due_reject_flips(),
                        claimed.into_handoff(run_id, ending),
                    )
                }
                None => (Vec::new(), Vec::new(), None),
            }
        };
        // The handoff is durable before anything else settles; a failure
        // leaves the claims in the closed model for a repeated call to
        // re-derive (in-memory: until the process ends).
        if let Some(handoff) = handoff {
            self.handoff.record(handoff).await?;
        }
        if ending == RunQueueEnding::Stopped {
            self.handoff.stop(run_id).await?;
        }
        let submitted = flip_submitted(self.thread_service.as_ref(), run_id, submit_flips).await;
        let reject_outcome =
            flip_rejected_busy(self.thread_service.as_ref(), run_id, reject_flips).await;
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| poisoned_lock("settle_unconsumed"))?;
            if let Some(model) = state.get_mut(&run_id) {
                model.confirm_submit_flips(&submitted);
                model.confirm_reject_flips(&reject_outcome.confirmable());
                if model.is_settled() {
                    state.remove(&run_id);
                }
            }
        }
        self.retry_pending_ack_effects(run_id, &[]).await?;
        Ok(reject_outcome.flipped)
    }
}

/// A queue's handoff wiring: the durable store (set at construction) and the
/// dispatcher told about each record (bound late). Shared by both backends.
#[derive(Default)]
pub(crate) struct SteerHandoffWiring {
    pub(crate) store: Option<Arc<dyn crate::SteerHandoffStore>>,
    dispatch: OnceLock<Arc<dyn crate::SteerHandoffDispatch>>,
}

impl SteerHandoffWiring {
    pub(crate) fn enabled(&self) -> bool {
        self.store.is_some()
    }

    pub(crate) fn bind_dispatch(
        &self,
        dispatch: Arc<dyn crate::SteerHandoffDispatch>,
    ) -> Result<(), HostInputQueueError> {
        self.dispatch.set(dispatch).map_err(|rejected| {
            tracing::debug!(
                dispatch_type = std::any::type_name_of_val(rejected.as_ref()),
                "steer handoff dispatcher already bound"
            );
            HostInputQueueError::Unavailable {
                reason: "steer handoff dispatcher already bound".to_string(),
            }
        })
    }

    pub(crate) async fn record(
        &self,
        handoff: crate::SteerHandoff,
    ) -> Result<(), HostInputQueueError> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let thread_scope = handoff.thread_scope.clone();
        let thread_id = handoff.thread_id.clone();
        store.record(handoff).await?;
        if let Some(dispatch) = self.dispatch.get() {
            dispatch.handoff_recorded(&thread_scope, &thread_id).await;
        }
        Ok(())
    }

    pub(crate) async fn stop(&self, run_id: TurnRunId) -> Result<(), HostInputQueueError> {
        match &self.store {
            Some(store) => store.stop(run_id).await,
            None => Ok(()),
        }
    }
}

// ---------------------------------------------------------------------------
// Cursor/ack token wire format — shared with the durable queue so both
// backends speak the identical `input-cursor:{n}` / `input-ack:{n}` tokens. A
// durable queue rehydrated after restart must mint the same tokens the loop's
// persisted input cursor already references.
// ---------------------------------------------------------------------------

pub(crate) fn envelope_for(
    sequence: u64,
    input: LoopInput,
) -> Result<HostInputEnvelope, HostInputQueueError> {
    Ok(HostInputEnvelope {
        input,
        cursor: cursor_token(sequence)?,
        ack_token: ack_token(sequence)?,
    })
}

pub(crate) fn cursor_sequence(token: &LoopInputCursorToken) -> Result<u64, HostInputQueueError> {
    if token.is_origin() {
        return Ok(0);
    }
    token
        .as_str()
        .strip_prefix("input-cursor:")
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| HostInputQueueError::InvalidCursor {
            reason: "input cursor token is malformed".to_string(),
        })
}

pub(crate) fn cursor_token(sequence: u64) -> Result<LoopInputCursorToken, HostInputQueueError> {
    LoopInputCursorToken::new(format!("input-cursor:{sequence}")).map_err(|error| {
        // Carry the validation cause to the log before collapsing to the
        // sanitized variant (`.claude/rules/error-handling.md`: never a bare
        // `map_err(|_|)`).
        tracing::debug!(
            component = "host_input_queue",
            operation = "cursor_token",
            sequence,
            %error,
            "cursor token construction failed"
        );
        HostInputQueueError::Internal
    })
}

pub(crate) fn ack_token(sequence: u64) -> Result<LoopInputAckToken, HostInputQueueError> {
    LoopInputAckToken::new(format!("input-ack:{sequence}")).map_err(|error| {
        // Carry the validation cause to the log before collapsing to the
        // sanitized variant (`.claude/rules/error-handling.md`: never a bare
        // `map_err(|_|)`).
        tracing::debug!(
            component = "host_input_queue",
            operation = "ack_token",
            sequence,
            %error,
            "ack token construction failed"
        );
        HostInputQueueError::Internal
    })
}

pub(crate) fn ack_sequence(token: &LoopInputAckToken) -> Result<u64, HostInputQueueError> {
    token
        .as_str()
        .strip_prefix("input-ack:")
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| HostInputQueueError::InvalidCursor {
            reason: "input ack token is malformed".to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironclaw_host_api::ids::{AgentId, TenantId};

    /// Unscoped enqueue, as the tests drive it.
    trait EnqueueDedup {
        fn enqueue_dedup(
            &mut self,
            input: LoopInput,
            status: QueuedMessageStatusUpdate,
        ) -> Result<EnqueueDisposition, HostInputQueueError>;
    }

    impl EnqueueDedup for RunQueueModel {
        fn enqueue_dedup(
            &mut self,
            input: LoopInput,
            status: QueuedMessageStatusUpdate,
        ) -> Result<EnqueueDisposition, HostInputQueueError> {
            self.enqueue_dedup_scoped(input, status, None)
        }
    }
    use ironclaw_turns::{LoopMessageRef, TurnScope};

    #[test]
    fn recently_consumed_dedup_window_stays_bounded() {
        let mut model = RunQueueModel::default();
        let scope = ThreadScope {
            tenant_id: TenantId::new("tenant-iq").unwrap(),
            agent_id: AgentId::new("agent-iq").unwrap(),
            project_id: None,
            owner_user_id: None,
            mission_id: None,
        };
        let thread_id = ThreadId::new("thread-iq").unwrap();
        let mut oldest_message_id = None;

        for index in 0..=RECENTLY_CONSUMED_DEDUP_LIMIT {
            let message_id = ThreadMessageId::new();
            if index == 0 {
                oldest_message_id = Some(message_id);
            }
            let message_ref = format!("msg:{message_id}");
            let disposition = model
                .enqueue_dedup(
                    LoopInput::Steering {
                        message_ref: LoopMessageRef::new(&message_ref).unwrap(),
                    },
                    QueuedMessageStatusUpdate {
                        turn_id: TurnId::new(),
                        scope: scope.clone(),
                        thread_id: thread_id.clone(),
                        message_id,
                    },
                )
                .expect("enqueue distinct input");
            let EnqueueDisposition::Inserted { sequence } = disposition else {
                panic!("distinct input must insert");
            };
            model
                .validate_and_ack(&[ack_token(sequence).unwrap()])
                .expect("ack inserted input");
            model.confirm_submit_flips(&[sequence]);
        }

        assert_eq!(
            model.recently_consumed.len(),
            RECENTLY_CONSUMED_DEDUP_LIMIT,
            "consumed replay identities must not grow durable state without bound"
        );
        assert!(
            !model
                .recently_consumed
                .iter()
                .any(|consumed| Some(consumed.message_id) == oldest_message_id),
            "the oldest consumed identity must be evicted when the window is full"
        );
    }

    #[test]
    fn ack_effect_moves_to_pending_and_survives_rehydration() {
        let mut model = RunQueueModel::default();
        let scope = ThreadScope {
            tenant_id: TenantId::new("tenant-iq").unwrap(),
            agent_id: AgentId::new("agent-iq").unwrap(),
            project_id: None,
            owner_user_id: None,
            mission_id: None,
        };
        let thread_id = ThreadId::new("thread-iq").unwrap();
        let message_id = ThreadMessageId::new();
        let effect = LoopInputAckEffect {
            child_scope: TurnScope::new(
                TenantId::new("tenant-iq").unwrap(),
                Some(AgentId::new("agent-iq").unwrap()),
                None,
                thread_id.clone(),
            ),
            parent_run_id: TurnRunId::new(),
            child_run_id: TurnRunId::new(),
        };
        let disposition = model
            .enqueue_dedup(
                LoopInput::Steering {
                    message_ref: LoopMessageRef::new("msg:effect").unwrap(),
                },
                QueuedMessageStatusUpdate {
                    turn_id: TurnId::new(),
                    scope: scope.clone(),
                    thread_id: thread_id.clone(),
                    message_id,
                },
            )
            .expect("enqueue");
        let EnqueueDisposition::Inserted { sequence } = disposition else {
            panic!("effect entry must insert");
        };
        model.entries[0].ack_effect = Some(effect.clone());

        let outcome = model
            .validate_and_ack(&[ack_token(sequence).unwrap()])
            .expect("ack");
        assert_eq!(outcome.due_ack_effects.len(), 1);
        assert_eq!(outcome.due_ack_effects[0].effect, effect);

        let mut rehydrated: RunQueueModel =
            serde_json::from_slice(&serde_json::to_vec(&model).unwrap()).unwrap();
        assert_eq!(rehydrated.pending_ack_effects, outcome.due_ack_effects);

        // The transcript flip and callback are two obligations for the same
        // consumed input, so they occupy one capacity slot, not two.
        rehydrated.confirm_submit_flips(&[sequence]);
        for index in 1..MAX_QUEUED_INPUTS_PER_RUN {
            rehydrated
                .enqueue_dedup(
                    LoopInput::Steering {
                        message_ref: LoopMessageRef::new(format!("msg:effect-{index}")).unwrap(),
                    },
                    QueuedMessageStatusUpdate {
                        turn_id: TurnId::new(),
                        scope: scope.clone(),
                        thread_id: thread_id.clone(),
                        message_id: ThreadMessageId::new(),
                    },
                )
                .expect("each distinct tracked input gets one capacity slot");
        }
        let overflow = rehydrated.enqueue_dedup(
            LoopInput::Steering {
                message_ref: LoopMessageRef::new("msg:effect-overflow").unwrap(),
            },
            QueuedMessageStatusUpdate {
                turn_id: TurnId::new(),
                scope,
                thread_id,
                message_id: ThreadMessageId::new(),
            },
        );
        assert!(matches!(
            overflow,
            Err(HostInputQueueError::CapacityExhausted)
        ));
    }
}
