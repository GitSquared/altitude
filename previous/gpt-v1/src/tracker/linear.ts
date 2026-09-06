import { createHash } from "node:crypto";

import type { Attention, AttentionClass } from "../core/model.ts";
import type { AttentionResolution } from "../core/events.ts";
import {
  isBlockingAttentionClass,
  type TrackerAttentionCorrelation,
  type TrackerNotificationRoute,
  type TrackerOperatorInput,
} from "./protocol.ts";

export type LinearTaskBinding = Readonly<{
  taskId: string;
  issueId: string;
  issueIdentifier?: string;
  agentSessionId?: string;
}>;

export type LinearIssueCommentCreate = Readonly<{
  issueId: string;
  body: string;
  parentCommentId?: string;
  idempotencyKey: string;
  routing: TrackerNotificationRoute;
}>;

export type LinearAgentInputRequest = Readonly<{
  agentSessionId: string;
  body: string;
  idempotencyKey: string;
  routing: TrackerNotificationRoute;
}>;

/** The only Linear mutations the first adapter is allowed to perform. */
export interface LinearTrackerMutations {
  /** Implementations must make repeated calls with the same key idempotent. */
  createIssueComment(input: LinearIssueCommentCreate): Promise<Readonly<{
    commentId: string;
  }>>;

  /** Implementations must make repeated calls with the same key idempotent. */
  requestAgentInput(input: LinearAgentInputRequest): Promise<Readonly<{
    activityId?: string;
  }>>;
}

export type LinearAttentionProjection = Readonly<{
  correlation: TrackerAttentionCorrelation;
  routing: TrackerNotificationRoute;
  agentInputActivityId?: string;
}>;

export type LinearWebhookNormalizationContext = Readonly<{
  /** A capsule-ledger mapping into Linear; it does not make the issue canonical. */
  binding: LinearTaskBinding;
  appUserId?: string;
  mentionTokens?: readonly string[];
  /** Only currently open Updates and the single active Decision or Review. */
  openAttention?: readonly TrackerAttentionCorrelation[];
}>;

function requireId(value: string, label: string): string {
  const normalized = value.trim();
  if (!normalized || normalized.length > 200) throw new Error(`${label} must be between 1 and 200 characters`);
  return normalized;
}

function markdownText(value: string): string {
  return value.trim().replace(/\n{3,}/g, "\n\n");
}

function markdownLabel(value: string): string {
  return markdownText(value).replace(/[\[\]]/g, "");
}

function renderEvidence(attention: Attention): string[] {
  return (attention.evidence ?? []).map((item) => `- [${markdownLabel(item.label)}](${item.uri})`);
}

function projectionMarker(
  taskId: string,
  attentionId: string,
  attentionClass: AttentionClass,
  phase: "raised" | "resolved",
): string {
  const attributes = [taskId, attentionId, attentionClass, phase].map(encodeURIComponent);
  return `<!-- altitude.projection/v1 task=${attributes[0]} attention=${attributes[1]} class=${attributes[2]} phase=${attributes[3]} -->`;
}

export function linearAttentionRoute(
  attentionClass: AttentionClass,
  hasAgentSession: boolean,
): TrackerNotificationRoute {
  if (isBlockingAttentionClass(attentionClass)) {
    return {
      attentionClass,
      mode: "response_required",
      blocking: true,
      surfaces: hasAgentSession
        ? ["issue_comment", "agent_session_input"]
        : ["issue_comment"],
      commentNotification: "operator_settings",
    };
  }
  return {
    attentionClass,
    mode: attentionClass === "update" ? "queued" : "record",
    blocking: false,
    surfaces: ["issue_comment"],
    commentNotification: "operator_settings",
  };
}

