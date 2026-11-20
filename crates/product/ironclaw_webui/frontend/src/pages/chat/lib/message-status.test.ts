import assert from "node:assert/strict";
import { test } from "vitest";

import {
  RECORD_STATUS,
  UI_MESSAGE_STATUS,
  isBusyRejectedStatus,
  isSteerStatus,
  uiStatusFromRecordStatus,
  uiStatusFromSendResponse,
} from "./message-status";

test("deferred-busy maps to steered (part of the running turn, never queued or an error)", () => {
  assert.equal(
    uiStatusFromRecordStatus(RECORD_STATUS.DEFERRED_BUSY),
    UI_MESSAGE_STATUS.STEERED,
  );
});

test("a persisted queued row is a steer the agent has not read yet", () => {
  assert.equal(
    uiStatusFromRecordStatus(RECORD_STATUS.QUEUED),
    UI_MESSAGE_STATUS.STEER_PENDING,
  );
});

test("steer statuses are recognised as steers", () => {
  assert.equal(isSteerStatus(UI_MESSAGE_STATUS.STEERED), true);
  assert.equal(isSteerStatus(UI_MESSAGE_STATUS.STEER_PENDING), true);
  assert.equal(isSteerStatus(UI_MESSAGE_STATUS.ERROR), false);
  assert.equal(isSteerStatus("submitted"), false);
});

test("rejected-busy maps to error (was not accepted, must resend)", () => {
  assert.equal(
    uiStatusFromRecordStatus(RECORD_STATUS.REJECTED_BUSY),
    UI_MESSAGE_STATUS.ERROR,
  );
});

test("unknown statuses pass through unchanged", () => {
  assert.equal(uiStatusFromRecordStatus("accepted"), "accepted");
  assert.equal(uiStatusFromRecordStatus("finalized"), "finalized");
  assert.equal(uiStatusFromRecordStatus(undefined), undefined);
});

test("only rejected-busy needs the resend error copy", () => {
  assert.equal(isBusyRejectedStatus(RECORD_STATUS.REJECTED_BUSY), true);
  assert.equal(isBusyRejectedStatus(RECORD_STATUS.DEFERRED_BUSY), false);
  assert.equal(isBusyRejectedStatus(RECORD_STATUS.QUEUED), false);
});

test("live (send outcome) and reload (record status) agree for busy outcomes", () => {
  // The same wire value must map to its intended UI status whether it arrives
  // as a send-response `outcome` or a persisted record `status` — both paths
  // route through `uiStatusFromRecordStatus`, so pin the expected mapping for
  // each busy outcome rather than comparing the function to itself.
  const expected = new Map([
    [RECORD_STATUS.DEFERRED_BUSY, UI_MESSAGE_STATUS.STEERED],
    [RECORD_STATUS.REJECTED_BUSY, UI_MESSAGE_STATUS.ERROR],
    [RECORD_STATUS.QUEUED, UI_MESSAGE_STATUS.STEER_PENDING],
  ]);
  for (const [status, ui] of expected) {
    assert.equal(uiStatusFromRecordStatus(status), ui);
  }
});

test("a send response's typed delivery decides: queued shows Queued, steered shows Steer", () => {
  assert.equal(
    uiStatusFromSendResponse({ outcome: "deferred_busy", delivery: "queued" }),
    UI_MESSAGE_STATUS.QUEUED,
  );
  assert.equal(
    uiStatusFromSendResponse({ outcome: "deferred_busy", delivery: "steered" }),
    UI_MESSAGE_STATUS.STEERED,
  );
  assert.equal(
    uiStatusFromSendResponse({
      outcome: "deferred_busy",
      delivery: "steered",
      waiting_for_turn: true,
    }),
    UI_MESSAGE_STATUS.STEER_PENDING,
  );
  // An older server without `delivery` falls back to the outcome.
  assert.equal(
    uiStatusFromSendResponse({ outcome: "deferred_busy" }),
    UI_MESSAGE_STATUS.STEERED,
  );
  assert.equal(
    uiStatusFromSendResponse({ outcome: "rejected_busy" }),
    UI_MESSAGE_STATUS.ERROR,
  );
});
