import {
  compareCanonicalStrings,
  isSha256Digest,
  sha256,
  stableJson,
  utf8,
  type Sha256Digest,
} from "./canonical.ts";
import { captureLocalGitCheckpoint } from "./local-git.ts";
import {
  describeCapsuleMember,
  capsuleManifestDigest,
  type ActiveOwnershipLease,
  type CapsuleCheckpoint,
  type CapsuleMemberContent,
  type GitRepositorySnapshot,
} from "./manifest.ts";
import { portableCapsulePath } from "./paths.ts";
import { decodeCapsuleWire, encodeCapsuleWire, MAX_CAPSULE_WIRE_BYTES } from "./wire.ts";
import {
  activatePendingTaskWorkspaceCheckpoint,
  readActiveTaskWorkspaceMarker,
  readPendingTaskWorkspaceCheckpoint,
  TASK_WORKSPACE_FORMAT,
  writePendingTaskWorkspaceCheckpoint,
} from "./workspace-marker.ts";

export const TASK_CHECKPOINT_PROTOCOL = "altitude.task-checkpoint/v1" as const;
const MAX_TASK_CHECKPOINT_HEADER_BYTES = 4 * 1024 * 1024;

export interface TaskCheckpointCaptureRequest {
  readonly schema: typeof TASK_CHECKPOINT_PROTOCOL;
  readonly taskId: string;
  readonly capsuleId: string;
  readonly revision: number;
  readonly checkpointedAt: string;
  readonly ownership: ActiveOwnershipLease;
  readonly previousCheckpointDigest: Sha256Digest;
  readonly repositoryBindingDigest: Sha256Digest;
  readonly additionalMembers: readonly CapsuleMemberContent[];
}

type TaskCheckpointHeader = Readonly<{
  schema: typeof TASK_CHECKPOINT_PROTOCOL;
  operation: "capture";
  taskId: string;
  capsuleId: string;
  revision: number;
  checkpointedAt: string;
  ownership: ActiveOwnershipLease;
  previousCheckpointDigest: Sha256Digest;
  repositoryBindingDigest: Sha256Digest;
  additionalMembers: readonly ReturnType<typeof describeCapsuleMember>[];
}>;

function record(value: unknown, label: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw new Error(`${label} is invalid`);
  return value as Record<string, unknown>;
}

function requireId(value: unknown, label: string): asserts value is string {
  if (typeof value !== "string" || !/^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/u.test(value)) {
    throw new Error(`${label} is invalid`);
  }
}

function requireRevision(value: unknown): asserts value is number {
  if (!Number.isSafeInteger(value) || (value as number) < 1) throw new Error("Task checkpoint revision is invalid");
}

function parseOwnership(value: unknown): ActiveOwnershipLease {
  const ownership = record(value, "Task checkpoint ownership");
  if (ownership.status !== "active" || !Number.isSafeInteger(ownership.epoch) || (ownership.epoch as number) < 0) {
    throw new Error("Task checkpoint ownership is invalid");
  }
  requireId(ownership.leaseId, "Task checkpoint lease id");
  requireId(ownership.ownerNodeId, "Task checkpoint owner node id");
  return {
    epoch: ownership.epoch as number,
    leaseId: ownership.leaseId,
    ownerNodeId: ownership.ownerNodeId,
    status: "active",
  };
}

function parseInstant(value: unknown): string {
  if (typeof value !== "string" || !Number.isFinite(Date.parse(value)) || new Date(Date.parse(value)).toISOString() !== value) {
    throw new Error("Task checkpoint time is invalid");
  }
  return value;
}

function normalizeAdditionalMembers(members: readonly CapsuleMemberContent[]): CapsuleMemberContent[] {
  const normalized = members.map((member) => ({
    path: portableCapsulePath(member.path),
    bytes: member.bytes.slice(),
  })).sort((left, right) => compareCanonicalStrings(left.path, right.path));
  let previous: string | undefined;
  for (const member of normalized) {
    if (member.path.startsWith("git/")) throw new Error("Additional task checkpoint members cannot occupy the Git namespace");
    if (previous === member.path) throw new Error(`Duplicate task checkpoint member ${member.path}`);
    previous = member.path;
  }
  return normalized;
}

