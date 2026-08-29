import { randomUUID } from "node:crypto";

import { captureLocalGitCapsule } from "../capsule/local-git.ts";
import {
  CapsuleCheckpointNotFoundError,
  type FilesystemCapsuleCheckpointStore,
  type CapsuleCheckpointReference,
} from "../capsule/filesystem.ts";
import { capsuleManifestDigest, type CapsuleCheckpoint } from "../capsule/manifest.ts";
import type { CapsuleMemberContent } from "../capsule/manifest.ts";
import type { FilesystemRepositorySeedCache } from "../capsule/seed-cache.ts";
import type { TaskCapsuleBootstrapProvisioner, TaskCapsuleManager } from "../runtime/task-manager.ts";

type PendingCheckpointRecoverer = Readonly<{
  recoverPending(taskId: string): Promise<CapsuleCheckpointReference | undefined>;
}>;

export interface PreparedLocalCapsule {
  readonly checkpoint: CapsuleCheckpointReference;
  readonly capsuleState: "already_running" | "started" | "created";
  readonly workspaceReused: boolean;
}

export class LocalCapsulePreparer {
  constructor(
    private readonly checkpoints: Pick<FilesystemCapsuleCheckpointStore, "publish" | "activate" | "readActive">,
    private readonly seeds: Pick<FilesystemRepositorySeedCache, "put" | "read">,
    private readonly capsules: Pick<TaskCapsuleManager, "ensure">,
    private readonly bootstrap: Pick<TaskCapsuleBootstrapProvisioner, "bootstrap">,
    private readonly pending?: PendingCheckpointRecoverer,
  ) {}

  async prepare(input: {
    readonly taskId: string;
    readonly taskImage: string;
    readonly network: string;
    readonly nodeId: string;
    readonly repositoryPath?: string;
    readonly additionalMembers?: readonly CapsuleMemberContent[];
  }): Promise<PreparedLocalCapsule> {
    let checkpoint: CapsuleCheckpoint | undefined;
    let seed: Uint8Array;
    try {
      checkpoint = await this.checkpoints.readActive(input.taskId);
    } catch (error) {
      if (!(error instanceof CapsuleCheckpointNotFoundError)) throw error;
    }
    if (checkpoint !== undefined) {
      if (checkpoint.manifest.ownership.ownerNodeId !== input.nodeId) {
        throw new Error(
          `Capsule ${checkpoint.manifest.capsuleId} is owned by node ${checkpoint.manifest.ownership.ownerNodeId}, not ${input.nodeId}`,
        );
      }
      seed = await this.seeds.read(capsuleManifestDigest(checkpoint.manifest));
    } else if (input.repositoryPath !== undefined) {
      const captured = await captureLocalGitCapsule({
        capsuleId: input.taskId,
        taskId: input.taskId,
        repositoryPath: input.repositoryPath,
        revision: 1,
        ownership: {
          epoch: 1,
          leaseId: randomUUID(),
          ownerNodeId: input.nodeId,
          status: "active",
        },
        ...(input.additionalMembers === undefined ? {} : { additionalMembers: input.additionalMembers }),
      });
      const reference = await this.checkpoints.publish(captured.checkpoint);
      checkpoint = await this.checkpoints.activate(reference);
      await this.seeds.put(reference.digest, captured.repositorySeedBundle);
      seed = captured.repositorySeedBundle;
    } else {
      throw new CapsuleCheckpointNotFoundError(`Task ${input.taskId} has no active capsule checkpoint`);
    }
    const reference = {
      capsuleId: checkpoint.manifest.capsuleId,
      revision: checkpoint.manifest.revision,
      digest: capsuleManifestDigest(checkpoint.manifest),
    };
    const capsule = await this.capsules.ensure({
      taskId: input.taskId,
      image: input.taskImage,
      network: input.network,
    });
    const recovered = await this.pending?.recoverPending(input.taskId);
    if (recovered !== undefined) {
      checkpoint = await this.checkpoints.readActive(input.taskId);
      seed = await this.seeds.read(capsuleManifestDigest(checkpoint.manifest));
    }
    const materialized = await this.bootstrap.bootstrap({
      taskId: input.taskId,
      checkpoint,
      repositorySeedBundle: seed,
    });
    return {
      checkpoint: reference,
      capsuleState: capsule.state,
      workspaceReused: materialized.reused,
    };
  }
}
