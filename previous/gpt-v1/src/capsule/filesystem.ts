import { randomUUID } from "node:crypto";
import { constants } from "node:fs";
import {
  chmod,
  lstat,
  mkdir,
  mkdtemp,
  open,
  readdir,
  rename,
  rm,
} from "node:fs/promises";
import { isAbsolute, join, parse, relative, resolve, sep } from "node:path";

import {
  isSha256Digest,
  stableJson,
  type Sha256Digest,
  utf8,
} from "./canonical.ts";
import {
  capsuleManifestDigest,
  parseCapsuleManifest,
  verifyCapsuleMembers,
  type CapsuleCheckpoint,
  type CapsuleManifestV1,
  type CapsuleMemberContent,
} from "./manifest.ts";
import { portableCapsulePath } from "./paths.ts";

export const ACTIVE_CHECKPOINT_FORMAT =
  "altitude.active-checkpoint/v1" as const;

export interface CapsuleCheckpointReference {
  readonly capsuleId: string;
  readonly revision: number;
  readonly digest: Sha256Digest;
}

export interface CapsuleCheckpointFilesystemLimits {
  readonly maxManifestBytes: number;
  readonly maxMembers: number;
  readonly maxMemberBytes: number;
  readonly maxTotalBytes: number;
}

export const DEFAULT_CAPSULE_CHECKPOINT_FILESYSTEM_LIMITS:
  CapsuleCheckpointFilesystemLimits = Object.freeze({
    maxManifestBytes: 4 * 1024 * 1024,
    maxMembers: 20_000,
    maxMemberBytes: 512 * 1024 * 1024,
    maxTotalBytes: 4 * 1024 * 1024 * 1024,
  });

interface ActiveCheckpointPointer extends CapsuleCheckpointReference {
  readonly format: typeof ACTIVE_CHECKPOINT_FORMAT;
}

export class UnsafeCapsuleFilesystemError extends Error {
  override readonly name = "UnsafeCapsuleFilesystemError";
}

export class CapsuleCheckpointLimitError extends Error {
  override readonly name = "CapsuleCheckpointLimitError";
}

export class CapsuleCheckpointNotFoundError extends Error {
  override readonly name = "CapsuleCheckpointNotFoundError";
}

export class ImmutableCapsuleCheckpointConflictError extends Error {
  override readonly name = "ImmutableCapsuleCheckpointConflictError";
}

/**
 * Stores immutable, portable checkpoints under a caller-selected root.
 *
 * A checkpoint is first written into a private staging directory. Every file
 * and the manifest are fsynced, then the staged directory is read back through
 * the same bounded, no-follow verifier used after restart. Only a verified
 * directory is atomically renamed into the immutable checkpoint namespace.
 * Activation is a second atomic operation that updates a small pointer only
 * after the referenced checkpoint has passed verification.
 */
export class FilesystemCapsuleCheckpointStore {
  readonly root: string;
  readonly limits: CapsuleCheckpointFilesystemLimits;
  readonly #initialization: Promise<void>;

  constructor(
    root: string,
    limits: Partial<CapsuleCheckpointFilesystemLimits> = {},
  ) {
    if (!isAbsolute(root)) {
      throw new UnsafeCapsuleFilesystemError(
        "Capsule checkpoint root must be an explicit absolute path",
      );
    }
    const normalized = resolve(root);
    if (normalized === parse(normalized).root) {
      throw new UnsafeCapsuleFilesystemError(
        "A filesystem root cannot be used as the capsule checkpoint root",
      );
    }
    this.root = normalized;
    this.limits = validateLimits({
      ...DEFAULT_CAPSULE_CHECKPOINT_FILESYSTEM_LIMITS,
      ...limits,
    });
    this.#initialization = this.#initialize();
  }

