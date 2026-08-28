import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import type { EventActor } from "../src/core/index.ts";
import { CURRENT_SCHEMA_VERSION, TaskStore } from "../src/storage/index.ts";

const temporaryDirectories: string[] = [];
const actor: EventActor = { kind: "system", component: "test" };

afterEach(() => {
  for (const directory of temporaryDirectories.splice(0)) {
    rmSync(directory, { recursive: true, force: true });
  }
});

function databasePath(): string {
  const directory = mkdtempSync(join(tmpdir(), "altitude-store-test-"));
  temporaryDirectories.push(directory);
  return join(directory, "altitude.sqlite");
}

function idFactory(): () => string {
  let next = 1;
  return () => `id-${next++}`;
}

function createStore(path = databasePath(), now = 1_000): TaskStore {
  return new TaskStore(path, { clock: () => now, createId: idFactory() });
}

function createTask(store: TaskStore, id = "task-1"): void {
  store.append({
    taskId: id,
    type: "task.created",
    payload: {
      title: "Build Altitude",
      intent: "Make delegated work portable",
      intentLevel: "principles",
    },
    actor,
    occurredAt: 1_000,
  });
}

describe("SQLite task store", () => {
  test("uses WAL, migrates once, and rebuilds durable sequence state after reopen", () => {
    const path = databasePath();
    let store = createStore(path);
    expect(store.journalMode()).toBe("wal");
    expect(store.schemaVersion()).toBe(CURRENT_SCHEMA_VERSION);
    createTask(store);
    store.append({
      taskId: "task-1",
      type: "task.state_changed",
      payload: { to: "queued" },
      actor,
      occurredAt: 1_001,
    });
    store.close();

    store = createStore(path, 2_000);
    expect(store.schemaVersion()).toBe(CURRENT_SCHEMA_VERSION);
    expect(store.getTask("task-1")).toMatchObject({ status: "queued", revision: 2 });
    expect(store.getEvents("task-1").map((event) => event.sequence)).toEqual([1, 2]);
    expect(store.rebuildTaskProjection("task-1")).toEqual(store.getTask("task-1")!);
    store.close();
  });

  test("commits event, projection, and outbox atomically and deduplicates retries", () => {
    const store = createStore();
    const input = {
      taskId: "task-1",
      type: "task.created" as const,
      payload: {
        title: "Build Altitude",
        intent: "Make delegated work portable",
        intentLevel: "principles" as const,
      },
      actor,
      occurredAt: 1_000,
      idempotencyKey: "create-from-cli-1",
    };

    const first = store.append(input, [{
      topic: "task.events",
      payload: { kind: "task.created" },
      dedupeKey: "project-create-1",
    }]);
    const retry = store.append(input, [{
      topic: "task.events",
      payload: { kind: "this must not be inserted again" },
      dedupeKey: "project-create-1",
    }]);

    expect(first.duplicate).toBeFalse();
    expect(retry.duplicate).toBeTrue();
    expect(retry.event.id).toBe(first.event.id);
    expect(store.getEvents("task-1")).toHaveLength(1);
    expect(store.listOutbox()).toHaveLength(1);

    expect(() => store.append({
      ...input,
      payload: { ...input.payload, title: "A different task" },
    })).toThrow("already used for a different event");
    store.close();
  });

  test("rolls back the event and projection when an outbox insert fails", () => {
    const store = createStore();
    createTask(store);

    expect(() => store.append({
      taskId: "task-1",
      type: "task.state_changed",
      payload: { to: "queued" },
      actor,
      occurredAt: 1_001,
    }, [
      { topic: "linear", payload: { order: 1 }, dedupeKey: "same-delivery" },
      { topic: "linear", payload: { order: 2 }, dedupeKey: "same-delivery" },
    ])).toThrow();

    expect(store.getTask("task-1")).toMatchObject({ status: "draft", revision: 1 });
    expect(store.getEvents("task-1")).toHaveLength(1);
    expect(store.listOutbox()).toHaveLength(0);
    store.close();
  });

  test("persists the one-blocking-attention invariant", () => {
    const store = createStore();
    createTask(store);
    for (const to of ["queued", "provisioning", "running"] as const) {
      store.append({
        taskId: "task-1",
        type: "task.state_changed",
        payload: { to },
        actor,
      });
    }
    store.append({
      taskId: "task-1",
      type: "attention.raised",
      payload: {
        attention: {
          id: "decision-1",
          class: "decision",
          title: "Choose",
          summary: "A new constraint changed the route.",
          question: "Local or remote?",
        },
      },
      actor,
    });

    expect(store.getTask("task-1")).toMatchObject({ status: "waiting_decision" });
    expect(() => store.append({
      taskId: "task-1",
      type: "attention.raised",
      payload: {
        attention: {
          id: "review-1",
          class: "review",
          title: "Also review",
          summary: "This cannot be active yet.",
          artifacts: [{ kind: "other", label: "Artifact", uri: "urn:test" }],
          checks: ["checked"],
        },
      },
      actor,
    })).toThrow("at most one active blocking");
    expect(store.getEvents("task-1")).toHaveLength(5);
    store.close();
  });

  test("reclaims scheduled work after a process restart and expired lease", () => {
    const path = databasePath();
    let store = createStore(path);
    createTask(store);
    const scheduled = store.scheduleAction({
      taskId: "task-1",
      kind: "resume-agent",
      payload: { sessionId: "claude-1" },
      runAt: 1_100,
      dedupeKey: "resume-claude-1",
    });
    expect(scheduled.duplicate).toBeFalse();
    expect(store.scheduleAction({
      taskId: "task-1",
      kind: "resume-agent",
      payload: { sessionId: "claude-1" },
      runAt: 1_100,
      dedupeKey: "resume-claude-1",
    }).duplicate).toBeTrue();

    const firstClaim = store.claimScheduledActions({ workerId: "worker-a", now: 1_100, leaseMs: 100 });
    expect(firstClaim).toHaveLength(1);
    expect(firstClaim[0]).toMatchObject({ id: scheduled.action.id, attempts: 1, state: "claimed" });
    store.close();

    store = createStore(path, 1_150);
    expect(store.claimScheduledActions({ workerId: "worker-b", now: 1_199, leaseMs: 100 })).toHaveLength(0);
    const recovered = store.claimScheduledActions({ workerId: "worker-b", now: 1_200, leaseMs: 100 });
    expect(recovered).toHaveLength(1);
    expect(recovered[0]).toMatchObject({ attempts: 2, leaseOwner: "worker-b" });
    store.completeScheduledAction(scheduled.action.id, "worker-b", 1_201);
    expect(store.listScheduledActions("completed")).toHaveLength(1);
    store.close();
  });

  test("recovers both delivery and timer leases explicitly on startup", () => {
    const store = createStore();
    store.append({
      taskId: "task-1",
      type: "task.created",
      payload: {
        title: "Recover",
        intent: "Never lose durable work",
        intentLevel: "precise_instructions",
      },
      actor,
      occurredAt: 1_000,
    }, [{ topic: "linear", payload: { body: "hello" } }]);
    store.scheduleAction({ taskId: "task-1", kind: "wake", payload: {}, runAt: 1_000 });
    store.claimOutbox({ workerId: "old-worker", now: 1_000, leaseMs: 50 });
    store.claimScheduledActions({ workerId: "old-worker", now: 1_000, leaseMs: 50 });

    expect(store.recoverExpiredLeases(1_049)).toEqual({ outbox: 0, scheduledActions: 0 });
    expect(store.recoverExpiredLeases(1_050)).toEqual({ outbox: 1, scheduledActions: 1 });
    expect(store.listOutbox("pending")).toHaveLength(1);
    expect(store.listScheduledActions("scheduled")).toHaveLength(1);
    store.close();
  });
});
