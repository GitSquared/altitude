import { createHmac, timingSafeEqual } from "node:crypto";

import type { AttentionResolution } from "../core/events.ts";
import type { Task } from "../core/model.ts";
import type { TaskStore } from "../storage/task-store.ts";
import { linearWebhookIssueId, normalizeLinearWebhook } from "./linear.ts";
import type { TrackerOperatorInput } from "./protocol.ts";
import { LinearTrackerStateStore } from "./state.ts";

const MAX_WEBHOOK_BYTES = 256 * 1024;
const DEFAULT_REPLAY_WINDOW_MS = 60_000;
export const LINEAR_INPUT_ROUTE_TOPIC = "tracker.linear.input.route";

export type LinearInputRoutePayload = Readonly<{
  input: TrackerOperatorInput;
  resolution?: AttentionResolution;
}>;

export type LinearWebhookHandler = (request: Request) => Promise<Response>;

export function parseLinearInputRoutePayload(value: unknown): LinearInputRoutePayload {
  if (value === null || typeof value !== "object" || Array.isArray(value)) {
    throw new Error("Linear input route payload is invalid");
  }
  const payload = value as Record<string, unknown>;
  const trackerInput = payload.input;
  if (trackerInput === null || typeof trackerInput !== "object" || Array.isArray(trackerInput)) {
    throw new Error("Linear input route payload has no tracker input");
  }
  const candidate = trackerInput as Partial<TrackerOperatorInput>;
  if (candidate.schema !== "altitude.tracker-input/v1"
    || candidate.provider !== "linear"
    || typeof candidate.taskId !== "string"
    || typeof candidate.text !== "string"
    || candidate.text.length === 0
    || candidate.taskId.length === 0) {
    throw new Error("Linear input route payload has an invalid tracker input");
  }
  return value as LinearInputRoutePayload;
}

function response(status: number, value: unknown): Response {
  return Response.json(value, {
    status,
    headers: { "cache-control": "no-store" },
  });
}

function verifiedSignature(raw: Uint8Array, signature: string | null, secret: string): boolean {
  if (signature === null || !/^[a-f\d]{64}$/i.test(signature)) return false;
  const expected = createHmac("sha256", secret).update(raw).digest();
  const provided = Buffer.from(signature, "hex");
  return provided.length === expected.length && timingSafeEqual(provided, expected);
}

function webhookTime(value: unknown): number | undefined {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) return undefined;
  return value < 10_000_000_000 ? value * 1_000 : value;
}

function openCorrelations(
  task: Task,
  correlations: readonly ReturnType<LinearTrackerStateStore["listCorrelations"]>[number][],
): ReturnType<LinearTrackerStateStore["listCorrelations"]> {
  const open = new Set([
    ...task.openUpdates.map((attention) => attention.id),
    ...(task.activeAttention === null ? [] : [task.activeAttention.id]),
  ]);
  return correlations.filter((correlation) => open.has(correlation.attentionId));
}

function reviewResolution(text: string): AttentionResolution {
  const normalized = text.trim();
  if (/^(accept|accepted|approve|approved)[.!]?$/i.test(normalized)) {
    return { kind: "accepted", note: normalized };
  }
  return { kind: "revision_requested", instructions: normalized };
}

function attentionResolution(input: TrackerOperatorInput): AttentionResolution | undefined {
  switch (input.attention?.class) {
    case "decision":
      return { kind: "answered", answer: input.text };
    case "review":
      return reviewResolution(input.text);
    case "update":
      return { kind: "acknowledged", note: input.text };
    default:
      return undefined;
  }
}

