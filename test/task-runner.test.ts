import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import type {
  AgentAdapter,
  AgentInteractiveStartRequest,
  AgentRun,
  AgentRunCompletion,
  AgentRunRequest,
  AgentTakeRequest,
} from "../src/agent/protocol.ts";
import { TaskRunCoordinator } from "../src/run/task-runner.ts";
import { TaskAgentSessionStore } from "../src/storage/task-agent-session-store.ts";
import { TaskStore } from "../src/storage/task-store.ts";
import { TaskMcpGrantStore } from "../src/tools/grants.ts";

const directories: string[] = [];
afterEach(() => {
  for (const directory of directories.splice(0)) rmSync(directory, { recursive: true, force: true });
});

function state() {
  const root = mkdtempSync(join(tmpdir(), "altitude-task-runner-"));
  directories.push(root);
  const store = new TaskStore(join(root, "altitude.db"), { clock: () => 1_000 });
  store.append({
    taskId: "task-1",
    type: "task.created",
    payload: { title: "Useful run", intent: "Implement the smallest real vertical slice", intentLevel: "principles" },
    actor: { kind: "operator" },
  });
  store.append({
    taskId: "task-1",
    type: "task.state_changed",
    payload: { to: "queued" },
    actor: { kind: "operator" },
  });
  return store;
}

class FakeAgent {
  readonly requests: AgentRunRequest[] = [];
  readonly interactiveStarts: AgentInteractiveStartRequest[] = [];
  readonly takes: AgentTakeRequest[] = [];
  released = false;
  completion: AgentRunCompletion = { outcome: "success", exit: { exitCode: 0 } };
  start(request: AgentRunRequest): Promise<AgentRun> {
    this.requests.push(request);
    return Promise.resolve({
      taskId: request.taskId,
      sessionId: request.sessionId,
      launch: {
        target: { kind: "identity-capsule", identityId: "claude-personal" },
        executable: "claude",
        args: [],
        io: "piped",
        purpose: "test",
      },
      events: { async *[Symbol.asyncIterator]() {} },
      completion: Promise.resolve(this.completion),
      requestYield: async () => true,
      stop: async () => undefined,
    });
  }
  prepareInteractiveTake(request: AgentTakeRequest): ReturnType<AgentAdapter["prepareInteractiveTake"]> {
    this.takes.push(request);
    return Promise.resolve({
      taskId: request.taskId,
      sessionId: request.sessionId,
      launch: {
        target: { kind: "identity-capsule", identityId: "claude-personal", taskId: request.taskId },
        executable: "claude",
        args: ["--resume", request.sessionId],
        cwd: request.controlDirectory,
        io: "interactive",
        purpose: "test take",
      },
      release: async () => { this.released = true; },
    });
  }
  prepareInteractiveStart(
    request: AgentInteractiveStartRequest,
  ): ReturnType<AgentAdapter["prepareInteractiveStart"]> {
    this.interactiveStarts.push(request);
    return Promise.resolve({
      taskId: request.taskId,
      sessionId: request.sessionId,
      launch: {
        target: { kind: "identity-capsule", identityId: "claude-personal", taskId: request.taskId },
        executable: "claude",
        args: ["--session-id", request.sessionId, request.prompt],
        cwd: request.controlDirectory,
        io: "interactive",
        purpose: "test interactive start",
      },
      release: async () => { this.released = true; },
    });
  }
}

