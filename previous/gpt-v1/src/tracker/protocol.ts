import type {
  AttentionClass,
  BlockingAttentionClass,
} from "../core/model.ts";

export type TrackerNotificationMode = "record" | "queued" | "response_required";

/**
 * Routing is policy metadata, not a claim about what Linear ultimately shows.
 * The operator's Linear notification settings remain in control of comment
 * delivery; blocking requests additionally use the native Agent Session input
 * surface when one is bound.
 */
export type TrackerNotificationRoute = Readonly<{
  attentionClass: AttentionClass;
  mode: TrackerNotificationMode;
  blocking: boolean;
  surfaces: readonly ("issue_comment" | "agent_session_input")[];
  commentNotification: "operator_settings";
}>;

export type TrackerSource = Readonly<{
  provider: "linear";
  surface: "issue" | "issue_comment" | "issue_comment_reply" | "agent_session";
  externalId: string;
  parentExternalId?: string;
}>;

export type TrackerInputRoute = Readonly<{
  reason: "mention" | "tracked_comment_reply" | "native_agent_response";
  blocking: boolean;
  attentionClass?: AttentionClass;
}>;

export type TrackerOperatorInput = Readonly<{
  schema: "altitude.tracker-input/v1";
  provider: "linear";
  taskId: string;
  text: string;
  actorId?: string;
  occurredAt?: number;
  source: TrackerSource;
  route: TrackerInputRoute;
  attention?: Readonly<{
    id: string;
    class: AttentionClass;
  }>;
  /** Dedupe for the physical webhook delivery. */
  dedupeKey: string;
  /** Dedupe for the semantic operator input across equivalent webhook types. */
  idempotencyKey: string;
}>;

export type TrackerAttentionCorrelation = Readonly<{
  provider: "linear";
  taskId: string;
  attentionId: string;
  attentionClass: AttentionClass;
  issueId: string;
  issueCommentId: string;
  agentSessionId?: string;
}>;

export function isBlockingAttentionClass(
  value: AttentionClass,
): value is BlockingAttentionClass {
  return value === "decision" || value === "review";
}
