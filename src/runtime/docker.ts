import type { CommandResult, CommandSpec } from "../process/command.ts";
import {
  CommandCapsuleRuntime,
  assertCapsuleLaunchSpec,
  environmentArgs,
  type CapsuleExecSpec,
  type CapsuleLaunchSpec,
  type RuntimeProbe,
} from "./capsule-runtime.ts";

export function dockerVolumeCreateSpec(spec: CapsuleLaunchSpec): CommandSpec {
  assertCapsuleLaunchSpec(spec);
  return {
    executable: "docker",
    args: ["volume", "create", "--label", `dev.altitude.capsule=${spec.capsuleId}`, spec.workspaceVolume],
  };
}
export function dockerNetworkCreateSpec(spec: CapsuleLaunchSpec): CommandSpec {
  assertCapsuleLaunchSpec(spec);
  return {
    executable: "docker",
    args: ["network", "create", "--label", `dev.altitude.capsule=${spec.capsuleId}`, spec.network],
  };
}

export function dockerLaunchSpec(spec: CapsuleLaunchSpec): CommandSpec {
  assertCapsuleLaunchSpec(spec);
  return {
    executable: "docker",
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
      "--security-opt",
      "no-new-privileges",
      "--cpus",
      String(spec.resources.cpus),
      "--memory",
      spec.resources.memory,
      "--pids-limit",
      String(spec.resources.pids),
      "--add-host",
      "host.docker.internal:host-gateway",
      "--user",
      "1000:1000",
      "--workdir",
      "/workspace",
      "--mount",
      `type=volume,src=${spec.workspaceVolume},dst=/workspace`,
      "--tmpfs",
      "/tmp:rw,noexec,nosuid,size=1g",
      "--tmpfs",
      "/run:rw,noexec,nosuid,size=64m",
      "--network",
      spec.network,
      ...environmentArgs("--env", spec.environment),
      spec.image,
      ...spec.command,
    ],
  };
}

export function dockerExecSpec(spec: CapsuleExecSpec): CommandSpec {
  if (spec.command.length === 0) throw new Error("Exec command is required");
  return {
    executable: "docker",
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

export class DockerCapsuleRuntime extends CommandCapsuleRuntime {
  readonly kind = "docker" as const;

  async probe(): Promise<RuntimeProbe> {
    try {
      const result = await this.runner.run({ executable: "docker", args: ["info", "--format", "{{json .}}"] });
      return {
        available: result.exitCode === 0,
        detail: result.exitCode === 0 ? "Docker Engine is ready" : result.stderr.trim(),
      };
    } catch (error) {
      return { available: false, detail: error instanceof Error ? error.message : String(error) };
    }
  }

  async prepare(spec: CapsuleLaunchSpec): Promise<void> {
    await this.requireSuccess(dockerVolumeCreateSpec(spec), "Creating Docker workspace volume");
    const network = await this.runner.run(dockerNetworkCreateSpec(spec));
    if (network.exitCode !== 0 && !/already exists/i.test(network.stderr)) {
      throw new Error(`Creating Docker task network failed: ${network.stderr.trim()}`);
    }
  }

  async launch(spec: CapsuleLaunchSpec): Promise<void> {
    await this.requireSuccess(dockerLaunchSpec(spec), "Launching Docker capsule");
  }

  async exec(spec: CapsuleExecSpec): Promise<CommandResult | number> {
    const command = dockerExecSpec(spec);
    return spec.interactive === true ? await this.runner.runInteractive(command) : await this.runner.run(command);
  }

  async stop(capsuleId: string): Promise<void> {
    await this.requireSuccess(
      { executable: "docker", args: ["container", "stop", "--time", "30", capsuleId] },
      "Stopping Docker capsule",
    );
  }

  async destroy(capsuleId: string): Promise<void> {
    await this.requireSuccess(
      { executable: "docker", args: ["container", "rm", "--force", capsuleId] },
      "Deleting Docker capsule",
    );
  }
}
