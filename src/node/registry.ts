import { randomUUID } from "node:crypto";
import {
  chmod,
  lstat,
  mkdir,
  readFile,
  readdir,
  rename,
  rm,
  writeFile,
} from "node:fs/promises";
import { join } from "node:path";

import {
  parseNodeConnection,
  type NodeConnection,
} from "./iroh.ts";

export const REGISTERED_NODE_PROTOCOL = "altitude.registered-node/v1" as const;
const MAX_RECORD_BYTES = 128 * 1024;
const SAFE_ID = /^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/u;

export interface RegisteredNode {
  readonly schema: typeof REGISTERED_NODE_PROTOCOL;
  readonly connection: NodeConnection;
}

function notFound(error: unknown): boolean {
  return typeof error === "object"
    && error !== null
    && "code" in error
    && error.code === "ENOENT";
}

function nodeId(value: unknown): string {
  if (typeof value !== "string" || !SAFE_ID.test(value)) {
    throw new Error("Node id is invalid");
  }
  return value;
}

export function parseRegisteredNode(value: unknown): RegisteredNode {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error("Registered node is invalid");
  }
  const record = value as Record<string, unknown>;
  if (record.schema !== REGISTERED_NODE_PROTOCOL) {
    throw new Error("Registered node schema is unsupported");
  }
  return {
    schema: REGISTERED_NODE_PROTOCOL,
    connection: parseNodeConnection(record.connection),
  };
}

export function registeredNode(connection: NodeConnection): RegisteredNode {
  return parseRegisteredNode({
    schema: REGISTERED_NODE_PROTOCOL,
    connection,
  });
}

/** A private, atomic registry of remembered node routes. */
export class FilesystemNodeRegistry {
  constructor(private readonly root: string) {}

  async get(id: string): Promise<RegisteredNode | undefined> {
    const safeId = nodeId(id);
    const path = this.path(safeId);
    let stat;
    try {
      stat = await lstat(path);
    } catch (error) {
      if (notFound(error)) return undefined;
      throw error;
    }
    if (stat.isSymbolicLink() || !stat.isFile()) {
      throw new Error(`Registered node ${safeId} must be a regular file`);
    }
    if (stat.size > MAX_RECORD_BYTES) {
      throw new Error(`Registered node ${safeId} is too large`);
    }
    const record = parseRegisteredNode(
      JSON.parse(await readFile(path, "utf8")) as unknown,
    );
    if (record.connection.nodeId !== safeId) {
      throw new Error(`Registered node ${safeId} has a mismatched id`);
    }
    return record;
  }

  async list(): Promise<readonly RegisteredNode[]> {
    await this.ensureRoot();
    const entries = await readdir(this.root, { withFileTypes: true });
    const ids: string[] = [];
    for (const entry of entries) {
      if (!entry.name.endsWith(".json")) continue;
      if (!entry.isFile()) {
        throw new Error(`Registered node entry ${entry.name} is not a regular file`);
      }
      ids.push(nodeId(entry.name.slice(0, -".json".length)));
    }
    const records = await Promise.all(ids.sort().map(async (id) => {
      const record = await this.get(id);
      if (record === undefined) throw new Error(`Registered node ${id} disappeared`);
      return record;
    }));
    return records;
  }

  async put(value: RegisteredNode): Promise<RegisteredNode> {
    const record = parseRegisteredNode(value);
    const id = nodeId(record.connection.nodeId);
    await this.ensureRoot();
    const path = this.path(id);
    try {
      const existing = await lstat(path);
      if (existing.isSymbolicLink() || !existing.isFile()) {
        throw new Error(`Registered node ${id} must be a regular file`);
      }
    } catch (error) {
      if (!notFound(error)) throw error;
    }
    const temporary = join(this.root, `.${id}-${randomUUID()}.tmp`);
    try {
      await writeFile(temporary, `${JSON.stringify(record, null, 2)}\n`, {
        flag: "wx",
        mode: 0o600,
      });
      await chmod(temporary, 0o600);
      await rename(temporary, path);
    } finally {
      await rm(temporary, { force: true }).catch(() => undefined);
    }
    return record;
  }

  private path(id: string): string {
    return join(this.root, `${nodeId(id)}.json`);
  }

  private async ensureRoot(): Promise<void> {
    await mkdir(this.root, { recursive: true, mode: 0o700 });
    const stat = await lstat(this.root);
    if (stat.isSymbolicLink() || !stat.isDirectory()) {
      throw new Error("Node registry must be a regular directory");
    }
    await chmod(this.root, 0o700);
  }
}
