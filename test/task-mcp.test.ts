import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { Client, StreamableHTTPClientTransport } from "@modelcontextprotocol/client";

import { TaskStore } from "../src/storage/task-store.ts";
import { IntentSubmissionStore } from "../src/intent/submission.ts";
import {
  TaskMcpGrantStore,
  claudeTaskMcpConfig,
} from "../src/tools/grants.ts";
import { createTaskMcpEndpoint } from "../src/tools/mcp.ts";
import { TASK_TOOL_PROTOCOL, type TaskToolExecutor, type TaskToolRequest } from "../src/tools/protocol.ts";

const temporaryDirectories: string[] = [];

afterEach(() => {
  for (const directory of temporaryDirectories.splice(0)) rmSync(directory, { recursive: true, force: true });
});

function databasePath(): string {
  const root = mkdtempSync(join(tmpdir(), "altitude-mcp-test-"));
  temporaryDirectories.push(root);
  return join(root, "altitude.db");
}

function runningTask(path: string): TaskStore {
  const store = new TaskStore(path, {
    clock: () => 1_000,
    createId: (() => {
      let id = 0;
      return () => `event-${++id}`;
    })(),
  });
  store.append({
    taskId: "task-1",
    type: "task.created",
    payload: { title: "Build the bridge", intent: "Give agents narrow task tools", intentLevel: "principles" },
    actor: { kind: "operator" },
  });
  for (const to of ["queued", "provisioning", "running"] as const) {
    store.append({
      taskId: "task-1",
      type: "task.state_changed",
      payload: { to },
      actor: { kind: "system", component: "test" },
    });
  }
  return store;
}

function routedFetch(endpoint: { fetch(request: Request): Promise<Response> }): typeof fetch {
  return (async (input: string | URL | Request, init?: RequestInit): Promise<Response> => {
    const original = input instanceof Request
      ? new Request(input, init)
      : new Request(input.toString(), init);
    const headers = new Headers(original.headers);
    headers.set("host", "localhost");
    return await endpoint.fetch(new Request(original, { headers }));
  }) as typeof fetch;
}

describe("task MCP grants", () => {
  test("persists only a token hash and fences expiry and revocation", () => {
    const path = databasePath();
    let now = 1_000;
    let store = runningTask(path);
    let grants = new TaskMcpGrantStore(store.database, () => now);
    const issued = grants.issue({
      taskId: "task-1",
      identityId: "claude-personal",
      agentSessionId: "session-1",
      allowedTools: ["get_task", "read_file"],
      ttlMs: 60_000,
    });
    const stored = store.database.query<{ token_hash: string }, []>("SELECT token_hash FROM task_mcp_grants").get();
    expect(stored?.token_hash).not.toContain(issued.token);
    expect(grants.authenticate(issued.token)).toMatchObject({ taskId: "task-1", identityId: "claude-personal" });
    store.close();

    store = new TaskStore(path);
    grants = new TaskMcpGrantStore(store.database, () => now);
    expect(grants.authenticate(issued.token)?.allowedTools).toEqual(["get_task", "read_file"]);
    expect(grants.revoke(issued.id)).toBeTrue();
    expect(grants.authenticate(issued.token)).toBeUndefined();

    const expiring = grants.issue({ taskId: "task-1", identityId: "claude-personal", ttlMs: 60_000 });
    now += 60_000;
    expect(grants.authenticate(expiring.token)).toBeUndefined();
    expect(grants.purgeExpired()).toBeGreaterThanOrEqual(2);
    store.close();
  });

  test("renders a Claude HTTP config while keeping the bearer out of the URL", () => {
    const token = "a".repeat(64);
    const config = claudeTaskMcpConfig("http://host.container.internal:43100/mcp", token) as {
      mcpServers: { altitude: { url: string; headers: { Authorization: string } } };
    };
    expect(config.mcpServers.altitude.url).not.toContain(token);
    expect(config.mcpServers.altitude.headers.Authorization).toBe(`Bearer ${token}`);
  });
});