  checkpointDirectory(reference: CapsuleCheckpointReference): string {
    validateCheckpointReference(reference);
    return this.#scoped(
      "capsules",
      reference.capsuleId,
      "checkpoints",
      checkpointDirectoryName(reference),
    );
  }

  async publish(
    checkpoint: CapsuleCheckpoint,
  ): Promise<CapsuleCheckpointReference> {
    await this.#initialization;
    const normalized = normalizeCheckpoint(checkpoint);
    assertCheckpointWithinLimits(normalized, this.limits);
    const reference = referenceForManifest(normalized.manifest);
    const capsuleDirectory = await this.#ensureCapsuleDirectory(
      reference.capsuleId,
    );
    const checkpointsDirectory = join(capsuleDirectory, "checkpoints");
    const finalDirectory = this.checkpointDirectory(reference);

    if (await pathExists(finalDirectory)) {
      try {
        await readCheckpointDirectory(finalDirectory, reference, this.limits);
      } catch (error) {
        throw new ImmutableCapsuleCheckpointConflictError(
          `Checkpoint ${formatReference(reference)} already exists but is not the same valid immutable checkpoint`,
          { cause: error },
        );
      }
      return reference;
    }

    const stagingRoot = this.#scoped(".staging");
    await assertRegularDirectory(stagingRoot, "checkpoint staging root");
    const stagingDirectory = await mkdtemp(
      join(stagingRoot, `${reference.capsuleId}-${reference.revision}-`),
    );
    await chmod(stagingDirectory, 0o700);
    let stagingStillExists = true;
    try {
      await writeCheckpointDirectory(stagingDirectory, normalized);
      await readCheckpointDirectory(stagingDirectory, reference, this.limits);

      try {
        await rename(stagingDirectory, finalDirectory);
        stagingStillExists = false;
      } catch (error) {
        if (!isAlreadyExists(error)) throw error;
        try {
          await readCheckpointDirectory(finalDirectory, reference, this.limits);
        } catch (verificationError) {
          throw new ImmutableCapsuleCheckpointConflictError(
            `Checkpoint ${formatReference(reference)} collided with invalid existing content`,
            { cause: verificationError },
          );
        }
      }
      await syncDirectory(checkpointsDirectory);
      return reference;
    } finally {
      if (stagingStillExists) {
        await rm(stagingDirectory, { recursive: true, force: true }).catch(
          () => undefined,
        );
      }
    }
  }

  async read(
    reference: CapsuleCheckpointReference,
  ): Promise<CapsuleCheckpoint> {
    await this.#initialization;
    validateCheckpointReference(reference);
    const directory = this.checkpointDirectory(reference);
    try {
      await this.#assertCapsuleDirectory(reference.capsuleId, true);
      return await readCheckpointDirectory(directory, reference, this.limits);
    } catch (error) {
      if (isNotFound(error)) {
        throw new CapsuleCheckpointNotFoundError(
          `Checkpoint ${formatReference(reference)} does not exist`,
          { cause: error },
        );
      }
      throw error;
    }
  }

  async activate(
    reference: CapsuleCheckpointReference,
  ): Promise<CapsuleCheckpoint> {
    const checkpoint = await this.read(reference);
    const capsuleDirectory = await this.#ensureCapsuleDirectory(
      reference.capsuleId,
    );
    const activePath = join(capsuleDirectory, "active.json");
    await assertReplaceableRegularFile(activePath);
    const temporary = join(
      capsuleDirectory,
      `.active-${randomUUID()}.tmp`,
    );
    const pointer: ActiveCheckpointPointer = {
      format: ACTIVE_CHECKPOINT_FORMAT,
      capsuleId: reference.capsuleId,
      revision: reference.revision,
      digest: reference.digest,
    };
    try {
      await writePrivateFile(temporary, utf8(`${stableJson(pointer)}\n`));
      await rename(temporary, activePath);
      await syncDirectory(capsuleDirectory);
    } finally {
      await rm(temporary, { force: true }).catch(() => undefined);
    }
    return checkpoint;
  }

  async readActive(capsuleId: string): Promise<CapsuleCheckpoint> {
    await this.#initialization;
    validateIdentifier(capsuleId, "capsuleId");
    const activePath = this.#scoped("capsules", capsuleId, "active.json");
    let bytes: Uint8Array;
    try {
      await this.#assertCapsuleDirectory(capsuleId, false);
      bytes = await readRegularFile(
        activePath,
        this.limits.maxManifestBytes,
        "Active checkpoint pointer",
      );
    } catch (error) {
      if (isNotFound(error)) {
        throw new CapsuleCheckpointNotFoundError(
          `Capsule ${JSON.stringify(capsuleId)} has no active checkpoint`,
          { cause: error },
        );
      }
      throw error;
    }
    const pointer = parseActivePointer(bytes, capsuleId);
    return this.read(pointer);
  }

  async #initialize(): Promise<void> {
    const rootCreated = await ensurePrivateDirectory(this.root);
    if (!rootCreated) await assertRegularDirectory(this.root, "checkpoint root");
    await ensurePrivateDirectory(this.#scoped("capsules"));
    await ensurePrivateDirectory(this.#scoped(".staging"));
    await syncDirectory(this.root);
  }

  async #ensureCapsuleDirectory(capsuleId: string): Promise<string> {
    validateIdentifier(capsuleId, "capsuleId");
    await assertRegularDirectory(this.root, "checkpoint root");
    await assertRegularDirectory(
      this.#scoped("capsules"),
      "capsule namespace",
    );
    const capsuleDirectory = this.#scoped("capsules", capsuleId);
    await ensurePrivateDirectory(capsuleDirectory);
    await ensurePrivateDirectory(join(capsuleDirectory, "checkpoints"));
    await syncDirectory(capsuleDirectory);
    await syncDirectory(this.#scoped("capsules"));
    return capsuleDirectory;
  }

  async #assertCapsuleDirectory(
    capsuleId: string,
    includeCheckpoints: boolean,
  ): Promise<void> {
    validateIdentifier(capsuleId, "capsuleId");
    const capsuleDirectory = this.#scoped("capsules", capsuleId);
    await assertRegularDirectory(this.root, "checkpoint root");
    await assertRegularDirectory(
      this.#scoped("capsules"),
      "capsule namespace",
    );
    await assertRegularDirectory(capsuleDirectory, "capsule directory");
    if (includeCheckpoints) {
      await assertRegularDirectory(
        join(capsuleDirectory, "checkpoints"),
        "capsule checkpoints directory",
      );
    }
  }

  #scoped(...segments: readonly string[]): string {
    const candidate = resolve(this.root, ...segments);
    const fromRoot = relative(this.root, candidate);
    if (
      fromRoot === "" && segments.length > 0 ||
      fromRoot === ".." ||
      fromRoot.startsWith(`..${sep}`) ||
      isAbsolute(fromRoot)
    ) {
      throw new UnsafeCapsuleFilesystemError(
        `Filesystem path escaped capsule root ${this.root}`,
      );
    }
    return candidate;
  }
}

