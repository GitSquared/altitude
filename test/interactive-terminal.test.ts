import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { reconcileExitedInteractiveAgent } from "../src/run/interactive-terminal.ts";
import { SqliteAgentWriterLeaseStore } from "../src/storage/agent-writer-lease-store.ts";
import { TaskAgentSessionStore } from "../src/storage/task-agent-session-store.ts";
import { TaskStore } from "../src/storage/task-store.ts";
import { TaskMcpGrantStore } from "../src/tools/grants.ts";
import { TERMINAL_SESSION_PROTOCOL } from "../src/terminal/session.ts";

const roots: string[] = [];
afterEach(() => {
  for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true });
});

describe("interactive terminal reconciliation", () => {
  test("uses terminal exit evidence and the exact writer fence before pausing", async () => {
    const root = mkdtempSync(join(tmpdir(), "altitude-interactive-terminal-"));
    roots.push(root);
    const database = join(root, "altitude.db");
    let store = new TaskStore(database, { clock: () => 2_000 });
    store.append({
      taskId: "task-1",
      type: "task.created",
      payload: { title: "Detached work", intent: "Keep working", intentLevel: "principles" },
      actor: { kind: "operator" },
    });
    store.append({
      taskId: "task-1",
      type: "task.state_changed",
      payload: { to: "queued" },
      actor: { kind: "operator" },
    });
    store.append({
      taskId: "task-1",
      type: "task.state_changed",
      payload: { to: "provisioning" },
      actor: { kind: "system", component: "test" },
    });
    store.append({
      taskId: "task-1",
      type: "task.state_changed",
      payload: { to: "taken" },
      actor: { kind: "system", component: "test" },
    });
    const sessions = new TaskAgentSessionStore(
      store.database,
      () => 2_000,
      () => "01900000-0000-7000-8000-000000000001",
    );
    const session = sessions.ensure("task-1", "claude-personal");
    const grants = new TaskMcpGrantStore(store.database, () => 2_000);
    const grant = grants.issue({
      taskId: "task-1",
      identityId: "claude-personal",
      agentSessionId: session.sessionId,
    });
    sessions.bindMcp({
      taskId: "task-1",
      grantId: grant.id,
      controlDirectory: "/identity/control/tasks/task-1",
      configPath: "/identity/control/tasks/task-1/mcp.json",
    });
    const leases = new SqliteAgentWriterLeaseStore(store.database);
    const lease = await leases.acquire({
      taskId: "task-1",
      sessionId: session.sessionId,
      holderId: "claude-interactive:holder-1",
      mode: "interactive",
      acquiredAtMs: 2_000,
    });
    expect(lease?.revision).toBe(1);

    await reconcileExitedInteractiveAgent({
      taskId: "task-1",
      status: {
        schema: TERMINAL_SESSION_PROTOCOL,
        state: "exited",
        exitCode: 0,
        signal: null,
        updatedAt: "2026-08-31T10:00:00.000Z",
      },
      tasks: store,
      sessions,
      leases,
      grants,
    });
    expect(store.getTask("task-1")?.status).toBe("paused");
    expect(await leases.current("task-1")).toBeUndefined();
    expect(grants.authenticate(grant.token)).toBeUndefined();
    store.close();

    store = new TaskStore(database);
    expect(store.getTask("task-1")?.status).toBe("paused");
    expect(await new SqliteAgentWriterLeaseStore(store.database).current("task-1")).toBeUndefined();
    store.close();
  });

  test("never clears a different or newer writer", async () => {
    using store = new TaskStore(":memory:");
    store.append({
      taskId: "task-1",
      type: "task.created",
      payload: { title: "Fence", intent: "Fence writers", intentLevel: "principles" },
      actor: { kind: "operator" },
    });
    const sessions = new TaskAgentSessionStore(
      store.database,
      undefined,
      () => "01900000-0000-7000-8000-000000000001",
    );
    sessions.ensure("task-1", "claude-personal");
    const leases = new SqliteAgentWriterLeaseStore(store.database);
    await leases.acquire({
      taskId: "task-1",
      sessionId: "01900000-0000-7000-8000-000000000099",
      holderId: "newer-writer",
      mode: "headless",
      acquiredAtMs: 3_000,
    });
    await expect(reconcileExitedInteractiveAgent({
      taskId: "task-1",
      status: {
        schema: TERMINAL_SESSION_PROTOCOL,
        state: "failed",
        error: "supervisor died",
        updatedAt: "2026-08-31T10:00:00.000Z",
      },
      tasks: store,
      sessions,
      leases,
      grants: { revoke: () => true },
    })).rejects.toThrow("does not fence its current writer");
    expect((await leases.current("task-1"))?.holderId).toBe("newer-writer");
  });

  test("does not treat a lost supervisor as proof that Claude exited", async () => {
    using store = new TaskStore(":memory:");
    store.append({
      taskId: "task-1",
      type: "task.created",
      payload: { title: "Liveness", intent: "Keep one writer", intentLevel: "principles" },
      actor: { kind: "operator" },
    });
    const sessions = new TaskAgentSessionStore(
      store.database,
      () => 4_000,
      () => "01900000-0000-7000-8000-000000000001",
    );
    const session = sessions.ensure("task-1", "claude-personal");
    const leases = new SqliteAgentWriterLeaseStore(store.database);
    await leases.acquire({
      taskId: "task-1",
      sessionId: session.sessionId,
      holderId: "claude-interactive:live-child",
      mode: "interactive",
      acquiredAtMs: 4_000,
    });

    await expect(reconcileExitedInteractiveAgent({
      taskId: "task-1",
      status: {
        schema: TERMINAL_SESSION_PROTOCOL,
        state: "failed",
        error: "supervisor disappeared",
        childPid: 123,
        childAlive: true,
        updatedAt: "2026-08-31T10:00:00.000Z",
      },
      tasks: store,
      sessions,
      leases,
      grants: { revoke: () => true },
    })).rejects.toThrow("child still appears alive");
    expect((await leases.current("task-1"))?.holderId).toBe("claude-interactive:live-child");
  });
});
