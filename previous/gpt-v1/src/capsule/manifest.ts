import {
  compareCanonicalStrings,
  isSha256Digest,
  sha256,
  stableJson,
  type Sha256Digest,
  utf8,
} from "./canonical.ts";
import {
  portableCapsulePath,
  safeRepositoryPath,
  type PortableCapsulePath,
  type SafeRepositoryPath,
} from "./paths.ts";

export const CAPSULE_FORMAT = "altitude.capsule/v1" as const;

export interface ActiveOwnershipLease {
  readonly epoch: number;
  readonly leaseId: string;
  readonly ownerNodeId: string;
  readonly status: "active";
}

export interface CapsuleMemberDescriptor {
  readonly path: PortableCapsulePath;
  readonly size: number;
  readonly sha256: Sha256Digest;
}

export interface UntrackedMember {
  readonly repositoryPath: SafeRepositoryPath;
  readonly memberPath: PortableCapsulePath;
}

export interface GitRepositorySnapshot {
  readonly kind: "git";
  readonly remoteUrl: string;
  readonly defaultBranch: string;
  readonly branch: string;
  readonly baseCommit: string;
  readonly headCommit: string;
  readonly bundleMember: PortableCapsulePath | null;
  readonly stagedPatchMember: PortableCapsulePath;
  readonly unstagedPatchMember: PortableCapsulePath;
  readonly untracked: readonly UntrackedMember[];
}

export interface CapsuleManifestV1 {
  readonly format: typeof CAPSULE_FORMAT;
  readonly capsuleId: string;
  readonly taskId: string;
  readonly revision: number;
  readonly checkpointedAt: string;
  readonly ownership: ActiveOwnershipLease;
  readonly repository: GitRepositorySnapshot;
  readonly members: readonly CapsuleMemberDescriptor[];
}

export interface CapsuleMemberContent {
  readonly path: PortableCapsulePath;
  readonly bytes: Uint8Array;
}

export interface CapsuleCheckpoint {
  readonly manifest: CapsuleManifestV1;
  readonly members: readonly CapsuleMemberContent[];
}

export interface CreateCapsuleCheckpointInput {
  readonly capsuleId: string;
  readonly taskId: string;
  readonly revision: number;
  readonly checkpointedAt: string;
  readonly ownership: ActiveOwnershipLease;
  readonly repository: GitRepositorySnapshot;
  readonly members: readonly CapsuleMemberContent[];
}

export class InvalidCapsuleManifestError extends Error {
  override readonly name = "InvalidCapsuleManifestError";
}

export class CapsuleMemberVerificationError extends Error {
  override readonly name = "CapsuleMemberVerificationError";
}

function assertIdentifier(value: unknown, label: string): asserts value is string {
  if (
    typeof value !== "string" ||
    !/^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/u.test(value)
  ) {
    throw new InvalidCapsuleManifestError(`${label} is not a valid stable identifier`);
  }
}

function assertEpoch(value: unknown, label: string): asserts value is number {
  if (!Number.isSafeInteger(value) || (value as number) < 0) {
    throw new InvalidCapsuleManifestError(`${label} must be a non-negative integer`);
  }
}

function assertGitCommit(value: unknown, label: string): asserts value is string {
  if (typeof value !== "string" || !/^[a-f0-9]{40}(?:[a-f0-9]{24})?$/u.test(value)) {
    throw new InvalidCapsuleManifestError(`${label} must be a full Git object ID`);
  }
}

function assertCredentialFreeRemoteUrl(value: unknown): asserts value is string {
  if (typeof value !== "string" || value.length === 0 || /[\u0000-\u001f\u007f]/u.test(value)) {
    throw new InvalidCapsuleManifestError("repository.remoteUrl is invalid");
  }
  if (/^https?:\/\//iu.test(value)) {
    let url: URL;
    try {
      url = new URL(value);
    } catch (error) {
      throw new InvalidCapsuleManifestError("repository.remoteUrl is invalid", {
        cause: error,
      });
    }
    if (url.username !== "" || url.password !== "") {
      throw new InvalidCapsuleManifestError(
        "repository.remoteUrl cannot contain portable credentials",
      );
    }
  }
}

function copyMember(member: CapsuleMemberContent): CapsuleMemberContent {
  return {
    path: portableCapsulePath(member.path),
    bytes: member.bytes.slice(),
  };
}

export function describeCapsuleMember(
  member: CapsuleMemberContent,
): CapsuleMemberDescriptor {
  return {
    path: portableCapsulePath(member.path),
    size: member.bytes.byteLength,
    sha256: sha256(member.bytes),
  };
}

