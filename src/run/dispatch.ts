import { isSha256Digest, type Sha256Digest } from "../capsule/canonical.ts";
import type { CapsuleCheckpointReference } from "../capsule/filesystem.ts";
import type { ScheduledActionInput } from "../storage/task-store.ts";

export const TASK_DISPATCH_ACTION_KIND = "task.run" as const;
export const TASK_DISPATCH_PAYLOAD_FORMAT =
  "altitude.task-dispatch/v1" as const;

const IDENTIFIER = /^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/u;

export interface TaskDispatchPayload {
  readonly schema: typeof TASK_DISPATCH_PAYLOAD_FORMAT;
  readonly transferId: string;
  readonly targetNodeId: string;
  readonly checkpointDigest: Sha256Digest;
  readonly checkpointRevision: number;
}

export function parseTaskDispatchPayload(value: unknown): TaskDispatchPayload {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error("Task dispatch payload is invalid");
  }
  const payload = value as Record<string, unknown>;
  if (payload.schema !== TASK_DISPATCH_PAYLOAD_FORMAT
    || typeof payload.transferId !== "string"
    || !IDENTIFIER.test(payload.transferId)
    || typeof payload.targetNodeId !== "string"
    || !IDENTIFIER.test(payload.targetNodeId)
    || !isSha256Digest(payload.checkpointDigest)
    || !Number.isSafeInteger(payload.checkpointRevision)
    || (payload.checkpointRevision as number) < 1) {
    throw new Error("Task dispatch payload is invalid");
  }
  return {
    schema: TASK_DISPATCH_PAYLOAD_FORMAT,
    transferId: payload.transferId,
    targetNodeId: payload.targetNodeId,
    checkpointDigest: payload.checkpointDigest,
    checkpointRevision: payload.checkpointRevision as number,
  };
}

export function taskDispatchAction(input: {
  readonly taskId: string;
  readonly transferId: string;
  readonly targetNodeId: string;
  readonly checkpoint: CapsuleCheckpointReference;
  readonly runAt: number;
}): ScheduledActionInput {
  if (!IDENTIFIER.test(input.taskId)) throw new Error("Dispatch task id is invalid");
  const payload = parseTaskDispatchPayload({
    schema: TASK_DISPATCH_PAYLOAD_FORMAT,
    transferId: input.transferId,
    targetNodeId: input.targetNodeId,
    checkpointDigest: input.checkpoint.digest,
    checkpointRevision: input.checkpoint.revision,
  });
  if (input.checkpoint.capsuleId !== input.taskId) {
    throw new Error("Task dispatch checkpoint belongs to another capsule");
  }
  if (!Number.isSafeInteger(input.runAt) || input.runAt < 0) {
    throw new Error("Task dispatch time is invalid");
  }
  return {
    taskId: input.taskId,
    kind: TASK_DISPATCH_ACTION_KIND,
    payload,
    runAt: input.runAt,
    dedupeKey: `transfer:${input.transferId}:run`,
  };
}
