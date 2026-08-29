import { capsuleManifestDigest, type CapsuleCheckpoint, type CapsuleManifestV1 } from "../capsule/manifest.ts";
import type { CapsuleCheckpointReference, FilesystemCapsuleCheckpointStore } from "../capsule/filesystem.ts";
import type { FilesystemRepositorySeedCache } from "../capsule/seed-cache.ts";
import { TASK_CHECKPOINT_PROTOCOL, repositorySeedBindingDigest } from "../capsule/task-checkpoint.ts";
import type { TaskCapsuleCheckpointProvisioner } from "../runtime/task-manager.ts";
import type { TaskStore } from "../storage/task-store.ts";
import { portableTaskStateMember } from "./task-state.ts";

type CheckpointStore = Pick<FilesystemCapsuleCheckpointStore, "publish" | "activate" | "readActive">;
type SeedCache = Pick<FilesystemRepositorySeedCache, "put" | "read">;
type CheckpointProvisioner = Pick<TaskCapsuleCheckpointProvisioner, "capture" | "pending" | "activate">;

function reference(checkpoint: CapsuleCheckpoint): CapsuleCheckpointReference {
  return {
    capsuleId: checkpoint.manifest.capsuleId,
    revision: checkpoint.manifest.revision,
    digest: capsuleManifestDigest(checkpoint.manifest),
  };
}

function assertCheckpointAdvance(previous: CapsuleManifestV1, next: CapsuleManifestV1): void {
  if (next.capsuleId !== previous.capsuleId || next.taskId !== previous.taskId
    || next.revision !== previous.revision + 1) {
    throw new Error("Task checkpoint does not advance its active capsule by one revision");
  }
  if (next.ownership.epoch !== previous.ownership.epoch
    || next.ownership.leaseId !== previous.ownership.leaseId
    || next.ownership.ownerNodeId !== previous.ownership.ownerNodeId) {
    throw new Error("Task checkpoint attempted to change capsule ownership during a local capture");
  }
  if (next.repository.remoteUrl !== previous.repository.remoteUrl
    || next.repository.defaultBranch !== previous.repository.defaultBranch
    || next.repository.baseCommit !== previous.repository.baseCommit) {
    throw new Error("Task checkpoint attempted to change the repository seed binding");
  }
  if (!next.members.some((member) => member.path === "state/task.json")) {
    throw new Error("Task checkpoint is missing its portable task state");
  }
}

/** Publishes task-capsule changes through a recoverable pending -> active handshake. */
export class LocalCapsuleCheckpointer {
  constructor(
    private readonly checkpoints: CheckpointStore,
    private readonly seeds: SeedCache,
    private readonly provisioner: CheckpointProvisioner,
    private readonly tasks: Pick<TaskStore, "getTask" | "getEvents" | "listIntentArtifacts">,
    private readonly now: () => number = Date.now,
  ) {}

  async capture(taskId: string): Promise<CapsuleCheckpointReference> {
    const previous = await this.checkpoints.readActive(taskId);
    const previousDigest = capsuleManifestDigest(previous.manifest);
    const seed = await this.seeds.read(previousDigest);
    const checkpoint = await this.provisioner.capture({
      schema: TASK_CHECKPOINT_PROTOCOL,
      taskId,
      capsuleId: previous.manifest.capsuleId,
      revision: previous.manifest.revision + 1,
      checkpointedAt: new Date(this.now()).toISOString(),
      ownership: previous.manifest.ownership,
      previousCheckpointDigest: previousDigest,
      repositoryBindingDigest: repositorySeedBindingDigest(previous.manifest.repository),
      additionalMembers: [portableTaskStateMember(this.tasks, taskId)],
    });
    assertCheckpointAdvance(previous.manifest, checkpoint.manifest);
    return await this.#publishAndActivate(checkpoint, seed);
  }

  async recoverPending(taskId: string): Promise<CapsuleCheckpointReference | undefined> {
    const pending = await this.provisioner.pending(taskId);
    if (pending === undefined) return undefined;
    const pendingReference = reference(pending);
    const active = await this.checkpoints.readActive(taskId);
    const activeReference = reference(active);
    if (pendingReference.digest === activeReference.digest) {
      await this.provisioner.activate(taskId, pendingReference.digest);
      return pendingReference;
    }
    assertCheckpointAdvance(active.manifest, pending.manifest);
    const seed = await this.seeds.read(activeReference.digest);
    return await this.#publishAndActivate(pending, seed);
  }

  async #publishAndActivate(checkpoint: CapsuleCheckpoint, repositorySeed: Uint8Array): Promise<CapsuleCheckpointReference> {
    const published = await this.checkpoints.publish(checkpoint);
    await this.seeds.put(published.digest, repositorySeed);
    await this.checkpoints.activate(published);
    await this.provisioner.activate(checkpoint.manifest.taskId, published.digest);
    return published;
  }
}