export function createCapsuleCheckpoint(
  input: CreateCapsuleCheckpointInput,
): CapsuleCheckpoint {
  const members = input.members
    .map(copyMember)
    .sort((left, right) => compareCanonicalStrings(left.path, right.path));
  const paths = new Set<string>();
  for (const member of members) {
    if (paths.has(member.path)) {
      throw new InvalidCapsuleManifestError(`Duplicate capsule member ${member.path}`);
    }
    paths.add(member.path);
  }

  const manifest: CapsuleManifestV1 = {
    format: CAPSULE_FORMAT,
    capsuleId: input.capsuleId,
    taskId: input.taskId,
    revision: input.revision,
    checkpointedAt: input.checkpointedAt,
    ownership: input.ownership,
    repository: input.repository,
    members: members.map(describeCapsuleMember),
  };
  const parsed = parseCapsuleManifest(manifest);
  assertRepositoryMembersExist(parsed);
  return { manifest: parsed, members };
}

export function capsuleManifestDigest(manifest: CapsuleManifestV1): Sha256Digest {
  return sha256(utf8(stableJson(manifest)));
}

export function parseCapsuleManifest(value: unknown): CapsuleManifestV1 {
  if (!isRecord(value) || value.format !== CAPSULE_FORMAT) {
    throw new InvalidCapsuleManifestError(`Expected ${CAPSULE_FORMAT}`);
  }
  assertIdentifier(value.capsuleId, "capsuleId");
  assertIdentifier(value.taskId, "taskId");
  assertEpoch(value.revision, "revision");
  if (typeof value.checkpointedAt !== "string" || !isIsoInstant(value.checkpointedAt)) {
    throw new InvalidCapsuleManifestError("checkpointedAt must be an ISO-8601 instant");
  }

  const ownership = parseOwnership(value.ownership);
  const repository = parseRepository(value.repository);
  if (!Array.isArray(value.members)) {
    throw new InvalidCapsuleManifestError("members must be an array");
  }
  const members = value.members.map(parseMemberDescriptor);
  let previous: string | undefined;
  for (const member of members) {
    if (previous !== undefined && compareCanonicalStrings(previous, member.path) >= 0) {
      throw new InvalidCapsuleManifestError(
        "members must be uniquely sorted by portable path",
      );
    }
    previous = member.path;
  }

  const manifest: CapsuleManifestV1 = {
    format: CAPSULE_FORMAT,
    capsuleId: value.capsuleId,
    taskId: value.taskId,
    revision: value.revision,
    checkpointedAt: value.checkpointedAt,
    ownership,
    repository,
    members,
  };
  assertRepositoryMembersExist(manifest);
  return manifest;
}

function parseOwnership(value: unknown): ActiveOwnershipLease {
  if (!isRecord(value) || value.status !== "active") {
    throw new InvalidCapsuleManifestError("ownership must contain an active lease");
  }
  assertEpoch(value.epoch, "ownership.epoch");
  assertIdentifier(value.leaseId, "ownership.leaseId");
  assertIdentifier(value.ownerNodeId, "ownership.ownerNodeId");
  return {
    epoch: value.epoch,
    leaseId: value.leaseId,
    ownerNodeId: value.ownerNodeId,
    status: "active",
  };
}

function parseRepository(value: unknown): GitRepositorySnapshot {
  if (!isRecord(value) || value.kind !== "git") {
    throw new InvalidCapsuleManifestError("repository must be a Git snapshot");
  }
  assertCredentialFreeRemoteUrl(value.remoteUrl);
  assertGitRefName(value.defaultBranch, "repository.defaultBranch");
  assertGitRefName(value.branch, "repository.branch");
  assertGitCommit(value.baseCommit, "repository.baseCommit");
  assertGitCommit(value.headCommit, "repository.headCommit");
  const bundleMember = value.bundleMember === null
    ? null
    : portableCapsulePath(asString(value.bundleMember, "repository.bundleMember"));
  const stagedPatchMember = portableCapsulePath(
    asString(value.stagedPatchMember, "repository.stagedPatchMember"),
  );
  const unstagedPatchMember = portableCapsulePath(
    asString(value.unstagedPatchMember, "repository.unstagedPatchMember"),
  );
  if (!Array.isArray(value.untracked)) {
    throw new InvalidCapsuleManifestError("repository.untracked must be an array");
  }
  const untracked = value.untracked.map((entry): UntrackedMember => {
    if (!isRecord(entry)) {
      throw new InvalidCapsuleManifestError("Invalid untracked member");
    }
    const repositoryPath = safeRepositoryPath(
      asString(entry.repositoryPath, "untracked.repositoryPath"),
    );
    const memberPath = portableCapsulePath(
      asString(entry.memberPath, "untracked.memberPath"),
    );
    const expectedMemberPath = portableCapsulePath(`git/untracked/${repositoryPath}`);
    if (memberPath !== expectedMemberPath) {
      throw new InvalidCapsuleManifestError(
        `Untracked path ${repositoryPath} must map to ${expectedMemberPath}`,
      );
    }
    return { repositoryPath, memberPath };
  });
  const sorted = [...untracked].sort((left, right) =>
    compareCanonicalStrings(left.repositoryPath, right.repositoryPath)
  );
  if (untracked.some((entry, index) => entry !== sorted[index])) {
    throw new InvalidCapsuleManifestError("repository.untracked must be sorted");
  }
  const seen = new Set<string>();
  for (const entry of untracked) {
    if (seen.has(entry.repositoryPath)) {
      throw new InvalidCapsuleManifestError(
        `Duplicate untracked path ${entry.repositoryPath}`,
      );
    }
    seen.add(entry.repositoryPath);
  }
  return {
    kind: "git",
    remoteUrl: value.remoteUrl,
    defaultBranch: value.defaultBranch,
    branch: value.branch,
    baseCommit: value.baseCommit,
    headCommit: value.headCommit,
    bundleMember,
    stagedPatchMember,
    unstagedPatchMember,
    untracked,
  };
}

