import { describe, expect, test } from "bun:test";

import { AGENT_RETRY_PAYLOAD_FORMAT } from "../run/retry.ts";
import { AgentRetryWorker, NodeOutboxTopicWorker } from "./worker.ts";
import { TaskStore } from "../storage/task-store.ts";

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
    const worker = new AgentRetryWorker(store, async (taskId, retry) => {
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
    const worker = new AgentRetryWorker(store, async () => {
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
    const worker = new AgentRetryWorker(store, async () => {
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

describe("node outbox topic worker", () => {
  function queue(store: TaskStore, topic = "tracker.linear.input.route"): void {
    store.append({
      taskId: "task-1",
      type: "task.renamed",
      payload: { title: "Routed" },
      actor: { kind: "system", component: "test" },
    }, [{ topic, payload: { value: "hello" }, dedupeKey: `${topic}:1` }]);
  }

  test("delivers only its topic exactly once", async () => {
    const now = 1_000;
    using store = state(() => now);
    queue(store);
    queue(store, "another.topic");
    const deliveries: unknown[] = [];
    const worker = new NodeOutboxTopicWorker(store, {
      topic: "tracker.linear.input.route",
      parse(value) { return value as { value: string }; },
      async execute(taskId, value) {
        deliveries.push({ taskId, value });
        return "executed";
      },
    }, { workerId: "outbox-1", now: () => now });

    expect(await worker.runOnce()).toBe(1);
    expect(deliveries).toEqual([{ taskId: "task-1", value: { value: "hello" } }]);
    expect(store.listOutbox("delivered")).toHaveLength(1);
    expect(store.listOutbox("pending")).toEqual([
      expect.objectContaining({ topic: "another.topic" }),
    ]);
    expect(await worker.runOnce()).toBe(0);
  });

  test("retries transient delivery failure and dead-letters malformed payloads", async () => {
    let now = 1_000;
    using store = state(() => now);
    queue(store);
    let attempts = 0;
    const worker = new NodeOutboxTopicWorker(store, {
      topic: "tracker.linear.input.route",
      parse(value) { return value as { value: string }; },
      async execute() {
        attempts += 1;
        if (attempts === 1) throw new Error("agent still owns the writer lease");
        return "executed";
      },
    }, { workerId: "outbox-1", now: () => now, onError: () => undefined });

    await worker.runOnce();
    expect(store.listOutbox("pending")[0]).toMatchObject({
      attempts: 1,
      availableAt: 6_000,
      lastError: "agent still owns the writer lease",
    });
    now = 6_000;
    expect(await worker.runOnce()).toBe(1);
    expect(store.listOutbox("delivered")[0]?.attempts).toBe(2);

    store.append({
      taskId: "task-1",
      type: "task.renamed",
      payload: { title: "Malformed" },
      actor: { kind: "system", component: "test" },
    }, [{
      topic: "tracker.linear.input.route",
      payload: { broken: true },
      dedupeKey: "malformed",
    }]);
    const strict = new NodeOutboxTopicWorker(store, {
      topic: "tracker.linear.input.route",
      parse() { throw new Error("invalid route payload"); },
      async execute() { return "executed"; },
    }, { workerId: "outbox-2", now: () => now, onError: () => undefined });
    expect(await strict.runOnce()).toBe(1);
    expect(store.listOutbox("dead")[0]).toMatchObject({ lastError: "invalid route payload" });
  });
});
