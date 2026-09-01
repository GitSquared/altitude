import { randomUUID } from "node:crypto";
import { chmod, lstat, mkdir, open, readFile, rename, rm, writeFile } from "node:fs/promises";
import { join, resolve } from "node:path";

import { isSha256Digest, sha256, stableJson, type Sha256Digest } from "./canonical.ts";
import { MAX_CAPSULE_WIRE_BYTES } from "./wire.ts";

export const TASK_WORKSPACE_FORMAT = "altitude.task-workspace/v1" as const;
export const TASK_WORKSPACE_PENDING_FORMAT = "altitude.task-workspace-pending/v1" as const;

export interface TaskWorkspaceCheckpointMarker {
  readonly schema: typeof TASK_WORKSPACE_FORMAT;
  readonly capsuleId: string;
  readonly taskId: string;
  readonly revision: number;
  readonly checkpointDigest: Sha256Digest;
}

export interface PendingTaskWorkspaceCheckpoint {
  readonly previousCheckpointDigest: Sha256Digest;
  readonly checkpoint: TaskWorkspaceCheckpointMarker;
  readonly wireBytes: Uint8Array;
}

type PendingMetadata = Readonly<{
  schema: typeof TASK_WORKSPACE_PENDING_FORMAT;
  previousCheckpointDigest: Sha256Digest;
  checkpoint: TaskWorkspaceCheckpointMarker;
  wireSize: number;
  wireSha256: Sha256Digest;
}>;

function requireId(value: unknown, label: string): asserts value is string {
  if (typeof value !== "string" || !/^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/u.test(value)) {
    throw new Error(`${label} is invalid`);
  }
}

function record(value: unknown, label: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw new Error(`${label} is invalid`);
  return value as Record<string, unknown>;
}

function parseMarker(value: unknown): TaskWorkspaceCheckpointMarker {
  const marker = record(value, "Task workspace checkpoint marker");
  if (marker.schema !== TASK_WORKSPACE_FORMAT) {
    throw new Error("Task workspace checkpoint marker schema is invalid");
  }
  requireId(marker.capsuleId, "Task workspace capsule id");
  requireId(marker.taskId, "Task workspace task id");
  if (!Number.isSafeInteger(marker.revision) || (marker.revision as number) < 0) {
    throw new Error("Task workspace revision is invalid");
  }
  if (!isSha256Digest(marker.checkpointDigest)) throw new Error("Task workspace checkpoint digest is invalid");
  return {
    schema: TASK_WORKSPACE_FORMAT,
    capsuleId: marker.capsuleId,
    taskId: marker.taskId,
    revision: marker.revision as number,
    checkpointDigest: marker.checkpointDigest,
  };
}

function altitudeDirectory(rootInput: string): string {
  return join(resolve(rootInput), ".git", "altitude");
}

function isNotFound(error: unknown): boolean {
  return typeof error === "object" && error !== null && "code" in error && error.code === "ENOENT";
}

async function requireRegularDirectory(path: string, label: string): Promise<void> {
  const stat = await lstat(path);
  if (stat.isSymbolicLink() || !stat.isDirectory()) throw new Error(`${label} is unsafe`);
}

async function ensureMarkerDirectory(root: string): Promise<string> {
  const git = join(resolve(root), ".git");
  await requireRegularDirectory(git, "Task Git directory");
  const directory = altitudeDirectory(root);
  try {
    await requireRegularDirectory(directory, "Task workspace marker directory");
  } catch (error) {
    if (!isNotFound(error)) throw error;
    await mkdir(directory, { mode: 0o700 });
  }
  await chmod(directory, 0o700);
  return directory;
}

async function existingMarkerDirectory(root: string): Promise<string | undefined> {
  const git = join(resolve(root), ".git");
  try {
    await requireRegularDirectory(git, "Task Git directory");
    const directory = altitudeDirectory(root);
    await requireRegularDirectory(directory, "Task workspace marker directory");
    return directory;
  } catch (error) {
    if (isNotFound(error)) return undefined;
    throw error;
  }
}

async function readJsonFile(path: string, label: string): Promise<unknown | undefined> {
  try {
    const stat = await lstat(path);
    if (stat.isSymbolicLink() || !stat.isFile() || stat.size > 64 * 1024) throw new Error(`${label} is unsafe`);
    return JSON.parse(await readFile(path, "utf8")) as unknown;
  } catch (error) {
    if (isNotFound(error)) return undefined;
    throw error;
  }
}

async function writeAtomicJson(directory: string, filename: string, value: unknown): Promise<void> {
  const temporary = join(directory, `.${filename}-${randomUUID()}.tmp`);
  const destination = join(directory, filename);
  try {
    await writeFile(temporary, `${stableJson(value)}\n`, { flag: "wx", mode: 0o600 });
    await chmod(temporary, 0o600);
    await rename(temporary, destination);
    const handle = await open(destination, "r");
    try { await handle.sync(); } finally { await handle.close(); }
  } finally {
    await rm(temporary, { force: true }).catch(() => undefined);
  }
}

export async function readActiveTaskWorkspaceMarker(root: string): Promise<TaskWorkspaceCheckpointMarker | undefined> {
  const directory = await existingMarkerDirectory(root);
  if (directory === undefined) return undefined;
  const value = await readJsonFile(join(directory, "active.json"), "Task workspace active marker");
  return value === undefined ? undefined : parseMarker(value);
}

export async function writeActiveTaskWorkspaceMarker(root: string, marker: TaskWorkspaceCheckpointMarker): Promise<void> {
  const normalized = parseMarker(marker);
  const directory = await ensureMarkerDirectory(root);
  await writeAtomicJson(directory, "active.json", normalized);
}

