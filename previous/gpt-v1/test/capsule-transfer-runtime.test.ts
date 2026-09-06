import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  AmbiguousCapsuleActivationError,
  CapsuleCheckpointNotFoundError,
  FilesystemCapsuleCheckpointStore,
  FilesystemCapsuleTransferReceiver,
  FilesystemRepositorySeedCache,
  InProcessCapsuleTransferTransport,
  LocalToRemoteCapsuleTransfer,
  capsuleManifestDigest,
  createCapsuleCheckpoint,
  createOwnershipTransferCheckpoint,
  decodeCapsuleTransferPackage,
  encodeCapsuleTransferPackage,
  portableCapsulePath,
  utf8,
  type CapsuleCheckpoint,
  type CapsuleTransferTransport,
} from "../src/capsule/index.ts";

const roots: string[] = [];
const SEED = utf8("fixture repository seed bundle");

afterEach(() => {
  for (const root of roots.splice(0)) {
    rmSync(root, { recursive: true, force: true });
  }
});

function root(label: string): string {
  const value = mkdtempSync(join(tmpdir(), "altitude-" + label + "-"));
  roots.push(value);
  return value;
}

function checkpoint(): CapsuleCheckpoint {
  return createCapsuleCheckpoint({
    capsuleId: "task-1",
    taskId: "task-1",
    revision: 1,
    checkpointedAt: "2026-08-29T10:00:00.000Z",
    ownership: {
      epoch: 4,
      leaseId: "lease-macbook",
      ownerNodeId: "macbook",
      status: "active",
    },
    repository: {
      kind: "git",
      remoteUrl: "git@github.com:gitsquared/altitude.git",
      defaultBranch: "main",
      branch: "gaby/v1-foundation",
      baseCommit: "a".repeat(40),
      headCommit: "b".repeat(40),
      bundleMember: null,
      stagedPatchMember: portableCapsulePath("git/staged.patch"),
      unstagedPatchMember: portableCapsulePath("git/unstaged.patch"),
      untracked: [],
    },
    members: [
      { path: portableCapsulePath("git/staged.patch"), bytes: new Uint8Array() },
      { path: portableCapsulePath("git/unstaged.patch"), bytes: new Uint8Array() },
      {
        path: portableCapsulePath("state/task.json"),
        bytes: utf8('{"schema":"altitude.task-state/v1"}\n'),
      },
    ],
  });
}

async function sourceState(): Promise<{
  checkpoints: FilesystemCapsuleCheckpointStore;
  seeds: FilesystemRepositorySeedCache;
  initial: CapsuleCheckpoint;
}> {
  const sourceRoot = root("transfer-source");
  const checkpoints = new FilesystemCapsuleCheckpointStore(
    join(sourceRoot, "capsules"),
  );
  const seeds = new FilesystemRepositorySeedCache(
    join(sourceRoot, "seeds"),
  );
  const initial = checkpoint();
  const published = await checkpoints.publish(initial);
  await checkpoints.activate(published);
  await seeds.put(published.digest, SEED);
  return { checkpoints, seeds, initial };
}