function parseMemberDescriptor(value: unknown): CapsuleMemberDescriptor {
  if (!isRecord(value)) {
    throw new InvalidCapsuleManifestError("Invalid capsule member descriptor");
  }
  if (!Number.isSafeInteger(value.size) || (value.size as number) < 0) {
    throw new InvalidCapsuleManifestError("Member size must be a non-negative integer");
  }
  if (!isSha256Digest(value.sha256)) {
    throw new InvalidCapsuleManifestError("Member sha256 is invalid");
  }
  return {
    path: portableCapsulePath(asString(value.path, "member.path")),
    size: value.size as number,
    sha256: value.sha256,
  };
}

function assertRepositoryMembersExist(manifest: CapsuleManifestV1): void {
  const paths = new Set(manifest.members.map((member) => member.path));
  const referenced = [
    manifest.repository.stagedPatchMember,
    manifest.repository.unstagedPatchMember,
    ...manifest.repository.untracked.map((entry) => entry.memberPath),
  ];
  if (manifest.repository.bundleMember !== null) {
    referenced.push(manifest.repository.bundleMember);
  }
  for (const path of referenced) {
    if (!paths.has(path)) {
      throw new InvalidCapsuleManifestError(
        `Repository snapshot references missing member ${path}`,
      );
    }
  }
}

function assertGitRefName(value: unknown, label: string): asserts value is string {
  if (
    typeof value !== "string" ||
    value.length === 0 ||
    value.startsWith("-") ||
    value.startsWith("/") ||
    value.endsWith("/") ||
    value.includes("..") ||
    value.includes("//") ||
    value.includes("@{") ||
    /[\u0000-\u0020~^:?*[\\\u007f]/u.test(value)
  ) {
    throw new InvalidCapsuleManifestError(`${label} is not a safe Git ref name`);
  }
}

function asString(value: unknown, label: string): string {
  if (typeof value !== "string") {
    throw new InvalidCapsuleManifestError(`${label} must be a string`);
  }
  return value;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isIsoInstant(value: string): boolean {
  const timestamp = Date.parse(value);
  return Number.isFinite(timestamp) && new Date(timestamp).toISOString() === value;
}

export function verifyCapsuleMembers(
  manifest: CapsuleManifestV1,
  members: readonly CapsuleMemberContent[],
): void {
  const actual = new Map<string, Uint8Array>();
  for (const member of members) {
    const path = portableCapsulePath(member.path);
    if (actual.has(path)) {
      throw new CapsuleMemberVerificationError(`Duplicate member ${path}`);
    }
    actual.set(path, member.bytes);
  }
  for (const expected of manifest.members) {
    const bytes = actual.get(expected.path);
    if (bytes === undefined) {
      throw new CapsuleMemberVerificationError(`Missing member ${expected.path}`);
    }
    if (bytes.byteLength !== expected.size) {
      throw new CapsuleMemberVerificationError(
        `Size mismatch for ${expected.path}: expected ${expected.size}, got ${bytes.byteLength}`,
      );
    }
    const actualDigest = sha256(bytes);
    if (actualDigest !== expected.sha256) {
      throw new CapsuleMemberVerificationError(
        `Checksum mismatch for ${expected.path}`,
      );
    }
    actual.delete(expected.path);
  }
  const extra = [...actual.keys()].sort()[0];
  if (extra !== undefined) {
    throw new CapsuleMemberVerificationError(`Unexpected member ${extra}`);
  }
}
