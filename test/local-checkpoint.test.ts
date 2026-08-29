import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  FilesystemCapsuleCheckpointStore,
  FilesystemRepositorySeedCache,
  capsuleManifestDigest,
  createCapsuleCheckpoint,
  portableCapsulePath,
  type CapsuleCheckpoint,
} from "../src/capsule/index.ts";
import {
  claudeTranscriptCapsuleMembers,
  createClaudeTranscriptBundle,
  decodeClaudeTranscriptCapsuleMembers,
} from "../src/agent/transcript.ts";
import type { TaskCheckpointCaptureRequest } from "../src/capsule/task-checkpoint.ts";
import { LocalCapsuleCheckpointer } from "../src/run/local-checkpoint.ts";
import { portableTaskStateMember } from "../src/run/task-state.ts";
import { TaskStore } from "../src/storage/task-store.ts";
import { TaskAgentSessionStore } from "../src/storage/task-agent-session-store.ts";

const directories: string[] = [];
afterEach(() => {
  for (const directory of directories.splice(0)) rmSync(directory, { recursive: true, force: true });
});

const empty = new Uint8Array();

function checkpoint(revision = 1, additional: TaskCheckpointCaptureRequest["additionalMembers"] = []): CapsuleCheckpoint {
  return createCapsuleCheckpoint({
    capsuleId: "task-1",
    taskId: "task-1",
    revision,
    checkpointedAt: `2026-08-29T08:0${revision}:00.000Z`,
    ownership: { epoch: 1, leaseId: "lease-1", ownerNodeId: "local", status: "active" },
    repository: {
      kind: "git",
      remoteUrl: "git@github.com:gitsquared/fixture.git",
      defaultBranch: "main",
      branch: "altitude/task-1",
      baseCommit: "a".repeat(40),
      headCommit: "a".repeat(40),
      bundleMember: null,
      stagedPatchMember: portableCapsulePath("git/staged.patch"),
      unstagedPatchMember: portableCapsulePath("git/unstaged.patch"),
      untracked: [],
    },
    members: [
      { path: portableCapsulePath("git/staged.patch"), bytes: empty },
      { path: portableCapsulePath("git/unstaged.patch"), bytes: empty },
      ...additional,
    ],
  });
}

function taskStore(): TaskStore {
  const store = new TaskStore(":memory:", { clock: () => 10, createId: () => crypto.randomUUID() });
  store.append({
    taskId: "task-1",
    type: "task.created",
    payload: { title: "Fixture", intent: "Persist the result", intentLevel: "fully_delegated" },
    actor: { kind: "operator" },
  });
  store.append({
    taskId: "task-1",
    type: "task.state_changed",
    payload: { to: "queued" },
    actor: { kind: "operator" },
  });
  return store;
}