describe("filesystem capsule transfer", () => {
  test("stages, commits, and activates one verified capsule across two node roots", async () => {
    const source = await sourceState();
    const targetRoot = root("transfer-target");
    const targetCheckpoints = new FilesystemCapsuleCheckpointStore(
      join(targetRoot, "capsules"),
    );
    const targetSeeds = new FilesystemRepositorySeedCache(
      join(targetRoot, "seeds"),
    );
    const receiver = new FilesystemCapsuleTransferReceiver(
      targetCheckpoints,
      targetSeeds,
      "straylight",
    );
    const coordinator = new LocalToRemoteCapsuleTransfer(
      source.checkpoints,
      source.seeds,
      "macbook",
      new InProcessCapsuleTransferTransport(receiver),
      { now: () => Date.parse("2026-08-29T10:05:00.000Z") },
    );

    const prepared = await coordinator.prepare({
      taskId: "task-1",
      transferId: "transfer-1",
      targetNodeId: "straylight",
      targetLeaseId: "lease-straylight",
    });
    expect((await source.checkpoints.readActive("task-1")).manifest.revision).toBe(1);
    expect(targetCheckpoints.readActive("task-1")).rejects.toBeInstanceOf(
      CapsuleCheckpointNotFoundError,
    );
    expect((await targetCheckpoints.read(prepared.targetCheckpoint)).manifest.ownership)
      .toMatchObject({
        ownerNodeId: "straylight",
        leaseId: "lease-straylight",
        epoch: 5,
      });

    const committed = await coordinator.commit(prepared);
    expect(committed.activation.checkpoint).toEqual(prepared.targetCheckpoint);
    for (const store of [source.checkpoints, targetCheckpoints]) {
      const active = await store.readActive("task-1");
      expect(capsuleManifestDigest(active.manifest)).toBe(
        prepared.targetCheckpoint.digest,
      );
      expect(active.manifest.ownership).toMatchObject({
        ownerNodeId: "straylight",
        status: "active",
      });
    }
    expect(await targetSeeds.read(prepared.targetCheckpoint.digest)).toEqual(SEED);

    const retried = await coordinator.commit(prepared);
    expect(retried.activation).toEqual(committed.activation);
  });

  test("verifies the package and refuses a package addressed to another node", async () => {
    const source = checkpoint();
    const transferred = createOwnershipTransferCheckpoint({
      source,
      transferId: "transfer-2",
      sourceNodeId: "macbook",
      sourceLeaseId: "lease-macbook",
      targetNodeId: "straylight",
      targetLeaseId: "lease-straylight",
      preparedAt: "2026-08-29T10:05:00.000Z",
    });
    const packageBytes = encodeCapsuleTransferPackage({
      checkpoint: transferred,
      repositorySeedBundle: SEED,
    });
    expect(decodeCapsuleTransferPackage(packageBytes).transfer.transferId).toBe(
      "transfer-2",
    );
    const corrupted = packageBytes.slice();
    const last = corrupted.byteLength - 1;
    corrupted[last] = corrupted[last]! ^ 1;
    expect(() => decodeCapsuleTransferPackage(corrupted)).toThrow(
      "failed verification",
    );

    const targetRoot = root("wrong-transfer-target");
    const wrongCheckpoints = new FilesystemCapsuleCheckpointStore(
      join(targetRoot, "capsules"),
    );
    await wrongCheckpoints.readActive("task-1").catch(() => undefined);
    const wrongTarget = new FilesystemCapsuleTransferReceiver(
      wrongCheckpoints,
      new FilesystemRepositorySeedCache(join(targetRoot, "seeds")),
      "buildbox",
    );
    expect(wrongTarget.prepare(packageBytes)).rejects.toThrow(
      "Transfer targets straylight",
    );
  });

  test("fail-stops the source when the target activation ACK is lost", async () => {
    const source = await sourceState();
    const targetRoot = root("ambiguous-target");
    const receiver = new FilesystemCapsuleTransferReceiver(
      new FilesystemCapsuleCheckpointStore(join(targetRoot, "capsules")),
      new FilesystemRepositorySeedCache(join(targetRoot, "seeds")),
      "straylight",
    );
    const transport: CapsuleTransferTransport = {
      prepare: (payload) => receiver.prepare(payload),
      activate: async () => {
        throw new Error("connection closed after commit");
      },
    };
    const coordinator = new LocalToRemoteCapsuleTransfer(
      source.checkpoints,
      source.seeds,
      "macbook",
      transport,
      { now: () => Date.parse("2026-08-29T10:05:00.000Z") },
    );
    const prepared = await coordinator.prepare({
      taskId: "task-1",
      transferId: "transfer-3",
      targetNodeId: "straylight",
      targetLeaseId: "lease-straylight",
    });

    expect(coordinator.commit(prepared)).rejects.toBeInstanceOf(
      AmbiguousCapsuleActivationError,
    );
    const sourceActive = await source.checkpoints.readActive("task-1");
    expect(sourceActive.manifest.ownership.ownerNodeId).toBe("straylight");
    expect(capsuleManifestDigest(sourceActive.manifest)).toBe(
      prepared.targetCheckpoint.digest,
    );
  });
});