describe("task MCP endpoint", () => {
  test("serves only granted tools through the official MCP client", async () => {
    const store = runningTask(databasePath());
    const submissions = new IntentSubmissionStore(store, { clock: () => 1_000 });
    const visual = submissions.issue({
      taskId: "task-1",
      capsuleId: "task-1",
      baseUrl: "http://127.0.0.1:3000/",
      headSha: "a".repeat(40),
      checkpointDigest: `sha256:${"b".repeat(64)}`,
      checkpointRevision: 2,
    });
    submissions.consume(visual.sessionId, visual.token, {
      schemaVersion: "redmarker-review/v1",
      project: { name: "Fixture" },
      page: { pathname: "/", title: "Fixture", viewport: { width: 800, height: 600 } },
      marks: [{ id: "mark-1" }],
    });
    const grants = new TaskMcpGrantStore(store.database, () => 1_000);
    const grant = grants.issue({
      taskId: "task-1",
      identityId: "claude-personal",
      agentSessionId: "session-1",
      allowedTools: ["get_task", "read_file", "report_update", "create_pull_request", "inspect_pull_request"],
      ttlMs: 60_000,
    });
    const calls: Array<{ taskId: string; request: TaskToolRequest }> = [];
    const executor: TaskToolExecutor = {
      execute(taskId, request) {
        calls.push({ taskId, request });
        return Promise.resolve({
          schema: TASK_TOOL_PROTOCOL,
          ok: true,
          result: { path: "README.md", content: "# altitude", sha256: `sha256:${"a".repeat(64)}` },
        });
      },
    };
    const endpoint = createTaskMcpEndpoint({ grants, store, executor });
    const transport = new StreamableHTTPClientTransport(new URL("http://localhost/mcp"), {
      requestInit: { headers: { authorization: `Bearer ${grant.token}` } },
      fetch: routedFetch(endpoint),
    });
    const client = new Client({ name: "altitude-test", version: "1.0.0" });
    await client.connect(transport);
    try {
      const listed = await client.listTools();
      expect(listed.tools.map((tool) => tool.name).sort()).toEqual([
        "create_pull_request",
        "get_task",
        "inspect_pull_request",
        "read_file",
        "report_update",
      ]);

      const task = await client.callTool({ name: "get_task", arguments: {} });
      expect(task.structuredContent).toMatchObject({
        intentArtifacts: [{ tool: "redmarker", base: { checkpointRevision: 2 } }],
      });

      const read = await client.callTool({ name: "read_file", arguments: { path: "README.md" } });
      expect(read.isError).not.toBeTrue();
      expect(calls).toEqual([{
        taskId: "task-1",
        request: { schema: TASK_TOOL_PROTOCOL, operation: "read_file", path: "README.md" },
      }]);

      await client.callTool({
        name: "create_pull_request",
        arguments: {
          repository: "example/project",
          head: "agent/capsule",
          base: "main",
          title: "Capsule work",
          body: "Checked by Altitude",
        },
      });
      await client.callTool({
        name: "inspect_pull_request",
        arguments: {
          url: "https://github.com/example/project/pull/42",
          evidence: {
            worktreeClean: true,
            headPushed: true,
            pathExercised: true,
            feedbackAddressed: true,
          },
        },
      });
      expect(calls.slice(1)).toEqual([
        {
          taskId: "task-1",
          request: {
            schema: TASK_TOOL_PROTOCOL,
            operation: "github_pr_create",
            repository: "example/project",
            head: "agent/capsule",
            base: "main",
            title: "Capsule work",
            body: "Checked by Altitude",
          },
        },
        {
          taskId: "task-1",
          request: {
            schema: TASK_TOOL_PROTOCOL,
            operation: "github_pr_inspect",
            url: "https://github.com/example/project/pull/42",
            evidence: {
              worktreeClean: true,
              headPushed: true,
              pathExercised: true,
              feedbackAddressed: true,
            },
          },
        },
      ]);

      const update = await client.callTool({
        name: "report_update",
        arguments: {
          title: "Bridge works",
          summary: "The task-scoped MCP round trip passed.",
          idempotencyKey: "bridge-ready",
        },
      });
      expect(update.isError).not.toBeTrue();
      const retry = await client.callTool({
        name: "report_update",
        arguments: {
          title: "Bridge works",
          summary: "The task-scoped MCP round trip passed.",
          idempotencyKey: "bridge-ready",
        },
      });
      expect(retry.isError).not.toBeTrue();
      expect(store.getTask("task-1")?.openUpdates).toHaveLength(1);
    } finally {
      await client.close();
      await endpoint.close();
      store.close();
    }
  });

  test("rejects missing grants and DNS-rebinding hosts before MCP parsing", async () => {
    const store = runningTask(databasePath());
    const grants = new TaskMcpGrantStore(store.database, () => 1_000);
    const executor: TaskToolExecutor = { execute: async () => ({ schema: TASK_TOOL_PROTOCOL, ok: true, result: {} }) };
    const endpoint = createTaskMcpEndpoint({ grants, store, executor });
    const body = JSON.stringify({ jsonrpc: "2.0", id: 1, method: "initialize", params: {} });
    const unauthorized = await endpoint.fetch(new Request("http://localhost/mcp", {
      method: "POST",
      headers: { host: "localhost", "content-type": "application/json" },
      body,
    }));
    expect(unauthorized.status).toBe(401);
    const rebound = await endpoint.fetch(new Request("http://attacker.example/mcp", {
      method: "POST",
      headers: { host: "attacker.example", "content-type": "application/json" },
      body,
    }));
    expect(rebound.status).toBe(403);
    await endpoint.close();
    store.close();
  });
});