describe("local capsule checkpointer", () => {
  test("publishes source and task history before acknowledging the task workspace", async () => {
    const root = mkdtempSync(join(tmpdir(), "altitude-local-checkpoint-"));
    directories.push(root);
    const checkpoints = new FilesystemCapsuleCheckpointStore(join(root, "checkpoints"));
    const seeds = new FilesystemRepositorySeedCache(join(root, "seeds"));
    const initial = checkpoint();
    const initialReference = await checkpoints.publish(initial);
    await checkpoints.activate(initialReference);
    await seeds.put(initialReference.digest, new Uint8Array([1, 2, 3]));
    using tasks = taskStore();
    const calls: string[] = [];
    const provisioner = {
      capture: async (request: TaskCheckpointCaptureRequest) => {
        calls.push("capture");
        expect(request.previousCheckpointDigest).toBe(initialReference.digest);
        return checkpoint(request.revision, request.additionalMembers);
      },
      pending: async () => undefined,
      activate: async (_taskId: string, digest: string) => { calls.push(`activate:${digest}`); },
    };
    const checkpointer = new LocalCapsuleCheckpointer(checkpoints, seeds, provisioner, tasks, () => Date.parse("2026-08-29T08:02:00.000Z"));
    const published = await checkpointer.capture("task-1");

    expect(published.revision).toBe(2);
    expect(calls).toEqual(["capture", `activate:${published.digest}`]);
    expect((await checkpoints.readActive("task-1")).manifest.members.map((member) => String(member.path))).toContain("state/task.json");
    expect(await seeds.read(published.digest)).toEqual(new Uint8Array([1, 2, 3]));
  });

  test("recovers a pending task-side checkpoint after a host interruption", async () => {
    const root = mkdtempSync(join(tmpdir(), "altitude-local-checkpoint-recovery-"));
    directories.push(root);
    const checkpoints = new FilesystemCapsuleCheckpointStore(join(root, "checkpoints"));
    const seeds = new FilesystemRepositorySeedCache(join(root, "seeds"));
    const initial = checkpoint();
    const initialReference = await checkpoints.publish(initial);
    await checkpoints.activate(initialReference);
    await seeds.put(initialReference.digest, new Uint8Array([4, 5, 6]));
    using tasks = taskStore();
    const state = portableTaskStateMember(tasks, "task-1");
    const pending = checkpoint(2, [state]);
    const activations: string[] = [];
    const provisioner = {
      capture: async () => { throw new Error("unexpected capture"); },
      pending: async () => pending,
      activate: async (_taskId: string, digest: string) => { activations.push(digest); },
    };
    const recovered = await new LocalCapsuleCheckpointer(checkpoints, seeds, provisioner, tasks).recoverPending("task-1");
    expect(recovered?.digest).toBe(capsuleManifestDigest(pending.manifest));
    expect((await checkpoints.readActive("task-1")).manifest.revision).toBe(2);
    expect(activations).toEqual([recovered!.digest]);
    expect(await seeds.read(recovered!.digest)).toEqual(new Uint8Array([4, 5, 6]));
  });

  test("exports the current Claude transcript and preserves it across non-agent checkpoints", async () => {
    const root = mkdtempSync(join(tmpdir(), "altitude-local-transcript-checkpoint-"));
    directories.push(root);
    const checkpoints = new FilesystemCapsuleCheckpointStore(join(root, "checkpoints"));
    const seeds = new FilesystemRepositorySeedCache(join(root, "seeds"));
    const initialReference = await checkpoints.publish(checkpoint());
    await checkpoints.activate(initialReference);
    await seeds.put(initialReference.digest, new Uint8Array([7, 8, 9]));
    using tasks = taskStore();
    const sessions = new TaskAgentSessionStore(
      tasks.database,
      () => 10,
      () => "01900000-0000-7000-8000-000000000001",
    );
    const session = sessions.ensure("task-1", "claude-personal");
    sessions.bindMcp({
      taskId: "task-1",
      grantId: "grant-1",
      controlDirectory: "/identity/control/tasks/task-1",
      configPath: "/identity/control/tasks/task-1/mcp.json",
    });
    sessions.markRunStarted("task-1");
    const transcript = createClaudeTranscriptBundle({
      taskId: "task-1",
      sessionId: session.sessionId,
      projectDirectoryName: session.projectDirectoryName,
      files: [{ path: session.sessionId + ".jsonl", bytes: new TextEncoder().encode("context\n") }],
    });
    const provisioner = {
      capture: async (request: TaskCheckpointCaptureRequest) =>
        checkpoint(request.revision, request.additionalMembers),
      pending: async () => undefined,
      activate: async () => undefined,
    };
    const withAgent = new LocalCapsuleCheckpointer(
      checkpoints,
      seeds,
      provisioner,
      tasks,
      () => Date.parse("2026-08-29T08:02:00.000Z"),
      sessions,
      { export: async () => claudeTranscriptCapsuleMembers(transcript) },
    );
    await withAgent.capture("task-1");
    expect(decodeClaudeTranscriptCapsuleMembers(
      (await checkpoints.readActive("task-1")).members,
    )).toEqual(transcript);

    const withoutAgent = new LocalCapsuleCheckpointer(
      checkpoints,
      seeds,
      provisioner,
      tasks,
      () => Date.parse("2026-08-29T08:03:00.000Z"),
      sessions,
    );
    await withoutAgent.capture("task-1");
    expect(decodeClaudeTranscriptCapsuleMembers(
      (await checkpoints.readActive("task-1")).members,
    )).toEqual(transcript);
  });
});
