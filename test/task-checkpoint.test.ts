import { afterEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  TASK_CHECKPOINT_PROTOCOL,
  TASK_WORKSPACE_FORMAT,
  activatePendingTaskWorkspaceCheckpoint,
  capsuleManifestDigest,
  captureLocalGitCapsule,
  captureTaskWorkspaceCheckpoint,
  decodeCapsuleWire,
  decodeTaskCheckpointRequest,
  encodeTaskCheckpointRequest,
  portableCapsulePath,
  readActiveTaskWorkspaceMarker,
  readPendingTaskWorkspaceCheckpoint,
  repositorySeedBindingDigest,
  initializeTaskEnvironment,
  restoreTaskWorkspace,
  utf8,
  writeActiveTaskWorkspaceMarker,
} from "../src/capsule/index.ts";

const directories: string[] = [];
afterEach(() => {
  for (const directory of directories.splice(0)) rmSync(directory, { recursive: true, force: true });
});

function git(repository: string, ...args: string[]): void {
  const result = Bun.spawnSync(["git", ...args], { cwd: repository, stdout: "pipe", stderr: "pipe" });
  if (result.exitCode !== 0) throw new Error(new TextDecoder().decode(result.stderr));
}

describe("task workspace checkpoint", () => {
  test("captures a changed workspace into a recoverable pending capsule before activation", async () => {
    const repository = mkdtempSync(join(tmpdir(), "altitude-task-checkpoint-"));
    directories.push(repository);
    git(repository, "init", "--initial-branch=main");
    git(repository, "config", "user.name", "Altitude Test");
    git(repository, "config", "user.email", "altitude@example.test");
    git(repository, "remote", "add", "origin", "git@github.com:gitsquared/fixture.git");
    writeFileSync(join(repository, "README.md"), "# before\n");
    git(repository, "add", "README.md");
    git(repository, "-c", "commit.gpgsign=false", "commit", "-m", "base");
    git(repository, "update-ref", "refs/remotes/origin/main", "HEAD");
    git(repository, "symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/main");

    const ownership = { epoch: 1, leaseId: "lease-1", ownerNodeId: "local", status: "active" as const };
    const initial = await captureLocalGitCapsule({
      capsuleId: "task-1",
      taskId: "task-1",
      repositoryPath: repository,
      revision: 1,
      checkpointedAt: "2026-08-29T08:00:00.000Z",
      ownership,
    });
    const previousCheckpointDigest = capsuleManifestDigest(initial.checkpoint.manifest);
    mkdirSync(join(repository, ".git", "altitude"), { recursive: true });
    await writeActiveTaskWorkspaceMarker(repository, {
      schema: TASK_WORKSPACE_FORMAT,
      capsuleId: "task-1",
      taskId: "task-1",
      revision: 1,
      checkpointDigest: previousCheckpointDigest,
    });
    writeFileSync(join(repository, "README.md"), "# after\n");
    writeFileSync(join(repository, "new.txt"), "portable\n");
    await initializeTaskEnvironment(repository);
    writeFileSync(join(repository, ".altitude", "environment", "bin", "remembered-tool"), "tool bytes\n");

    const request = {
      schema: TASK_CHECKPOINT_PROTOCOL,
      capsuleId: "task-1",
      taskId: "task-1",
      revision: 2,
      checkpointedAt: "2026-08-29T08:05:00.000Z",
      ownership,
      previousCheckpointDigest,
      repositoryBindingDigest: repositorySeedBindingDigest(initial.checkpoint.manifest.repository),
      additionalMembers: [{ path: portableCapsulePath("state/task.json"), bytes: utf8('{"status":"paused"}\n') }],
    } as const;
    const encodedRequest = encodeTaskCheckpointRequest(request);
    expect(decodeTaskCheckpointRequest(encodedRequest)).toEqual(request);

    const checkpoint = await captureTaskWorkspaceCheckpoint(repository, request);
    const digest = capsuleManifestDigest(checkpoint.manifest);
    expect(checkpoint.manifest.revision).toBe(2);
    expect(checkpoint.manifest.members.map((member) => String(member.path))).toContain("state/task.json");
    expect(checkpoint.manifest.members.map((member) => String(member.path))).toContain("workspace/environment.json");
    expect(checkpoint.manifest.repository.untracked.map((entry) => String(entry.repositoryPath))).toEqual(["new.txt"]);

    const pending = await readPendingTaskWorkspaceCheckpoint(repository);
    expect(pending?.previousCheckpointDigest).toBe(previousCheckpointDigest);
    expect(capsuleManifestDigest(decodeCapsuleWire(pending!.wireBytes).manifest)).toBe(digest);
    expect((await readActiveTaskWorkspaceMarker(repository))?.checkpointDigest).toBe(previousCheckpointDigest);

    const restarted = mkdtempSync(join(tmpdir(), "altitude-task-checkpoint-restart-"));
    directories.push(restarted);
    expect(await restoreTaskWorkspace(restarted, {
      checkpoint,
      checkpointDigest: digest,
      repositorySeedBundle: initial.repositorySeedBundle,
    })).toEqual({ reused: false });
    expect(await Bun.file(join(restarted, ".altitude", "environment", "bin", "remembered-tool")).text())
      .toBe("tool bytes\n");
    expect(await Bun.file(join(restarted, "new.txt")).text()).toBe("portable\n");

    await activatePendingTaskWorkspaceCheckpoint(repository, digest);
    expect((await readActiveTaskWorkspaceMarker(repository))?.checkpointDigest).toBe(digest);
    expect(await readPendingTaskWorkspaceCheckpoint(repository)).toBeUndefined();
  });

  test("rejects request member corruption before touching the workspace", () => {
    const bytes = encodeTaskCheckpointRequest({
      schema: TASK_CHECKPOINT_PROTOCOL,
      capsuleId: "task-1",
      taskId: "task-1",
      revision: 2,
      checkpointedAt: "2026-08-29T08:05:00.000Z",
      ownership: { epoch: 1, leaseId: "lease-1", ownerNodeId: "local", status: "active" },
      previousCheckpointDigest: `sha256:${"1".repeat(64)}`,
      repositoryBindingDigest: `sha256:${"2".repeat(64)}`,
      additionalMembers: [{ path: portableCapsulePath("state/task.json"), bytes: utf8("state") }],
    });
    bytes[bytes.length - 1] = bytes[bytes.length - 1]! ^ 0xff;
    expect(() => decodeTaskCheckpointRequest(bytes)).toThrow("failed verification");
  });

  test("never follows a replaced Git directory into task checkpoint metadata", async () => {
    const root = mkdtempSync(join(tmpdir(), "altitude-task-marker-root-"));
    const outside = mkdtempSync(join(tmpdir(), "altitude-task-marker-outside-"));
    directories.push(root, outside);
    symlinkSync(outside, join(root, ".git"));
    await expect(readActiveTaskWorkspaceMarker(root)).rejects.toThrow("Task Git directory is unsafe");
  });
});
