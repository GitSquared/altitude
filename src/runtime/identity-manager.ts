import type { CommandResult, CommandRunner, CommandSpec } from "../process/command.ts";
import type { RuntimeKind } from "./capsule-runtime.ts";
import {
  identityLaunchCommand,
  identityNetworkCreateCommand,
  identityVolumeCreateCommand,
  identityVolumeInitializeCommand,
  stableCapsuleName,
  type IdentityCapsuleLaunchSpec,
} from "./identity-capsule.ts";

export type IdentityCapsuleEnsureResult = Readonly<{
  capsuleId: string;
  state: "already_running" | "started" | "created";
}>;

function runtimeCommand(runtime: RuntimeKind, apple: readonly string[], docker: readonly string[]): CommandSpec {
  return runtime === "apple-container"
    ? { executable: "container", args: apple }
    : { executable: "docker", args: docker };
}

function alreadyExists(result: CommandResult): boolean {
  return /already exists|name is already in use|conflict.+container name/iu.test(`${result.stdout}\n${result.stderr}`);
}

function alreadyRunning(result: CommandResult): boolean {
  return /already running|is running/iu.test(`${result.stdout}\n${result.stderr}`);
}

export class IdentityCapsuleManager {
  constructor(
    private readonly runtime: RuntimeKind,
    private readonly runner: CommandRunner,
  ) {}

  async ensure(spec: IdentityCapsuleLaunchSpec): Promise<IdentityCapsuleEnsureResult> {
    await this.#allowExisting(identityVolumeCreateCommand(this.runtime, spec), "Creating identity volume");
    await this.#allowExisting(identityNetworkCreateCommand(this.runtime, spec), "Creating identity network");
    await this.#required(identityVolumeInitializeCommand(this.runtime, spec), "Initializing identity volume");

    const capsuleId = stableCapsuleName("identity", spec.identityId);
    if ((await this.runner.run(this.#healthCommand(capsuleId))).exitCode === 0) {
      return { capsuleId, state: "already_running" };
    }

    const launched = await this.runner.run(identityLaunchCommand(this.runtime, spec));
    if (launched.exitCode === 0) {
      await this.#requireHealthy(capsuleId);
      return { capsuleId, state: "created" };
    }
    if (!alreadyExists(launched)) {
      throw new Error(`Launching identity capsule failed (${launched.exitCode}): ${launched.stderr.trim()}`);
    }

    const started = await this.runner.run(this.#startCommand(capsuleId));
    if (started.exitCode !== 0 && !alreadyRunning(started)) {
      throw new Error(`Starting identity capsule failed (${started.exitCode}): ${started.stderr.trim()}`);
    }
    await this.#requireHealthy(capsuleId);
    return { capsuleId, state: "started" };
  }

  #healthCommand(capsuleId: string): CommandSpec {
    return runtimeCommand(
      this.runtime,
      ["exec", "--user", "1000:1000", capsuleId, "/usr/bin/true"],
      ["exec", "--user", "1000:1000", capsuleId, "/usr/bin/true"],
    );
  }

  #startCommand(capsuleId: string): CommandSpec {
    return runtimeCommand(
      this.runtime,
      ["start", capsuleId],
      ["container", "start", capsuleId],
    );
  }

  async #allowExisting(command: CommandSpec, action: string): Promise<void> {
    const result = await this.runner.run(command);
    if (result.exitCode !== 0 && !alreadyExists(result)) {
      throw new Error(`${action} failed (${result.exitCode}): ${result.stderr.trim()}`);
    }
  }

  async #required(command: CommandSpec, action: string): Promise<void> {
    const result = await this.runner.run(command);
    if (result.exitCode !== 0) {
      throw new Error(`${action} failed (${result.exitCode}): ${result.stderr.trim()}`);
    }
  }

  async #requireHealthy(capsuleId: string): Promise<void> {
    const health = await this.runner.run(this.#healthCommand(capsuleId));
    if (health.exitCode !== 0) {
      throw new Error(`Identity capsule ${capsuleId} did not become ready: ${health.stderr.trim()}`);
    }
  }
}
