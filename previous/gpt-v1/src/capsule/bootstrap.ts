import { lstat, mkdir, mkdtemp, readdir, rm, writeFile } from "node:fs/promises";
import { dirname, join, relative, resolve, sep } from "node:path";

import { executeCommandPlan, InMemoryCommandReceiptStore, type CommandExecutor, type PlannedCommand } from "./commands.ts";
import { capsuleManifestDigest, parseCapsuleManifest, verifyCapsuleMembers, type CapsuleCheckpoint, type CapsuleMemberContent } from "./manifest.ts";
import { planGitRestore } from "./git.ts";
import { resolvePortablePath } from "./paths.ts";
import { isSha256Digest, sha256, stableJson, utf8, type Sha256Digest } from "./canonical.ts";
import {
  activatePendingTaskWorkspaceCheckpoint,
  readActiveTaskWorkspaceMarker,
  readPendingTaskWorkspaceCheckpoint,
  TASK_WORKSPACE_FORMAT,
  writeActiveTaskWorkspaceMarker,
} from "./workspace-marker.ts";
import { restoreTaskEnvironment } from "./task-environment.ts";

export const TASK_BOOTSTRAP_PROTOCOL = "altitude.task-bootstrap/v1" as const;
const MAX_HEADER_BYTES = 4 * 1024 * 1024;
const MAX_BOOTSTRAP_BYTES = 1024 * 1024 * 1024;
const MAX_COMMAND_OUTPUT_BYTES = 4 * 1024 * 1024;

type BootstrapHeader = Readonly<{
  schema: typeof TASK_BOOTSTRAP_PROTOCOL;
  checkpointDigest: Sha256Digest;
  manifest: unknown;
  repositorySeed: Readonly<{ size: number; sha256: Sha256Digest }>;
}>;

export interface TaskBootstrapPayload {
  readonly checkpoint: CapsuleCheckpoint;
  readonly checkpointDigest: Sha256Digest;
  readonly repositorySeedBundle: Uint8Array;
}

function concat(chunks: readonly Uint8Array[]): Uint8Array {
  const size = chunks.reduce((total, chunk) => total + chunk.byteLength, 0);
  if (!Number.isSafeInteger(size) || size > MAX_BOOTSTRAP_BYTES) throw new Error("Task bootstrap payload is too large");
  const output = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) {
    output.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return output;
}

export function encodeTaskBootstrapPayload(input: {
  readonly checkpoint: CapsuleCheckpoint;
  readonly repositorySeedBundle: Uint8Array;
}): Uint8Array {
  verifyCapsuleMembers(input.checkpoint.manifest, input.checkpoint.members);
  const checkpointDigest = capsuleManifestDigest(input.checkpoint.manifest);
  const header: BootstrapHeader = {
    schema: TASK_BOOTSTRAP_PROTOCOL,
    checkpointDigest,
    manifest: input.checkpoint.manifest,
    repositorySeed: {
      size: input.repositorySeedBundle.byteLength,
      sha256: sha256(input.repositorySeedBundle),
    },
  };
  const headerBytes = utf8(stableJson(header));
  if (headerBytes.byteLength > MAX_HEADER_BYTES) throw new Error("Task bootstrap header is too large");
  const prefix = new Uint8Array(4);
  new DataView(prefix.buffer).setUint32(0, headerBytes.byteLength, false);
  const byPath = new Map(input.checkpoint.members.map((member) => [member.path, member.bytes] as const));
  return concat([
    prefix,
    headerBytes,
    input.repositorySeedBundle,
    ...input.checkpoint.manifest.members.map((member) => byPath.get(member.path)!),
  ]);
}

function record(value: unknown): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw new Error("Task bootstrap header is invalid");
  return value as Record<string, unknown>;
}

function safeSize(value: unknown, label: string): number {
  if (!Number.isSafeInteger(value) || (value as number) < 0 || (value as number) > MAX_BOOTSTRAP_BYTES) {
    throw new Error(`${label} is invalid`);
  }
  return value as number;
}

