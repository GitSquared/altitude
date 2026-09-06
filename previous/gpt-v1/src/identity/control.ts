import { createHash, randomUUID } from "node:crypto";
import { chmod, lstat, mkdir, realpath, rename, rm, writeFile } from "node:fs/promises";
import { join, relative, resolve, sep } from "node:path";

import type { ProcessLauncher } from "../runtime/protocol.ts";

export const IDENTITY_CONTROL_PROTOCOL = "altitude.identity-control/v1" as const;

export type IdentityControlRequest = Readonly<{
  schema: typeof IDENTITY_CONTROL_PROTOCOL;
  operation: "write_task_mcp_config";
  taskId: string;
  config: unknown;
}>;

export type IdentityControlResponse =
  | Readonly<{
      schema: typeof IDENTITY_CONTROL_PROTOCOL;
      ok: true;
      result: Readonly<{ controlDirectory: string; configPath: string }>;
    }>
  | Readonly<{
      schema: typeof IDENTITY_CONTROL_PROTOCOL;
      ok: false;
      error: Readonly<{ code: string; message: string }>;
    }>;

function requireId(value: string, label: string): void {
  if (!/^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/u.test(value)) throw new Error(`${label} is invalid`);
}

function taskKey(taskId: string): string {
  requireId(taskId, "Task id");
  return `t-${createHash("sha256").update(taskId, "utf8").digest("hex").slice(0, 24)}`;
}

export function identityTaskControlDirectory(taskId: string): string {
  return `/identity/control/tasks/${taskKey(taskId)}`;
}

function isNotFound(error: unknown): boolean {
  return typeof error === "object" && error !== null && "code" in error && error.code === "ENOENT";
}

function inside(root: string, candidate: string): boolean {
  const path = relative(root, candidate);
  return path === "" || (path !== ".." && !path.startsWith(`..${sep}`));
}

async function requireDirectory(path: string): Promise<void> {
  const stat = await lstat(path);
  if (stat.isSymbolicLink() || !stat.isDirectory()) throw new Error(`${path} is not a regular identity directory`);
}

export async function writeIdentityTaskMcpConfig(
  identityRoot: string,
  request: IdentityControlRequest,
): Promise<IdentityControlResponse> {
  try {
    if (request.schema !== IDENTITY_CONTROL_PROTOCOL || request.operation !== "write_task_mcp_config") {
      throw new Error("Unsupported identity control request");
    }
    requireId(request.taskId, "Task id");
    const encoded = `${JSON.stringify(request.config, null, 2)}\n`;
    if (new TextEncoder().encode(encoded).byteLength > 256 * 1024) throw new Error("MCP config is too large");
    const root = await realpath(resolve(identityRoot));
    const control = join(root, "control");
    const tasks = join(control, "tasks");
    for (const directory of [control, tasks]) {
      try {
        await requireDirectory(directory);
      } catch (error) {
        if (!isNotFound(error)) throw error;
        await mkdir(directory, { mode: 0o700 });
      }
    }
    const directory = join(tasks, taskKey(request.taskId));
    if (!inside(root, directory)) throw new Error("Task control directory escaped the identity root");
    try {
      await requireDirectory(directory);
    } catch (error) {
      if (!isNotFound(error)) throw error;
      await mkdir(directory, { mode: 0o700 });
    }
    const configPath = join(directory, "mcp.json");
    try {
      const stat = await lstat(configPath);
      if (stat.isSymbolicLink() || !stat.isFile()) throw new Error("Task MCP config is not a regular file");
    } catch (error) {
      if (!isNotFound(error)) throw error;
    }
    const temporary = join(directory, `.mcp-${randomUUID()}.tmp`);
    try {
      await writeFile(temporary, encoded, { flag: "wx", mode: 0o600 });
      await chmod(temporary, 0o600);
      await rename(temporary, configPath);
    } finally {
      await rm(temporary, { force: true }).catch(() => undefined);
    }
    return {
      schema: IDENTITY_CONTROL_PROTOCOL,
      ok: true,
      result: {
        controlDirectory: identityTaskControlDirectory(request.taskId),
        configPath: `${identityTaskControlDirectory(request.taskId)}/mcp.json`,
      },
    };
  } catch (error) {
    return {
      schema: IDENTITY_CONTROL_PROTOCOL,
      ok: false,
      error: { code: "identity_control_failed", message: error instanceof Error ? error.message : String(error) },
    };
  }
}

