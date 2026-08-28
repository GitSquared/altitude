import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { createAltitudeNodeService, nodeInstanceId } from "../src/node/server.ts";
import { TaskStore } from "../src/storage/task-store.ts";
import { TaskMcpGrantStore } from "../src/tools/grants.ts";
import { TASK_TOOL_PROTOCOL, type TaskToolExecutor } from "../src/tools/protocol.ts";

const directories: string[] = [];

afterEach(() => {
  for (const directory of directories.splice(0)) rmSync(directory, { recursive: true, force: true });
});

function state(): { store: TaskStore; grants: TaskMcpGrantStore } {
  const directory = mkdtempSync(join(tmpdir(), "altitude-node-test-"));
  directories.push(directory);
  const store = new TaskStore(join(directory, "altitude.db"));
  return { store, grants: new TaskMcpGrantStore(store.database) };
}

describe("Altitude node service", () => {
  test("reports a small no-cache health response without exposing state", async () => {
    const { store, grants } = state();
    const executor: TaskToolExecutor = {
      execute: async () => ({ schema: TASK_TOOL_PROTOCOL, ok: true, result: {} }),
    };
    const instanceId = nodeInstanceId("/state/altitude.db");
    const service = createAltitudeNodeService({ runtime: "docker", store, grants, executor, instanceId });
    const response = await service.fetch(new Request("http://localhost/healthz"));
    expect(response.status).toBe(200);
    expect(response.headers.get("cache-control")).toBe("no-store");
    expect(await response.json()).toMatchObject({ ok: true, service: "altitude-node", runtime: "docker", instanceId });

    const head = await service.fetch(new Request("http://localhost/healthz", { method: "HEAD" }));
    expect(head.status).toBe(200);
    expect(await head.text()).toBe("");
    await service.close();
    store.close();
  });

  test("routes only the MCP endpoint and retains its authentication fence", async () => {
    const { store, grants } = state();
    const executor: TaskToolExecutor = {
      execute: async () => ({ schema: TASK_TOOL_PROTOCOL, ok: true, result: {} }),
    };
    const service = createAltitudeNodeService({ runtime: "apple-container", store, grants, executor });
    const missing = await service.fetch(new Request("http://localhost/nope"));
    expect(missing.status).toBe(404);
    const mcp = await service.fetch(new Request("http://localhost/mcp", {
      method: "POST",
      headers: { host: "localhost", "content-type": "application/json" },
      body: "{}",
    }));
    expect(mcp.status).toBe(401);
    await service.close();
    store.close();
  });
});
