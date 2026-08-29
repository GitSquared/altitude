import { describe, expect, test } from "bun:test";

import { AGENT_RETRY_PAYLOAD_FORMAT } from "../src/run/retry.ts";
import { NodeScheduledActionWorker } from "../src/node/worker.ts";
import { TaskStore } from "../src/storage/task-store.ts";

function state(now: () => number): TaskStore {
  const store = new TaskStore(":memory:", { clock: now, createId: () => crypto.randomUUID() });
  store.append({
    taskId: "task-1",
    type: "task.created",
    payload: { title: "Retry", intent: "Resume without babysitting", intentLevel: "fully_delegated" },
    actor: { kind: "operator" },
  });
  return store;
}

const payload = {
  schema: AGENT_RETRY_PAYLOAD_FORMAT,
  sessionId: "01900000-0000-7000-8000-000000000001",
  runNumber: 1,
} as const;

describe("node scheduled action worker", () => {
  test("claims and completes a durable exact-session retry", async () => {
    let now = 1_000;
    using store = state(() => now);
    store.scheduleAction({ taskId: "task-1", kind: "agent.retry", payload, runAt: now });
    const executions: unknown[] = [];
    const worker = new NodeScheduledActionWorker(store, async (taskId, retry) => {
      executions.push({ taskId, retry });
      return "executed";
    }, { workerId: "worker-1", now: () => now });

    expect(await worker.runOnce()).toBe(1);
    expect(executions).toEqual([{ taskId: "task-1", retry: payload }]);
    expect(store.listScheduledActions("completed")).toHaveLength(1);
    now += 1;
    expect(await worker.runOnce()).toBe(0);
  });

  test("releases transient failures with bounded backoff and retries later", async () => {
    let now = 1_000;
    using store = state(() => now);
    store.scheduleAction({ taskId: "task-1", kind: "agent.retry", payload, runAt: now });
    let attempts = 0;
    const worker = new NodeScheduledActionWorker(store, async () => {
      attempts += 1;
      if (attempts === 1) throw new Error("runtime temporarily unavailable");
      return "executed";
    }, { workerId: "worker-1", now: () => now, onError: () => undefined });

    await worker.runOnce();
    expect(store.listScheduledActions("scheduled")[0]).toMatchObject({
      runAt: 6_000,
      attempts: 1,
      lastError: "runtime temporarily unavailable",
    });
    now = 5_999;
    expect(await worker.runOnce()).toBe(0);
    now = 6_000;
    expect(await worker.runOnce()).toBe(1);
    expect(attempts).toBe(2);
    expect(store.listScheduledActions("completed")[0]?.attempts).toBe(2);
  });

  test("dead-letters malformed retries without stealing work owned by another action kind", async () => {
    const now = 1_000;
    using store = state(() => now);
    store.scheduleAction({ taskId: "task-1", kind: "mystery", payload: {}, runAt: now });
    store.scheduleAction({ taskId: "task-1", kind: "agent.retry", payload: { schema: "wrong" }, runAt: now });
    let executions = 0;
    const worker = new NodeScheduledActionWorker(store, async () => {
      executions += 1;
      return "executed";
    }, { workerId: "worker-1", now: () => now, onError: () => undefined });

    expect(await worker.runOnce()).toBe(1);
    expect(executions).toBe(0);
    expect(store.listScheduledActions("dead")).toHaveLength(1);
    expect(store.listScheduledActions("scheduled")).toEqual([
      expect.objectContaining({ kind: "mystery" }),
    ]);
  });
});