export async function runIdentityControlStdio(identityRoot = "/identity"): Promise<number> {
  if (process.env.ALTITUDE_IDENTITY_CONTROL !== "1") {
    process.stderr.write("Identity control helper may only run through an Altitude identity capsule binding.\n");
    return 1;
  }
  let request: IdentityControlRequest;
  try {
    const body = await new Response(Bun.stdin.stream()).text();
    if (new TextEncoder().encode(body).byteLength > 256 * 1024) throw new Error("Identity control request is too large");
    request = JSON.parse(body) as IdentityControlRequest;
  } catch (error) {
    process.stdout.write(`${JSON.stringify({
      schema: IDENTITY_CONTROL_PROTOCOL,
      ok: false,
      error: { code: "invalid_request", message: error instanceof Error ? error.message : String(error) },
    } satisfies IdentityControlResponse)}\n`);
    return 1;
  }
  const response = await writeIdentityTaskMcpConfig(identityRoot, request);
  process.stdout.write(`${JSON.stringify(response)}\n`);
  return response.ok ? 0 : 1;
}

async function collect(stream: AsyncIterable<Uint8Array>, limit: number): Promise<string> {
  const decoder = new TextDecoder();
  let text = "";
  let size = 0;
  for await (const chunk of stream) {
    size += chunk.byteLength;
    if (size > limit) throw new Error("Identity control response is too large");
    text += decoder.decode(chunk, { stream: true });
  }
  return `${text}${decoder.decode()}`;
}

export class IdentityControlProvisioner {
  constructor(
    private readonly launcher: ProcessLauncher,
    private readonly executable = "altitude",
  ) {}

  async writeTaskMcpConfig(input: {
    readonly identityId: string;
    readonly taskId: string;
    readonly config: unknown;
  }): Promise<{ readonly controlDirectory: string; readonly configPath: string }> {
    requireId(input.identityId, "Identity id");
    requireId(input.taskId, "Task id");
    const child = await this.launcher.launch({
      target: { kind: "identity-capsule", identityId: input.identityId, taskId: input.taskId },
      executable: this.executable,
      args: ["_identity-control"],
      cwd: "/identity",
      env: { ALTITUDE_IDENTITY_CONTROL: "1" },
      io: "piped",
      purpose: "Write a task-scoped MCP config inside the node-local identity",
    });
    try {
      const request: IdentityControlRequest = {
        schema: IDENTITY_CONTROL_PROTOCOL,
        operation: "write_task_mcp_config",
        taskId: input.taskId,
        config: input.config,
      };
      await child.writeStdin(JSON.stringify(request));
      await child.closeStdin();
      const [exit, stdout, stderr] = await Promise.all([
        child.exited,
        collect(child.stdout, 512 * 1024),
        collect(child.stderr, 64 * 1024),
      ]);
      const response = JSON.parse(stdout) as IdentityControlResponse;
      if (response.schema !== IDENTITY_CONTROL_PROTOCOL || typeof response.ok !== "boolean") {
        throw new Error("Identity control helper returned an unsupported response");
      }
      if (!response.ok) throw new Error(response.error.message);
      if (exit.exitCode !== 0) throw new Error(`Identity control exited ${exit.exitCode}: ${stderr.trim()}`);
      return response.result;
    } catch (error) {
      await child.terminate("SIGTERM").catch(() => undefined);
      throw error;
    }
  }
}
