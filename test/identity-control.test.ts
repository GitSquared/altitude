import { afterEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, statSync, symlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, join } from "node:path";

import {
  IDENTITY_CONTROL_PROTOCOL,
  IdentityControlProvisioner,
  identityTaskControlDirectory,
  writeIdentityTaskMcpConfig,
} from "../src/identity/control.ts";
import type { ProcessExit, ProcessLaunchSpec, ProcessLauncher, RunningProcess } from "../src/runtime/protocol.ts";

const temporaryDirectories: string[] = [];

afterEach(() => {
  for (const directory of temporaryDirectories.splice(0)) rmSync(directory, { recursive: true, force: true });
});

function identityRoot(): string {
  const root = mkdtempSync(join(tmpdir(), "altitude-identity-control-test-"));
  temporaryDirectories.push(root);
  return root;
}

class TextStream implements AsyncIterable<Uint8Array> {
  constructor(private readonly text: string) {}
  async *[Symbol.asyncIterator](): AsyncIterator<Uint8Array> {
    yield new TextEncoder().encode(this.text);
  }
}

class FakeProcess implements RunningProcess {
  readonly stderr = new TextStream("");
  readonly exited: Promise<ProcessExit>;
  input = "";
  closed = false;
  terminated = false;

  constructor(readonly stdout: AsyncIterable<Uint8Array>, exitCode = 0) {
    this.exited = Promise.resolve({ exitCode });
  }

  writeStdin(chunk: string | Uint8Array): Promise<void> {
    this.input += typeof chunk === "string" ? chunk : new TextDecoder().decode(chunk);
    return Promise.resolve();
  }
  closeStdin(): Promise<void> { this.closed = true; return Promise.resolve(); }
  terminate(): Promise<void> { this.terminated = true; return Promise.resolve(); }
}

describe("identity task control", () => {
  test("writes a private MCP config in a hashed identity-local control directory", async () => {
    const root = identityRoot();
    const config = {
      mcpServers: { altitude: { url: "http://host.container.internal:43100/mcp", headers: { Authorization: `Bearer ${"a".repeat(64)}` } } },
    };
    const response = await writeIdentityTaskMcpConfig(root, {
      schema: IDENTITY_CONTROL_PROTOCOL,
      operation: "write_task_mcp_config",
      taskId: "linear:ALT-123",
      config,
    });
    expect(response.ok).toBeTrue();
    if (!response.ok) return;
    expect(response.result.controlDirectory).toBe(identityTaskControlDirectory("linear:ALT-123"));
    const directory = basename(response.result.controlDirectory);
    const path = join(root, "control", "tasks", directory, "mcp.json");
    expect(JSON.parse(readFileSync(path, "utf8"))).toEqual(config);
    expect(statSync(path).mode & 0o777).toBe(0o600);
  });

  test("rejects a symlinked identity control boundary", async () => {
    const root = identityRoot();
    const outside = identityRoot();
    mkdirSync(join(root, "control"));
    symlinkSync(outside, join(root, "control", "tasks"));
    const response = await writeIdentityTaskMcpConfig(root, {
      schema: IDENTITY_CONTROL_PROTOCOL,
      operation: "write_task_mcp_config",
      taskId: "task-1",
      config: {},
    });
    expect(response).toMatchObject({ ok: false, error: { code: "identity_control_failed" } });
  });

  test("provisions through the identity capsule without putting the bearer in argv or env", async () => {
    const result = {
      controlDirectory: identityTaskControlDirectory("task-1"),
      configPath: `${identityTaskControlDirectory("task-1")}/mcp.json`,
    };
    const process = new FakeProcess(new TextStream(`${JSON.stringify({
      schema: IDENTITY_CONTROL_PROTOCOL,
      ok: true,
      result,
    })}\n`));
    const launches: Array<ProcessLaunchSpec & { readonly io: "piped" }> = [];
    const launcher: ProcessLauncher = {
      launch(spec) { launches.push(spec); return Promise.resolve(process); },
    };
    const provisioner = new IdentityControlProvisioner(launcher);
    const token = "a".repeat(64);
    expect(await provisioner.writeTaskMcpConfig({
      identityId: "claude-personal",
      taskId: "task-1",
      config: { headers: { Authorization: `Bearer ${token}` } },
    })).toEqual(result);
    expect(launches[0]).toMatchObject({
      target: { kind: "identity-capsule", identityId: "claude-personal", taskId: "task-1" },
      args: ["_identity-control"],
      cwd: "/identity",
      env: { ALTITUDE_IDENTITY_CONTROL: "1" },
    });
    expect(JSON.stringify(launches[0])).not.toContain(token);
    expect(process.input).toContain(token);
  });
});
