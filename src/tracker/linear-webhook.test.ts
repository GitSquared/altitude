import { afterEach, describe, expect, test } from "bun:test";
import { createHmac } from "node:crypto";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { TaskStore } from "../storage/task-store.ts";
import {
  createLinearWebhookHandler,
  LinearTrackerStateStore,
} from "./index.ts";

const directories: string[] = [];
const now = 1_787_913_600_000;
const secret = "linear-test-webhook-secret";

afterEach(() => {
  for (const directory of directories.splice(0)) {
    rmSync(directory, { recursive: true, force: true });
  }
});

function fixture(): { path: string; store: TaskStore; state: LinearTrackerStateStore } {
  const directory = mkdtempSync(join(tmpdir(), "altitude-linear-webhook-test-"));
  directories.push(directory);
  const path = join(directory, "altitude.sqlite");
  const store = new TaskStore(path, { clock: () => now });
  store.append({
    taskId: "task-1",
    type: "task.created",
    payload: {
      title: "Build the adapter",
      intent: "Route issue intent into the capsule",
      intentLevel: "principles",
    },
    actor: { kind: "operator" },
  });
  return {
    path,
    store,
    state: new LinearTrackerStateStore(store.database, () => now),
  };
}

function signed(payload: unknown, signingSecret = secret): Request {
  const body = JSON.stringify(payload);
  const signature = createHmac("sha256", signingSecret).update(body).digest("hex");
  return new Request("http://localhost/v1/tracker/linear/webhook", {
    method: "POST",
    headers: {
      "content-type": "application/json",
      "linear-signature": signature,
    },
    body,
  });
}

function mentionPayload(overrides: Record<string, unknown> = {}): unknown {
  return {
    type: "AppUserNotification",
    action: "issueCommentMention",
    webhookId: "delivery-1",
    webhookTimestamp: now,
    notification: {
      issueId: "issue-1",
      actorId: "operator-1",
      comment: {
        id: "comment-1",
        body: "@altitude keep the adapter capsule-led",
      },
    },
    ...overrides,
  };
}

describe("Linear tracker state", () => {
  test("persists only node-local lookup metadata across a reopen", () => {
    const { path, store, state } = fixture();
    state.bind({
      taskId: "task-1",
      issueId: "issue-1",
      issueIdentifier: "ALT-1",
      agentSessionId: "session-1",
      appUserId: "app-user-1",
      mentionTokens: ["@altitude", "@alt"],
    });
    state.recordCorrelation({
      provider: "linear",
      taskId: "task-1",
      attentionId: "update-1",
      attentionClass: "update",
      issueId: "issue-1",
      issueCommentId: "comment-update-1",
      agentSessionId: "session-1",
    });
    store.close();

    const reopened = new TaskStore(path, { clock: () => now + 1 });
    const reopenedState = new LinearTrackerStateStore(reopened.database, () => now + 1);
    expect(reopenedState.getByIssueId("issue-1")).toEqual({
      taskId: "task-1",
      issueId: "issue-1",
      issueIdentifier: "ALT-1",
      agentSessionId: "session-1",
      appUserId: "app-user-1",
      mentionTokens: ["@altitude", "@alt"],
    });
    expect(reopenedState.listCorrelations("task-1")).toHaveLength(1);
    reopened.close();
    expect(readFileSync(path).includes(Buffer.from(secret))).toBeFalse();
  });
});

