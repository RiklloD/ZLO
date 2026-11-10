import assert from "node:assert/strict";
import { test } from "vitest";

import {
  formatProjectDate,
  formatProjectRole,
  formatProjectRelativeTime,
  formatProjectState,
  formatThreadState,
  formatThreadType,
  projectStateTone,
  projectCount,
  summarizeOverview,
  threadPresentation,
} from "./projects-presenters";

const t = (key, params = {}) => {
  const translations = {
    "projects.count.steps": "Steps: {count}",
    "projects.relative.hoursAgo": "{count} localized hours ago",
    "projects.relative.inMinutes": "in {count} localized minutes",
    "projects.relative.noActivity": "Localized no activity",
    "projects.date.notAvailable": "Localized date unavailable",
    "projects.projectRole.owner": "Localized owner",
    "projects.projectRole.unknown": "Localized unknown project role",
    "projects.status.active": "Localized active",
    "projects.status.archived": "Localized archived project",
    "projects.thread.type.missionRun": "Localized mission run",
    "projects.threadState.done": "Localized done",
  };
  if (translations[key]) {
    return translations[key].replace(/\{(\w+)\}/g, (match, name) =>
      params[name] !== undefined ? params[name] : match
    );
  }
  return key;
};

test("project enum formatters localize API-backed state and role values", () => {
  assert.equal(formatProjectState("active", t), "Localized active");
  assert.equal(formatProjectState("archived", t), "Localized archived project");
  assert.equal(formatProjectState("unexpected", t), "unexpected");
  assert.equal(formatProjectRole("owner", t), "Localized owner");
  assert.equal(formatProjectRole(null, t), "Localized unknown project role");
  assert.equal(formatProjectRole("unexpected", t), "unexpected");
  assert.equal(projectStateTone("active"), "success");
  assert.equal(projectStateTone("archived"), "muted");
  assert.equal(formatThreadState("Done", t), "Localized done");
  assert.equal(formatThreadType("mission_run", t), "Localized mission run");
  assert.equal(formatThreadType("ad_hoc", t), "ad hoc");
});

test("project summary uses authoritative API lifecycle counts beyond the loaded page", () => {
  assert.deepEqual(
    summarizeOverview({
      projects: [{ state: "active" }],
      lifecycleCounts: {
        total: 501,
        active: 450,
        archived: 51,
      },
    }),
    {
      totalProjects: 501,
      activeProjects: 450,
      archivedProjects: 51,
    },
  );
});

test("threadPresentation interpolates fallback title without a translator", () => {
  assert.equal(threadPresentation({ id: "abcdefghij" }, null).title, "Thread abcdefgh");
});

test("date and relative-time formatters use localized fallback and count params", () => {
  const originalNow = Date.now;
  Date.now = () => new Date("2026-07-07T12:00:00Z").getTime();
  try {
    assert.equal(formatProjectDate(null, t), "Localized date unavailable");
    assert.equal(formatProjectRelativeTime(null, t), "Localized no activity");
    assert.equal(
      formatProjectRelativeTime("2026-07-07T10:00:00Z", t),
      "2 localized hours ago"
    );
    assert.equal(
      formatProjectRelativeTime("2026-07-07T12:05:00Z", t),
      "in 5 localized minutes"
    );
  } finally {
    Date.now = originalNow;
  }
});

test("count formatter interpolates the count", () => {
  assert.equal(projectCount(t, "steps", 7), "Steps: 7");
  assert.equal(projectCount(t, "steps", undefined), "Steps: 0");
});
