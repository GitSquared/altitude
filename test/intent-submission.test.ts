import { describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { RedmarkerIntentTool } from "../src/intent/redmarker.ts";
import {
  createIntentSubmissionHandler,
  IntentSubmissionStore,
} from "../src/intent/submission.ts";
import { TaskStore } from "../src/storage/task-store.ts";

const HEAD_SHA = "a".repeat(40);
const CHECKPOINT_DIGEST = `sha256:${"b".repeat(64)}`;
const review = {
  schemaVersion: "redmarker-review/v1",
  project: { name: "Altitude" },
  page: {
    pathname: "/tasks",
    title: "Tasks",
    viewport: { width: 1440, height: 900 },
  },
  marks: [{ id: "mark-1" }],
} as const;

function taskStore(taskId = "task-1", path = ":memory:"): TaskStore {
  const store = new TaskStore(path);
  store.append({
    taskId,
    type: "task.created",
    payload: { title: "Visual review", intent: "Review the task", intentLevel: "precise_instructions" },
    actor: { kind: "operator" },
  });
  return store;
}

function submissions(store: TaskStore, now = Date.parse("2026-08-28T12:00:00Z")): IntentSubmissionStore {
  return new IntentSubmissionStore(store, { clock: () => now });
}

describe("intent-tool submission", () => {
  test("accepts a Redmarker review exactly once and binds it durably to a head", () => {
    using store = taskStore();
    const firstProcess = submissions(store);
    const grant = firstProcess.issue({
      taskId: "task-1",
      capsuleId: "capsule-1",
      baseUrl: "http://127.0.0.1:43123/tasks",
      headSha: HEAD_SHA,
      checkpointDigest: CHECKPOINT_DIGEST,
      checkpointRevision: 3,
    });

    const restartedProcess = submissions(store);
    const artifact = restartedProcess.consume(grant.sessionId, grant.token, review);
    expect(artifact.schema).toBe("altitude.intent-artifact/v1");
    expect(artifact.base.headSha).toBe(HEAD_SHA);
    expect(artifact.base).toMatchObject({ checkpointDigest: CHECKPOINT_DIGEST, checkpointRevision: 3 });
    expect(artifact.payloadSchema).toBe("redmarker-review/v1");
    expect(store.listIntentArtifacts("task-1")).toEqual([artifact]);
    expect(store.getIntentArtifact(artifact.artifactId)).toEqual(artifact);
    expect(() => restartedProcess.consume(grant.sessionId, grant.token, review)).toThrow(
      "submission_not_found",
    );
  });

  test("accepts an outstanding grant after the database is closed and reopened", () => {
    const root = mkdtempSync(join(tmpdir(), "altitude-intent-restart-"));
    const path = join(root, "altitude.db");
    try {
      let store = taskStore("task-1", path);
      const beforeRestart = submissions(store);
      const grant = beforeRestart.issue({
        taskId: "task-1",
        capsuleId: "capsule-1",
        baseUrl: "http://127.0.0.1:3000/",
        headSha: HEAD_SHA,
        checkpointDigest: CHECKPOINT_DIGEST,
        checkpointRevision: 3,
      });
      store.close();

      store = new TaskStore(path);
      const artifact = submissions(store).consume(grant.sessionId, grant.token, review);
      expect(store.listIntentArtifacts("task-1")).toEqual([artifact]);
      store.close();
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("does not consume the one-time grant when artifact persistence rolls back", () => {
    using store = taskStore();
    const successful = new IntentSubmissionStore(store, {
      clock: () => 1_000,
      createArtifactId: () => "artifact-collision",
    });
    const first = successful.issue({
      taskId: "task-1",
      capsuleId: "capsule-1",
      baseUrl: "http://127.0.0.1:3000/",
      headSha: HEAD_SHA,
      checkpointDigest: CHECKPOINT_DIGEST,
      checkpointRevision: 3,
    });
    successful.consume(first.sessionId, first.token, review);

    const second = successful.issue({
      taskId: "task-1",
      capsuleId: "capsule-1",
      baseUrl: "http://127.0.0.1:3000/",
      headSha: HEAD_SHA,
      checkpointDigest: CHECKPOINT_DIGEST,
      checkpointRevision: 3,
    });
    expect(() => successful.consume(second.sessionId, second.token, review)).toThrow();

    const recovered = new IntentSubmissionStore(store, {
      clock: () => 1_000,
      createArtifactId: () => "artifact-after-retry",
    });
    expect(recovered.consume(second.sessionId, second.token, review).artifactId).toBe(
      "artifact-after-retry",
    );
  });

  test("does not expose its token in process arguments and uses attach mode", () => {
    const launch = new RedmarkerIntentTool().launch({
      taskId: "task-1",
      capsuleId: "capsule-1",
      targetUrl: "http://127.0.0.1:3000/",
      submissionUrl: "http://127.0.0.1:43100/v1/intent-tools/redmarker/session",
      submissionToken: "private-one-time-token",
    });

    expect(launch.args).toEqual(["--url", "http://127.0.0.1:3000/"]);
    expect(launch.args.join(" ")).not.toContain("private-one-time-token");
    expect(launch.environment.REDMARKER_SUBMIT_TOKEN).toBe("private-one-time-token");
  });

  test("keeps submission credentials on a loopback endpoint", () => {
    const tool = new RedmarkerIntentTool();
    expect(() => tool.launch({
      taskId: "task-1",
      capsuleId: "capsule-1",
      targetUrl: "https://preview.example/",
      submissionUrl: "http://127.0.0.1:43100/v1/intent-tools/redmarker/session",
      submissionToken: "private-one-time-token",
    })).toThrow("loopback");
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

  test("maps a valid HTTP submission directly into durable storage", async () => {
    using store = taskStore();
    const durable = submissions(store);
    const grant = durable.issue({
      taskId: "task-1",
      capsuleId: "capsule-1",
      baseUrl: "http://127.0.0.1:3000/",
      headSha: HEAD_SHA,
      checkpointDigest: CHECKPOINT_DIGEST,
      checkpointRevision: 3,
    });
    const handler = createIntentSubmissionHandler(durable);
    const response = await handler(new Request(
      `http://localhost/v1/intent-tools/redmarker/${grant.sessionId}`,
      {
        method: "POST",
        headers: {
          authorization: `Bearer ${grant.token}`,
          "content-type": "application/json",
        },
        body: JSON.stringify(review),
      },
    ));

    expect(response.status).toBe(202);
    expect(response.headers.get("cache-control")).toBe("no-store");
    expect(store.listIntentArtifacts("task-1")).toHaveLength(1);
  });

  test("rejects a streamed oversized body before parsing it", async () => {
    using store = taskStore();
    const durable = submissions(store);
    const grant = durable.issue({
      taskId: "task-1",
      capsuleId: "capsule-1",
      baseUrl: "http://127.0.0.1:3000/",
      headSha: HEAD_SHA,
      checkpointDigest: CHECKPOINT_DIGEST,
      checkpointRevision: 3,
    });
    const body = new ReadableStream<Uint8Array>({
      start(controller) {
        controller.enqueue(new Uint8Array(2 * 1024 * 1024));
        controller.enqueue(new Uint8Array([1]));
        controller.close();
      },
    });
    const handler = createIntentSubmissionHandler(durable);
    const response = await handler(new Request(
      `http://localhost/v1/intent-tools/redmarker/${grant.sessionId}`,
      {
        method: "POST",
        headers: {
          authorization: `Bearer ${grant.token}`,
          "content-type": "application/json",
        },
        body,
        duplex: "half",
      },
    ));

    expect(response.status).toBe(413);
    expect(store.listIntentArtifacts("task-1")).toHaveLength(0);
  });
});
