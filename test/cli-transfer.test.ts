import { afterEach, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { EndpointAddr, EndpointTicket, SecretKey } from "@number0/iroh";

import {
  FilesystemCapsuleCheckpointStore,
  FilesystemCapsuleTransferReceiver,
  FilesystemRepositorySeedCache,
  createCapsuleCheckpoint,
  portableCapsulePath,
  utf8,
} from "../src/capsule/index.ts";
import {
  claudeTranscriptCapsuleMembers,
  createClaudeTranscriptBundle,
  decodeClaudeTranscriptCapsuleMembers,
} from "../src/agent/transcript.ts";
import { runCli, type CliOptions } from "../src/cli/main.ts";
import {
  defaultLocalConfig,
  writeLocalConfig,
} from "../src/config/local.ts";
import { resolveAltitudePaths } from "../src/config/paths.ts";
import {
  FilesystemNodeRegistry,
  registeredNode,
} from "../src/node/registry.ts";
import {
  ALTITUDE_NODE_TICKET_PROTOCOL,
  encodeAltitudeNodeTicket,
  type AltitudeNodeClient,
} from "../src/node/iroh.ts";
import {
  PORTABLE_TASK_STATE_MEMBER,
  decodePortableTaskState,
  portableTaskStateMember,
} from "../src/run/task-state.ts";
import { activateTransferredTask } from "../src/run/transfer-activation.ts";
import { TaskStore } from "../src/storage/task-store.ts";
import { TaskAgentSessionStore } from "../src/storage/task-agent-session-store.ts";

const roots: string[] = [];

afterEach(() => {
  for (const root of roots.splice(0)) {
    rmSync(root, { recursive: true, force: true });
  }
});

function output(): {
  value: string;
  write(chunk: string | Uint8Array): boolean;
} {
  return {
    value: "",
    write(this: { value: string }, chunk: string | Uint8Array) {
      this.value += typeof chunk === "string"
        ? chunk
        : new TextDecoder().decode(chunk);
      return true;
    },
  };
}

test("task send moves a paused capsule through the native node boundary", async () => {
  const sourceRoot = mkdtempSync(join(tmpdir(), "altitude-cli-send-source-"));
  const targetRoot = mkdtempSync(join(tmpdir(), "altitude-cli-send-target-"));
  roots.push(sourceRoot, targetRoot);
  const paths = resolveAltitudePaths({ ALTITUDE_HOME: sourceRoot });
  const config = {
    ...defaultLocalConfig(),
    node: { ...defaultLocalConfig().node, id: "macbook" },
  };
  await writeLocalConfig(paths.config, config);
  const endpointId = SecretKey.generate().public();
  await new FilesystemNodeRegistry(paths.nodes).put(registeredNode({
    nodeId: "straylight",
    ticket: encodeAltitudeNodeTicket({
      schema: ALTITUDE_NODE_TICKET_PROTOCOL,
      nodeId: "straylight",
      endpointId: endpointId.toString(),
      endpointTicket: EndpointTicket.fromAddr(new EndpointAddr(endpointId)).toString(),
      pairingToken: "a".repeat(43),
    }),
  }));

  const store = new TaskStore(paths.database, { clock: () => 1_000 });
  store.append({
    taskId: "task-send",
    type: "task.created",
    payload: {
      title: "Move me",
      intent: "Continue on the remote node",
      intentLevel: "principles",
    },
    actor: { kind: "operator" },
  });
  for (const to of ["queued", "provisioning", "running", "paused"] as const) {
    store.append({
      taskId: "task-send",
      type: "task.state_changed",
      payload: { to },
      actor: { kind: "system", component: "fixture" },
    });
  }
  const sessions = new TaskAgentSessionStore(
    store.database,
    () => 1_000,
    () => "01900000-0000-7000-8000-000000000001",
  );
  const session = sessions.ensure("task-send", "claude-personal");
  sessions.bindMcp({
    taskId: "task-send",
    grantId: "grant-source",
    controlDirectory: "/identity/control/tasks/task-send",
    configPath: "/identity/control/tasks/task-send/mcp.json",
  });
  sessions.markRunStarted("task-send");
  sessions.recordOutcome("task-send", "yielded");
  const transcript = createClaudeTranscriptBundle({
    taskId: "task-send",
    sessionId: session.sessionId,
    projectDirectoryName: session.projectDirectoryName,
    files: [{
      path: session.sessionId + ".jsonl",
      bytes: utf8("portable context\n"),
    }],
  });
  const initial = createCapsuleCheckpoint({
    capsuleId: "task-send",
    taskId: "task-send",
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
      portableTaskStateMember(store, "task-send", sessions),
      ...claudeTranscriptCapsuleMembers(transcript),
    ],
  });
  const sourceCheckpoints = new FilesystemCapsuleCheckpointStore(
    paths.capsules,
  );
  const initialReference = await sourceCheckpoints.publish(initial);
  await sourceCheckpoints.activate(initialReference);
  await new FilesystemRepositorySeedCache(
    join(paths.cache, "repository-seeds"),
  ).put(initialReference.digest, utf8("repository seed"));
  store.close();

  const targetCheckpoints = new FilesystemCapsuleCheckpointStore(
    join(targetRoot, "capsules"),
  );
  const receiver = new FilesystemCapsuleTransferReceiver(
    targetCheckpoints,
    new FilesystemRepositorySeedCache(join(targetRoot, "seeds")),
    "straylight",
  );
  using targetStore = new TaskStore(join(targetRoot, "altitude.db"), {
    clock: () => Date.parse("2026-08-29T10:05:00.000Z"),
  });
  const targetSessions = new TaskAgentSessionStore(targetStore.database);
  const operations: string[] = [];
  let closed = false;
  const client: AltitudeNodeClient = {
    async health() {
      operations.push("health");
      return { nodeId: "straylight", version: "0.0.0" };
    },
    async prepare(payload) {
      operations.push("prepare");
      return await receiver.prepare(payload);
    },
    async activate(input) {
      operations.push("activate");
      const receipt = await receiver.activate(input);
      const checkpoint = await targetCheckpoints.read(receipt.checkpoint);
      const state = checkpoint.members.find(
        (member) => member.path === PORTABLE_TASK_STATE_MEMBER,
      );
      if (state === undefined) throw new Error("missing portable task state");
      activateTransferredTask({
        store: targetStore,
        sessions: targetSessions,
        portableState: decodePortableTaskState(state.bytes),
        targetIdentityId: "claude-remote",
        targetNodeId: "straylight",
        transferId: receipt.transferId,
        checkpoint: receipt.checkpoint,
        activatedAt: Date.parse("2026-08-29T10:05:00.000Z"),
      });
      return receipt;
    },
    async close() {
      closed = true;
    },
  };
  const stdout = output();
  const stderr = output();
  const options: CliOptions = {
    env: { ALTITUDE_HOME: sourceRoot },
    stdout,
    stderr,
    createNodeClient: async () => client,
    clock: () => Date.parse("2026-08-29T10:05:00.000Z"),
  };
  expect(await runCli([
    "task",
    "send",
    "task-send",
    "--node",
    "straylight",
  ], options)).toBe(0);
  expect(stderr.value).toBe("");
  expect(stdout.value).toContain("Moved task task-send to straylight");
  expect(operations).toEqual(["prepare", "activate"]);
  expect(closed).toBeTrue();

  using sourceAfter = new TaskStore(paths.database);
  expect(sourceAfter.getTask("task-send")?.status).toBe("transferring");
  expect((await sourceCheckpoints.readActive("task-send")).manifest.ownership)
    .toMatchObject({ ownerNodeId: "straylight", epoch: 2 });
  expect((await targetCheckpoints.readActive("task-send")).manifest.ownership)
    .toMatchObject({ ownerNodeId: "straylight", epoch: 2 });
  const target = await targetCheckpoints.readActive("task-send");
  expect(decodeClaudeTranscriptCapsuleMembers(target.members)).toEqual(
    transcript,
  );
  const portable = target.members.find(
    (member) => member.path === PORTABLE_TASK_STATE_MEMBER,
  );
  expect(portable && new TextDecoder().decode(portable.bytes)).toContain(
    session.sessionId,
  );
  expect(portable && new TextDecoder().decode(portable.bytes)).not.toContain(
    "claude-personal",
  );
  expect(targetStore.getTask("task-send")?.status).toBe("queued");
  expect(targetSessions.get("task-send")?.identityId).toBe("claude-remote");
  expect(targetStore.listScheduledActions("scheduled")).toEqual([
    expect.objectContaining({
      kind: "task.run",
      dedupeKey: expect.stringContaining("transfer:"),
    }),
  ]);
});