function validateRequest(input: Omit<TaskCheckpointCaptureRequest, "additionalMembers"> & {
  readonly additionalMembers: readonly CapsuleMemberContent[];
}): TaskCheckpointCaptureRequest {
  if (input.schema !== TASK_CHECKPOINT_PROTOCOL) throw new Error("Task checkpoint protocol is invalid");
  requireId(input.taskId, "Task checkpoint task id");
  requireId(input.capsuleId, "Task checkpoint capsule id");
  requireRevision(input.revision);
  const checkpointedAt = parseInstant(input.checkpointedAt);
  const ownership = parseOwnership(input.ownership);
  if (!isSha256Digest(input.previousCheckpointDigest)) throw new Error("Previous task checkpoint digest is invalid");
  if (!isSha256Digest(input.repositoryBindingDigest)) throw new Error("Task repository binding digest is invalid");
  return {
    schema: TASK_CHECKPOINT_PROTOCOL,
    taskId: input.taskId,
    capsuleId: input.capsuleId,
    revision: input.revision,
    checkpointedAt,
    ownership,
    previousCheckpointDigest: input.previousCheckpointDigest,
    repositoryBindingDigest: input.repositoryBindingDigest,
    additionalMembers: normalizeAdditionalMembers(input.additionalMembers),
  };
}

function concat(chunks: readonly Uint8Array[]): Uint8Array {
  const size = chunks.reduce((total, chunk) => total + chunk.byteLength, 0);
  if (!Number.isSafeInteger(size) || size > MAX_CAPSULE_WIRE_BYTES) throw new Error("Task checkpoint request is too large");
  const output = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) {
    output.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return output;
}

export function encodeTaskCheckpointRequest(input: TaskCheckpointCaptureRequest): Uint8Array {
  const request = validateRequest(input);
  const header: TaskCheckpointHeader = {
    schema: TASK_CHECKPOINT_PROTOCOL,
    operation: "capture",
    taskId: request.taskId,
    capsuleId: request.capsuleId,
    revision: request.revision,
    checkpointedAt: request.checkpointedAt,
    ownership: request.ownership,
    previousCheckpointDigest: request.previousCheckpointDigest,
    repositoryBindingDigest: request.repositoryBindingDigest,
    additionalMembers: request.additionalMembers.map(describeCapsuleMember),
  };
  const headerBytes = utf8(stableJson(header));
  if (headerBytes.byteLength > MAX_TASK_CHECKPOINT_HEADER_BYTES) throw new Error("Task checkpoint request header is too large");
  const prefix = new Uint8Array(4);
  new DataView(prefix.buffer).setUint32(0, headerBytes.byteLength, false);
  return concat([prefix, headerBytes, ...request.additionalMembers.map((member) => member.bytes)]);
}

export function decodeTaskCheckpointRequest(bytes: Uint8Array): TaskCheckpointCaptureRequest {
  if (bytes.byteLength < 4 || bytes.byteLength > MAX_CAPSULE_WIRE_BYTES) throw new Error("Task checkpoint request size is invalid");
  const headerSize = new DataView(bytes.buffer, bytes.byteOffset, 4).getUint32(0, false);
  if (headerSize < 2 || headerSize > MAX_TASK_CHECKPOINT_HEADER_BYTES || 4 + headerSize > bytes.byteLength) {
    throw new Error("Task checkpoint request header size is invalid");
  }
  let value: unknown;
  try {
    value = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes.subarray(4, 4 + headerSize))) as unknown;
  } catch (error) {
    throw new Error("Task checkpoint request header is not valid UTF-8 JSON", { cause: error });
  }
  const header = record(value, "Task checkpoint request header");
  if (header.schema !== TASK_CHECKPOINT_PROTOCOL || header.operation !== "capture") {
    throw new Error("Task checkpoint request operation is invalid");
  }
  if (!Array.isArray(header.additionalMembers)) throw new Error("Task checkpoint member descriptors are invalid");
  let offset = 4 + headerSize;
  const additionalMembers: CapsuleMemberContent[] = [];
  for (const raw of header.additionalMembers) {
    const descriptor = record(raw, "Task checkpoint member descriptor");
    const path = portableCapsulePath(typeof descriptor.path === "string" ? descriptor.path : "");
    if (!Number.isSafeInteger(descriptor.size) || (descriptor.size as number) < 0 || !isSha256Digest(descriptor.sha256)) {
      throw new Error(`Task checkpoint member descriptor for ${path} is invalid`);
    }
    const size = descriptor.size as number;
    if (offset + size > bytes.byteLength) throw new Error(`Task checkpoint member ${path} is truncated`);
    const memberBytes = bytes.slice(offset, offset + size);
    offset += size;
    if (sha256(memberBytes) !== descriptor.sha256) throw new Error(`Task checkpoint member ${path} failed verification`);
    additionalMembers.push({ path, bytes: memberBytes });
  }
  if (offset !== bytes.byteLength) throw new Error("Task checkpoint request has trailing bytes");
  return validateRequest({
    schema: TASK_CHECKPOINT_PROTOCOL,
    taskId: typeof header.taskId === "string" ? header.taskId : "",
    capsuleId: typeof header.capsuleId === "string" ? header.capsuleId : "",
    revision: header.revision as number,
    checkpointedAt: typeof header.checkpointedAt === "string" ? header.checkpointedAt : "",
    ownership: header.ownership as ActiveOwnershipLease,
    previousCheckpointDigest: header.previousCheckpointDigest as Sha256Digest,
    repositoryBindingDigest: header.repositoryBindingDigest as Sha256Digest,
    additionalMembers,
  });
}

