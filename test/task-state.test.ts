import { describe, expect, test } from "bun:test";

import { TaskStore } from "../src/storage/task-store.ts";
import { PORTABLE_TASK_STATE_FORMAT, portableTaskStateMember } from "../src/run/task-state.ts";

describe("portable task state", () => {
  test("carries the replayable task projection and its additive event history", () => {
    using store = new TaskStore(":memory:", { clock: () => 42, createId: () => crypto.randomUUID() });
    store.append({
      taskId: "task-1",
      type: "task.created",
      payload: { title: "Fixture", intent: "Keep intent portable", intentLevel: "principles" },
      actor: { kind: "operator" },
    });
    store.append({
      taskId: "task-1",
      type: "task.state_changed",
      payload: { to: "queued", reason: "Ready" },
      actor: { kind: "operator" },
    });

    const member = portableTaskStateMember(store, "task-1");
    const value = JSON.parse(new TextDecoder().decode(member.bytes)) as {
      schema: string;
      task: { status: string; revision: number };
      events: Array<{ sequence: number }>;
    };
    expect(String(member.path)).toBe("state/task.json");
    expect(value.schema).toBe(PORTABLE_TASK_STATE_FORMAT);
    expect(value.task).toMatchObject({ status: "queued", revision: 2 });
    expect(value.events.map((event) => event.sequence)).toEqual([1, 2]);
  });
});
