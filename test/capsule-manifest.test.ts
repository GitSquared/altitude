import { describe, expect, test } from "bun:test";

import {
  CAPSULE_FORMAT,
  CapsuleMemberVerificationError,
  InvalidCapsuleArchiveError,
  InvalidCapsuleManifestError,
  UnsafeCapsulePathError,
  capsuleManifestDigest,
  createCapsuleCheckpoint,
  directoryCheckpointCodec,
  portableCapsulePath,
  safeRepositoryPath,
  sha256,
  untrackedMemberPath,
  utf8,
  verifyCapsuleMembers,
  type CapsuleCheckpoint,
  type CapsuleMemberContent,
  type GitRepositorySnapshot,
} from "../src/capsule/index.ts";

const BASE = "1".repeat(40);
const HEAD = "2".repeat(40);

function member(path: string, value: string): CapsuleMemberContent {
  return { path: portableCapsulePath(path), bytes: utf8(value) };
}

function repository(
  untracked: readonly string[] = [],
): GitRepositorySnapshot {
  return {
    kind: "git",
    remoteUrl: "git@github.com:gitsquared/example.git",
    defaultBranch: "main",
    branch: "altitude/task-1",
    baseCommit: BASE,
    headCommit: HEAD,
    bundleMember: portableCapsulePath("git/repository.bundle"),
    stagedPatchMember: portableCapsulePath("git/staged.patch"),
    unstagedPatchMember: portableCapsulePath("git/unstaged.patch"),
    untracked: untracked.map((path) => {
      const repositoryPath = safeRepositoryPath(path);
      return { repositoryPath, memberPath: untrackedMemberPath(repositoryPath) };
    }),
  };
}

function checkpoint(): CapsuleCheckpoint {
  return createCapsuleCheckpoint({
    capsuleId: "capsule-1",
    taskId: "task-1",
    revision: 7,
    checkpointedAt: "2026-08-28T08:30:00.000Z",
    ownership: {
      epoch: 4,
      ownerNodeId: "macbook",
      leaseId: "lease-4",
      status: "active",
    },
    repository: repository(["notes/a.txt"]),
    members: [
      member("git/unstaged.patch", "unstaged"),
      member("git/untracked/notes/a.txt", "hello"),
      member("git/repository.bundle", "bundle"),
      member("git/staged.patch", "staged"),
      member("state/task.sqlite", "sqlite bytes"),
    ],
  });
}

describe("altitude.capsule/v1 manifest", () => {
  test("describes members with deterministic order, size, and checksum", () => {
    const result = checkpoint();
    expect(result.manifest.format).toBe(CAPSULE_FORMAT);
    expect(result.manifest.members.map(({ path }) => String(path))).toEqual([
      "git/repository.bundle",
      "git/staged.patch",
      "git/unstaged.patch",
      "git/untracked/notes/a.txt",
      "state/task.sqlite",
    ]);
    expect(result.manifest.members[1] as unknown).toEqual({
      path: "git/staged.patch",
      size: 6,
      sha256: sha256(utf8("staged")),
    });

    const reversed = createCapsuleCheckpoint({
      capsuleId: result.manifest.capsuleId,
      taskId: result.manifest.taskId,
      revision: result.manifest.revision,
      checkpointedAt: result.manifest.checkpointedAt,
      ownership: result.manifest.ownership,
      repository: result.manifest.repository,
      members: [...result.members].reverse(),
    });
    expect(capsuleManifestDigest(reversed.manifest)).toBe(
      capsuleManifestDigest(result.manifest),
    );
  });

  test("keeps node-local identities and credentials outside portable paths", () => {
    const unsafe = [
      "identity/claude",
      "cache/repository",
      "runtime/container-id",
      "session/.claude/.credentials.json",
      "workspace/.ssh/id_ed25519",
      "git/untracked/.config/gh/hosts.yml",
      "../state/task.sqlite",
      "/state/task.sqlite",
      "state\\task.sqlite",
      "git/../state/task.sqlite",
    ];
    for (const path of unsafe) {
      expect(() => portableCapsulePath(path)).toThrow(UnsafeCapsulePathError);
    }
  });

  test("rejects manifests that reference a member they do not carry", () => {
    expect(() =>
      createCapsuleCheckpoint({
        capsuleId: "capsule-1",
        taskId: "task-1",
        revision: 1,
        checkpointedAt: "2026-08-28T08:30:00.000Z",
        ownership: {
          epoch: 1,
          ownerNodeId: "macbook",
          leaseId: "lease-1",
          status: "active",
        },
        repository: repository(),
        members: [
          member("git/staged.patch", ""),
          member("git/unstaged.patch", ""),
        ],
      })
    ).toThrow(InvalidCapsuleManifestError);
  });

  test("rejects remote URLs with embedded credentials", () => {
    const valid = checkpoint();
    expect(() =>
      createCapsuleCheckpoint({
        capsuleId: valid.manifest.capsuleId,
        taskId: valid.manifest.taskId,
        revision: valid.manifest.revision,
        checkpointedAt: valid.manifest.checkpointedAt,
        ownership: valid.manifest.ownership,
        repository: {
          ...valid.manifest.repository,
          remoteUrl: "https://token@example.test/org/repo.git",
        },
        members: valid.members,
      })
    ).toThrow("cannot contain portable credentials");
  });
});

describe("directory checkpoint archive seam", () => {
  test("round-trips a deterministic directory representation", () => {
    const original = checkpoint();
    const encoded = directoryCheckpointCodec.encode(original);
    expect(encoded.map(({ path }) => path)).toEqual([
      "capsule.json",
      "members/git/repository.bundle",
      "members/git/staged.patch",
      "members/git/unstaged.patch",
      "members/git/untracked/notes/a.txt",
      "members/state/task.sqlite",
    ]);
    const restored = directoryCheckpointCodec.decode(encoded);
    expect(restored.manifest).toEqual(original.manifest);
    expect(restored.members.map(({ path }) => path)).toEqual(
      original.members.map(({ path }) => path),
    );
  });

  test("fails verification when a member is missing", () => {
    const original = checkpoint();
    const incomplete = original.members.filter(
      ({ path }) => path !== "state/task.sqlite",
    );
    expect(() => verifyCapsuleMembers(original.manifest, incomplete)).toThrow(
      new CapsuleMemberVerificationError("Missing member state/task.sqlite"),
    );
  });

  test("fails verification when bytes do not match their checksum", () => {
    const original = checkpoint();
    const corrupted = original.members.map((entry) =>
      entry.path === "state/task.sqlite"
        ? member(entry.path, "tampered!!!!")
        : entry
    );
    expect(() => verifyCapsuleMembers(original.manifest, corrupted)).toThrow(
      "Checksum mismatch for state/task.sqlite",
    );
  });

  test("rejects unexpected and unsafe archive entries", () => {
    const encoded = directoryCheckpointCodec.encode(checkpoint());
    expect(() =>
      directoryCheckpointCodec.decode([
        ...encoded,
        { path: "members/../identity/claude", bytes: utf8("secret") },
      ])
    ).toThrow(UnsafeCapsulePathError);
    expect(() =>
      directoryCheckpointCodec.decode([
        ...encoded,
        { path: "other.txt", bytes: utf8("surprise") },
      ])
    ).toThrow(InvalidCapsuleArchiveError);
  });
});
