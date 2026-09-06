import { createHash } from "node:crypto";

import { LinearClient } from "@linear/sdk";

import type {
  LinearAgentInputRequest,
  LinearIssueCommentCreate,
  LinearTrackerMutations,
} from "./linear.ts";

const DEFAULT_LINEAR_GRAPHQL_ENDPOINT = "https://api.linear.app/graphql";

export interface LinearMutationClient {
  createComment(input: LinearCommentMutationInput): Promise<Readonly<{
    success: boolean;
    comment?: Promise<Readonly<{ id: string }>> | Readonly<{ id: string }> | undefined;
  }>>;
  createAgentActivity(input: LinearAgentActivityMutationInput): Promise<Readonly<{
    success: boolean;
    agentActivity?: Promise<Readonly<{ id: string }>> | Readonly<{ id: string }> | undefined;
  }>>;
}

export type LinearCommentMutationInput = Readonly<{
  id: string;
  issueId: string;
  body: string;
  doNotSubscribeToIssue: boolean;
  parentId?: string;
}>;

export type LinearAgentActivityMutationInput = Readonly<{
  id: string;
  agentSessionId: string;
  content: Readonly<{ type: "elicitation"; body: string }>;
}>;

function deterministicUuid(key: string): string {
  const bytes = createHash("sha256").update(key, "utf8").digest().subarray(0, 16);
  bytes[6] = (bytes[6]! & 0x0f) | 0x40;
  bytes[8] = (bytes[8]! & 0x3f) | 0x80;
  const hex = bytes.toString("hex");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

function required(value: string, label: string, maximum = 100_000): string {
  const normalized = value.trim();
  if (!normalized || normalized.length > maximum) {
    throw new Error(`${label} must be between 1 and ${maximum} characters`);
  }
  return normalized;
}

/** Official typed Linear SDK boundary; credentials remain process-only. */
export class LinearSdkMutations implements LinearTrackerMutations {
  readonly #client: LinearMutationClient;

  constructor(input: Readonly<{
    accessToken: string;
    endpoint?: string;
    client?: LinearMutationClient;
  }>) {
    const accessToken = required(input.accessToken, "Linear access token", 16_384);
    const endpoint = input.endpoint ?? DEFAULT_LINEAR_GRAPHQL_ENDPOINT;
    const url = new URL(endpoint);
    if (url.protocol !== "https:" && url.hostname !== "127.0.0.1" && url.hostname !== "localhost") {
      throw new Error("Linear GraphQL endpoint must use HTTPS or loopback");
    }
    this.#client = input.client ?? new LinearClient({ accessToken, apiUrl: endpoint });
  }

  async createIssueComment(input: LinearIssueCommentCreate): Promise<{ commentId: string }> {
    const payload = await this.#client.createComment({
      id: deterministicUuid(required(input.idempotencyKey, "Linear comment idempotency key", 1_000)),
      issueId: required(input.issueId, "Linear issue id", 200),
      body: required(input.body, "Linear comment body"),
      doNotSubscribeToIssue: true,
      ...(input.parentCommentId === undefined
        ? {}
        : { parentId: required(input.parentCommentId, "Linear parent comment id", 200) }),
    });
    const comment = await payload.comment;
    if (!payload.success || comment === undefined || !comment.id.trim()) {
      throw new Error("Linear did not confirm comment creation");
    }
    return { commentId: comment.id };
  }

  async requestAgentInput(input: LinearAgentInputRequest): Promise<{ activityId: string }> {
    const payload = await this.#client.createAgentActivity({
      id: deterministicUuid(required(input.idempotencyKey, "Linear activity idempotency key", 1_000)),
      agentSessionId: required(input.agentSessionId, "Linear Agent Session id", 200),
      content: {
        type: "elicitation",
        body: required(input.body, "Linear elicitation body"),
      },
    });
    const activity = await payload.agentActivity;
    if (!payload.success || activity === undefined || !activity.id.trim()) {
      throw new Error("Linear did not confirm Agent Activity creation");
    }
    return { activityId: activity.id };
  }
}