function referenceForManifest(
  manifest: CapsuleManifestV1,
): CapsuleCheckpointReference {
  return {
    capsuleId: manifest.capsuleId,
    revision: manifest.revision,
    digest: capsuleManifestDigest(manifest),
  };
}

function checkpointDirectoryName(
  reference: CapsuleCheckpointReference,
): string {
  return `${reference.revision}-${reference.digest.slice("sha256:".length)}`;
}

function formatReference(reference: CapsuleCheckpointReference): string {
  return `${reference.capsuleId}@${reference.revision}/${reference.digest}`;
}

function validateCheckpointReference(
  reference: CapsuleCheckpointReference,
): void {
  validateIdentifier(reference.capsuleId, "capsuleId");
  if (!Number.isSafeInteger(reference.revision) || reference.revision < 0) {
    throw new UnsafeCapsuleFilesystemError(
      "Checkpoint revision must be a non-negative integer",
    );
  }
  if (!isSha256Digest(reference.digest)) {
    throw new UnsafeCapsuleFilesystemError("Checkpoint digest is invalid");
  }
}

function validateIdentifier(value: string, label: string): void {
  if (!/^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/u.test(value)) {
    throw new UnsafeCapsuleFilesystemError(`${label} is invalid`);
  }
}

function validateLimits(
  limits: CapsuleCheckpointFilesystemLimits,
): CapsuleCheckpointFilesystemLimits {
  for (const [key, value] of Object.entries(limits)) {
    if (!Number.isSafeInteger(value) || value <= 0) {
      throw new CapsuleCheckpointLimitError(
        `${key} must be a positive safe integer`,
      );
    }
  }
  return Object.freeze({ ...limits });
}

