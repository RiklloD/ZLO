// Single source of truth for chat message status values and the
// wire/record-status -> UI-status mapping.
//
// The same logical message is rendered twice: optimistically by
// `useChat.send` (from a send response `outcome`) and durably by
// `messagesFromTimeline` (from a persisted `ThreadMessageRecord.status`).
// Both paths MUST map the same wire value to the same UI status, or a
// message flips appearance across a reload — e.g. a busy-deferred message
// showing a "queued" badge live but an "error" bubble after refresh
// (the bug this module exists to prevent).

// Wire/record status values produced by the backend (send `outcome` or
// persisted `ThreadMessageRecord.status`).
export const RECORD_STATUS = Object.freeze({
  // Sent while the thread was busy and steered into the running turn: the
  // agent reads it at its next step (a pending approval is declined for
  // it). Renders as a steer — never "queued", never an error.
  DEFERRED_BUSY: "deferred_busy",
  // Not delivered: the running turn could not take it (or a Stop ended the
  // turn before the agent read it). Renders as an error with resend copy.
  REJECTED_BUSY: "rejected_busy",
  // A persisted steer the agent has not read yet.
  QUEUED: "queued",
  // The run actively processing this message.
  RUNNING: "running",
} as const);

// UI-facing status values consumed by `MessageBubble`.
export const UI_MESSAGE_STATUS = Object.freeze({
  // Part of the running turn (live send outcome).
  STEERED: "steered",
  // A steer the agent has not read yet (persisted `queued` row).
  STEER_PENDING: "steer_pending",
  ERROR: "error",
  RUNNING: "running",
} as const);

// Map a wire/record status to the UI status `MessageBubble` renders.
// Unknown statuses pass through unchanged (e.g. "accepted", "finalized",
// "submitted") so this mapper only normalizes the values that diverge.
export function uiStatusFromRecordStatus(recordStatus) {
  switch (recordStatus) {
    case RECORD_STATUS.DEFERRED_BUSY:
      return UI_MESSAGE_STATUS.STEERED;
    case RECORD_STATUS.QUEUED:
      return UI_MESSAGE_STATUS.STEER_PENDING;
    case RECORD_STATUS.REJECTED_BUSY:
      return UI_MESSAGE_STATUS.ERROR;
    default:
      return recordStatus;
  }
}

// True for a user message that steered the running turn (read or not).
export function isSteerStatus(uiStatus) {
  return (
    uiStatus === UI_MESSAGE_STATUS.STEERED ||
    uiStatus === UI_MESSAGE_STATUS.STEER_PENDING
  );
}

// True when a busy outcome means the message was not delivered, so the UI
// must attach the durable "resend it" copy. A deferred-busy message was
// steered into the running turn and gets no error copy.
export function isBusyRejectedStatus(recordStatus) {
  return recordStatus === RECORD_STATUS.REJECTED_BUSY;
}
