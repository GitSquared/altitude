import { describe, expect, test } from "bun:test";
import { resolve } from "node:path";

import type { CommandResult, CommandRunner, CommandSpec } from "../src/process/command.ts";
import { capsuleImageBuildCommand } from "../src/runtime/images.ts";
import { IdentityCapsuleManager } from "../src/runtime/identity-manager.ts";
import { identityVolumeName, type IdentityCapsuleLaunchSpec } from "../src/runtime/identity-capsule.ts";

const ok: CommandResult = { exitCode: 0, stdout: "", stderr: "" };

class SequenceRunner implements CommandRunner {
  readonly commands: CommandSpec[] = [];
  constructor(private readonly results: CommandResult[]) {}
  run(spec: CommandSpec): Promise<CommandResult> {
    this.commands.push(spec);
    const result = this.results.shift();
    if (result === undefined) throw new Error("Unexpected command");
    return Promise.resolve(result);
  }
  runInteractive(): Promise<number> {
    throw new Error("Unexpected interactive command");
  }
}

const identity: IdentityCapsuleLaunchSpec = {
  identityId: "claude-personal",
  image: "altitude-identity:dev",
  identityVolume: identityVolumeName("claude-personal"),
  network: "altitude-control",
  command: ["sleep", "infinity"],
};

describe("capsule image builds", () => {
  test("uses the same explicit OCI build contract for Apple container and Docker", () => {
    const contextDirectory = resolve("/tmp/altitude-source");
    const input = {
      image: "altitude-identity:dev",
      contextDirectory,
      containerfile: resolve(contextDirectory, "images/identity/Containerfile"),
      buildArguments: { CLAUDE_CODE_VERSION: "2.1.248" },
    };
    const apple = capsuleImageBuildCommand("apple-container", input);
    const docker = capsuleImageBuildCommand("docker", input);
    expect(apple.executable).toBe("container");
    expect(docker.executable).toBe("docker");
    expect(apple.args).toEqual(docker.args);
    expect(apple.args).toContain("CLAUDE_CODE_VERSION=2.1.248");
  });

  test("rejects a Containerfile outside the explicit context", () => {
    expect(() => capsuleImageBuildCommand("docker", {
      image: "altitude-task:dev",
      contextDirectory: "/tmp/context",
      containerfile: "/tmp/elsewhere/Containerfile",
    })).toThrow("inside its build context");
  });
});

describe("identity capsule manager", () => {
  test("initializes the persistent volume before creating a new identity capsule", async () => {
    const runner = new SequenceRunner([
      ok,
      ok,
      ok,
      { exitCode: 1, stdout: "", stderr: "not found" },
      ok,
      ok,
    ]);
    const result = await new IdentityCapsuleManager("docker", runner).ensure(identity);
    expect(result.state).toBe("created");
    expect(runner.commands.map((command) => command.args?.[0])).toEqual([
      "volume",
      "network",
      "run",
      "exec",
      "run",
      "exec",
    ]);
    expect(runner.commands[2]?.args).toContain("ALTITUDE_IDENTITY_INIT=1");
    expect(runner.commands[4]?.args).not.toContain("/workspace");
  });

  test("starts an existing stopped capsule without replacing its identity volume", async () => {
    const runner = new SequenceRunner([
      ok,
      { exitCode: 1, stdout: "", stderr: "network already exists" },
      ok,
      { exitCode: 1, stdout: "", stderr: "not running" },
      { exitCode: 125, stdout: "", stderr: "container name is already in use" },
      ok,
      ok,
    ]);
    const result = await new IdentityCapsuleManager("docker", runner).ensure(identity);
    expect(result.state).toBe("started");
    expect(runner.commands[5]?.args?.slice(0, 2)).toEqual(["container", "start"]);
  });

  test("does not relaunch a healthy capsule", async () => {
    const runner = new SequenceRunner([ok, ok, ok, ok]);
    const result = await new IdentityCapsuleManager("apple-container", runner).ensure(identity);
    expect(result.state).toBe("already_running");
    expect(runner.commands).toHaveLength(4);
  });
});