export function renderLinearAttentionComment(input: Readonly<{
  taskId: string;
  attention: Attention;
}>): string {
  const { attention } = input;
  const heading = attention.class === "trace"
    ? "Trace"
    : attention.class === "update"
      ? "Update"
      : attention.class === "decision"
        ? "Decision needed"
        : "Review needed";
  const lines = [
    `**${heading}: ${markdownText(attention.title)}**`,
    markdownText(attention.summary),
  ];

  if (attention.class === "decision") {
    lines.push(`**Question**\n${markdownText(attention.question)}`);
    if (attention.recommendation) lines.push(`**Recommendation**\n${markdownText(attention.recommendation)}`);
    if (attention.options?.length) {
      lines.push([
        "**Options**",
        ...attention.options.map((option) => {
          const tradeoff = option.tradeoff ? ` — ${markdownText(option.tradeoff)}` : "";
          return `- **${markdownLabel(option.label)}:** ${markdownText(option.value)}${tradeoff}`;
        }),
      ].join("\n"));
    }
    if (attention.originalIntent) lines.push(`**Original intent**\n${markdownText(attention.originalIntent)}`);
    if (attention.requestedIntentLevel) {
      lines.push(`**Requested level**\n${attention.requestedIntentLevel.replaceAll("_", " ")}`);
    }
    if (attention.consequence) lines.push(`**Cost of waiting**\n${markdownText(attention.consequence)}`);
  }

  if (attention.class === "review") {
    lines.push([
      "**Artifacts**",
      ...attention.artifacts.map((artifact) => `- [${markdownLabel(artifact.label)}](${artifact.uri})`),
    ].join("\n"));
    lines.push(["**Checks**", ...attention.checks.map((check) => `- ${markdownText(check)}`)].join("\n"));
    if (attention.caveats?.length) {
      lines.push(["**Caveats**", ...attention.caveats.map((caveat) => `- ${markdownText(caveat)}`)].join("\n"));
    }
  }

  const evidence = renderEvidence(attention);
  if (evidence.length) lines.push(["**Evidence**", ...evidence].join("\n"));
  lines.push(projectionMarker(input.taskId, attention.id, attention.class, "raised"));
  return lines.join("\n\n");
}

function renderNativeInput(attention: Extract<Attention, { class: "decision" | "review" }>): string {
  const heading = attention.class === "decision" ? "Decision needed" : "Review needed";
  return [
    `**${heading}: ${markdownText(attention.title)}**`,
    markdownText(attention.class === "decision" ? attention.question : attention.summary),
    "Full context and durable history are in the new comment on this issue.",
  ].join("\n\n");
}

function renderResolution(resolution: AttentionResolution): string {
  switch (resolution.kind) {
    case "acknowledged":
      return resolution.note ? `Acknowledged: ${markdownText(resolution.note)}` : "Acknowledged.";
    case "answered":
      return `Answered: ${markdownText(resolution.answer)}`;
    case "accepted":
      return resolution.note ? `Accepted: ${markdownText(resolution.note)}` : "Accepted.";
    case "revision_requested":
      return `Revision requested: ${markdownText(resolution.instructions)}`;
  }
}

/**
 * Linear is an append-only projection here: raises create a new issue comment,
 * blocking requests also use native Agent Session input, and resolutions reply
 * to the original comment instead of editing history in place.
 */
export class LinearTrackerAdapter {
  constructor(private readonly mutations: LinearTrackerMutations) {}

  async projectAttention(input: Readonly<{
    binding: LinearTaskBinding;
    attention: Attention;
  }>): Promise<LinearAttentionProjection> {
    const taskId = requireId(input.binding.taskId, "Task id");
    const issueId = requireId(input.binding.issueId, "Linear issue id");
    const attentionId = requireId(input.attention.id, "Attention id");
    const routing = linearAttentionRoute(input.attention.class, input.binding.agentSessionId !== undefined);
    const comment = await this.mutations.createIssueComment({
      issueId,
      body: renderLinearAttentionComment({
        taskId,
        attention: input.attention,
      }),
      idempotencyKey: `linear:attention:${taskId}:${attentionId}:comment`,
      routing,
    });
    const commentId = requireId(comment.commentId, "Linear comment id");

    let agentInputActivityId: string | undefined;
    if (isBlockingAttentionClass(input.attention.class) && input.binding.agentSessionId) {
      const attention = input.attention as Extract<Attention, { class: "decision" | "review" }>;
      const native = await this.mutations.requestAgentInput({
        agentSessionId: requireId(input.binding.agentSessionId, "Linear Agent Session id"),
        body: renderNativeInput(attention),
        idempotencyKey: `linear:attention:${taskId}:${attentionId}:agent-input`,
        routing,
      });
      agentInputActivityId = native.activityId;
    }

    return {
      correlation: {
        provider: "linear",
        taskId,
        attentionId,
        attentionClass: input.attention.class,
        issueId,
        issueCommentId: commentId,
        ...(input.binding.agentSessionId === undefined ? {} : { agentSessionId: input.binding.agentSessionId }),
      },
      routing,
      ...(agentInputActivityId === undefined ? {} : { agentInputActivityId }),
    };
  }

