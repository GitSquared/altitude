import type { EventEnvelope } from "../core/events.ts";
import type { Task } from "../core/model.ts";
import { stableJson, utf8 } from "../capsule/canonical.ts";
import type { CapsuleMemberContent } from "../capsule/manifest.ts";
import { portableCapsulePath } from "../capsule/paths.ts";
import type { TaskStore } from "../storage/task-store.ts";

export const PORTABLE_TASK_STATE_FORMAT = "altitude.task-state/v1" as const;
export const PORTABLE_TASK_STATE_MEMBER = portableCapsulePath("state/task.json");
const MAX_PORTABLE_TASK_STATE_BYTES = 32 * 1024 * 1024;

export interface PortableTaskState {
  readonly schema: typeof PORTABLE_TASK_STATE_FORMAT;
  readonly task: Task;
  readonly events: readonly EventEnvelope[];
}

export function portableTaskStateMember(
  store: Pick<TaskStore, "getTask" | "getEvents">,
  taskId: string,
): CapsuleMemberContent {
  const task = store.getTask(taskId);
  if (task === undefined) throw new Error(`Task ${taskId} does not exist`);
  const snapshot: PortableTaskState = {
    schema: PORTABLE_TASK_STATE_FORMAT,
    task,
    events: store.getEvents(taskId),
  };
  const bytes = utf8(`${stableJson(snapshot)}\n`);
  if (bytes.byteLength > MAX_PORTABLE_TASK_STATE_BYTES) {
    throw new Error(`Portable task state for ${taskId} exceeds ${MAX_PORTABLE_TASK_STATE_BYTES} bytes`);
  }
  return { path: PORTABLE_TASK_STATE_MEMBER, bytes };
}
