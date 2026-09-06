import { afterEach, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import type {
  AgentAdapter,
  AgentRunRequest,
} from "../src/agent/protocol.ts";
import {
  claudeTranscriptCapsuleMembers,
  createClaudeTranscriptBundle,
} from "../src/agent/transcript.ts";
import {
  FilesystemCapsuleCheckpointStore,
  FilesystemCapsuleTransferReceiver,
  FilesystemRepositorySeedCache,
  InProcessCapsuleTransferTransport,
  LocalToRemoteCapsuleTransfer,
  createCapsuleCheckpoint,
  portableCapsulePath,
  utf8,
} from "../src/capsule/index.ts";
import { restoreIdentityTranscript } from "../src/identity/transcript.ts";
import { portableTaskStateMember, decodePortableTaskState } from "../src/run/task-state.ts";
import { TaskRunCoordinator } from "../src/run/task-runner.ts";
import { TaskAgentSessionStore } from "../src/storage/task-agent-session-store.ts";
import { TaskStore } from "../src/storage/task-store.ts";
import { TaskMcpGrantStore } from "../src/tools/grants.ts";

const SESSION_ID = "01900000-0000-7000-8000-000000000001";
const roots: string[] = [];

afterEach(() => {
  for (const root of roots.splice(0)) {
    rmSync(root, { recursive: true, force: true });
  }
});

function root(label: string): string {
  const value = mkdtempSync(join(tmpdir(), "altitude-agent-handoff-" + label + "-"));
  roots.push(value);
  return value;
}

function transition(store: TaskStore, to: "queued" | "provisioning" | "running" | "paused" | "transferring"): void {
  store.append({
    taskId: "task-1",
    type: "task.state_changed",
    payload: { to },
    actor: { kind: "system", component: "fixture" },
  });
}

test("a transferred capsule resumes the exact Claude session in another node-local identity", async () => {
  const sourceRoot = root("source");
  using sourceTasks = new TaskStore(join(sourceRoot, "altitude.db"), {
    clock: () => 1_000,
  });
  sourceTasks.append({
    taskId: "task-1",
    type: "task.created",
    payload: {
      title: "Continue remotely",
      intent: "Keep the same delegated train of thought",
      intentLevel: "principles",
    },
    actor: { kind: "operator" },
  });
  for (const status of ["queued", "provisioning", "running", "paused"] as const) {
    transition(sourceTasks, status);
  }
  const sourceSessions = new TaskAgentSessionStore(
    sourceTasks.database,
    () => 1_000,
    () => SESSION_ID,
  );
  const sourceSession = sourceSessions.ensure("task-1", "source-identity");
  sourceSessions.bindMcp({
    taskId: "task-1",
    grantId: "source-grant",
    controlDirectory: "/identity/control/tasks/source",
    configPath: "/identity/control/tasks/source/mcp.json",
  });
  sourceSessions.markRunStarted("task-1");
  sourceSessions.recordOutcome("task-1", "yielded");
  const transcript = createClaudeTranscriptBundle({
    taskId: "task-1",
    sessionId: SESSION_ID,
    projectDirectoryName: sourceSession.projectDirectoryName,
    files: [{ path: SESSION_ID + ".jsonl", bytes: utf8("first node context\n") }],
  });

  const sourceCheckpoints = new FilesystemCapsuleCheckpointStore(
    join(sourceRoot, "capsules"),
  );
  const sourceSeeds = new FilesystemRepositorySeedCache(join(sourceRoot, "seeds"));
  const initial = createCapsuleCheckpoint({
    capsuleId: "task-1",
    taskId: "task-1",
    revision: 1,
    checkpointedAt: "2026-08-29T10:00:00.000Z",
    ownership: {
      epoch: 1,
      leaseId: "lease-macbook",
      ownerNodeId: "macbook",
      status: "active",
    },
    repository: {
      kind: "git",
      remoteUrl: "git@github.com:gitsquared/altitude.git",
      defaultBranch: "main",
      branch: "gaby/v1-foundation",
      baseCommit: "a".repeat(40),
      headCommit: "b".repeat(40),
      bundleMember: null,
      stagedPatchMember: portableCapsulePath("git/staged.patch"),
      unstagedPatchMember: portableCapsulePath("git/unstaged.patch"),
      untracked: [],
    },
    members: [
      { path: portableCapsulePath("git/staged.patch"), bytes: new Uint8Array() },
      { path: portableCapsulePath("git/unstaged.patch"), bytes: new Uint8Array() },
      portableTaskStateMember(sourceTasks, "task-1", sourceSessions),
      ...claudeTranscriptCapsuleMembers(transcript),
    ],
  });
  const initialReference = await sourceCheckpoints.publish(initial);
  await sourceCheckpoints.activate(initialReference);
  await sourceSeeds.put(initialReference.digest, utf8("repository seed"));

  transition(sourceTasks, "transferring");
  const targetRoot = root("target");
  const targetCheckpoints = new FilesystemCapsuleCheckpointStore(join(targetRoot, "capsules"));
  const targetSeeds = new FilesystemRepositorySeedCache(join(targetRoot, "seeds"));
  const receiver = new FilesystemCapsuleTransferReceiver(
    targetCheckpoints,
    targetSeeds,
    "straylight",
  );
  const transfer = new LocalToRemoteCapsuleTransfer(
    sourceCheckpoints,
    sourceSeeds,
    "macbook",
    new InProcessCapsuleTransferTransport(receiver),
    {
      now: () => Date.parse("2026-08-29T10:05:00.000Z"),
      createId: (() => {
        const ids = ["transfer-1", "lease-straylight"];
        return () => ids.shift()!;
      })(),
    },
  );
  const prepared = await transfer.prepare({
    taskId: "task-1",
    targetNodeId: "straylight",
    additionalMembers: [portableTaskStateMember(
      sourceTasks,
      "task-1",
      sourceSessions,
    )],
  });
  await transfer.commit(prepared);

  const targetCheckpoint = await targetCheckpoints.readActive("task-1");
  const taskStateMember = targetCheckpoint.members.find(
    (member) => member.path === "state/task.json",
  )!;
  const portable = decodePortableTaskState(taskStateMember.bytes);
  using targetTasks = new TaskStore(join(targetRoot, "altitude.db"), {
    clock: () => 2_000,
  });
  targetTasks.importPortableTaskState(portable);
  const targetSessions = new TaskAgentSessionStore(targetTasks.database, () => 2_000);
  const targetSession = targetSessions.importPortable(
    portable.agentSession!,
    "target-identity",
  );
  targetTasks.append({
    taskId: "task-1",
    type: "task.state_changed",
    payload: { to: "queued", reason: "Activated on straylight" },
    actor: { kind: "system", component: "capsule-transfer" },
  });

  const targetIdentity = join(targetRoot, "identity");
  mkdirSync(join(targetIdentity, "claude", "projects"), {
    recursive: true,
    mode: 0o700,
  });
  writeFileSync(join(targetIdentity, "claude", ".credentials.json"), "target-secret");
  expect(await restoreIdentityTranscript(targetIdentity, transcript)).toBe("installed");
  expect(readFileSync(join(targetIdentity, "claude", ".credentials.json"), "utf8"))
    .toBe("target-secret");
  expect(targetCheckpoint.members.map((member) => new TextDecoder().decode(member.bytes)).join("\n"))
    .not.toContain("source-identity");
  expect(targetCheckpoint.members.map((member) => member.path).join("\n"))
    .not.toContain("credentials");

  const requests: AgentRunRequest[] = [];
  const agent: Pick<AgentAdapter, "start" | "prepareInteractiveTake"> = {
    async start(request) {
      requests.push(request);
      return {
        taskId: request.taskId,
        sessionId: request.sessionId,
        launch: {
          target: {
            kind: "identity-capsule",
            identityId: "target-identity",
            taskId: request.taskId,
          },
          executable: "claude",
          args: ["--resume", request.sessionId],
          io: "piped",
          purpose: "Resume imported session",
        },
        events: { async *[Symbol.asyncIterator]() {} },
        completion: Promise.resolve({
          outcome: "success",
          exit: { exitCode: 0 },
        }),
        requestYield: async () => false,
        stop: async () => undefined,
      };
    },
    async prepareInteractiveTake() {
      throw new Error("not used");
    },
  };
  const coordinator = new TaskRunCoordinator(
    targetTasks,
    targetSessions,
    new TaskMcpGrantStore(targetTasks.database, () => 2_000),
    {
      writeTaskMcpConfig: async () => ({
        controlDirectory: "/identity/control/tasks/target",
        configPath: "/identity/control/tasks/target/mcp.json",
      }),
    },
    agent,
    () => 2_000,
  );
  const resumed = await coordinator.start({
    taskId: "task-1",
    identityId: "target-identity",
    mcpEndpoint: "http://172.16.0.5:4317/mcp",
  });
  expect(requests[0]).toMatchObject({
    taskId: "task-1",
    sessionId: SESSION_ID,
    projectDirectoryName: targetSession.projectDirectoryName,
    resume: true,
  });
  expect(resumed.session.sessionId).toBe(SESSION_ID);
  expect(targetSessions.get("task-1")?.identityId).toBe("target-identity");
});
