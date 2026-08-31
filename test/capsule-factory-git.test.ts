import { afterEach, describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { createFactoryGitRepository } from "../src/capsule/factory-git.ts";
import { BunCommandRunner } from "../src/process/command.ts";

const temporaryDirectories: string[] = [];

afterEach(() => {
  for (const directory of temporaryDirectories.splice(0)) {
    rmSync(directory, { recursive: true, force: true });
  }
});

async function git(repository: string, args: readonly string[]): Promise<string> {
  const result = await new BunCommandRunner().run({
    executable: "git",
    args,
    cwd: repository,
    env: { GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: "/dev/null" },
  });
  if (result.exitCode !== 0) throw new Error(result.stderr);
  return result.stdout.trim();
}

describe("factory Git repository", () => {
  test("starts a task from an empty factory boundary rather than host state", async () => {
    const root = mkdtempSync(join(tmpdir(), "altitude-factory-test-"));
    temporaryDirectories.push(root);
    const fakeHome = join(root, "fake-home");
    writeFileSync(join(root, "host-marker"), "must not enter the capsule");

    const repository = await createFactoryGitRepository({
      parentDirectory: join(fakeHome, "seeds"),
      taskId: "task:first",
    });

    expect(readdirSync(repository.path)).toEqual([".git"]);
    expect(await git(repository.path, ["rev-parse", "--abbrev-ref", "HEAD"])).toBe("main");
    expect(await git(repository.path, ["log", "-1", "--format=%an <%ae>%n%s"])).toBe(
      "Altitude Factory <factory@altitude.local>\nInitialize task capsule",
    );
    expect(await git(repository.path, ["remote", "get-url", "origin"])).toBe(
      "altitude://unbound/task%3Afirst",
    );
    expect(await git(repository.path, ["config", "--local", "--get", "commit.gpgsign"])).toBe("false");

    await repository.cleanup();
    await repository.cleanup();
    expect(existsSync(repository.path)).toBe(false);
  });

  test("cleans a partial repository when Git initialization fails", async () => {
    const root = mkdtempSync(join(tmpdir(), "altitude-factory-failure-"));
    temporaryDirectories.push(root);
    const parent = join(root, "seeds");
    const runner = {
      async run() {
        return { exitCode: 9, stdout: "", stderr: "synthetic failure" };
      },
    };

    expect(createFactoryGitRepository({ parentDirectory: parent, taskId: "task-1", runner }))
      .rejects.toThrow("synthetic failure");
    expect(readdirSync(parent)).toEqual([]);
  });
});