  async projectResolution(input: Readonly<{
    correlation: TrackerAttentionCorrelation;
    resolution: AttentionResolution;
  }>): Promise<Readonly<{ commentId: string }>> {
    if (input.correlation.provider !== "linear") throw new Error("Linear adapter received a non-Linear correlation");
    const routing = linearAttentionRoute(
      input.correlation.attentionClass,
      input.correlation.agentSessionId !== undefined,
    );
    const result = await this.mutations.createIssueComment({
      issueId: requireId(input.correlation.issueId, "Linear issue id"),
      parentCommentId: requireId(input.correlation.issueCommentId, "Linear parent comment id"),
      body: [
        renderResolution(input.resolution),
        projectionMarker(
          input.correlation.taskId,
          input.correlation.attentionId,
          input.correlation.attentionClass,
          "resolved",
        ),
      ].join("\n\n"),
      idempotencyKey: `linear:attention:${input.correlation.taskId}:${input.correlation.attentionId}:resolution:${resolutionKey(input.resolution)}`,
      routing,
    });
    return { commentId: requireId(result.commentId, "Linear comment id") };
  }
}

function resolutionKey(resolution: AttentionResolution): string {
  return createHash("sha256").update(stableJson(resolution)).digest("hex").slice(0, 24);
}

function record(value: unknown): Record<string, unknown> | undefined {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown>
    : undefined;
}

function text(value: unknown): string | undefined {
  return typeof value === "string" && value.trim() ? value.trim() : undefined;
}

function inputText(value: unknown): string | undefined {
  const valueText = text(value);
  return valueText?.slice(0, 8_000);
}

function stableJson(value: unknown): string {
  if (value === null || typeof value !== "object") return JSON.stringify(value) ?? "null";
  if (Array.isArray(value)) return `[${value.map(stableJson).join(",")}]`;
  const object = value as Record<string, unknown>;
  return `{${Object.keys(object).sort().filter((key) => object[key] !== undefined).map((key) => `${JSON.stringify(key)}:${stableJson(object[key])}`).join(",")}}`;
}

function webhookDedupeKey(payload: Record<string, unknown>): string {
  const webhookId = text(payload.webhookId);
  if (webhookId) return `linear:webhook:${webhookId}`;
  return `linear:webhook:sha256:${createHash("sha256").update(stableJson(payload)).digest("hex")}`;
}

function includesMention(body: string | undefined, tokens: readonly string[]): boolean {
  if (!body) return false;
  const normalized = body.toLowerCase();
  return tokens.some((token) => token.trim() && normalized.includes(token.trim().toLowerCase()));
}

function optionalTimestamp(value: unknown): number | undefined {
  if (typeof value !== "number" || !Number.isSafeInteger(value) || value < 0) return undefined;
  return value < 10_000_000_000 ? value * 1_000 : value;
}

function externalIssueId(payload: Record<string, unknown>): string | undefined {
  const notification = record(payload.notification);
  const notificationIssue = record(notification?.issue);
  const session = record(payload.agentSession);
  const sessionIssue = record(session?.issue);
  const data = record(payload.data);
  const dataIssue = record(data?.issue);
  return text(notification?.issueId)
    ?? text(notificationIssue?.id)
    ?? text(session?.issueId)
    ?? text(sessionIssue?.id)
    ?? text(data?.issueId)
    ?? text(dataIssue?.id);
}

