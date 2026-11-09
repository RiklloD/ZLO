import assert from "node:assert/strict";
import { beforeEach, test, vi } from "vitest";

const { apiListProjects, apiListThreads } = vi.hoisted(() => ({
  apiListProjects: vi.fn(),
  apiListThreads: vi.fn(),
}));

vi.mock("../../../lib/api", () => ({
  getProject: vi.fn(),
  listProjects: apiListProjects,
  listThreads: apiListThreads,
}));

import {
  fetchProjectThreads,
  fetchProjectsOverview,
} from "./projects-api";

beforeEach(() => {
  vi.clearAllMocks();
});

test("project overview rejects records missing required wire fields", async () => {
  apiListProjects.mockResolvedValue({
    projects: [{}],
    total_projects: 1,
    active_projects: 1,
    archived_projects: 0,
  });

  await assert.rejects(fetchProjectsOverview(), /invalid project response/);
});

test("project thread lists reject records missing required wire fields", async () => {
  apiListThreads.mockResolvedValue({ threads: [{}], next_cursor: null });

  await assert.rejects(
    fetchProjectThreads("project-1"),
    /invalid project thread response/,
  );
});