export function decodeTaskBootstrapPayload(bytes: Uint8Array): TaskBootstrapPayload {
  if (bytes.byteLength < 4 || bytes.byteLength > MAX_BOOTSTRAP_BYTES) throw new Error("Task bootstrap payload size is invalid");
  const headerSize = new DataView(bytes.buffer, bytes.byteOffset, 4).getUint32(0, false);
  if (headerSize < 2 || headerSize > MAX_HEADER_BYTES || 4 + headerSize > bytes.byteLength) {
    throw new Error("Task bootstrap header size is invalid");
  }
  let headerValue: unknown;
  try {
    headerValue = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes.subarray(4, 4 + headerSize))) as unknown;
  } catch (error) {
    throw new Error("Task bootstrap header is not valid UTF-8 JSON", { cause: error });
  }
  const header = record(headerValue);
  if (header.schema !== TASK_BOOTSTRAP_PROTOCOL || !isSha256Digest(header.checkpointDigest)) {
    throw new Error("Task bootstrap protocol or digest is invalid");
  }
  const manifest = parseCapsuleManifest(header.manifest);
  if (capsuleManifestDigest(manifest) !== header.checkpointDigest) throw new Error("Task bootstrap manifest digest mismatch");
  const seed = record(header.repositorySeed);
  const seedSize = safeSize(seed.size, "Task bootstrap seed size");
  if (!isSha256Digest(seed.sha256)) throw new Error("Task bootstrap seed digest is invalid");
  let offset = 4 + headerSize;
  if (offset + seedSize > bytes.byteLength) throw new Error("Task bootstrap seed is truncated");
  const repositorySeedBundle = bytes.slice(offset, offset + seedSize);
  offset += seedSize;
  if (sha256(repositorySeedBundle) !== seed.sha256) throw new Error("Task bootstrap seed digest mismatch");
  const members: CapsuleMemberContent[] = [];
  for (const descriptor of manifest.members) {
    if (offset + descriptor.size > bytes.byteLength) throw new Error(`Task bootstrap member ${descriptor.path} is truncated`);
    members.push({ path: descriptor.path, bytes: bytes.slice(offset, offset + descriptor.size) });
    offset += descriptor.size;
  }
  if (offset !== bytes.byteLength) throw new Error("Task bootstrap payload has trailing bytes");
  verifyCapsuleMembers(manifest, members);
  return { checkpoint: { manifest, members }, checkpointDigest: header.checkpointDigest, repositorySeedBundle };
}

async function boundedOutput(stream: ReadableStream<Uint8Array>, limit = MAX_COMMAND_OUTPUT_BYTES): Promise<Uint8Array> {
  const reader = stream.getReader();
  const chunks: Uint8Array[] = [];
  let size = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      size += value.byteLength;
      if (size > limit) throw new Error(`Bootstrap stream exceeds ${limit} bytes`);
      chunks.push(value);
    }
  } finally {
    reader.releaseLock();
  }
  return concat(chunks);
}

async function run(program: string, args: readonly string[], cwd: string, stdin?: Uint8Array): Promise<{ exitCode: number; stdout: Uint8Array; stderr: Uint8Array }> {
  const child = Bun.spawn([program, ...args], {
    cwd,
    env: { ...process.env, GIT_CONFIG_NOSYSTEM: "1", GIT_TERMINAL_PROMPT: "0" },
    stdin: stdin === undefined ? "ignore" : new Blob([stdin]),
    stdout: "pipe",
    stderr: "pipe",
  });
  const [exitCode, stdout, stderr] = await Promise.all([child.exited, boundedOutput(child.stdout), boundedOutput(child.stderr)]);
  return { exitCode, stdout, stderr };
}

class BootstrapCommandExecutor implements CommandExecutor {
  constructor(
    private readonly memberRoot: string,
    private readonly members: ReadonlyMap<string, Uint8Array>,
  ) {}

  async execute(command: PlannedCommand): Promise<{ exitCode: number; stdout: Uint8Array; stderr: Uint8Array }> {
    const args = command.arguments.map((argument) => argument.kind === "literal"
      ? argument.value
      : resolvePortablePath(this.memberRoot, argument.path));
    const stdin = command.stdin.kind === "none" ? undefined : this.members.get(command.stdin.path);
    if (command.stdin.kind !== "none" && stdin === undefined) throw new Error(`Missing bootstrap member ${command.stdin.path}`);
    return await run(command.program, args, command.cwd, stdin);
  }
}

function inside(root: string, candidate: string): boolean {
  const fromRoot = relative(root, candidate);
  return fromRoot === "" || (fromRoot !== ".." && !fromRoot.startsWith(`..${sep}`));
}

async function restoreUntracked(root: string, path: string, bytes: Uint8Array): Promise<void> {
  const destination = resolve(root, ...path.split("/"));
  if (!inside(root, destination)) throw new Error(`Untracked path ${path} escaped the workspace`);
  const parentSegments = dirname(path) === "." ? [] : dirname(path).split("/");
  let cursor = root;
  for (const segment of parentSegments) {
    cursor = join(cursor, segment);
    try {
      const stat = await lstat(cursor);
      if (stat.isSymbolicLink() || !stat.isDirectory()) throw new Error(`Untracked parent for ${path} is unsafe`);
    } catch (error) {
      if (typeof error !== "object" || error === null || !("code" in error) || error.code !== "ENOENT") throw error;
      await mkdir(cursor, { mode: 0o755 });
    }
  }
  try {
    await lstat(destination);
    throw new Error(`Untracked path ${path} would overwrite restored content`);
  } catch (error) {
    if (typeof error !== "object" || error === null || !("code" in error) || error.code !== "ENOENT") throw error;
  }
  await writeFile(destination, bytes, { flag: "wx", mode: 0o644 });
}

