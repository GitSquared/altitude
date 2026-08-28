import { describe, expect, test } from "bun:test";

import {
  DomainInvariantError,
  EVENT_SCHEMA_VERSION,
  reduceTask,
  type EventActor,
  type EventEnvelopeOf,
  type EventPayloads,
  type EventType,
  type Task,
} from "../src/core/index.ts";

const actor: EventActor = { kind: "system", component: "test" };

function envelope<Type extends EventType>(
  task: Task | undefined,
  type: Type,
  payload: EventPayloads[Type],
  occurredAt = 1_000,
): EventEnvelopeOf<Type> {
  const sequence = (task?.revision ?? 0) + 1;
  return {
    id: `event-${sequence}`,
    taskId: task?.id ?? "task-1",
    sequence,
    schemaVersion: EVENT_SCHEMA_VERSION,
    occurredAt,
    actor,
    type,
    payload,
  } as EventEnvelopeOf<Type>;
}

function createTask(): Task {
  return reduceTask(undefined, envelope(undefined, "task.created", {
    title: "Build portable capsules",
    intent: "Move delegated work without moving identity",
    intentLevel: "principles",
  }));
}

function runningTask(): Task {
  let task = createTask();
  task = reduceTask(task, envelope(task, "task.state_changed", { to: "queued" }, 1_001));
  task = reduceTask(task, envelope(task, "task.state_changed", { to: "provisioning" }, 1_002));
  return reduceTask(task, envelope(task, "task.state_changed", { to: "running" }, 1_003));
}

describe("task reducer", () => {
  test("creates a deterministic projection and keeps intent level separate from lifecycle", () => {
    let task = createTask();

    expect(task).toMatchObject({
      id: "task-1",
      status: "draft",
      intentLevel: "principles",
      revision: 1,
      activeAttention: null,
      openUpdates: [],
    });

    task = reduceTask(task, envelope(task, "task.intent_changed", {
      level: "option_selection",
      intent: "Pick the safest transfer design",
    }, 1_010));

    expect(task.status).toBe("draft");
    expect(task.intentLevel).toBe("option_selection");
    expect(task.intent).toBe("Pick the safest transfer design");
  });

  test("rejects skipped event versions and illegal lifecycle transitions", () => {
    const task = createTask();
    const skipped = {
      ...envelope(task, "task.state_changed", { to: "queued" }),
      sequence: 3,
    };

    expect(() => reduceTask(task, skipped)).toThrow("Expected event sequence 2");
    expect(() => reduceTask(task, envelope(task, "task.state_changed", { to: "running" })))
      .toThrow("Invalid task transition draft -> running");
    expect(() => reduceTask(task, envelope(task, "task.state_changed", { to: "waiting_decision" })))
      .toThrow("Waiting states are entered by raising blocking attention");
    const running = runningTask();
    expect(() => reduceTask(running, envelope(running, "task.state_changed", { to: "completed" })))
      .toThrow("Invalid task transition running -> completed");
  });

  test("Trace is history, Updates remain independently open, and neither blocks work", () => {
    let task = runningTask();
    task = reduceTask(task, envelope(task, "attention.raised", {
      attention: {
        id: "trace-1",
        class: "trace",
        title: "Repository cached",
        summary: "Reused the local mirror.",
      },
    }));
    expect(task.status).toBe("running");
    expect(task.openUpdates).toHaveLength(0);

    for (const id of ["update-1", "update-2"]) {
      task = reduceTask(task, envelope(task, "attention.raised", {
        attention: {
          id,
          class: "update",
          title: `Update ${id}`,
          summary: "Work continues.",
        },
      }));
    }
    expect(task.status).toBe("running");
    expect(task.openUpdates.map((update) => update.id)).toEqual(["update-1", "update-2"]);

    task = reduceTask(task, envelope(task, "attention.resolved", {
      attentionId: "update-1",
      resolution: { kind: "acknowledged" },
    }));
    expect(task.openUpdates.map((update) => update.id)).toEqual(["update-2"]);
  });

  test("allows only one blocking Decision and restores the exact prior execution state", () => {
    let task = runningTask();
    task = reduceTask(task, envelope(task, "task.state_changed", { to: "taken" }));
    task = reduceTask(task, envelope(task, "attention.raised", {
      attention: {
        id: "decision-1",
        class: "decision",
        title: "Choose the transfer source",
        summary: "Both nodes have reachable mirrors.",
        question: "Should the local or remote copy win?",
        options: [
          { label: "Local", value: "local" },
          { label: "Remote", value: "remote" },
        ],
      },
    }));

    expect(task.status).toBe("waiting_decision");
    expect(task.activeAttention?.resumeStatus).toBe("taken");
    expect(() => reduceTask(task, envelope(task, "attention.raised", {
      attention: {
        id: "review-while-blocked",
        class: "review",
        title: "Review",
        summary: "This must wait.",
        artifacts: [{ kind: "other", label: "Artifact", uri: "urn:test" }],
        checks: ["checked"],
      },
    }))).toThrow("at most one active blocking");
    expect(() => reduceTask(task, envelope(task, "task.state_changed", { to: "stopped" })))
      .toThrow("Resolve, supersede, or withdraw");

    task = reduceTask(task, envelope(task, "attention.resolved", {
      attentionId: "decision-1",
      resolution: { kind: "answered", answer: "Use local" },
    }));
    expect(task.status).toBe("taken");
    expect(task.activeAttention).toBeNull();
  });

  test("Review acceptance completes work while revision returns it to the agent", () => {
    const raiseReview = (task: Task, id: string): Task => reduceTask(task, envelope(task, "attention.raised", {
      attention: {
        id,
        class: "review",
        title: "Capsule is ready",
        summary: "The transfer round trip passed.",
        artifacts: [{ kind: "pull_request", label: "PR", uri: "https://example.test/pr/1" }],
        checks: ["CI passed", "Round trip exercised"],
      },
    }));

    let task = raiseReview(runningTask(), "review-1");
    task = reduceTask(task, envelope(task, "attention.resolved", {
      attentionId: "review-1",
      resolution: { kind: "revision_requested", instructions: "Show the interrupted-transfer case." },
    }));
    expect(task.status).toBe("running");

    task = raiseReview(task, "review-2");
    task = reduceTask(task, envelope(task, "attention.resolved", {
      attentionId: "review-2",
      resolution: { kind: "accepted" },
    }));
    expect(task.status).toBe("completed");
  });

  test("requires evidence of checked work before entering Review", () => {
    const task = runningTask();
    expect(() => reduceTask(task, envelope(task, "attention.raised", {
      attention: {
        id: "review-empty",
        class: "review",
        title: "Ready",
        summary: "Trust me.",
        artifacts: [],
        checks: [],
      },
    }))).toThrow(DomainInvariantError);
  });
});
