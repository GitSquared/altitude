import { describe, expect, test } from "bun:test";

import type { Attention, AttentionResolution } from "../core/index.ts";
import {
  LinearTrackerAdapter,
  linearAttentionRoute,
  normalizeLinearWebhook,
  type LinearAgentInputRequest,
  type LinearIssueCommentCreate,
  type LinearTrackerMutations,
} from "./index.ts";

class FakeLinearMutations implements LinearTrackerMutations {
  readonly comments: LinearIssueCommentCreate[] = [];
  readonly inputs: LinearAgentInputRequest[] = [];

  async createIssueComment(input: LinearIssueCommentCreate): Promise<{ commentId: string }> {
    this.comments.push(input);
    return { commentId: `comment-${this.comments.length}` };
  }

  async requestAgentInput(input: LinearAgentInputRequest): Promise<{ activityId: string }> {
    this.inputs.push(input);
    return { activityId: `activity-${this.inputs.length}` };
  }
}

const trace: Attention = {
  id: "trace-1",
  class: "trace",
  title: "Repository oriented",
  summary: "Found the runtime boundary.",
};

const update: Attention = {
  id: "update-1",
  class: "update",
  title: "Checkpoint",
  summary: "The durable core is passing.",
};

const decision: Attention = {
  id: "decision-1",
  class: "decision",
  title: "Choose the handoff",
  summary: "Both transports preserve the capsule boundary.",
  question: "Should the first remote use an Iroh pairing ticket?",
  recommendation: "Start with the native Iroh route.",
  options: [
    { label: "Iroh", value: "iroh", tradeoff: "One native, portable node route." },
    { label: "Local only", value: "local", tradeoff: "No unattended remote work yet." },
  ],
  originalIntent: "Send small capsules away and bring them back.",
  requestedIntentLevel: "option_selection",
  consequence: "Remote handoff work waits for the choice.",
};

const review: Attention = {
  id: "review-1",
  class: "review",
  title: "Review the first useful run",
  summary: "A capsule completed without borrowing host identity.",
  artifacts: [
    { kind: "pull_request", label: "Draft PR", uri: "https://github.com/example/altitude/pull/1" },
  ],
  checks: ["bun run check", "capsule restored on the target node"],
  evidence: [{ label: "Run receipt", uri: "https://example.test/receipt" }],
};

describe("Linear tracker projection", () => {
  test("projects every attention level into additive issue history", async () => {
    const mutations = new FakeLinearMutations();
    const adapter = new LinearTrackerAdapter(mutations);

    for (const attention of [trace, update, decision, review]) {
      await adapter.projectAttention({
        binding: { taskId: "task-1", issueId: "issue-1", agentSessionId: "session-1" },
        attention,
      });
    }

    expect(mutations.comments).toHaveLength(4);
    expect(mutations.comments.map((comment) => comment.routing.mode)).toEqual([
      "record",
      "queued",
      "response_required",
      "response_required",
    ]);
    expect(mutations.comments.every((comment) => comment.body.includes("altitude.projection/v1"))).toBeTrue();
    expect(mutations.inputs).toHaveLength(2);
    expect(mutations.inputs.map((input) => input.routing.attentionClass)).toEqual(["decision", "review"]);
  });

  test("supports multiple Updates while reserving native input for one blocking request", async () => {
    const mutations = new FakeLinearMutations();
    const adapter = new LinearTrackerAdapter(mutations);
    const first = await adapter.projectAttention({
      binding: { taskId: "task-1", issueId: "issue-1", agentSessionId: "session-1" },
      attention: update,
    });
    const second = await adapter.projectAttention({
      binding: { taskId: "task-1", issueId: "issue-1", agentSessionId: "session-1" },
      attention: { ...update, id: "update-2", title: "Another checkpoint" },
    });
    const blocking = await adapter.projectAttention({
      binding: { taskId: "task-1", issueId: "issue-1", agentSessionId: "session-1" },
      attention: decision,
    });

    expect([first, second].map((projection) => projection.correlation.attentionClass)).toEqual(["update", "update"]);
    expect(blocking.correlation.attentionClass).toBe("decision");
    expect(mutations.inputs).toHaveLength(1);
  });

  test("appends a resolution reply instead of rewriting the request", async () => {
    const mutations = new FakeLinearMutations();
    const adapter = new LinearTrackerAdapter(mutations);
    const projection = await adapter.projectAttention({
      binding: { taskId: "task-1", issueId: "issue-1", agentSessionId: "session-1" },
      attention: decision,
    });
    const resolution: AttentionResolution = { kind: "answered", answer: "Use native Iroh first." };

    await adapter.projectResolution({ correlation: projection.correlation, resolution });

    expect(mutations.comments).toHaveLength(2);
    expect(mutations.comments[1]?.parentCommentId).toBe("comment-1");
    expect(mutations.comments[1]?.body).toContain("Answered: Use native Iroh first.");
    expect(mutations.comments[1]?.idempotencyKey).toStartWith("linear:attention:task-1:decision-1:resolution:");
  });

  test("leaves comment notifications to operator settings and marks blocking surfaces", () => {
    expect(linearAttentionRoute("update", true)).toEqual({
      attentionClass: "update",
      mode: "queued",
      blocking: false,
      surfaces: ["issue_comment"],
      commentNotification: "operator_settings",
    });
    expect(linearAttentionRoute("review", true).surfaces).toEqual(["issue_comment", "agent_session_input"]);
  });
});