function normalizeCheckpoint(checkpoint: CapsuleCheckpoint): CapsuleCheckpoint {
  const manifest = parseCapsuleManifest(checkpoint.manifest);
  const members = checkpoint.members.map(
    (member): CapsuleMemberContent => ({
      path: portableCapsulePath(member.path),
      bytes: member.bytes.slice(),
    }),
  );
  verifyCapsuleMembers(manifest, members);
  return { manifest, members };
}

function assertCheckpointWithinLimits(
  checkpoint: CapsuleCheckpoint,
  limits: CapsuleCheckpointFilesystemLimits,
): void {
  const manifestBytes = utf8(`${stableJson(checkpoint.manifest)}\n`);
  if (manifestBytes.byteLength > limits.maxManifestBytes) {
    throw new CapsuleCheckpointLimitError(
      `Capsule manifest exceeds ${limits.maxManifestBytes} bytes`,
    );
  }
  assertDescriptorsWithinLimits(checkpoint.manifest, limits);
}

function assertDescriptorsWithinLimits(
  manifest: CapsuleManifestV1,
  limits: CapsuleCheckpointFilesystemLimits,
): void {
  if (manifest.members.length > limits.maxMembers) {
    throw new CapsuleCheckpointLimitError(
      `Capsule contains ${manifest.members.length} members; limit is ${limits.maxMembers}`,
    );
  }
  let total = 0;
  for (const member of manifest.members) {
    if (member.size > limits.maxMemberBytes) {
      throw new CapsuleCheckpointLimitError(
        `Capsule member ${member.path} exceeds ${limits.maxMemberBytes} bytes`,
      );
    }
    total += member.size;
    if (!Number.isSafeInteger(total) || total > limits.maxTotalBytes) {
      throw new CapsuleCheckpointLimitError(
        `Capsule members exceed ${limits.maxTotalBytes} total bytes`,
      );
    }
  }
}

async function writeCheckpointDirectory(
  directory: string,
  checkpoint: CapsuleCheckpoint,
): Promise<void> {
  await assertRegularDirectory(directory, "checkpoint staging directory");
  const membersRoot = join(directory, "members");
  await ensurePrivateDirectory(membersRoot);
  const memberMap = new Map(
    checkpoint.members.map((member) => [member.path, member.bytes] as const),
  );
  const createdDirectories = new Set<string>([directory, membersRoot]);
  for (const descriptor of checkpoint.manifest.members) {
    const bytes = memberMap.get(descriptor.path);
    if (bytes === undefined) {
      throw new UnsafeCapsuleFilesystemError(
        `Verified checkpoint lost member ${descriptor.path}`,
      );
    }
    const segments = descriptor.path.split("/");
    const filename = segments.pop();
    if (filename === undefined) {
      throw new UnsafeCapsuleFilesystemError("Capsule member filename is missing");
    }
    let parent = membersRoot;
    for (const segment of segments) {
      parent = join(parent, segment);
      await ensurePrivateDirectory(parent);
      createdDirectories.add(parent);
    }
    await writePrivateFile(join(parent, filename), bytes);
  }
  await writePrivateFile(
    join(directory, "capsule.json"),
    utf8(`${stableJson(checkpoint.manifest)}\n`),
  );
  for (const createdDirectory of [...createdDirectories].sort(
    (left, right) => right.length - left.length,
  )) {
    await syncDirectory(createdDirectory);
  }
}

