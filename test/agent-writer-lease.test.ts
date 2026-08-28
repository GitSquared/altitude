import { describe, expect, test } from "bun:test";

import { InMemoryAgentWriterLeaseStore } from "../src/agent/writer-lease.ts";

describe("agent writer lease", () => {
  test("allows one writer and rejects a stale release after reacquisition", async () => {
    const store = new InMemoryAgentWriterLeaseStore();
    const first = await store.acquire({
      taskId: "task-1",
      sessionId: "session-1",
      holderId: "headless-1",
      mode: "headless",
      acquiredAtMs: 1,
    });
    expect(first?.revision).toBe(1);
    expect(await store.acquire({
      taskId: "task-1",
      sessionId: "session-1",
      holderId: "interactive-1",
      mode: "interactive",
      acquiredAtMs: 2,
    })).toBeUndefined();

    expect(await store.release(first!)).toBe(true);
    const second = await store.acquire({
      taskId: "task-1",
      sessionId: "session-1",
      holderId: "interactive-1",
      mode: "interactive",
      acquiredAtMs: 3,
    });
    expect(second?.revision).toBe(2);
    expect(await store.release(first!)).toBe(false);
    expect((await store.current("task-1"))?.holderId).toBe("interactive-1");
  });
});
