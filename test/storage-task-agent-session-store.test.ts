import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { TaskAgentSessionStore } from "../src/storage/task-agent-session-store.ts";
import { TaskStore } from "../src/storage/task-store.ts";

const directories: string[] = [];
afterEach(() => {
  for (const directory of directories.splice(0)) rmSync(directory, { recursive: true, force: true });
});

function database(): TaskStore {
  const root = mkdtempSync(join(tmpdir(), "altitude-session-store-"));
  directories.push(root);
  const store = new TaskStore(join(root, "altitude.db"));
  store.append({
    taskId: "task-1",
    type: "task.created",
    payload: { title: "Run", intent: "Run the capsule", intentLevel: "principles" },
    actor: { kind: "operator" },
  });
  return store;
}

describe("durable task agent sessions", () => {
  test("keeps one fixed Claude session and stores no bearer secret", () => {
    using store = database();
    let now = 1_000;
    const sessions = new TaskAgentSessionStore(
      store.database,
      () => now,
      () => "01900000-0000-7000-8000-000000000001",
    );
    const created = sessions.ensure("task-1", "claude-personal");
    expect(created.runCount).toBe(0);
    expect(created.sessionId).toBe("01900000-0000-7000-8000-000000000001");
    now = 2_000;
    const bound = sessions.bindMcp({
      taskId: "task-1",
      grantId: "grant-1",
      controlDirectory: "/identity/control/tasks/t-abc",
      configPath: "/identity/control/tasks/t-abc/mcp.json",
    });
    expect(bound.grantId).toBe("grant-1");
    expect(sessions.markRunStarted("task-1").runCount).toBe(1);
    expect(sessions.recordOutcome("task-1", "success").lastOutcome).toBe("success");
    expect(sessions.recordOutcome("task-1", "rate_limited").consecutiveRateLimits).toBe(1);
    expect(sessions.recordOutcome("task-1", "rate_limited").consecutiveRateLimits).toBe(2);
    expect(sessions.recordOutcome("task-1", "success").consecutiveRateLimits).toBe(0);
    expect(sessions.ensure("task-1", "claude-personal").sessionId).toBe(created.sessionId);
    const encoded = JSON.stringify(store.database.query("SELECT * FROM task_agent_sessions").get());
    expect(encoded).not.toContain("Bearer");
    expect(encoded).not.toContain("token");
  });

  test("refuses to silently move a session into another identity", () => {
    using store = database();
    const sessions = new TaskAgentSessionStore(
      store.database,
      () => 1_000,
      () => "01900000-0000-7000-8000-000000000002",
    );
    sessions.ensure("task-1", "claude-personal");
    expect(() => sessions.ensure("task-1", "claude-work")).toThrow("bound to identity");
  });
});
