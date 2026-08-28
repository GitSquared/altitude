import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { captureLocalGitCapsule } from "../src/capsule/local-git.ts";
import { verifyCapsuleMembers } from "../src/capsule/manifest.ts";

const directories: string[] = [];

afterEach(() => {
  for (const directory of directories.splice(0)) rmSync(directory, { recursive: true, force: true });
});

function run(repository: string, ...args: string[]): string {
  const result = Bun.spawnSync(["git", ...args], {
    cwd: repository,
    env: { ...process.env, GIT_CONFIG_NOSYSTEM: "1" },
    stdout: "pipe",
    stderr: "pipe",
  });
  if (result.exitCode !== 0) throw new Error(new TextDecoder().decode(result.stderr));
  return new TextDecoder().decode(result.stdout).trim();
}

function repository(): string {
  const root = mkdtempSync(join(tmpdir(), "altitude-local-capture-"));
  directories.push(root);
  run(root, "init", "--initial-branch=main");
  run(root, "config", "user.name", "Altitude Test");
  run(root, "config", "user.email", "altitude@example.test");
  run(root, "remote", "add", "origin", "git@github.com:gitsquared/altitude-fixture.git");
  writeFileSync(join(root, "tracked.txt"), "base\n");
  run(root, "add", "tracked.txt");
  run(root, "-c", "commit.gpgsign=false", "commit", "-m", "base");
  run(root, "update-ref", "refs/remotes/origin/main", "HEAD");
  run(root, "symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/main");
  return root;
}

describe("concrete local Git capsule capture", () => {
  test("keeps a seed bundle node-local while making worktree state portable", async () => {
    const root = repository();
    writeFileSync(join(root, "tracked.txt"), "staged\n");
    run(root, "add", "tracked.txt");
    writeFileSync(join(root, "tracked.txt"), "unstaged\n");
    writeFileSync(join(root, "new file.txt"), "untracked\n");

    const captured = await captureLocalGitCapsule({
      capsuleId: "capsule-1",
      taskId: "task-1",
      repositoryPath: root,
      revision: 1,
      ownership: { epoch: 1, leaseId: "lease-1", ownerNodeId: "local", status: "active" },
      checkpointedAt: "2026-08-29T08:00:00.000Z",
    });
    verifyCapsuleMembers(captured.checkpoint.manifest, captured.checkpoint.members);
    expect(captured.repositorySeedBundle.byteLength).toBeGreaterThan(0);
    expect(captured.checkpoint.manifest.repository).toMatchObject({
      remoteUrl: "git@github.com:gitsquared/altitude-fixture.git",
      defaultBranch: "main",
      branch: "main",
    });
    expect(captured.checkpoint.manifest.repository.bundleMember).toBeNull();
    expect(captured.checkpoint.members.map((member) => String(member.path))).toEqual([
      "git/staged.patch",
      "git/unstaged.patch",
      "git/untracked/new file.txt",
    ]);
    expect(new TextDecoder().decode(captured.checkpoint.members[2]!.bytes)).toBe("untracked\n");
    expect(JSON.stringify(captured.checkpoint.manifest)).not.toContain(root);
  });

  test("includes unpublished commits as a portable delta", async () => {
    const root = repository();
    writeFileSync(join(root, "second.txt"), "second\n");
    run(root, "add", "second.txt");
    run(root, "-c", "commit.gpgsign=false", "commit", "-m", "second");
    const captured = await captureLocalGitCapsule({
      capsuleId: "capsule-2",
      taskId: "task-2",
      repositoryPath: root,
      revision: 1,
      ownership: { epoch: 1, leaseId: "lease-2", ownerNodeId: "local", status: "active" },
    });
    expect(String(captured.checkpoint.manifest.repository.bundleMember)).toBe("git/repository.bundle");
    expect(captured.checkpoint.members[0]?.bytes.byteLength).toBeGreaterThan(0);
  });
});
