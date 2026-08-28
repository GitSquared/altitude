import { describe, expect, test } from "bun:test";

import {
  identityLaunchCommand,
  identityVolumeCreateCommand,
  identityVolumeName,
  stableCapsuleName,
  type IdentityCapsuleLaunchSpec,
} from "../src/runtime/identity-capsule.ts";
import {
  CapsuleProcessLauncher,
  runtimeProcessCommand,
  type PipedCommandSpawner,
} from "../src/runtime/process-launcher.ts";
import type { RunningProcess } from "../src/runtime/protocol.ts";

const identity: IdentityCapsuleLaunchSpec = {
  identityId: "claude-personal",
  image: "ghcr.io/gitsquared/altitude-claude@sha256:abc",
  identityVolume: identityVolumeName("claude-personal"),
  network: "altitude-control",
  command: ["/opt/altitude/bin/identity-runtime"],
  environment: { CLAUDE_CONFIG_DIR: "/identity/claude" },
};

const idleProcess: RunningProcess = {
  stdout: { async *[Symbol.asyncIterator]() {} },
  stderr: { async *[Symbol.asyncIterator]() {} },
  exited: Promise.resolve({ exitCode: 0 }),
  writeStdin: async () => undefined,
  closeStdin: async () => undefined,
  terminate: async () => undefined,
};

describe("node-local identity runtime", () => {
  test("creates one persistent identity volume without a source mount", () => {
    const volume = identityVolumeCreateCommand("apple-container", identity);
    const apple = identityLaunchCommand("apple-container", identity);
    const docker = identityLaunchCommand("docker", identity);

    expect(volume.args).toContain("8G");
    expect(apple.args).toContain(`${identity.identityVolume}:/identity`);
    expect(apple.args).not.toContain("/workspace");
    expect(docker.args).toContain(`type=volume,src=${identity.identityVolume},dst=/identity`);
    expect(docker.args).toContain("no-new-privileges");
    expect(docker.args?.join(" ")).not.toContain("docker.sock");
  });

  test("maps opaque ids to stable runtime-safe capsule names", () => {
    expect(stableCapsuleName("identity", "claude-personal")).toMatch(/^altitude-i-[a-f0-9]{20}$/);
    expect(stableCapsuleName("task", "task:linear:ABC-123")).toMatch(/^altitude-t-[a-f0-9]{20}$/);
    expect(() => stableCapsuleName("task", "../escape")).toThrow("Invalid task id");
  });
});

describe("capsule process launcher", () => {
  test("executes Claude in the identity capsule and treats taskId only as a control binding", () => {
    const command = runtimeProcessCommand("docker", {
      target: { kind: "identity-capsule", identityId: "claude-personal", taskId: "task-1" },
      executable: "claude",
      args: ["--resume", "session-1"],
      cwd: "/identity/control/tasks/task-1",
      env: { CLAUDE_CONFIG_DIR: "/identity/claude" },
      io: "piped",
      purpose: "Resume Claude",
    });

    expect(command.executable).toBe("docker");
    expect(command.args).toContain(stableCapsuleName("identity", "claude-personal"));
    expect(command.args).not.toContain(stableCapsuleName("task", "task-1"));
    expect(command.args?.join(" ")).not.toContain("/workspace");
  });

  test("keeps task commands out of identity storage and rejects host fallback", () => {
    expect(() => runtimeProcessCommand("apple-container", {
      target: { kind: "task-capsule", taskId: "task-1" },
      executable: "git",
      args: ["status"],
      cwd: "/workspace",
      env: { CLAUDE_CONFIG_DIR: "/identity/claude" },
      io: "piped",
      purpose: "Inspect task",
    })).toThrow("cannot receive a Claude identity");
    expect(() => runtimeProcessCommand("docker", {
      target: { kind: "host" },
      executable: "claude",
      args: [],
      io: "piped",
      purpose: "Unsafe fallback",
    })).toThrow("cannot silently execute on the host");
  });

  test("hands a shell-free runtime command to the injected spawner", async () => {
    const commands: string[][] = [];
    const spawner: PipedCommandSpawner = {
      spawn(command) {
        commands.push([command.executable, ...(command.args ?? [])]);
        return idleProcess;
      },
    };
    const launcher = new CapsuleProcessLauncher("apple-container", spawner);
    expect(await launcher.launch({
      target: { kind: "task-capsule", taskId: "task-1" },
      executable: "git",
      args: ["status", "--short"],
      cwd: "/workspace",
      io: "piped",
      purpose: "Inspect task status",
    })).toBe(idleProcess);
    expect(commands[0]).toEqual([
      "container",
      "exec",
      "--workdir",
      "/workspace",
      stableCapsuleName("task", "task-1"),
      "git",
      "status",
      "--short",
    ]);
  });
});
