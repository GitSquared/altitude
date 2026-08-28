import { randomUUID } from "node:crypto";
import { chmod, lstat, mkdir, readFile, rename, rm, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";

import type { RuntimeKind } from "../runtime/capsule-runtime.ts";
import { DEFAULT_NODE_PORT } from "../node/server.ts";

export const LOCAL_CONFIG_PROTOCOL = "altitude.local-config/v1" as const;

export interface AltitudeLocalConfig {
  readonly schema: typeof LOCAL_CONFIG_PROTOCOL;
  readonly runtime: RuntimeKind;
  readonly identity: Readonly<{
    id: string;
    image: string;
  }>;
  readonly taskImage: string;
  readonly network: string;
  readonly node: Readonly<{
    bindHost: string;
    port: number;
    capsuleMcpEndpoint: string;
  }>;
}

function validId(value: unknown): value is string {
  return typeof value === "string" && /^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/u.test(value);
}

function validRuntime(value: unknown): value is RuntimeKind {
  return value === "apple-container" || value === "docker";
}

function validImage(value: unknown): value is string {
  return typeof value === "string" && /^[^\s\u0000]+$/u.test(value);
}

export function validateLocalConfig(value: unknown): AltitudeLocalConfig {
  if (typeof value !== "object" || value === null) throw new Error("Altitude config must be an object");
  const config = value as Partial<AltitudeLocalConfig>;
  if (config.schema !== LOCAL_CONFIG_PROTOCOL) throw new Error("Altitude config schema is unsupported");
  if (!validRuntime(config.runtime)) throw new Error("Altitude config runtime is invalid");
  if (typeof config.identity !== "object" || config.identity === null
    || !validId(config.identity.id) || !validImage(config.identity.image)) {
    throw new Error("Altitude config identity is invalid");
  }
  if (!validImage(config.taskImage)) throw new Error("Altitude task image is invalid");
  if (!validId(config.network)) throw new Error("Altitude network is invalid");
  if (typeof config.node !== "object" || config.node === null) throw new Error("Altitude node config is invalid");
  if (typeof config.node.bindHost !== "string" || !config.node.bindHost.trim() || config.node.bindHost.includes("\0")) {
    throw new Error("Altitude node bind host is invalid");
  }
  if (!Number.isSafeInteger(config.node.port) || config.node.port! < 1 || config.node.port! > 65_535) {
    throw new Error("Altitude node port is invalid");
  }
  const endpoint = new URL(config.node.capsuleMcpEndpoint ?? "");
  if (!['http:', 'https:'].includes(endpoint.protocol) || endpoint.username || endpoint.password || endpoint.pathname !== "/mcp") {
    throw new Error("Altitude capsule MCP endpoint must be credential-free HTTP(S) ending in /mcp");
  }
  return config as AltitudeLocalConfig;
}

export function defaultLocalConfig(runtime: RuntimeKind): AltitudeLocalConfig {
  const capsuleHost = runtime === "apple-container" ? "host.container.internal" : "host.docker.internal";
  return {
    schema: LOCAL_CONFIG_PROTOCOL,
    runtime,
    identity: { id: "claude-personal", image: "altitude-identity:dev" },
    taskImage: "altitude-task:dev",
    network: "altitude-control",
    node: {
      bindHost: process.platform === "linux" ? "0.0.0.0" : "127.0.0.1",
      port: DEFAULT_NODE_PORT,
      capsuleMcpEndpoint: `http://${capsuleHost}:${DEFAULT_NODE_PORT}/mcp`,
    },
  };
}

function notFound(error: unknown): boolean {
  return typeof error === "object" && error !== null && "code" in error && error.code === "ENOENT";
}

export async function readLocalConfig(path: string): Promise<AltitudeLocalConfig | undefined> {
  let stat;
  try {
    stat = await lstat(path);
  } catch (error) {
    if (notFound(error)) return undefined;
    throw error;
  }
  if (stat.isSymbolicLink() || !stat.isFile()) throw new Error("Altitude config must be a regular file");
  if (stat.size > 128 * 1024) throw new Error("Altitude config is too large");
  return validateLocalConfig(JSON.parse(await readFile(path, "utf8")) as unknown);
}

export async function writeLocalConfig(path: string, config: AltitudeLocalConfig): Promise<void> {
  const validated = validateLocalConfig(config);
  const directory = dirname(path);
  await mkdir(directory, { recursive: true, mode: 0o700 });
  const directoryStat = await lstat(directory);
  if (directoryStat.isSymbolicLink() || !directoryStat.isDirectory()) {
    throw new Error("Altitude config directory must be a regular directory");
  }
  try {
    const existing = await lstat(path);
    if (existing.isSymbolicLink() || !existing.isFile()) throw new Error("Altitude config must be a regular file");
  } catch (error) {
    if (!notFound(error)) throw error;
  }
  const temporary = join(directory, `.config-${randomUUID()}.tmp`);
  try {
    await writeFile(temporary, `${JSON.stringify(validated, null, 2)}\n`, { flag: "wx", mode: 0o600 });
    await chmod(temporary, 0o600);
    await rename(temporary, path);
  } finally {
    await rm(temporary, { force: true }).catch(() => undefined);
  }
}