describe("task run coordinator", () => {
  test("binds a fixed session and secret grant before launching the managed turn", async () => {
    using store = state();
    const sessions = new TaskAgentSessionStore(
      store.database,
      () => 1_000,
      () => "01900000-0000-7000-8000-000000000001",
    );
    const grants = new TaskMcpGrantStore(store.database, () => 1_000);
    const configs: unknown[] = [];
    const agent = new FakeAgent();
    const coordinator = new TaskRunCoordinator(store, sessions, grants, {
      writeTaskMcpConfig: async (input) => {
        configs.push(input.config);
        return {
          controlDirectory: "/identity/control/tasks/t-abc",
          configPath: "/identity/control/tasks/t-abc/mcp.json",
        };
      },
    }, agent);
    const managed = await coordinator.start({
      taskId: "task-1",
      identityId: "claude-personal",
      mcpEndpoint: "http://172.16.0.5:43100/mcp",
    });
    expect(store.getTask("task-1")?.status).toBe("running");
    expect(agent.requests[0]).toMatchObject({
      taskId: "task-1",
      resume: false,
      controlDirectory: "/identity/control/tasks/t-abc",
    });
    expect(agent.requests[0]?.prompt).toContain("no source checkout in this identity capsule");
    expect(agent.requests[0]?.prompt).toContain("Trace, Update, Decision, and Review are additive");
    const encodedConfig = JSON.stringify(configs[0]);
    expect(encodedConfig).toContain("Authorization");
    expect(encodedConfig).not.toContain("?token=");
    const storedGrant = store.database.query<{ token_hash: string }, []>("SELECT token_hash FROM task_mcp_grants").get();
    expect(storedGrant?.token_hash).not.toContain("Bearer");
    expect(await managed.settle()).toMatchObject({ outcome: "success" });
    expect(store.getTask("task-1")?.status).toBe("paused");
    expect(sessions.get("task-1")?.runCount).toBe(1);
  });

  test("leaves a Review waiting instead of treating process exit as completion", async () => {
    using store = state();
    const sessions = new TaskAgentSessionStore(
      store.database,
      () => 1_000,
      () => "01900000-0000-7000-8000-000000000002",
    );
    const grants = new TaskMcpGrantStore(store.database, () => 1_000);
    const agent = new FakeAgent();
    const coordinator = new TaskRunCoordinator(store, sessions, grants, {
      writeTaskMcpConfig: async () => ({
        controlDirectory: "/identity/control/tasks/t-def",
        configPath: "/identity/control/tasks/t-def/mcp.json",
      }),
    }, agent);
    const managed = await coordinator.start({
      taskId: "task-1",
      identityId: "claude-personal",
      mcpEndpoint: "http://172.16.0.5:43100/mcp",
    });
    store.append({
      taskId: "task-1",
      type: "attention.raised",
      payload: { attention: {
        id: "review-1",
        class: "review",
        title: "Review it",
        summary: "Checked work is ready",
        artifacts: [{ kind: "other", label: "Diff", uri: "https://example.test/diff" }],
        checks: ["bun test"],
      } },
      actor: { kind: "agent", adapter: "claude-code-cli" },
    });
    await managed.settle();
    expect(store.getTask("task-1")?.status).toBe("waiting_review");
  });

  test("turns a rejected rate limit into one durable exact-session retry", async () => {
    using store = state();
    const sessions = new TaskAgentSessionStore(
      store.database,
      () => 1_000,
      () => "01900000-0000-7000-8000-000000000004",
    );
    const grants = new TaskMcpGrantStore(store.database, () => 1_000);
    const agent = new FakeAgent();
    agent.completion = {
      outcome: "rate_limited",
      exit: { exitCode: 1 },
      rateLimit: {
        type: "agent.rate_limit",
        observedAtMs: 1_000,
        status: "rejected",
        resetsAtMs: 50_000,
      },
    };
    const coordinator = new TaskRunCoordinator(store, sessions, grants, {
      writeTaskMcpConfig: async () => ({
        controlDirectory: "/identity/control/tasks/t-retry",
        configPath: "/identity/control/tasks/t-retry/mcp.json",
      }),
    }, agent, () => 1_000);
    const managed = await coordinator.start({
      taskId: "task-1",
      identityId: "claude-personal",
      mcpEndpoint: "http://172.16.0.5:43100/mcp",
      model: "claude-sonnet",
    });
    await managed.settle();
    await managed.settle();

    expect(store.getTask("task-1")?.status).toBe("waiting_external");
    expect(sessions.get("task-1")?.consecutiveRateLimits).toBe(1);
    expect(store.listScheduledActions("scheduled")).toEqual([
      expect.objectContaining({
        taskId: "task-1",
        kind: "agent.retry",
        runAt: 51_000,
        payload: {
          schema: "altitude.agent-retry/v1",
          sessionId: managed.session.sessionId,
          runNumber: 1,
          model: "claude-sonnet",
        },
      }),
    ]);
  });

  test("refreshes the grant and hands the exact fixed session to the native TUI", async () => {
    using store = state();
    const sessions = new TaskAgentSessionStore(
      store.database,
      () => 1_000,
      () => "01900000-0000-7000-8000-000000000003",
    );
    const grants = new TaskMcpGrantStore(store.database, () => 1_000);
    const agent = new FakeAgent();
    const coordinator = new TaskRunCoordinator(store, sessions, grants, {
      writeTaskMcpConfig: async () => ({
        controlDirectory: "/identity/control/tasks/t-take",
        configPath: "/identity/control/tasks/t-take/mcp.json",
      }),
    }, agent);
    const run = await coordinator.start({
      taskId: "task-1",
      identityId: "claude-personal",
      mcpEndpoint: "http://172.16.0.5:43100/mcp",
    });
    await run.settle();
    const oldGrant = sessions.get("task-1")?.grantId;
    const take = await coordinator.take({
      taskId: "task-1",
      identityId: "claude-personal",
      mcpEndpoint: "http://172.16.0.5:43100/mcp",
    });
    expect(store.getTask("task-1")?.status).toBe("taken");
    expect(agent.takes[0]?.sessionId).toBe(run.session.sessionId);
    expect(sessions.get("task-1")?.grantId).not.toBe(oldGrant);
    await take.finish();
    expect(agent.released).toBeTrue();
    expect(store.getTask("task-1")?.status).toBe("paused");
  });

  test("starts the first fixed session directly in the native TUI and records it only after spawn", async () => {
    using store = state();
    const sessions = new TaskAgentSessionStore(
      store.database,
      () => 1_000,
      () => "01900000-0000-7000-8000-000000000005",
    );
    const grants = new TaskMcpGrantStore(store.database, () => 1_000);
    const agent = new FakeAgent();
    const coordinator = new TaskRunCoordinator(store, sessions, grants, {
      writeTaskMcpConfig: async () => ({
        controlDirectory: "/identity/control/tasks/t-first",
        configPath: "/identity/control/tasks/t-first/mcp.json",
      }),
    }, agent);

    const first = await coordinator.take({
      taskId: "task-1",
      identityId: "claude-personal",
      mcpEndpoint: "http://172.16.0.5:43100/mcp",
    });
    expect(store.getTask("task-1")?.status).toBe("taken");
    expect(agent.interactiveStarts).toHaveLength(1);
    expect(agent.interactiveStarts[0]?.prompt).toContain("Canonical intent (principles)");
    expect(sessions.get("task-1")?.runCount).toBe(0);

    first.started();
    first.started();
    expect(sessions.get("task-1")?.runCount).toBe(1);
    await first.finish();
    expect(store.getTask("task-1")?.status).toBe("paused");

    const resumed = await coordinator.take({
      taskId: "task-1",
      identityId: "claude-personal",
      mcpEndpoint: "http://172.16.0.5:43100/mcp",
    });
    expect(agent.takes).toHaveLength(1);
    expect(agent.takes[0]?.sessionId).toBe(first.session.sessionId);
    await resumed.finish();
  });

  test("retries a native first session as new when the child never spawned", async () => {
    using store = state();
    const sessions = new TaskAgentSessionStore(
      store.database,
      () => 1_000,
      () => "01900000-0000-7000-8000-000000000006",
    );
    const agent = new FakeAgent();
    const coordinator = new TaskRunCoordinator(
      store,
      sessions,
      new TaskMcpGrantStore(store.database, () => 1_000),
      {
        writeTaskMcpConfig: async () => ({
          controlDirectory: "/identity/control/tasks/t-retry-first",
          configPath: "/identity/control/tasks/t-retry-first/mcp.json",
        }),
      },
      agent,
    );

    const failedSpawn = await coordinator.take({
      taskId: "task-1",
      identityId: "claude-personal",
      mcpEndpoint: "http://172.16.0.5:43100/mcp",
    });
    await failedSpawn.finish();
    expect(sessions.get("task-1")?.runCount).toBe(0);

    const retry = await coordinator.take({
      taskId: "task-1",
      identityId: "claude-personal",
      mcpEndpoint: "http://172.16.0.5:43100/mcp",
    });
    expect(agent.interactiveStarts).toHaveLength(2);
    expect(agent.takes).toHaveLength(0);
    expect(retry.session.sessionId).toBe(failedSpawn.session.sessionId);
    await retry.finish();
  });
});