describe("Linear webhook normalization", () => {
  const binding = { taskId: "task-1", issueId: "issue-1", agentSessionId: "session-1" } as const;

  test("routes a mentioned comment anywhere on the issue as operator input", () => {
    const input = normalizeLinearWebhook({
      type: "AppUserNotification",
      action: "issueCommentMention",
      webhookId: "delivery-1",
      webhookTimestamp: 1_787_913_600,
      notification: {
        issueId: "issue-1",
        actorId: "operator-1",
        comment: { id: "comment-anywhere", body: "@altitude keep the core portable" },
      },
    }, { binding, appUserId: "altitude-app", mentionTokens: ["@altitude"] });

    expect(input?.route).toEqual({ reason: "mention", blocking: false });
    expect(input?.source.surface).toBe("issue_comment");
    expect(input?.occurredAt).toBe(1_787_913_600_000);
    expect(input?.dedupeKey).toBe("linear:webhook:delivery-1");
    expect(input?.idempotencyKey).toBe("linear:input:task-1:issue-1:comment:comment-anywhere");
  });

  test("routes a reply to a tracked blocking comment without requiring another mention", () => {
    const input = normalizeLinearWebhook({
      type: "AppUserNotification",
      action: "issueNewComment",
      webhookId: "delivery-2",
      notification: {
        issueId: "issue-1",
        actorId: "operator-1",
        comment: {
          id: "reply-1",
          parentId: "decision-comment",
          body: "Use native Iroh first.",
        },
      },
    }, {
      binding,
      openAttention: [{
        provider: "linear",
        taskId: "task-1",
        attentionId: "decision-1",
        attentionClass: "decision",
        issueId: "issue-1",
        issueCommentId: "decision-comment",
        agentSessionId: "session-1",
      }],
    });

    expect(input?.route).toEqual({
      reason: "tracked_comment_reply",
      blocking: true,
      attentionClass: "decision",
    });
    expect(input?.attention).toEqual({ id: "decision-1", class: "decision" });
    expect(input?.text).toBe("Use native Iroh first.");
  });

  test("accepts the documented Comment data-change webhook shape", () => {
    const input = normalizeLinearWebhook({
      type: "Comment",
      action: "create",
      webhookId: "delivery-comment-data",
      webhookTimestamp: 1_787_913_600_000,
      actor: { id: "operator-1", type: "user" },
      data: {
        id: "comment-data-1",
        issueId: "issue-1",
        body: "@altitude keep the issue history additive",
      },
    }, { binding, mentionTokens: ["@altitude"] });

    expect(input).toMatchObject({
      actorId: "operator-1",
      text: "@altitude keep the issue history additive",
      source: { surface: "issue_comment", externalId: "comment-data-1" },
      route: { reason: "mention", blocking: false },
    });
  });

  test("normalizes native Agent Session answers to the same semantic comment key", () => {
    const correlation = {
      provider: "linear" as const,
      taskId: "task-1",
      attentionId: "decision-1",
      attentionClass: "decision" as const,
      issueId: "issue-1",
      issueCommentId: "decision-comment",
      agentSessionId: "session-1",
    };
    const nativeInput = normalizeLinearWebhook({
      type: "AgentSessionEvent",
      action: "prompted",
      webhookId: "delivery-native",
      agentSession: {
        id: "session-1",
        issueId: "issue-1",
        comment: { id: "reply-1", body: "Use native Iroh first." },
      },
      agentActivity: { body: "Use native Iroh first." },
    }, {
      binding,
      openAttention: [correlation],
    });
    const commentInput = normalizeLinearWebhook({
      type: "AppUserNotification",
      action: "issueNewComment",
      webhookId: "delivery-comment",
      notification: {
        issueId: "issue-1",
        comment: { id: "reply-1", parentId: "decision-comment", body: "Use native Iroh first." },
      },
    }, { binding, openAttention: [correlation] });

    expect(nativeInput?.route.reason).toBe("native_agent_response");
    expect(nativeInput?.attention).toEqual({ id: "decision-1", class: "decision" });
    expect(nativeInput?.dedupeKey).not.toBe(commentInput?.dedupeKey);
    expect(nativeInput?.idempotencyKey).toBe(commentInput?.idempotencyKey);
    expect(nativeInput?.idempotencyKey).toBe("linear:input:task-1:issue-1:comment:reply-1");
  });

  test("ignores ordinary issue chatter, app-authored echoes, and unrelated issues", () => {
    const ordinary = {
      type: "AppUserNotification",
      action: "issueNewComment",
      notification: {
        issueId: "issue-1",
        actorId: "operator-1",
        comment: { id: "comment-1", body: "ordinary context" },
      },
    };
    expect(normalizeLinearWebhook(ordinary, { binding })).toBeNull();
    expect(normalizeLinearWebhook({
      ...ordinary,
      action: "issueCommentMention",
      notification: { ...ordinary.notification, actorId: "altitude-app" },
    }, { binding, appUserId: "altitude-app" })).toBeNull();
    expect(normalizeLinearWebhook({
      ...ordinary,
      action: "issueCommentMention",
      notification: { ...ordinary.notification, issueId: "issue-elsewhere" },
    }, { binding })).toBeNull();
  });

  test("derives a stable fallback delivery key when Linear omits a webhook id", () => {
    const payload = {
      type: "AppUserNotification",
      action: "issueCommentMention",
      notification: {
        issueId: "issue-1",
        comment: { id: "comment-1", body: "@altitude take a look" },
      },
    };
    const first = normalizeLinearWebhook(payload, { binding });
    const reordered = normalizeLinearWebhook({
      notification: payload.notification,
      action: payload.action,
      type: payload.type,
    }, { binding });

    expect(first?.dedupeKey).toStartWith("linear:webhook:sha256:");
    expect(first?.dedupeKey).toBe(reordered?.dedupeKey);
  });
});