export function linearWebhookIssueId(value: unknown): string | undefined {
  const payload = record(value);
  return payload === undefined ? undefined : externalIssueId(payload);
}

function correlationForComment(
  context: LinearWebhookNormalizationContext,
  commentId: string | undefined,
): TrackerAttentionCorrelation | undefined {
  if (!commentId) return undefined;
  return context.openAttention?.find((candidate) => (
    candidate.provider === "linear"
    && candidate.taskId === context.binding.taskId
    && candidate.issueId === context.binding.issueId
    && candidate.issueCommentId === commentId
  ));
}

function activeNativeCorrelation(
  context: LinearWebhookNormalizationContext,
  sessionId: string | undefined,
): TrackerAttentionCorrelation | undefined {
  if (!sessionId) return undefined;
  return context.openAttention?.find((candidate) => (
    candidate.provider === "linear"
    && candidate.taskId === context.binding.taskId
    && candidate.agentSessionId === sessionId
    && isBlockingAttentionClass(candidate.attentionClass)
  ));
}

function semanticInputKey(input: Readonly<{
  taskId: string;
  issueId: string;
  sourceKind: string;
  sourceId: string;
}>): string {
  return `linear:input:${input.taskId}:${input.issueId}:${input.sourceKind}:${input.sourceId}`;
}

/**
 * Normalizes already-authenticated Linear webhook JSON. Signature and replay-
 * window verification belong at the HTTP boundary before this pure function.
 */