async function readCheckpointDirectory(
  directory: string,
  reference: CapsuleCheckpointReference,
  limits: CapsuleCheckpointFilesystemLimits,
): Promise<CapsuleCheckpoint> {
  await assertRegularDirectory(directory, "checkpoint directory");
  await assertCheckpointDirectoryShape(directory);
  const manifestBytes = await readRegularFile(
    join(directory, "capsule.json"),
    limits.maxManifestBytes,
    "Capsule manifest",
  );
  let value: unknown;
  try {
    value = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(manifestBytes));
  } catch (error) {
    throw new UnsafeCapsuleFilesystemError(
      "Capsule manifest is not valid UTF-8 JSON",
      { cause: error },
    );
  }
  const manifest = parseCapsuleManifest(value);
  const canonicalManifest = utf8(`${stableJson(manifest)}\n`);
  if (!bytesEqual(manifestBytes, canonicalManifest)) {
    throw new UnsafeCapsuleFilesystemError(
      "Capsule manifest is not in its canonical on-disk form",
    );
  }
  const actualReference = referenceForManifest(manifest);
  if (
    actualReference.capsuleId !== reference.capsuleId ||
    actualReference.revision !== reference.revision ||
    actualReference.digest !== reference.digest
  ) {
    throw new UnsafeCapsuleFilesystemError(
      `Checkpoint directory does not match ${formatReference(reference)}`,
    );
  }
  assertDescriptorsWithinLimits(manifest, limits);

  const files = await collectMemberFiles(
    join(directory, "members"),
    manifest,
    limits,
  );
  const members: CapsuleMemberContent[] = [];
  for (const descriptor of manifest.members) {
    const file = files.get(descriptor.path);
    if (file === undefined) {
      throw new UnsafeCapsuleFilesystemError(
        `Checkpoint is missing member ${descriptor.path}`,
      );
    }
    const bytes = await readRegularFile(
      file,
      Math.min(limits.maxMemberBytes, descriptor.size),
      `Capsule member ${descriptor.path}`,
    );
    members.push({ path: descriptor.path, bytes });
    files.delete(descriptor.path);
  }
  const unexpected = [...files.keys()].sort()[0];
  if (unexpected !== undefined) {
    throw new UnsafeCapsuleFilesystemError(
      `Checkpoint contains unexpected member ${unexpected}`,
    );
  }
  verifyCapsuleMembers(manifest, members);
  return { manifest, members };
}

