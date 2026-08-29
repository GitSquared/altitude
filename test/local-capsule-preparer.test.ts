import { afterEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { FilesystemCapsuleCheckpointStore } from "../src/capsule/filesystem.ts";
import { FilesystemRepositorySeedCache } from "../src/capsule/seed-cache.ts";
import { LocalCapsulePreparer } from "../src/run/local-capsule.ts";

const directories: string[] = [];
afterEach(() => {
  for (const directory of directories.splice(0)) rmSync(directory, { recursive: true, force: true });
});

function run(repository: string, ...args: string[]): void {
  const result = Bun.spawnSync(["git", ...args], { cwd: repository, stdout: "pipe", stderr: "pipe" });
  if (result.exitCode !== 0) throw new Error(new TextDecoder().decode(result.stderr));
}

describe("local capsule preparation", () => {
  test("captures once and can rematerialize later from node-local seed cache", async () => {
    const root = mkdtempSync(join(tmpdir(), "altitude-local-preparer-"));
    directories.push(root);
    const repository = join(root, "repository");
    mkdirSync(repository);
    run(repository, "init", "--initial-branch=main");
    run(repository, "config", "user.name", "Altitude Test");
    run(repository, "config", "user.email", "altitude@example.test");
    run(repository, "remote", "add", "origin", "git@github.com:gitsquared/fixture.git");
    writeFileSync(join(repository, "README.md"), "# fixture\n");
    run(repository, "add", "README.md");
    run(repository, "-c", "commit.gpgsign=false", "commit", "-m", "base");
    run(repository, "update-ref", "refs/remotes/origin/main", "HEAD");
    run(repository, "symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/main");

    const bootstraps: Array<{ seedSize: number }> = [];
    const preparer = new LocalCapsulePreparer(
      new FilesystemCapsuleCheckpointStore(join(root, "checkpoints")),
      new FilesystemRepositorySeedCache(join(root, "seeds")),
      { ensure: async () => ({ capsuleId: "capsule", volume: "volume", state: "created" }) },
      { bootstrap: async (input) => { bootstraps.push({ seedSize: input.repositorySeedBundle.byteLength }); return { reused: false }; } },
    );
    const first = await preparer.prepare({
      taskId: "task-1",
      taskImage: "altitude-task:dev",
      network: "altitude-control",
      nodeId: "local",
      repositoryPath: repository,
    });
    writeFileSync(join(repository, "after-first-capture.txt"), "must not leak into a retry\n");
    const retried = await preparer.prepare({
      taskId: "task-1",
      taskImage: "altitude-task:dev",
      network: "altitude-control",
      nodeId: "local",
      repositoryPath: repository,
    });
    expect(retried.checkpoint).toEqual(first.checkpoint);
    expect(bootstraps).toHaveLength(2);
    expect(bootstraps[1]?.seedSize).toBe(bootstraps[0]?.seedSize);

    expect(preparer.prepare({
      taskId: "task-1",
      taskImage: "altitude-task:dev",
      network: "altitude-control",
      nodeId: "straylight",
    })).rejects.toThrow("owned by node local, not straylight");
    expect(bootstraps).toHaveLength(2);

    const resumed = await preparer.prepare({
      taskId: "task-1",
      taskImage: "altitude-task:dev",
      network: "altitude-control",
      nodeId: "local",
    });
    expect(resumed.checkpoint).toEqual(first.checkpoint);
    expect(bootstraps).toHaveLength(3);
  });
});
