import { describe, expect, test } from "bun:test";

import {
  TASK_DISPATCH_ACTION_KIND,
  parseTaskDispatchPayload,
  taskDispatchAction,
} from "../src/run/dispatch.ts";
import { NodeScheduledActionKindWorker } from "../src/node/worker.ts";
import {
  decodePortableTaskState,
  portableTaskStateMember,
} from "../src/run/task-state.ts";
import { activateTransferredTask } from "../src/run/transfer-activation.ts";
import { TaskAgentSessionStore } from "../src/storage/task-agent-session-store.ts";
import { TaskStore } from "../src/storage/task-store.ts";

const CHECKPOINT = {
  capsuleId: "task-1",
  revision: 4,
  digest: `sha256:${"a".repeat(64)}` as const,
};

function transferredState(): ReturnType<typeof decodePortableTaskState> {
  using source = new TaskStore(":memory:", { clock: () => 1_000 });
  source.append({
    taskId: "task-1",
    type: "task.created",
    payload: {
      title: "Run remotely",
      intent: "Continue without babysitting",
      intentLevel: "fully_delegated",
    },
    actor: { kind: "operator" },
  });
  for (const to of ["queued", "provisioning", "running", "paused", "transferring"] as const) {
    source.append({
      taskId: "task-1",
      type: "task.state_changed",
      payload: { to },
      actor: { kind: "system", component: "fixture" },
    });
  }
  return decodePortableTaskState(
    portableTaskStateMember(source, "task-1").bytes,
  );
}

describe("durable remote task dispatch", () => {
  test("activates a transferred task and records its run job atomically", () => {
    using target = new TaskStore(":memory:", { clock: () => 2_000 });
    const input = {
      store: target,
      sessions: new TaskAgentSessionStore(target.database),
      portableState: transferredState(),
      targetIdentityId: "claude-remote",
      targetNodeId: "straylight",
      transferId: "transfer-1",
      checkpoint: CHECKPOINT,
      activatedAt: 2_000,
    } as const;
    expect(activateTransferredTask(input).status).toBe("queued");
    expect(target.listScheduledActions("scheduled")).toEqual([
      expect.objectContaining({
        taskId: "task-1",
        kind: TASK_DISPATCH_ACTION_KIND,
        runAt: 2_000,
        dedupeKey: "transfer:transfer-1:run",
        payload: expect.objectContaining({
          targetNodeId: "straylight",
          checkpointDigest: CHECKPOINT.digest,
          checkpointRevision: CHECKPOINT.revision,
        }),
      }),
    ]);

    expect(activateTransferredTask(input).status).toBe("queued");
    expect(target.listScheduledActions()).toHaveLength(1);
  });

  test("the node claims only valid dispatch work and leaves other kinds alone", async () => {
    let now = 2_000;
    using target = new TaskStore(":memory:", { clock: () => now });
    target.append({
      taskId: "task-1",
      type: "task.created",
      payload: {
        title: "Remote",
        intent: "Run once",
        intentLevel: "fully_delegated",
      },
      actor: { kind: "operator" },
    });
    target.scheduleAction(taskDispatchAction({
      taskId: "task-1",
      transferId: "transfer-2",
      targetNodeId: "straylight",
      checkpoint: CHECKPOINT,
      runAt: now,
    }));
    target.scheduleAction({
      taskId: "task-1",
      kind: "agent.retry",
      payload: {},
      runAt: now,
    });
    const executions: unknown[] = [];
    const worker = new NodeScheduledActionKindWorker(target, {
      kind: TASK_DISPATCH_ACTION_KIND,
      parse: parseTaskDispatchPayload,
      execute: async (taskId, payload) => {
        executions.push({ taskId, payload });
        return "executed";
      },
    }, { workerId: "dispatch-worker", now: () => now });
    expect(await worker.runOnce()).toBe(1);
    expect(executions).toEqual([
      {
        taskId: "task-1",
        payload: parseTaskDispatchPayload(
          target.listScheduledActions("completed")[0]?.payload,
        ),
      },
    ]);
    expect(target.listScheduledActions("scheduled")).toEqual([
      expect.objectContaining({ kind: "agent.retry" }),
    ]);
    now += 1;
    expect(await worker.runOnce()).toBe(0);
  });

  test("rejects dispatches addressed to malformed capsule references", () => {
    expect(() => parseTaskDispatchPayload({ schema: "wrong" })).toThrow(
      "invalid",
    );
    expect(() => taskDispatchAction({
      taskId: "task-1",
      transferId: "transfer-3",
      targetNodeId: "straylight",
      checkpoint: { ...CHECKPOINT, capsuleId: "task-2" },
      runAt: 2_000,
    })).toThrow("another capsule");
  });
});
