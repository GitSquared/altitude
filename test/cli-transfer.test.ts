import { afterEach, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  FilesystemCapsuleCheckpointStore,
  FilesystemCapsuleTransferReceiver,
  FilesystemRepositorySeedCache,
  createCapsuleCheckpoint,
  portableCapsulePath,
  utf8,
} from "../src/capsule/index.ts";
import { runCli, type CliOptions } from "../src/cli/main.ts";
import {
  defaultLocalConfig,
  writeLocalConfig,
} from "../src/config/local.ts";
import { resolveAltitudePaths } from "../src/config/paths.ts";
import type {
  CommandResult,
  CommandRunner,
  CommandSpec,
} from "../src/process/command.ts";
import { portableTaskStateMember } from "../src/run/task-state.ts";
import { TaskStore } from "../src/storage/task-store.ts";

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

test("task send moves a paused capsule through the remote command boundary", async () => {
  const sourceRoot = mkdtempSync(join(tmpdir(), "altitude-cli-send-source-"));
  const targetRoot = mkdtempSync(join(tmpdir(), "altitude-cli-send-target-"));
  roots.push(sourceRoot, targetRoot);
  const paths = resolveAltitudePaths({ ALTITUDE_HOME: sourceRoot });
  const config = {
    ...defaultLocalConfig("docker"),
    node: { ...defaultLocalConfig("docker").node, id: "macbook" },
  };
  await writeLocalConfig(paths.config, config);

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
      portableTaskStateMember(store, "task-send"),
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
  const commands: CommandSpec[] = [];
  const runner: CommandRunner = {
    async run(spec): Promise<CommandResult> {
      commands.push(spec);
      if (spec.args?.at(-1) === "prepare" && spec.stdin instanceof Uint8Array) {
        const receipt = await receiver.prepare(spec.stdin);
        return {
          exitCode: 0,
          stdout: JSON.stringify(receipt) + "\n",
          stderr: "",
        };
      }
      if (spec.args?.at(-1) === "activate" && typeof spec.stdin === "string") {
        const receipt = await receiver.activate(JSON.parse(spec.stdin));
        return {
          exitCode: 0,
          stdout: JSON.stringify(receipt) + "\n",
          stderr: "",
        };
      }
      return { exitCode: 1, stdout: "", stderr: "unexpected command" };
    },
    async runInteractive(): Promise<number> {
      throw new Error("not used");
    },
  };
  const stdout = output();
  const stderr = output();
  const options: CliOptions = {
    env: { ALTITUDE_HOME: sourceRoot },
    stdout,
    stderr,
    commandRunner: runner,
    clock: () => Date.parse("2026-08-29T10:05:00.000Z"),
  };
  expect(await runCli([
    "task",
    "send",
    "task-send",
    "--node",
    "straylight",
    "--host",
    "straylight.example",
    "--user",
    "altitude",
    "--identity-file",
    "/private/state/id_ed25519",
    "--known-hosts-file",
    "/private/state/known_hosts",
  ], options)).toBe(0);
  expect(stderr.value).toBe("");
  expect(stdout.value).toContain("Moved task task-send to straylight");
  expect(commands).toHaveLength(2);
  expect(commands.every((command) => command.executable === "ssh")).toBeTrue();

  using sourceAfter = new TaskStore(paths.database);
  expect(sourceAfter.getTask("task-send")?.status).toBe("transferring");
  expect((await sourceCheckpoints.readActive("task-send")).manifest.ownership)
    .toMatchObject({ ownerNodeId: "straylight", epoch: 2 });
  expect((await targetCheckpoints.readActive("task-send")).manifest.ownership)
    .toMatchObject({ ownerNodeId: "straylight", epoch: 2 });
});