async function collectMemberFiles(
  membersRoot: string,
  manifest: CapsuleManifestV1,
  limits: CapsuleCheckpointFilesystemLimits,
): Promise<Map<string, string>> {
  await assertRegularDirectory(membersRoot, "checkpoint members directory");
  const files = new Map<string, string>();
  let totalSize = 0;
  const expectedDirectories = new Set<string>();
  for (const member of manifest.members) {
    const segments = member.path.split("/");
    segments.pop();
    while (segments.length > 0) {
      expectedDirectories.add(segments.join("/"));
      segments.pop();
    }
  }

  const walk = async (directory: string, prefix: string): Promise<void> => {
    const entries = await readdir(directory, { withFileTypes: true });
    for (const entry of entries) {
      const relativePath = prefix === "" ? entry.name : `${prefix}/${entry.name}`;
      const absolutePath = join(directory, entry.name);
      const stats = await lstat(absolutePath);
      if (stats.isSymbolicLink()) {
        throw new UnsafeCapsuleFilesystemError(
          `Checkpoint entry ${relativePath} is a symlink`,
        );
      }
      if (stats.isDirectory()) {
        if (!expectedDirectories.has(relativePath)) {
          throw new UnsafeCapsuleFilesystemError(
            `Checkpoint contains unexpected member directory ${relativePath}`,
          );
        }
        await walk(absolutePath, relativePath);
        continue;
      }
      if (!stats.isFile() || stats.nlink !== 1) {
        throw new UnsafeCapsuleFilesystemError(
          `Checkpoint entry ${relativePath} is not a private regular file`,
        );
      }
      const portablePath = portableCapsulePath(relativePath);
      if (files.has(portablePath)) {
        throw new UnsafeCapsuleFilesystemError(
          `Checkpoint contains duplicate member ${portablePath}`,
        );
      }
      if (stats.size > limits.maxMemberBytes) {
        throw new CapsuleCheckpointLimitError(
          `Capsule member ${portablePath} exceeds ${limits.maxMemberBytes} bytes`,
        );
      }
      totalSize += stats.size;
      if (!Number.isSafeInteger(totalSize) || totalSize > limits.maxTotalBytes) {
        throw new CapsuleCheckpointLimitError(
          `Capsule members exceed ${limits.maxTotalBytes} total bytes`,
        );
      }
      files.set(portablePath, absolutePath);
      if (files.size > limits.maxMembers) {
        throw new CapsuleCheckpointLimitError(
          `Capsule contains more than ${limits.maxMembers} members`,
        );
      }
    }
  };

  await walk(membersRoot, "");
  return files;
}

async function assertCheckpointDirectoryShape(directory: string): Promise<void> {
  const names = (await readdir(directory)).sort();
  if (
    names.length !== 2 ||
    names[0] !== "capsule.json" ||
    names[1] !== "members"
  ) {
    const unexpected = names.find(
      (name) => name !== "capsule.json" && name !== "members",
    );
    throw new UnsafeCapsuleFilesystemError(
      unexpected === undefined
        ? "Checkpoint directory is incomplete"
        : `Checkpoint directory contains unexpected entry ${unexpected}`,
    );
  }
}

async function ensurePrivateDirectory(path: string): Promise<boolean> {
  let created = false;
  try {
    await mkdir(path, { mode: 0o700 });
    created = true;
    await chmod(path, 0o700);
  } catch (error) {
    if (!isAlreadyExists(error)) throw error;
  }
  await assertRegularDirectory(path, "capsule directory");
  return created;
}

async function assertRegularDirectory(path: string, label: string): Promise<void> {
  const stats = await lstat(path);
  if (stats.isSymbolicLink() || !stats.isDirectory()) {
    throw new UnsafeCapsuleFilesystemError(
      `${label} is not a no-follow regular directory: ${path}`,
    );
  }
}

async function assertReplaceableRegularFile(path: string): Promise<void> {
  try {
    const stats = await lstat(path);
    if (stats.isSymbolicLink() || !stats.isFile()) {
      throw new UnsafeCapsuleFilesystemError(
        `Active checkpoint pointer is not a no-follow regular file: ${path}`,
      );
    }
  } catch (error) {
    if (!isNotFound(error)) throw error;
  }
}

async function writePrivateFile(path: string, bytes: Uint8Array): Promise<void> {
  const handle = await open(
    path,
    constants.O_WRONLY |
      constants.O_CREAT |
      constants.O_EXCL |
      noFollowFlag(),
    0o600,
  );
  try {
    await handle.writeFile(bytes);
    await handle.chmod(0o600);
    await handle.sync();
  } finally {
    await handle.close();
  }
}