export function applyLinearOperatorInput(
  store: TaskStore,
  input: TrackerOperatorInput,
): Readonly<{ duplicate: boolean; eventId: string }> {
  const actor = {
    kind: "integration" as const,
    adapter: "linear",
    externalId: input.source.externalId,
  };
  const resolution = attentionResolution(input);
  const existing = store.getEventByIdempotencyKey(input.taskId, input.idempotencyKey);
  if (existing !== undefined) {
    const expectedType = resolution === undefined ? "operator.input_received" : "attention.resolved";
    if (existing.type !== expectedType) {
      throw new Error(`Tracker input key ${input.idempotencyKey} collides with ${existing.type}`);
    }
    return { duplicate: true, eventId: existing.id };
  }
  const event = resolution === undefined
    ? {
        taskId: input.taskId,
        type: "operator.input_received" as const,
        payload: {
          text: input.text,
          provider: input.provider,
          source: {
            surface: input.source.surface,
            externalId: input.source.externalId,
            ...(input.source.parentExternalId === undefined
              ? {}
              : { parentExternalId: input.source.parentExternalId }),
          },
          ...(input.actorId === undefined ? {} : { actorId: input.actorId }),
          routeReason: input.route.reason,
        },
        actor,
        ...(input.occurredAt === undefined ? {} : { occurredAt: input.occurredAt }),
        idempotencyKey: input.idempotencyKey,
      }
    : {
        taskId: input.taskId,
        type: "attention.resolved" as const,
        payload: { attentionId: input.attention!.id, resolution },
        actor,
        ...(input.occurredAt === undefined ? {} : { occurredAt: input.occurredAt }),
        idempotencyKey: input.idempotencyKey,
      };
  const result = store.append(event, [{
    topic: LINEAR_INPUT_ROUTE_TOPIC,
    payload: { input, ...(resolution === undefined ? {} : { resolution }) },
    dedupeKey: input.idempotencyKey,
  }]);
  return { duplicate: result.duplicate, eventId: result.event.id };
}

export function createLinearWebhookHandler(input: Readonly<{
  secret: string;
  store: TaskStore;
  state?: LinearTrackerStateStore;
  now?: () => number;
  replayWindowMs?: number;
}>): LinearWebhookHandler {
  const secret = input.secret;
  if (!secret.trim()) throw new Error("Linear webhook secret is required");
  const state = input.state ?? new LinearTrackerStateStore(input.store.database, input.now);
  const now = input.now ?? Date.now;
  const replayWindowMs = input.replayWindowMs ?? DEFAULT_REPLAY_WINDOW_MS;
  if (!Number.isSafeInteger(replayWindowMs) || replayWindowMs < 1) {
    throw new Error("Linear webhook replay window is invalid");
  }

  return async (request) => {
    if (request.method !== "POST") {
      return response(405, { error: "method_not_allowed" });
    }
    const contentLength = Number(request.headers.get("content-length"));
    if (Number.isFinite(contentLength) && contentLength > MAX_WEBHOOK_BYTES) {
      return response(413, { error: "payload_too_large" });
    }
    const raw = new Uint8Array(await request.arrayBuffer());
    if (raw.byteLength > MAX_WEBHOOK_BYTES) return response(413, { error: "payload_too_large" });
    if (!verifiedSignature(raw, request.headers.get("linear-signature"), secret)) {
      return response(401, { error: "invalid_signature" });
    }

    let payload: unknown;
    try {
      payload = JSON.parse(new TextDecoder().decode(raw)) as unknown;
    } catch {
      return response(400, { error: "invalid_json" });
    }
    const timestamp = webhookTime(
      payload !== null && typeof payload === "object"
        ? (payload as Record<string, unknown>).webhookTimestamp
        : undefined,
    );
    if (timestamp === undefined || Math.abs(now() - timestamp) > replayWindowMs) {
      return response(401, { error: "stale_webhook" });
    }

    const issueId = linearWebhookIssueId(payload);
    const binding = issueId === undefined ? undefined : state.getByIssueId(issueId);
    if (binding === undefined) return response(200, { accepted: true, routed: false });
    const task = input.store.getTask(binding.taskId);
    if (task === undefined) return response(200, { accepted: true, routed: false });
    const normalized = normalizeLinearWebhook(payload, {
      binding,
      ...(binding.appUserId === undefined ? {} : { appUserId: binding.appUserId }),
      mentionTokens: binding.mentionTokens,
      openAttention: openCorrelations(task, state.listCorrelations(binding.taskId)),
    });
    if (normalized === null) return response(200, { accepted: true, routed: false });
    const applied = applyLinearOperatorInput(input.store, normalized);
    return response(200, {
      accepted: true,
      routed: true,
      duplicate: applied.duplicate,
    });
  };
}