function pendingArchiveName(digest: Sha256Digest): string {
  return `pending-${digest.slice("sha256:".length)}.capsule`;
}

function parsePendingMetadata(value: unknown): PendingMetadata {
  const metadata = record(value, "Pending task workspace checkpoint");
  if (metadata.schema !== TASK_WORKSPACE_PENDING_FORMAT || !isSha256Digest(metadata.previousCheckpointDigest)) {
    throw new Error("Pending task workspace checkpoint metadata is invalid");
  }
  const checkpoint = parseMarker(metadata.checkpoint);
  if (!Number.isSafeInteger(metadata.wireSize) || (metadata.wireSize as number) <= 0
    || (metadata.wireSize as number) > MAX_CAPSULE_WIRE_BYTES || !isSha256Digest(metadata.wireSha256)) {
    throw new Error("Pending task workspace checkpoint wire metadata is invalid");
  }
  return {
    schema: TASK_WORKSPACE_PENDING_FORMAT,
    previousCheckpointDigest: metadata.previousCheckpointDigest,
    checkpoint,
    wireSize: metadata.wireSize as number,
    wireSha256: metadata.wireSha256,
  };
}

export async function readPendingTaskWorkspaceCheckpoint(root: string): Promise<PendingTaskWorkspaceCheckpoint | undefined> {
  const directory = await existingMarkerDirectory(root);
  if (directory === undefined) return undefined;
  const value = await readJsonFile(join(directory, "pending.json"), "Task workspace pending marker");
  if (value === undefined) return undefined;
  const metadata = parsePendingMetadata(value);
  const archivePath = join(directory, pendingArchiveName(metadata.checkpoint.checkpointDigest));
  const stat = await lstat(archivePath);
  if (stat.isSymbolicLink() || !stat.isFile() || stat.size !== metadata.wireSize) {
    throw new Error("Pending task workspace checkpoint archive is unsafe or incomplete");
  }
  const wireBytes = new Uint8Array(await readFile(archivePath));
  if (sha256(wireBytes) !== metadata.wireSha256) throw new Error("Pending task workspace checkpoint archive failed verification");
  return {
    previousCheckpointDigest: metadata.previousCheckpointDigest,
    checkpoint: metadata.checkpoint,
    wireBytes,
  };
}

export async function writePendingTaskWorkspaceCheckpoint(
  root: string,
  pending: PendingTaskWorkspaceCheckpoint,
): Promise<void> {
  if (!isSha256Digest(pending.previousCheckpointDigest)) throw new Error("Previous task checkpoint digest is invalid");
  const checkpoint = parseMarker(pending.checkpoint);
  if (pending.wireBytes.byteLength === 0 || pending.wireBytes.byteLength > MAX_CAPSULE_WIRE_BYTES) {
    throw new Error("Pending task workspace checkpoint archive size is invalid");
  }
  const active = await readActiveTaskWorkspaceMarker(root);
  if (active === undefined || active.checkpointDigest !== pending.previousCheckpointDigest
    || active.taskId !== checkpoint.taskId || active.capsuleId !== checkpoint.capsuleId
    || checkpoint.revision <= active.revision) {
    throw new Error("Pending task checkpoint does not advance the active workspace checkpoint");
  }
  const directory = await ensureMarkerDirectory(root);
  const archiveName = pendingArchiveName(checkpoint.checkpointDigest);
  const archivePath = join(directory, archiveName);
  const temporary = join(directory, `.${archiveName}-${randomUUID()}.tmp`);
  try {
    try {
      const existing = await lstat(archivePath);
      if (existing.isSymbolicLink() || !existing.isFile() || existing.size !== pending.wireBytes.byteLength
        || sha256(new Uint8Array(await readFile(archivePath))) !== sha256(pending.wireBytes)) {
        throw new Error("Immutable pending task checkpoint archive conflicts with existing content");
      }
    } catch (error) {
      if (!isNotFound(error)) throw error;
      await writeFile(temporary, pending.wireBytes, { flag: "wx", mode: 0o600 });
      await chmod(temporary, 0o600);
      await rename(temporary, archivePath);
    }
    await writeAtomicJson(directory, "pending.json", {
      schema: TASK_WORKSPACE_PENDING_FORMAT,
      previousCheckpointDigest: pending.previousCheckpointDigest,
      checkpoint,
      wireSize: pending.wireBytes.byteLength,
      wireSha256: sha256(pending.wireBytes),
    } satisfies PendingMetadata);
  } finally {
    await rm(temporary, { force: true }).catch(() => undefined);
  }
}

export async function activatePendingTaskWorkspaceCheckpoint(
  root: string,
  expectedDigest: Sha256Digest,
): Promise<TaskWorkspaceCheckpointMarker> {
  if (!isSha256Digest(expectedDigest)) throw new Error("Expected task checkpoint digest is invalid");
  const pending = await readPendingTaskWorkspaceCheckpoint(root);
  if (pending === undefined || pending.checkpoint.checkpointDigest !== expectedDigest) {
    const active = await readActiveTaskWorkspaceMarker(root);
    if (active?.checkpointDigest === expectedDigest) return active;
    throw new Error("Expected pending task workspace checkpoint does not exist");
  }
  await writeActiveTaskWorkspaceMarker(root, pending.checkpoint);
  const directory = altitudeDirectory(root);
  await rm(join(directory, "pending.json"), { force: true });
  await rm(join(directory, pendingArchiveName(expectedDigest)), { force: true });
  return pending.checkpoint;
}