export async function restoreTaskWorkspace(rootInput: string, payload: TaskBootstrapPayload): Promise<{ reused: boolean }> {
  const root = resolve(rootInput);
  const stat = await lstat(root);
  if (stat.isSymbolicLink() || !stat.isDirectory()) throw new Error("Task workspace must be a regular directory");
  const marker = await readActiveTaskWorkspaceMarker(root);
  const pending = await readPendingTaskWorkspaceCheckpoint(root);
  if (pending !== undefined) {
    if (pending.checkpoint.checkpointDigest === payload.checkpointDigest) {
      await activatePendingTaskWorkspaceCheckpoint(root, payload.checkpointDigest);
      return { reused: true };
    }
    throw new Error("Task workspace contains an unpublished pending checkpoint");
  }
  if (marker !== undefined) {
    if (marker.checkpointDigest === payload.checkpointDigest) return { reused: true };
    throw new Error("Task workspace already contains a different active checkpoint");
  }
  if ((await readdir(root)).length !== 0) throw new Error("Task workspace must be empty before bootstrap");

  const staging = await mkdtemp(join(resolve("/tmp"), "altitude-task-bootstrap-"));
  let started = false;
  try {
    started = true;
    const seedPath = join(staging, "repository.bundle");
    await writeFile(seedPath, payload.repositorySeedBundle, { mode: 0o600 });
    const clone = await run("git", ["clone", "--no-local", "--no-checkout", seedPath, "."], root);
    if (clone.exitCode !== 0) throw new Error(`Cloning task seed failed: ${new TextDecoder().decode(clone.stderr).trim()}`);
    const remote = await run("git", ["remote", "set-url", "origin", payload.checkpoint.manifest.repository.remoteUrl], root);
    if (remote.exitCode !== 0) throw new Error(`Setting task origin failed: ${new TextDecoder().decode(remote.stderr).trim()}`);

    const memberRoot = join(staging, "members");
    await mkdir(memberRoot, { mode: 0o700 });
    const memberMap = new Map<string, Uint8Array>();
    for (const member of payload.checkpoint.members) {
      const path = resolvePortablePath(memberRoot, member.path);
      await mkdir(dirname(path), { recursive: true, mode: 0o700 });
      await writeFile(path, member.bytes, { flag: "wx", mode: 0o600 });
      memberMap.set(member.path, member.bytes);
    }
    const plan = planGitRestore({
      capsuleId: payload.checkpoint.manifest.capsuleId,
      repositoryPath: root,
      snapshot: payload.checkpoint.manifest.repository,
      members: payload.checkpoint.manifest.members,
    });
    await executeCommandPlan(plan.commands, new BootstrapCommandExecutor(memberRoot, memberMap), new InMemoryCommandReceiptStore());
    for (const file of plan.files) {
      const bytes = memberMap.get(file.sourceMember);
      if (bytes === undefined) throw new Error(`Missing untracked member ${file.sourceMember}`);
      await restoreUntracked(root, file.destinationRepositoryPath, bytes);
    }
    try {
      await lstat(join(root, ".altitude"));
      throw new Error("Repository content occupies Altitude's reserved .altitude workbench path");
    } catch (error) {
      if (typeof error !== "object" || error === null || !("code" in error) || error.code !== "ENOENT") throw error;
    }
    await restoreTaskEnvironment(root, payload.checkpoint.members);
    await writeActiveTaskWorkspaceMarker(root, {
      schema: TASK_WORKSPACE_FORMAT,
      capsuleId: payload.checkpoint.manifest.capsuleId,
      taskId: payload.checkpoint.manifest.taskId,
      revision: payload.checkpoint.manifest.revision,
      checkpointDigest: payload.checkpointDigest,
    });
    return { reused: false };
  } catch (error) {
    if (started) {
      for (const entry of await readdir(root).catch(() => [])) {
        await rm(join(root, entry), { recursive: true, force: true }).catch(() => undefined);
      }
    }
    throw error;
  } finally {
    await rm(staging, { recursive: true, force: true });
  }
}

async function readStdinBounded(): Promise<Uint8Array> {
  return await boundedOutput(Bun.stdin.stream(), MAX_BOOTSTRAP_BYTES);
}

export async function runTaskBootstrapStdio(root = "/workspace"): Promise<number> {
  if (process.env.ALTITUDE_TASK_BOOTSTRAP !== "1") {
    process.stderr.write("Task bootstrap may only run through Altitude capsule setup.\n");
    return 1;
  }
  try {
    const bytes = await readStdinBounded();
    const payload = decodeTaskBootstrapPayload(bytes);
    const result = await restoreTaskWorkspace(root, payload);
    process.stdout.write(`${JSON.stringify({ schema: TASK_BOOTSTRAP_PROTOCOL, ok: true, result })}\n`);
    return 0;
  } catch (error) {
    process.stdout.write(`${JSON.stringify({
      schema: TASK_BOOTSTRAP_PROTOCOL,
      ok: false,
      error: error instanceof Error ? error.message : String(error),
    })}\n`);
    return 1;
  }
}
