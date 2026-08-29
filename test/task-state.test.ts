import { describe, expect, test } from "bun:test";

import { TaskStore } from "../src/storage/task-store.ts";
import { TaskAgentSessionStore } from "../src/storage/task-agent-session-store.ts";
import { IntentSubmissionStore } from "../src/intent/submission.ts";
import {
  PORTABLE_TASK_STATE_FORMAT,
  decodePortableTaskState,
  portableTaskStateMember,
} from "../src/run/task-state.ts";

describe("portable task state", () => {
  test("carries the replayable task projection and its additive event history", () => {
    using store = new TaskStore(":memory:", { clock: () => 42, createId: () => crypto.randomUUID() });
    store.append({
      taskId: "task-1",
      type: "task.created",
      payload: { title: "Fixture", intent: "Keep intent portable", intentLevel: "principles" },
      actor: { kind: "operator" },
    });
    const submissions = new IntentSubmissionStore(store, { clock: () => 42 });
    const grant = submissions.issue({
      taskId: "task-1",
      capsuleId: "task-1",
      baseUrl: "http://127.0.0.1:3000/",
      headSha: "a".repeat(40),
      checkpointDigest: `sha256:${"b".repeat(64)}`,
      checkpointRevision: 1,
    });
    submissions.consume(grant.sessionId, grant.token, {
      schemaVersion: "redmarker-review/v1",
      project: { name: "Fixture" },
      page: { pathname: "/", title: "Fixture", viewport: { width: 800, height: 600 } },
      marks: [{ id: "mark-1" }],
    });
    store.append({
      taskId: "task-1",
      type: "task.state_changed",
      payload: { to: "queued", reason: "Ready" },
      actor: { kind: "operator" },
    });
    const sessions = new TaskAgentSessionStore(
      store.database,
      () => 42,
      () => "01900000-0000-7000-8000-000000000001",
    );
    sessions.ensure("task-1", "claude-source");
    sessions.bindMcp({
      taskId: "task-1",
      grantId: "grant-source",
      controlDirectory: "/identity/control/tasks/task-1",
      configPath: "/identity/control/tasks/task-1/mcp.json",
    });
    sessions.markRunStarted("task-1");
    sessions.recordOutcome("task-1", "success");

    const member = portableTaskStateMember(store, "task-1", sessions);
    const value = JSON.parse(new TextDecoder().decode(member.bytes)) as {
      schema: string;
      task: { status: string; revision: number };
      events: Array<{ sequence: number }>;
      intentArtifacts: Array<{ tool: string; base: { checkpointRevision: number } }>;
      agentSession: { sessionId: string; runCount: number };
    };
    expect(String(member.path)).toBe("state/task.json");
    expect(value.schema).toBe(PORTABLE_TASK_STATE_FORMAT);
    expect(value.task).toMatchObject({ status: "queued", revision: 2 });
    expect(value.events.map((event) => event.sequence)).toEqual([1, 2]);
    expect(value.intentArtifacts).toMatchObject([
      { tool: "redmarker", base: { checkpointRevision: 1 } },
    ]);
    expect(value.agentSession).toMatchObject({
      sessionId: "01900000-0000-7000-8000-000000000001",
      runCount: 1,
    });
    expect(new TextDecoder().decode(member.bytes)).not.toContain("claude-source");
    expect(new TextDecoder().decode(member.bytes)).not.toContain("grant-source");
    expect(new TextDecoder().decode(member.bytes)).not.toContain("controlDirectory");

    using target = new TaskStore(":memory:");
    const snapshot = decodePortableTaskState(member.bytes);
    const imported = target.importPortableTaskState(snapshot);
    expect(imported).toEqual(store.getTask("task-1")!);
    expect(target.getEvents("task-1")).toEqual(store.getEvents("task-1"));
    expect(target.listIntentArtifacts("task-1")).toEqual(
      store.listIntentArtifacts("task-1"),
    );
    const targetSession = new TaskAgentSessionStore(target.database)
      .importPortable(snapshot.agentSession!, "claude-target");
    expect(targetSession).toMatchObject({
      sessionId: value.agentSession.sessionId,
      identityId: "claude-target",
      runCount: 1,
    });
    expect(targetSession.controlDirectory).toBeUndefined();
    expect(targetSession.grantId).toBeUndefined();
  });

  test("rejects a projection that does not replay from its claimed events", () => {
    const bytes = new TextEncoder().encode(JSON.stringify({
      schema: PORTABLE_TASK_STATE_FORMAT,
      task: { id: "task-1", status: "completed" },
      events: [],
      intentArtifacts: [],
    }));
    expect(() => decodePortableTaskState(bytes)).toThrow("event history is invalid");
  });
});
