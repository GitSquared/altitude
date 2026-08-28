import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import type { AgentWriterLeaseCandidate } from "../src/agent/writer-lease.ts";
import type { EventActor } from "../src/core/index.ts";
import { SqliteAgentWriterLeaseStore, TaskStore } from "../src/storage/index.ts";

const temporaryDirectories: string[] = [];
const actor: EventActor = { kind: "system", component: "test" };

afterEach(() => {
  for (const directory of temporaryDirectories.splice(0)) {
    rmSync(directory, { recursive: true, force: true });
  }
});

function databasePath(): string {
  const directory = mkdtempSync(join(tmpdir(), "altitude-writer-lease-test-"));
  temporaryDirectories.push(directory);
  return join(directory, "altitude.sqlite");
}

function createLedger(path: string): TaskStore {
  const ledger = new TaskStore(path, { clock: () => 1_000 });
  if (!ledger.getTask("task-1")) {
    ledger.append({
      taskId: "task-1",
      type: "task.created",
      payload: {
        title: "Build Altitude",
        intent: "Keep exactly one agent writer",
        intentLevel: "principles",
      },
      actor,
      occurredAt: 1_000,
    });
  }
  return ledger;
}

function candidate(overrides: Partial<AgentWriterLeaseCandidate> = {}): AgentWriterLeaseCandidate {
  return {
    taskId: "task-1",
    sessionId: "session-1",
    holderId: "headless-1",
    mode: "headless",
    acquiredAtMs: 1_001,
    ...overrides,
  };
}

describe("SQLite agent writer lease store", () => {
  test("serializes writers and retains a monotonic fencing revision after release", async () => {
    const ledger = createLedger(databasePath());
    const store = new SqliteAgentWriterLeaseStore(ledger.database);

    const first = await store.acquire(candidate());
    expect(first?.revision).toBe(1);
    expect(await store.acquire(candidate({ holderId: "interactive-1", mode: "interactive" }))).toBeUndefined();

    expect(await store.release(first!)).toBeTrue();
    const second = await store.acquire(candidate({
      holderId: "interactive-1",
      mode: "interactive",
      acquiredAtMs: 1_002,
    }));
    expect(second?.revision).toBe(2);
    expect(await store.release(first!)).toBeFalse();
    expect(await store.current("task-1")).toEqual(second);

    ledger.close();
  });

  test("persists active ownership across a crash-style reopen", async () => {
    const path = databasePath();
    let ledger = createLedger(path);
    let store = new SqliteAgentWriterLeaseStore(ledger.database);
    const first = await store.acquire(candidate());
    ledger.close();

    ledger = createLedger(path);
    store = new SqliteAgentWriterLeaseStore(ledger.database);
    expect(await store.current("task-1")).toEqual(first);
    expect(await store.acquire(candidate({ holderId: "other-process" }))).toBeUndefined();

    ledger.close();
  });

  test("requires the exact fencing revision for explicit abandoned-writer recovery", async () => {
    const ledger = createLedger(databasePath());
    const store = new SqliteAgentWriterLeaseStore(ledger.database);
    const abandoned = (await store.acquire(candidate()))!;

    expect(await store.expire("task-1", abandoned.revision + 1)).toBeFalse();
    expect(await store.current("task-1")).toEqual(abandoned);
    expect(await store.expire("task-1", abandoned.revision)).toBeTrue();

    const recovered = await store.acquire(candidate({
      holderId: "recovered-process",
      acquiredAtMs: 2_000,
    }));
    expect(recovered?.revision).toBe(2);
    expect(await store.expire("task-1", abandoned.revision)).toBeFalse();
    expect(await store.current("task-1")).toEqual(recovered);

    ledger.close();
  });

  test("keeps the exact lease identity in the release compare-and-swap", async () => {
    const ledger = createLedger(databasePath());
    const store = new SqliteAgentWriterLeaseStore(ledger.database);
    const lease = (await store.acquire(candidate()))!;

    expect(await store.release({ ...lease, holderId: "forged-holder" })).toBeFalse();
    expect(await store.release({ ...lease, sessionId: "forged-session" })).toBeFalse();
    expect(await store.current("task-1")).toEqual(lease);

    ledger.close();
  });
});
