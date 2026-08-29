import { describe, expect, test } from "bun:test";

import { createManagedAgentRetryExecutor } from "../src/run/retry-executor.ts";
import { AGENT_RETRY_PAYLOAD_FORMAT } from "../src/run/retry.ts";
import { TaskAgentSessionStore } from "../src/storage/task-agent-session-store.ts";
import { TaskStore } from "../src/storage/task-store.ts";

function state() {
  const store = new TaskStore(":memory:", { clock: () => 1_000, createId: () => crypto.randomUUID() });
  store.append({
    taskId: "task-1",
    type: "task.created",
    payload: { title: "Retry", intent: "Retry exactly once", intentLevel: "fully_delegated" },
    actor: { kind: "operator" },
  });
  for (const to of ["queued", "provisioning", "running", "waiting_external"] as const) {
    store.append({
      taskId: "task-1",
      type: "task.state_changed",
      payload: { to },
      actor: { kind: "system", component: "test" },
    });
  }
  const sessions = new TaskAgentSessionStore(
    store.database,
    () => 1_000,
    () => "01900000-0000-7000-8000-000000000001",
  );
  sessions.ensure("task-1", "claude-personal");
  sessions.bindMcp({
    taskId: "task-1",
    grantId: "grant-1",
    controlDirectory: "/identity/control/tasks/t-retry",
    configPath: "/identity/control/tasks/t-retry/mcp.json",
  });
  sessions.markRunStarted("task-1");
  return { store, sessions };
}

const payload = {
  schema: AGENT_RETRY_PAYLOAD_FORMAT,
  sessionId: "01900000-0000-7000-8000-000000000001",
  runNumber: 1,
} as const;

describe("managed agent retry executor", () => {
  test("runs only the still-current waiting session", async () => {
    const { store, sessions } = state();
    let calls = 0;
    const execute = createManagedAgentRetryExecutor({
      tasks: store,
      session: (taskId) => sessions.get(taskId),
      run: async () => { calls += 1; return 0; },
    });
    expect(await execute("task-1", payload)).toBe("executed");
    expect(await execute("task-1", { ...payload, runNumber: 2 })).toBe("skipped");
    expect(calls).toBe(1);
    store.close();
  });

  test("retries infrastructure failure only while the task remains externally waiting", async () => {
    const { store, sessions } = state();
    const execute = createManagedAgentRetryExecutor({
      tasks: store,
      session: (taskId) => sessions.get(taskId),
      run: async () => 1,
    });
    await expect(execute("task-1", payload)).rejects.toThrow("failed before advancing");
    store.append({
      taskId: "task-1",
      type: "task.state_changed",
      payload: { to: "failed", reason: "Agent failed after starting" },
      actor: { kind: "system", component: "test" },
    });
    expect(await execute("task-1", payload)).toBe("skipped");
    store.close();
  });
});
