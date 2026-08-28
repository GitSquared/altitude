import { describe, expect, test } from "bun:test";

import { RedmarkerIntentTool } from "../src/intent/redmarker.ts";
import { IntentSubmissionRegistry, createIntentSubmissionHandler } from "../src/intent/submission.ts";

const review = {
  schemaVersion: "redmarker-review/v1",
  project: { name: "Altitude" },
  page: {
    pathname: "/tasks",
    title: "Tasks",
    viewport: { width: 1440, height: 900 },
  },
  marks: [{ id: "mark-1" }],
};

describe("intent-tool submission", () => {
  test("accepts a Redmarker review exactly once and binds it to a head", () => {
    const now = new Date("2026-08-28T12:00:00Z");
    const registry = new IntentSubmissionRegistry(() => now);
    const grant = registry.issue({
      taskId: "task-1",
      capsuleId: "capsule-1",
      baseUrl: "http://127.0.0.1:43123/tasks",
      headSha: "abc123",
    });
    const artifact = registry.consume(grant.sessionId, grant.token, review);

    expect(artifact.schema).toBe("altitude.intent-artifact/v1");
    expect(artifact.base.headSha).toBe("abc123");
    expect(artifact.payloadSchema).toBe("redmarker-review/v1");
    expect(() => registry.consume(grant.sessionId, grant.token, review)).toThrow("submission_not_found");
  });

  test("does not expose its token in process arguments", () => {
    const launch = new RedmarkerIntentTool().launch({
      taskId: "task-1",
      capsuleId: "capsule-1",
      targetUrl: "http://127.0.0.1:3000/",
      submissionUrl: "http://127.0.0.1:43100/v1/intent-tools/redmarker/session",
      submissionToken: "private-one-time-token",
    });

    expect(launch.args.join(" ")).not.toContain("private-one-time-token");
    expect(launch.environment.REDMARKER_SUBMIT_TOKEN).toBe("private-one-time-token");
  });

  test("keeps submission credentials on a loopback endpoint", () => {
    const tool = new RedmarkerIntentTool();
    expect(() => tool.launch({
      taskId: "task-1",
      capsuleId: "capsule-1",
      targetUrl: "http://127.0.0.1:3000/",
      submissionUrl: "https://altitude.example/v1/intent-tools/redmarker/session",
      submissionToken: "private-one-time-token",
    })).toThrow("loopback");
    expect(() => tool.launch({
      taskId: "task-1",
      capsuleId: "capsule-1",
      targetUrl: "http://operator:secret@127.0.0.1:3000/",
      submissionUrl: "http://127.0.0.1:43100/v1/intent-tools/redmarker/session",
      submissionToken: "private-one-time-token",
    })).toThrow("must not contain credentials");
  });

  test("maps a valid HTTP submission into a durable artifact callback", async () => {
    const registry = new IntentSubmissionRegistry(() => new Date("2026-08-28T12:00:00Z"));
    const grant = registry.issue({
      taskId: "task-1",
      capsuleId: "capsule-1",
      baseUrl: "http://127.0.0.1:3000/",
      headSha: "abc123",
    });
    const accepted: string[] = [];
    const handler = createIntentSubmissionHandler(registry, async (artifact) => {
      accepted.push(artifact.artifactId);
    });
    const response = await handler(new Request(`http://localhost/v1/intent-tools/redmarker/${grant.sessionId}`, {
      method: "POST",
      headers: { authorization: `Bearer ${grant.token}`, "content-type": "application/json" },
      body: JSON.stringify(review),
    }));

    expect(response.status).toBe(202);
    expect(accepted).toHaveLength(1);
  });

  test("rejects a streamed oversized body before parsing it", async () => {
    const registry = new IntentSubmissionRegistry();
    const grant = registry.issue({
      taskId: "task-1",
      capsuleId: "capsule-1",
      baseUrl: "http://127.0.0.1:3000/",
      headSha: "abc123",
    });
    const body = new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(new Uint8Array(2 * 1024 * 1024));
        controller.enqueue(new Uint8Array([1]));
        controller.close();
      },
    });
    const handler = createIntentSubmissionHandler(registry, async () => undefined);
    const response = await handler(new Request(`http://localhost/v1/intent-tools/redmarker/${grant.sessionId}`, {
      method: "POST",
      headers: { authorization: `Bearer ${grant.token}`, "content-type": "application/json" },
      body,
      duplex: "half",
    }));

    expect(response.status).toBe(413);
  });
});