export function normalizeLinearWebhook(
  value: unknown,
  context: LinearWebhookNormalizationContext,
): TrackerOperatorInput | null {
  const payload = record(value);
  if (!payload) return null;
  const issueId = externalIssueId(payload);
  if (!issueId || issueId !== context.binding.issueId) return null;

  const type = text(payload.type);
  const action = text(payload.action) ?? "unknown";
  const notification = record(payload.notification);
  const notificationComment = record(notification?.comment);
  const session = record(payload.agentSession);
  const sessionComment = record(session?.comment);
  const activity = record(payload.agentActivity);
  const activityContent = record(activity?.content);
  const webhookActor = record(payload.actor);
  const actorId = text(notification?.actorId)
    ?? text(session?.creatorId)
    ?? text(webhookActor?.id);
  if (context.appUserId && actorId === context.appUserId) return null;

  const dedupeKey = webhookDedupeKey(payload);
  const occurredAt = optionalTimestamp(payload.webhookTimestamp);

  if (type === "AgentSessionEvent") {
    if (action !== "created" && action !== "prompted") return null;
    const sessionId = text(session?.id);
    if (!sessionId || (context.binding.agentSessionId && sessionId !== context.binding.agentSessionId)) return null;
    const commentId = text(sessionComment?.id) ?? text(session?.sourceCommentId);
    const body = inputText(activity?.body)
      ?? inputText(activityContent?.body)
      ?? inputText(sessionComment?.body)
      ?? inputText(payload.promptContext)
      ?? inputText(session?.promptContext);
    if (!body) return null;
    const correlation = action === "prompted" ? activeNativeCorrelation(context, sessionId) : undefined;
    const sourceId = commentId ?? text(payload.webhookId) ?? sessionId;
    return {
      schema: "altitude.tracker-input/v1",
      provider: "linear",
      taskId: context.binding.taskId,
      text: body,
      ...(actorId === undefined ? {} : { actorId }),
      ...(occurredAt === undefined ? {} : { occurredAt }),
      source: {
        provider: "linear",
        surface: "agent_session",
        externalId: sourceId,
      },
      route: correlation
        ? {
            reason: "native_agent_response",
            blocking: true,
            attentionClass: correlation.attentionClass,
          }
        : { reason: "mention", blocking: false },
      ...(correlation === undefined ? {} : {
        attention: { id: correlation.attentionId, class: correlation.attentionClass },
      }),
      dedupeKey,
      idempotencyKey: semanticInputKey({
        taskId: context.binding.taskId,
        issueId,
        sourceKind: commentId ? "comment" : "agent-session",
        sourceId,
      }),
    };
  }

  if (type === "Comment" && action === "create") {
    const data = record(payload.data);
    const commentId = text(data?.id);
    const parentCommentId = text(data?.parentId);
    const body = inputText(data?.body);
    if (!commentId || !body) return null;
    const correlation = correlationForComment(context, parentCommentId);
    const mentioned = includesMention(body, context.mentionTokens ?? ["@altitude"]);
    if (correlation === undefined && !mentioned) return null;
    return {
      schema: "altitude.tracker-input/v1",
      provider: "linear",
      taskId: context.binding.taskId,
      text: body,
      ...(actorId === undefined ? {} : { actorId }),
      ...(occurredAt === undefined ? {} : { occurredAt }),
      source: {
        provider: "linear",
        surface: parentCommentId ? "issue_comment_reply" : "issue_comment",
        externalId: commentId,
        ...(parentCommentId === undefined ? {} : { parentExternalId: parentCommentId }),
      },
      route: correlation === undefined
        ? { reason: "mention", blocking: false }
        : {
            reason: "tracked_comment_reply",
            blocking: isBlockingAttentionClass(correlation.attentionClass),
            attentionClass: correlation.attentionClass,
          },
      ...(correlation === undefined ? {} : {
        attention: { id: correlation.attentionId, class: correlation.attentionClass },
      }),
      dedupeKey,
      idempotencyKey: semanticInputKey({
        taskId: context.binding.taskId,
        issueId,
        sourceKind: "comment",
        sourceId: commentId,
      }),
    };
  }

  if (type !== "AppUserNotification") return null;
  const commentId = text(notification?.commentId) ?? text(notificationComment?.id);
  const parentCommentId = text(notificationComment?.parentId)
    ?? text(notification?.parentCommentId);
  const body = inputText(notificationComment?.body)
    ?? inputText(record(notification?.issue)?.description);
  const correlation = correlationForComment(context, parentCommentId);
  if (action === "issueNewComment" && correlation && commentId && body) {
    return {
      schema: "altitude.tracker-input/v1",
      provider: "linear",
      taskId: context.binding.taskId,
      text: body,
      ...(actorId === undefined ? {} : { actorId }),
      ...(occurredAt === undefined ? {} : { occurredAt }),
      source: {
        provider: "linear",
        surface: "issue_comment_reply",
        externalId: commentId,
        parentExternalId: parentCommentId!,
      },
      route: {
        reason: "tracked_comment_reply",
        blocking: isBlockingAttentionClass(correlation.attentionClass),
        attentionClass: correlation.attentionClass,
      },
      attention: { id: correlation.attentionId, class: correlation.attentionClass },
      dedupeKey,
      idempotencyKey: semanticInputKey({
        taskId: context.binding.taskId,
        issueId,
        sourceKind: "comment",
        sourceId: commentId,
      }),
    };
  }

  const mentionAction = action === "issueMention" || action === "issueCommentMention";
  const mentionTokens = context.mentionTokens ?? ["@altitude"];
  if (!mentionAction && !includesMention(body, mentionTokens)) return null;
  if (!body) return null;
  const sourceId = commentId ?? text(payload.webhookId) ?? issueId;
  return {
    schema: "altitude.tracker-input/v1",
    provider: "linear",
    taskId: context.binding.taskId,
    text: body,
    ...(actorId === undefined ? {} : { actorId }),
    ...(occurredAt === undefined ? {} : { occurredAt }),
    source: {
      provider: "linear",
      surface: commentId
        ? parentCommentId ? "issue_comment_reply" : "issue_comment"
        : "issue",
      externalId: sourceId,
      ...(parentCommentId === undefined ? {} : { parentExternalId: parentCommentId }),
    },
    route: { reason: "mention", blocking: false },
    dedupeKey,
    idempotencyKey: semanticInputKey({
      taskId: context.binding.taskId,
      issueId,
      sourceKind: commentId ? "comment" : "webhook",
      sourceId,
    }),
  };
}
