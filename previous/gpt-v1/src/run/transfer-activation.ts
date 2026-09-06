import type { CapsuleCheckpointReference } from "../capsule/filesystem.ts";
import type { Task } from "../core/model.ts";
import type { PortableTaskState } from "./task-state.ts";
import { taskDispatchAction } from "./dispatch.ts";
import type { TaskAgentSessionStore } from "../storage/task-agent-session-store.ts";
import type { TaskStore } from "../storage/task-store.ts";

export function activateTransferredTask(input: {
  readonly store: TaskStore;
  readonly sessions: Pick<TaskAgentSessionStore, "importPortable">;
  readonly portableState: PortableTaskState;
  readonly targetIdentityId: string;
  readonly targetNodeId: string;
  readonly transferId: string;
  readonly checkpoint: CapsuleCheckpointReference;
  readonly activatedAt: number;
}): Task {
  const imported = input.store.importPortableTaskState(input.portableState);
  if (input.portableState.agentSession !== undefined) {
    input.sessions.importPortable(
      input.portableState.agentSession,
      input.targetIdentityId,
    );
  }
  if (imported.status !== "transferring") return imported;
  return input.store.append({
    taskId: imported.id,
    type: "task.state_changed",
    payload: {
      to: "queued",
      reason: `Capsule activated on node ${input.targetNodeId}`,
    },
    actor: { kind: "system", component: "capsule-transfer" },
    idempotencyKey: `transfer:${input.transferId}:activated`,
    occurredAt: input.activatedAt,
  }, [], [taskDispatchAction({
    taskId: imported.id,
    transferId: input.transferId,
    targetNodeId: input.targetNodeId,
    checkpoint: input.checkpoint,
    runAt: input.activatedAt,
  })]).task;
}
