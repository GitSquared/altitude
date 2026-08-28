import type {
  Attention,
  IntentLevel,
  TaskStatus,
} from "./model.ts";

export const EVENT_SCHEMA_VERSION = 1 as const;

export type EventActor =
  | Readonly<{ kind: "operator"; id?: string }>
  | Readonly<{ kind: "agent"; adapter: string; sessionId?: string }>
  | Readonly<{ kind: "system"; component: string }>
  | Readonly<{ kind: "integration"; adapter: string; externalId?: string }>;

export type AttentionResolution =
  | Readonly<{ kind: "acknowledged"; note?: string }>
  | Readonly<{ kind: "answered"; answer: string }>
  | Readonly<{ kind: "accepted"; note?: string }>
  | Readonly<{ kind: "revision_requested"; instructions: string }>;

export type EventPayloads = {
  "task.created": Readonly<{
    title: string;
    intent: string;
    intentLevel: IntentLevel;
  }>;
  "task.state_changed": Readonly<{
    to: TaskStatus;
    reason?: string;
  }>;
  "task.intent_changed": Readonly<{
    level: IntentLevel;
    intent?: string;
  }>;
  "task.renamed": Readonly<{
    title: string;
  }>;
  "attention.raised": Readonly<{
    attention: Attention;
  }>;
  "attention.resolved": Readonly<{
    attentionId: string;
    resolution: AttentionResolution;
  }>;
  "attention.superseded": Readonly<{
    attentionId: string;
    reason: string;
    byAttentionId?: string;
  }>;
  "attention.withdrawn": Readonly<{
    attentionId: string;
    reason: string;
  }>;
};

export type EventType = keyof EventPayloads;

type EventBody = {
  [Type in EventType]: Readonly<{
    type: Type;
    payload: EventPayloads[Type];
  }>;
}[EventType];

type EnvelopeMetadata = Readonly<{
  id: string;
  taskId: string;
  sequence: number;
  schemaVersion: typeof EVENT_SCHEMA_VERSION;
  occurredAt: number;
  actor: EventActor;
  idempotencyKey?: string;
}>;

export type EventEnvelope = EventBody & EnvelopeMetadata;

export type EventEnvelopeOf<Type extends EventType> = Extract<EventEnvelope, { type: Type }>;

export type NewEvent<Type extends EventType = EventType> = Readonly<{
  taskId: string;
  type: Type;
  payload: EventPayloads[Type];
  actor: EventActor;
  occurredAt?: number;
  eventId?: string;
  idempotencyKey?: string;
}>;

export function eventAttentionId(event: EventEnvelope): string | null {
  switch (event.type) {
    case "attention.raised":
      return event.payload.attention.id;
    case "attention.resolved":
    case "attention.superseded":
    case "attention.withdrawn":
      return event.payload.attentionId;
    default:
      return null;
  }
}
