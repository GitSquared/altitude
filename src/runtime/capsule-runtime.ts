import type { CommandResult, CommandRunner, CommandSpec } from "../process/command.ts";

export type RuntimeKind = "apple-container" | "docker";

export interface CapsuleResources {
  readonly cpus: number;
  readonly memory: string;
  readonly pids: number;
  readonly volumeSize: string;
}
export interface CapsuleLaunchSpec {
  readonly capsuleId: string;
  readonly image: string;
  readonly workspaceVolume: string;
  readonly network: string;
  readonly command: readonly string[];
  readonly resources: CapsuleResources;
  readonly environment?: Readonly<Record<string, string>>;
}

export interface CapsuleExecSpec {
  readonly capsuleId: string;
  readonly command: readonly string[];
  readonly environment?: Readonly<Record<string, string>>;
  readonly interactive?: boolean;
  readonly workdir?: string;
}

export interface RuntimeProbe {
  readonly available: boolean;
  readonly detail: string;
}

export interface CapsuleRuntime {
  readonly kind: RuntimeKind;
  probe(): Promise<RuntimeProbe>;
  prepare(spec: CapsuleLaunchSpec): Promise<void>;
  launch(spec: CapsuleLaunchSpec): Promise<void>;
  exec(spec: CapsuleExecSpec): Promise<CommandResult | number>;
  stop(capsuleId: string): Promise<void>;
  destroy(capsuleId: string): Promise<void>;
}

export const DEFAULT_CAPSULE_RESOURCES: CapsuleResources = {
  cpus: 4,
  memory: "8G",
  pids: 1_024,
  volumeSize: "64G",
};

const CAPSULE_ID = /^[a-z0-9][a-z0-9-]{0,62}$/;
const RESOURCE_SIZE = /^[1-9][0-9]*(?:K|M|G|T)$/i;

export function assertCapsuleLaunchSpec(spec: CapsuleLaunchSpec): void {
  if (!CAPSULE_ID.test(spec.capsuleId)) throw new Error(`Invalid capsule ID: ${spec.capsuleId}`);
  if (!CAPSULE_ID.test(spec.workspaceVolume)) throw new Error("Invalid workspace volume name");
  if (!CAPSULE_ID.test(spec.network)) throw new Error("Invalid capsule network name");
  if (spec.image.trim().length === 0 || /\s/.test(spec.image)) throw new Error("Invalid capsule image");
  if (!Number.isFinite(spec.resources.cpus) || spec.resources.cpus <= 0) throw new Error("Invalid CPU limit");
  if (!Number.isInteger(spec.resources.pids) || spec.resources.pids < 32) throw new Error("Invalid PID limit");
  if (!RESOURCE_SIZE.test(spec.resources.memory)) throw new Error("Invalid memory limit");
  if (!RESOURCE_SIZE.test(spec.resources.volumeSize)) throw new Error("Invalid volume size");
  if (spec.command.length === 0) throw new Error("Capsule command is required");
}

export function environmentArgs(
  flag: string,
  environment: Readonly<Record<string, string>> | undefined,
): string[] {
  return Object.entries(environment ?? {})
    .sort(([left], [right]) => left.localeCompare(right))
    .flatMap(([key, value]) => {
      if (!/^[A-Z_][A-Z0-9_]*$/.test(key)) throw new Error(`Invalid environment key: ${key}`);
      return [flag, `${key}=${value}`];
    });
}

export abstract class CommandCapsuleRuntime implements CapsuleRuntime {
  abstract readonly kind: RuntimeKind;

  constructor(protected readonly runner: CommandRunner) {}

  abstract probe(): Promise<RuntimeProbe>;
  abstract prepare(spec: CapsuleLaunchSpec): Promise<void>;
  abstract launch(spec: CapsuleLaunchSpec): Promise<void>;
  abstract exec(spec: CapsuleExecSpec): Promise<CommandResult | number>;
  abstract stop(capsuleId: string): Promise<void>;
  abstract destroy(capsuleId: string): Promise<void>;

  protected async requireSuccess(command: CommandSpec, action: string): Promise<CommandResult> {
    const result = await this.runner.run(command);
    if (result.exitCode !== 0) {
      throw new Error(`${action} failed (${result.exitCode}): ${result.stderr.trim()}`);
    }
    return result;
  }
}
