import type { CommandResult, CommandSpec } from "../process/command.ts";
import {
  CommandCapsuleRuntime,
  assertCapsuleLaunchSpec,
  environmentArgs,
  type CapsuleExecSpec,
  type CapsuleLaunchSpec,
  type RuntimeProbe,
} from "./capsule-runtime.ts";

export function appleVolumeCreateSpec(spec: CapsuleLaunchSpec): CommandSpec {
  assertCapsuleLaunchSpec(spec);
  return {
    executable: "container",
    args: [
      "volume",
      "create",
      "--label",
      `dev.altitude.capsule=${spec.capsuleId}`,
      "--opt",
      "journal=ordered",
      "-s",
      spec.resources.volumeSize,
      spec.workspaceVolume,
    ],
  };
}

export function appleNetworkCreateSpec(spec: CapsuleLaunchSpec): CommandSpec {
  assertCapsuleLaunchSpec(spec);
  return {
    executable: "container",
    args: ["network", "create", "--label", `dev.altitude.capsule=${spec.capsuleId}`, spec.network],
  };
}

export function appleLaunchSpec(spec: CapsuleLaunchSpec): CommandSpec {
  assertCapsuleLaunchSpec(spec);
  return {
    executable: "container",
    args: [
      "run",
      "--detach",
      "--name",
      spec.capsuleId,
      "--label",
      `dev.altitude.capsule=${spec.capsuleId}`,
      "--init",
      "--read-only",
      "--cap-drop",
      "ALL",
      "--cpus",
      String(spec.resources.cpus),
      "--memory",
      spec.resources.memory,
      "--user",
      "1000:1000",
      "--workdir",
      "/workspace",
      "--volume",
      `${spec.workspaceVolume}:/workspace`,
      "--tmpfs",
      "/tmp",
      "--tmpfs",
      "/run",
      "--network",
      spec.network,
      ...environmentArgs("--env", spec.environment),
      spec.image,
      ...spec.command,
    ],
  };
}

export function appleExecSpec(spec: CapsuleExecSpec): CommandSpec {
  if (spec.command.length === 0) throw new Error("Exec command is required");
  return {
    executable: "container",
    args: [
      "exec",
      ...(spec.interactive === true ? ["--interactive", "--tty"] : []),
      ...(spec.workdir === undefined ? [] : ["--workdir", spec.workdir]),
      ...environmentArgs("--env", spec.environment),
      spec.capsuleId,
      ...spec.command,
    ],
  };
}

export class AppleContainerRuntime extends CommandCapsuleRuntime {
  readonly kind = "apple-container" as const;

  async probe(): Promise<RuntimeProbe> {
    try {
      const result = await this.runner.run({
        executable: "container",
        args: ["system", "status", "--format", "json"],
      });
      return {
        available: result.exitCode === 0,
        detail: result.exitCode === 0 ? "Apple container service is ready" : result.stderr.trim(),
      };
    } catch (error) {
      return { available: false, detail: error instanceof Error ? error.message : String(error) };
    }
  }

  async prepare(spec: CapsuleLaunchSpec): Promise<void> {
    const volume = await this.runner.run(appleVolumeCreateSpec(spec));
    if (volume.exitCode !== 0 && !/already exists/i.test(volume.stderr)) {
      throw new Error(`Creating Apple workspace volume failed: ${volume.stderr.trim()}`);
    }
    const network = await this.runner.run(appleNetworkCreateSpec(spec));
    if (network.exitCode !== 0 && !/already exists/i.test(network.stderr)) {
      throw new Error(`Creating Apple task network failed: ${network.stderr.trim()}`);
    }
  }

  async launch(spec: CapsuleLaunchSpec): Promise<void> {
    await this.requireSuccess(appleLaunchSpec(spec), "Launching Apple capsule");
  }

  async exec(spec: CapsuleExecSpec): Promise<CommandResult | number> {
    const command = appleExecSpec(spec);
    return spec.interactive === true ? await this.runner.runInteractive(command) : await this.runner.run(command);
  }

  async stop(capsuleId: string): Promise<void> {
    await this.requireSuccess(
      { executable: "container", args: ["stop", "--time", "30", capsuleId] },
      "Stopping Apple capsule",
    );
  }

  async destroy(capsuleId: string): Promise<void> {
    await this.requireSuccess(
      { executable: "container", args: ["delete", "--force", capsuleId] },
      "Deleting Apple capsule",
    );
  }
}
