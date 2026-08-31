import { afterEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { decodeTaskBootstrapPayload, encodeTaskBootstrapPayload, restoreTaskWorkspace } from "../src/capsule/bootstrap.ts";
import { captureLocalGitCapsule } from "../src/capsule/local-git.ts";
import { captureTaskEnvironment } from "../src/capsule/task-environment.ts";

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

async function captured(reservedCollision = false) {
  const repository = mkdtempSync(join(tmpdir(), "altitude-bootstrap-source-"));
  directories.push(repository);
  run(repository, "init", "--initial-branch=main");
  run(repository, "config", "user.name", "Altitude Test");
  run(repository, "config", "user.email", "altitude@example.test");
  run(repository, "remote", "add", "origin", "git@github.com:gitsquared/fixture.git");
  writeFileSync(join(repository, "tracked.txt"), "base\n");
  run(repository, "add", "tracked.txt");
  run(repository, "-c", "commit.gpgsign=false", "commit", "-m", "base");
  run(repository, "update-ref", "refs/remotes/origin/main", "HEAD");
  run(repository, "symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/main");
  if (reservedCollision) {
    mkdirSync(join(repository, ".altitude"));
    writeFileSync(join(repository, ".altitude", "owned.txt"), "repository content\n");
    run(repository, "add", "-f", ".altitude/owned.txt");
    run(repository, "-c", "commit.gpgsign=false", "commit", "-m", "occupy reserved path");
  }
  writeFileSync(join(repository, "tracked.txt"), "staged\n");
  run(repository, "add", "tracked.txt");
  writeFileSync(join(repository, "tracked.txt"), "unstaged\n");
  writeFileSync(join(repository, "untracked.txt"), "portable\n");
  await captureTaskEnvironment(repository);
  writeFileSync(join(repository, ".altitude", "environment", "bin", "capsule-tool"), "#!/bin/sh\nexit 0\n");
  const environmentMembers = await captureTaskEnvironment(repository);
  return await captureLocalGitCapsule({
    capsuleId: "capsule-1",
    taskId: "task-1",
    repositoryPath: repository,
    revision: 1,
    ownership: { epoch: 1, leaseId: "lease-1", ownerNodeId: "local", status: "active" },
    checkpointedAt: "2026-08-29T10:00:00.000Z",
    additionalMembers: environmentMembers,
  });
}

describe("task capsule bootstrap", () => {
  test("frames, verifies, and restores a real disposable workspace", async () => {
    const source = await captured();
    const encoded = encodeTaskBootstrapPayload({
      checkpoint: source.checkpoint,
      repositorySeedBundle: source.repositorySeedBundle,
    });
    const decoded = decodeTaskBootstrapPayload(encoded);
    const workspace = mkdtempSync(join(tmpdir(), "altitude-bootstrap-workspace-"));
    directories.push(workspace);
    expect(await restoreTaskWorkspace(workspace, decoded)).toEqual({ reused: false });
    expect(await Bun.file(join(workspace, "tracked.txt")).text()).toBe("unstaged\n");
    expect(await Bun.file(join(workspace, "untracked.txt")).text()).toBe("portable\n");
    expect(await Bun.file(join(workspace, ".altitude", "environment", "bin", "capsule-tool")).text())
      .toBe("#!/bin/sh\nexit 0\n");
    expect(run(workspace, "diff", "--cached", "--name-only")).toBe("tracked.txt");
    expect(run(workspace, "diff", "--name-only")).toBe("tracked.txt");
    expect(run(workspace, "remote", "get-url", "origin")).toBe("git@github.com:gitsquared/fixture.git");
    expect(await restoreTaskWorkspace(workspace, decoded)).toEqual({ reused: true });
  });

  test("rejects corruption and refuses to replace another workspace", async () => {
    const source = await captured();
    const encoded = encodeTaskBootstrapPayload({
      checkpoint: source.checkpoint,
      repositorySeedBundle: source.repositorySeedBundle,
    });
    encoded[encoded.length - 1] = encoded[encoded.length - 1]! ^ 1;
    expect(() => decodeTaskBootstrapPayload(encoded)).toThrow(/mismatch/ui);

    const workspace = mkdtempSync(join(tmpdir(), "altitude-bootstrap-nonempty-"));
    directories.push(workspace);
    writeFileSync(join(workspace, "mine.txt"), "keep\n");
    const clean = encodeTaskBootstrapPayload({
      checkpoint: source.checkpoint,
      repositorySeedBundle: source.repositorySeedBundle,
    });
    expect(restoreTaskWorkspace(workspace, decodeTaskBootstrapPayload(clean))).rejects.toThrow("must be empty");
    expect(await Bun.file(join(workspace, "mine.txt")).text()).toBe("keep\n");
  });

  test("refuses repository content at Altitude's reserved workbench path", async () => {
    const source = await captured(true);
    const workspace = mkdtempSync(join(tmpdir(), "altitude-bootstrap-reserved-"));
    directories.push(workspace);
    await expect(restoreTaskWorkspace(workspace, decodeTaskBootstrapPayload(encodeTaskBootstrapPayload({
      checkpoint: source.checkpoint,
      repositorySeedBundle: source.repositorySeedBundle,
    })))).rejects.toThrow("reserved .altitude workbench path");
    expect(readdirSync(workspace)).toEqual([]);
  });
});