async function readRegularFile(
  path: string,
  maxBytes: number,
  label: string,
): Promise<Uint8Array> {
  const before = await lstat(path);
  if (before.isSymbolicLink() || !before.isFile() || before.nlink !== 1) {
    throw new UnsafeCapsuleFilesystemError(
      `${label} is not a private no-follow regular file`,
    );
  }
  if (before.size > maxBytes) {
    throw new CapsuleCheckpointLimitError(
      `${label} exceeds ${maxBytes} bytes`,
    );
  }
  const handle = await open(path, constants.O_RDONLY | noFollowFlag());
  try {
    const opened = await handle.stat();
    if (!opened.isFile() || opened.nlink !== 1) {
      throw new UnsafeCapsuleFilesystemError(
        `${label} changed while it was being opened`,
      );
    }
    if (opened.size !== before.size || opened.size > maxBytes) {
      throw new UnsafeCapsuleFilesystemError(
        `${label} changed while it was being verified`,
      );
    }
    const bytes = await handle.readFile();
    if (bytes.byteLength !== opened.size) {
      throw new UnsafeCapsuleFilesystemError(
        `${label} changed while it was being read`,
      );
    }
    return bytes;
  } finally {
    await handle.close();
  }
}

async function syncDirectory(path: string): Promise<void> {
  let handle;
  try {
    handle = await open(path, constants.O_RDONLY | noFollowFlag());
    await handle.sync();
  } catch (error) {
    if (!isUnsupportedDirectorySync(error)) throw error;
  } finally {
    await handle?.close();
  }
}

function parseActivePointer(
  bytes: Uint8Array,
  expectedCapsuleId: string,
): ActiveCheckpointPointer {
  let value: unknown;
  try {
    value = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes));
  } catch (error) {
    throw new UnsafeCapsuleFilesystemError(
      "Active checkpoint pointer is not valid UTF-8 JSON",
      { cause: error },
    );
  }
  if (
    !isRecord(value) ||
    value.format !== ACTIVE_CHECKPOINT_FORMAT ||
    typeof value.capsuleId !== "string" ||
    typeof value.revision !== "number" ||
    typeof value.digest !== "string"
  ) {
    throw new UnsafeCapsuleFilesystemError(
      "Active checkpoint pointer has an unsupported format",
    );
  }
  const pointer: ActiveCheckpointPointer = {
    format: ACTIVE_CHECKPOINT_FORMAT,
    capsuleId: value.capsuleId,
    revision: value.revision,
    digest: value.digest as Sha256Digest,
  };
  validateCheckpointReference(pointer);
  if (pointer.capsuleId !== expectedCapsuleId) {
    throw new UnsafeCapsuleFilesystemError(
      "Active checkpoint pointer crosses capsule boundaries",
    );
  }
  if (!bytesEqual(bytes, utf8(`${stableJson(pointer)}\n`))) {
    throw new UnsafeCapsuleFilesystemError(
      "Active checkpoint pointer is not in canonical on-disk form",
    );
  }
  return pointer;
}

function bytesEqual(left: Uint8Array, right: Uint8Array): boolean {
  if (left.byteLength !== right.byteLength) return false;
  for (let index = 0; index < left.byteLength; index += 1) {
    if (left[index] !== right[index]) return false;
  }
  return true;
}

function noFollowFlag(): number {
  return typeof constants.O_NOFOLLOW === "number" ? constants.O_NOFOLLOW : 0;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isNotFound(error: unknown): boolean {
  return errorCode(error) === "ENOENT";
}

function isAlreadyExists(error: unknown): boolean {
  const code = errorCode(error);
  return code === "EEXIST" || code === "ENOTEMPTY";
}

function isUnsupportedDirectorySync(error: unknown): boolean {
  const code = errorCode(error);
  return code === "EINVAL" || code === "ENOTSUP" || code === "EISDIR";
}

function errorCode(error: unknown): string | undefined {
  return typeof error === "object" && error !== null && "code" in error &&
      typeof error.code === "string"
    ? error.code
    : undefined;
}

async function pathExists(path: string): Promise<boolean> {
  try {
    await lstat(path);
    return true;
  } catch (error) {
    if (isNotFound(error)) return false;
    throw error;
  }
}