export function repositorySeedBindingDigest(
  repository: Pick<GitRepositorySnapshot, "remoteUrl" | "defaultBranch" | "baseCommit">,
): Sha256Digest {
  return sha256(utf8(stableJson({
    remoteUrl: repository.remoteUrl,
    defaultBranch: repository.defaultBranch,
    baseCommit: repository.baseCommit,
  })));
}

async function readStdinBounded(): Promise<Uint8Array> {
  const reader = Bun.stdin.stream().getReader();
  const chunks: Uint8Array[] = [];
  let size = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      size += value.byteLength;
      if (size > MAX_CAPSULE_WIRE_BYTES) throw new Error("Task checkpoint input is too large");
      chunks.push(value);
    }
  } finally {
    reader.releaseLock();
  }
  return concat(chunks);
}

export async function captureTaskWorkspaceCheckpoint(
  root: string,
  input: TaskCheckpointCaptureRequest,
): Promise<CapsuleCheckpoint> {
  const request = validateRequest(input);
  if (process.env.ALTITUDE_TASK_ID !== undefined && process.env.ALTITUDE_TASK_ID !== request.taskId) {
    throw new Error("Task checkpoint request is bound to another task capsule");
  }
  const active = await readActiveTaskWorkspaceMarker(root);
  if (active === undefined || active.checkpointDigest !== request.previousCheckpointDigest
    || active.taskId !== request.taskId || active.capsuleId !== request.capsuleId
    || request.revision <= active.revision) {
    throw new Error("Task checkpoint request does not advance the active workspace checkpoint");
  }
  const captured = await captureLocalGitCheckpoint({
    taskId: request.taskId,
    capsuleId: request.capsuleId,
    repositoryPath: root,
    revision: request.revision,
    checkpointedAt: request.checkpointedAt,
    ownership: request.ownership,
    additionalMembers: request.additionalMembers,
  });
  if (repositorySeedBindingDigest(captured.checkpoint.manifest.repository) !== request.repositoryBindingDigest) {
    throw new Error("Task workspace changed its repository seed binding");
  }
  const wireBytes = encodeCapsuleWire(captured.checkpoint);
  const checkpointDigest = capsuleManifestDigest(captured.checkpoint.manifest);
  await writePendingTaskWorkspaceCheckpoint(root, {
    previousCheckpointDigest: request.previousCheckpointDigest,
    checkpoint: {
      schema: TASK_WORKSPACE_FORMAT,
      capsuleId: request.capsuleId,
      taskId: request.taskId,
      revision: request.revision,
      checkpointDigest,
    },
    wireBytes,
  });
  return captured.checkpoint;
}

async function activate(root: string): Promise<void> {
  const body = new TextDecoder("utf-8", { fatal: true }).decode(await readStdinBounded());
  const value = record(JSON.parse(body) as unknown, "Task checkpoint activation request");
  if (value.schema !== TASK_CHECKPOINT_PROTOCOL || value.operation !== "activate" || !isSha256Digest(value.checkpointDigest)) {
    throw new Error("Task checkpoint activation request is invalid");
  }
  await activatePendingTaskWorkspaceCheckpoint(root, value.checkpointDigest);
}

export async function runTaskCheckpointStdio(operation: string | undefined, root = "/workspace"): Promise<number> {
  if (process.env.ALTITUDE_TASK_CHECKPOINT !== "1") {
    process.stderr.write("Task checkpoint operations may only run through Altitude capsule management.\n");
    return 1;
  }
  try {
    if (operation === "capture") {
      const request = decodeTaskCheckpointRequest(await readStdinBounded());
      process.stdout.write(encodeCapsuleWire(await captureTaskWorkspaceCheckpoint(root, request)));
      return 0;
    }
    if (operation === "pending") {
      const pending = await readPendingTaskWorkspaceCheckpoint(root);
      if (pending === undefined) return 3;
      const checkpoint = decodeCapsuleWire(pending.wireBytes);
      if (capsuleManifestDigest(checkpoint.manifest) !== pending.checkpoint.checkpointDigest) {
        throw new Error("Pending task checkpoint digest does not match its workspace marker");
      }
      process.stdout.write(pending.wireBytes);
      return 0;
    }
    if (operation === "activate") {
      await activate(root);
      process.stdout.write(`${JSON.stringify({ schema: TASK_CHECKPOINT_PROTOCOL, ok: true })}\n`);
      return 0;
    }
    throw new Error("Unknown task checkpoint operation");
  } catch (error) {
    process.stderr.write(`${error instanceof Error ? error.message : String(error)}\n`);
    return 1;
  }
}
