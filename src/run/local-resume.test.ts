import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import type { StoredTaskAgentSession } from "../storage/task-agent-session-store.ts";
import { TaskStore } from "../storage/task-store.ts";
import {
  LOCAL_RESUME_ACTION_KIND,
  parseLocalResumePayload,
  planLocalResume,
  resumeLocalTasksOnOpen,
} from "./local-resume.ts";

const roots: string[] = [];
afterEach(() => {
  for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true });
});

function session(runCount = 2): StoredTaskAgentSession {
  return {
    taskId: "task-1",
    adapter: "claude-code-cli",
    identityId: "claude-personal",
    sessionId: "01900000-0000-7000-8000-000000000001",
    projectDirectoryName: "task-1",
    runCount,
    consecutiveRateLimits: 0,
    lastOutcome: "yielded",
    createdAt: 1,
    updatedAt: 2,
  };
}

function pausedTask(store: TaskStore): void {
  store.append({
    taskId: "task-1",
    type: "task.created",
    payload: { title: "Resume me", intent: "Continue after restart", intentLevel: "principles" },
    actor: { kind: "operator" },
  });
  store.append({
    taskId: "task-1",
    type: "task.state_changed",
    payload: { to: "queued" },
    actor: { kind: "operator" },
  });
  store.append({
    taskId: "task-1",
    type: "task.state_changed",
    payload: { to: "paused" },
    actor: { kind: "system", component: "terminal-supervisor" },
  });
}

describe("local resume marker", () => {
  test("binds restart intent to one exact started Claude run", () => {
    const planned = planLocalResume(session(), 5_000);
    expect(planned).toEqual({
      taskId: "task-1",
      kind: LOCAL_RESUME_ACTION_KIND,
      payload: {
        schema: "altitude.local-resume/v1",
        sessionId: "01900000-0000-7000-8000-000000000001",
        runNumber: 2,
      },
      runAt: 5_000,
      dedupeKey: `${LOCAL_RESUME_ACTION_KIND}:01900000-0000-7000-8000-000000000001:2`,
    });
    expect(parseLocalResumePayload(planned.payload)).toEqual({
      schema: "altitude.local-resume/v1",
      sessionId: "01900000-0000-7000-8000-000000000001",
      runNumber: 2,
    });
  });

  test("rejects stale shapes and sessions that never started", () => {
    expect(() => planLocalResume(session(0), 5_000)).toThrow("cannot resume");
    expect(() => parseLocalResumePayload({
      schema: "altitude.local-resume/v1",
      sessionId: "not-a-session",
      runNumber: 2,
    })).toThrow("invalid");
  });

  test("survives a database reopen and starts only the exact paused session", async () => {
    const root = mkdtempSync(join(tmpdir(), "altitude-local-resume-"));
    roots.push(root);
    const database = join(root, "altitude.db");
    let store = new TaskStore(database, { clock: () => 5_000 });
    pausedTask(store);
    store.scheduleAction(planLocalResume(session(), 5_000));
    store.close();

    store = new TaskStore(database, { clock: () => 5_000 });
    const started: string[] = [];
    expect(await resumeLocalTasksOnOpen({
      store,
      sessions: { get: () => session() },
      now: 5_000,
      workerId: "tui-open:test",
      start: async (taskId) => {
        started.push(taskId);
        return 0;
      },
    })).toEqual({ resumed: 1, deferred: 0 });
    expect(started).toEqual(["task-1"]);
    expect(store.listScheduledActions("completed")).toHaveLength(1);
    store.close();
  });

  test("completes a stale marker without launching and retains transient failure", async () => {
    using store = new TaskStore(":memory:", { clock: () => 5_000 });
    pausedTask(store);
    store.scheduleAction(planLocalResume(session(), 5_000));
    expect(await resumeLocalTasksOnOpen({
      store,
      sessions: { get: () => session(3) },
      now: 5_000,
      workerId: "tui-open:stale",
      start: async () => {
        throw new Error("must not launch");
      },
    })).toEqual({ resumed: 0, deferred: 0 });
    expect(store.listScheduledActions("completed")).toHaveLength(1);

    store.scheduleAction(planLocalResume(session(3), 6_000));
    expect(await resumeLocalTasksOnOpen({
      store,
      sessions: { get: () => session(3) },
      now: 6_000,
      workerId: "tui-open:failure",
      start: async () => 17,
    })).toEqual({ resumed: 0, deferred: 1 });
    expect(store.listScheduledActions("scheduled")[0]).toMatchObject({
      runAt: 11_000,
      lastError: "Claude restart exited with status 17",
    });
  });
});