describe("Linear webhook boundary", () => {
  test("authenticates, records, queues, and semantically deduplicates a mention", async () => {
    const { store, state } = fixture();
    state.bind({ taskId: "task-1", issueId: "issue-1" });
    const handler = createLinearWebhookHandler({
      secret,
      store,
      state,
      now: () => now,
    });

    const first = await handler(signed(mentionPayload()));
    const duplicateDelivery = await handler(signed(mentionPayload({ webhookId: "delivery-2" })));

    expect(first.status).toBe(200);
    expect(await first.json()).toEqual({ accepted: true, routed: true, duplicate: false });
    expect(await duplicateDelivery.json()).toEqual({ accepted: true, routed: true, duplicate: true });
    expect(store.getEvents("task-1").at(-1)).toMatchObject({
      type: "operator.input_received",
      payload: {
        text: "@altitude keep the adapter capsule-led",
        routeReason: "mention",
      },
    });
    expect(store.listOutbox()).toHaveLength(1);
    expect(store.listOutbox()[0]).toMatchObject({
      topic: "tracker.linear.input.route",
      state: "pending",
    });
    store.close();
  });

  test("resolves a correlated Decision once and queues its continuation", async () => {
    const { store, state } = fixture();
    for (const to of ["queued", "provisioning", "running"] as const) {
      store.append({
        taskId: "task-1",
        type: "task.state_changed",
        payload: { to },
        actor: { kind: "system", component: "test" },
      });
    }
    store.append({
      taskId: "task-1",
      type: "attention.raised",
      payload: {
        attention: {
          id: "decision-1",
          class: "decision",
          title: "Pick the first transport",
          summary: "The task needs a transport choice.",
          question: "Which transport should Altitude use first?",
        },
      },
      actor: { kind: "agent", adapter: "claude" },
    });
    state.bind({
      taskId: "task-1",
      issueId: "issue-1",
      agentSessionId: "session-1",
    });
    state.recordCorrelation({
      provider: "linear",
      taskId: "task-1",
      attentionId: "decision-1",
      attentionClass: "decision",
      issueId: "issue-1",
      issueCommentId: "decision-comment",
      agentSessionId: "session-1",
    });
    const handler = createLinearWebhookHandler({ secret, store, state, now: () => now });
    const payload = {
      type: "AppUserNotification",
      action: "issueNewComment",
      webhookId: "decision-delivery",
      webhookTimestamp: now,
      notification: {
        issueId: "issue-1",
        actorId: "operator-1",
        comment: {
          id: "decision-reply",
          parentId: "decision-comment",
          body: "Use native Iroh first.",
        },
      },
    };

    expect((await handler(signed(payload))).status).toBe(200);
    expect(await (await handler(signed(payload))).json()).toEqual({
      accepted: true,
      routed: false,
    });
    expect(store.getTask("task-1")).toMatchObject({ status: "running", activeAttention: null });
    expect(store.getEvents("task-1").at(-1)).toMatchObject({
      type: "attention.resolved",
      payload: {
        attentionId: "decision-1",
        resolution: { kind: "answered", answer: "Use native Iroh first." },
      },
    });
    expect(store.listOutbox()).toHaveLength(1);
    store.close();
  });

  test("rejects invalid signatures, stale timestamps, and oversized bodies", async () => {
    const { store, state } = fixture();
    state.bind({ taskId: "task-1", issueId: "issue-1" });
    const handler = createLinearWebhookHandler({ secret, store, state, now: () => now });

    expect((await handler(signed(mentionPayload(), "wrong-secret"))).status).toBe(401);
    expect((await handler(signed(mentionPayload({
      webhookTimestamp: now - 60_001,
    })))).status).toBe(401);
    const oversized = "x".repeat(256 * 1024 + 1);
    expect((await handler(new Request("http://localhost/v1/tracker/linear/webhook", {
      method: "POST",
      headers: { "linear-signature": "0".repeat(64) },
      body: oversized,
    }))).status).toBe(413);
    expect(store.getEvents("task-1")).toHaveLength(1);
    expect(store.listOutbox()).toHaveLength(0);
    store.close();
  });

  test("acknowledges authenticated events for unbound issues without retaining them", async () => {
    const { store, state } = fixture();
    const handler = createLinearWebhookHandler({ secret, store, state, now: () => now });
    const response = await handler(signed(mentionPayload()));
    expect(response.status).toBe(200);
    expect(await response.json()).toEqual({ accepted: true, routed: false });
    expect(store.getEvents("task-1")).toHaveLength(1);
    store.close();
  });
});
