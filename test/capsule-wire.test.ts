import { describe, expect, test } from "bun:test";

import {
  createCapsuleCheckpoint,
  decodeCapsuleWire,
  encodeCapsuleWire,
  portableCapsulePath,
  sha256,
  utf8,
} from "../src/capsule/index.ts";

function checkpoint() {
  const staged = { path: portableCapsulePath("git/staged.patch"), bytes: utf8("staged") };
  const unstaged = { path: portableCapsulePath("git/unstaged.patch"), bytes: utf8("unstaged") };
  return createCapsuleCheckpoint({
    capsuleId: "capsule-1",
    taskId: "task-1",
    revision: 2,
    checkpointedAt: "2026-08-29T08:00:00.000Z",
    ownership: { epoch: 1, leaseId: "lease-1", ownerNodeId: "local", status: "active" },
    repository: {
      kind: "git",
      remoteUrl: "git@github.com:gitsquared/fixture.git",
      defaultBranch: "main",
      branch: "altitude/task-1",
      baseCommit: "1".repeat(40),
      headCommit: "1".repeat(40),
      bundleMember: null,
      stagedPatchMember: staged.path,
      unstagedPatchMember: unstaged.path,
      untracked: [],
    },
    members: [staged, unstaged],
  });
}

describe("capsule wire envelope", () => {
  test("round-trips a verified checkpoint without base64 expansion", () => {
    const original = checkpoint();
    const encoded = encodeCapsuleWire(original);
    const decoded = decodeCapsuleWire(encoded);
    expect(decoded.manifest).toEqual(original.manifest);
    expect(decoded.members.map((member) => sha256(member.bytes))).toEqual(
      original.members.map((member) => sha256(member.bytes)),
    );
  });

  test("rejects truncated, corrupted, and trailing checkpoint bytes", () => {
    const encoded = encodeCapsuleWire(checkpoint());
    expect(() => decodeCapsuleWire(encoded.slice(0, -1))).toThrow("truncated");
    const corrupted = encoded.slice();
    corrupted[corrupted.length - 1] = corrupted[corrupted.length - 1]! ^ 0xff;
    expect(() => decodeCapsuleWire(corrupted)).toThrow("Checksum mismatch");
    const trailing = new Uint8Array(encoded.byteLength + 1);
    trailing.set(encoded);
    expect(() => decodeCapsuleWire(trailing)).toThrow("trailing bytes");
  });
});
