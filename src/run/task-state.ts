import type { EventEnvelope } from "../core/events.ts";
import type { Task } from "../core/model.ts";
import type { IntentArtifactEnvelope } from "../intent/protocol.ts";
import { isDeepStrictEqual } from "node:util";
import { replayTask } from "../core/reducer.ts";
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
  readonly intentArtifacts: readonly IntentArtifactEnvelope[];
}

export function decodePortableTaskState(bytes: Uint8Array): PortableTaskState {
  if (bytes.byteLength === 0 || bytes.byteLength > MAX_PORTABLE_TASK_STATE_BYTES) {
    throw new Error("Portable task state size is invalid");
  }
  let value: unknown;
  try {
    value = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes)) as unknown;
  } catch (error) {
    throw new Error("Portable task state is not valid UTF-8 JSON", { cause: error });
  }
  if (typeof value !== "object" || value === null) {
    throw new Error("Portable task state is invalid");
  }
  const snapshot = value as Partial<PortableTaskState>;
  if (snapshot.schema !== PORTABLE_TASK_STATE_FORMAT
    || typeof snapshot.task !== "object" || snapshot.task === null
    || !Array.isArray(snapshot.events) || !Array.isArray(snapshot.intentArtifacts)) {
    throw new Error("Portable task state schema is unsupported");
  }
  let replayed: Task;
  try {
    replayed = replayTask(snapshot.events as EventEnvelope[]);
  } catch (error) {
    throw new Error("Portable task event history is invalid", { cause: error });
  }
  if (!isDeepStrictEqual(replayed, snapshot.task)) {
    throw new Error("Portable task projection does not match its event history");
  }
  for (const artifact of snapshot.intentArtifacts) {
    if (typeof artifact !== "object" || artifact === null
      || artifact.schema !== "altitude.intent-artifact/v1"
      || artifact.taskId !== replayed.id) {
      throw new Error("Portable intent artifact is invalid");
    }
  }
  return {
    schema: PORTABLE_TASK_STATE_FORMAT,
    task: replayed,
    events: snapshot.events as EventEnvelope[],
    intentArtifacts: snapshot.intentArtifacts as IntentArtifactEnvelope[],
  };
}

export function portableTaskStateMember(
  store: Pick<TaskStore, "getTask" | "getEvents" | "listIntentArtifacts">,
  taskId: string,
): CapsuleMemberContent {
  const task = store.getTask(taskId);
  if (task === undefined) throw new Error(`Task ${taskId} does not exist`);
  const snapshot: PortableTaskState = {
    schema: PORTABLE_TASK_STATE_FORMAT,
    task,
    events: store.getEvents(taskId),
    intentArtifacts: store.listIntentArtifacts(taskId),
  };
  const bytes = utf8(`${stableJson(snapshot)}\n`);
  if (bytes.byteLength > MAX_PORTABLE_TASK_STATE_BYTES) {
    throw new Error(`Portable task state for ${taskId} exceeds ${MAX_PORTABLE_TASK_STATE_BYTES} bytes`);
  }
  return { path: PORTABLE_TASK_STATE_MEMBER, bytes };
}
