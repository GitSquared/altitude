import type { AttentionResolution, NewEvent } from "../core/events.ts";
import type { Attention } from "../core/model.ts";
import type { OutboxMessageInput, TaskStore } from "../storage/task-store.ts";
import { LinearTrackerAdapter } from "./linear.ts";
import type { LinearTrackerMutations } from "./linear.ts";
import { LinearTrackerStateStore } from "./state.ts";

export const LINEAR_PROJECTION_TOPIC = "tracker.linear.projection";

export type LinearProjectionPayload =
  | Readonly<{
      schema: "altitude.linear-projection/v1";
      action: "attention.raised";
      taskId: string;
      attention: Attention;
    }>
  | Readonly<{
      schema: "altitude.linear-projection/v1";
      action: "attention.resolved";
      taskId: string;
      attentionId: string;
      resolution: AttentionResolution;
    }>;

export function planLinearProjection(
  store: TaskStore,
  event: NewEvent<"attention.raised"> | NewEvent<"attention.resolved">,
): readonly OutboxMessageInput[] {
  const binding = new LinearTrackerStateStore(store.database).getByTaskId(event.taskId);
  if (binding === undefined) return [];
  const payload: LinearProjectionPayload = event.type === "attention.raised"
    ? {
        schema: "altitude.linear-projection/v1",
        action: "attention.raised",
        taskId: event.taskId,
        attention: event.payload.attention,
      }
    : {
        schema: "altitude.linear-projection/v1",
        action: "attention.resolved",
        taskId: event.taskId,
        attentionId: event.payload.attentionId,
        resolution: event.payload.resolution,
      };
  const attentionId = event.type === "attention.raised"
    ? event.payload.attention.id
    : event.payload.attentionId;
  return [{
    topic: LINEAR_PROJECTION_TOPIC,
    payload,
    dedupeKey: `linear:projection:${event.taskId}:${attentionId}:${event.type}`,
  }];
}

export function parseLinearProjectionPayload(value: unknown): LinearProjectionPayload {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new Error("Linear projection payload is invalid");
  }
  const payload = value as Record<string, unknown>;
  if (payload.schema !== "altitude.linear-projection/v1"
    || (payload.action !== "attention.raised" && payload.action !== "attention.resolved")
    || typeof payload.taskId !== "string"
    || !payload.taskId.trim()) {
    throw new Error("Linear projection payload is unsupported");
  }
  if (payload.action === "attention.raised") {
    if (payload.attention === null || typeof payload.attention !== "object") {
      throw new Error("Linear raise projection has no attention");
    }
  } else if (typeof payload.attentionId !== "string" || !payload.attentionId.trim()
    || payload.resolution === null || typeof payload.resolution !== "object") {
    throw new Error("Linear resolution projection is invalid");
  }
  return value as LinearProjectionPayload;
}

export function createLinearProjectionExecutor(input: Readonly<{
  store: TaskStore;
  mutations: LinearTrackerMutations;
  state?: LinearTrackerStateStore;
}>): (taskId: string, payload: LinearProjectionPayload) => Promise<"executed" | "skipped"> {
  const state = input.state ?? new LinearTrackerStateStore(input.store.database);
  const adapter = new LinearTrackerAdapter(input.mutations);
  return async (taskId, payload) => {
    if (payload.taskId !== taskId) throw new Error("Linear projection task binding changed in the outbox");
    const binding = state.getByTaskId(taskId);
    if (binding === undefined) return "skipped";
    if (payload.action === "attention.raised") {
      const projection = await adapter.projectAttention({ binding, attention: payload.attention });
      state.recordCorrelation(projection.correlation);
      return "executed";
    }
    const correlation = state.getCorrelation(taskId, payload.attentionId);
    if (correlation === undefined) {
      throw new Error(`Linear attention ${payload.attentionId} has not been projected yet`);
    }
    await adapter.projectResolution({ correlation, resolution: payload.resolution });
    return "executed";
  };
}
