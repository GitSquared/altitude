import type { CommandResult } from "../process/command.ts";

export interface CapsuleResources {
  readonly cpus: number;
  readonly memory: string;
  readonly pids: number;
  readonly volumeSize: string;
}
export interface CapsuleLaunchSpec {
  readonly capsuleId: string;
  readonly image: string;
  readonly volume: Readonly<{
    name: string;
    mountPath: "/identity" | "/workspace";
  }>;
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
  probe(): Promise<RuntimeProbe>;
  prepare(spec: CapsuleLaunchSpec): Promise<void>;
  ensure(spec: CapsuleLaunchSpec): Promise<"created" | "resumed" | "already_running">;
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
  if (!CAPSULE_ID.test(spec.volume.name)) throw new Error("Invalid capsule volume name");
  if (spec.volume.mountPath !== "/identity" && spec.volume.mountPath !== "/workspace") {
    throw new Error("Invalid capsule volume mount path");
  }
  if (spec.image.trim().length === 0 || /\s/.test(spec.image)) throw new Error("Invalid capsule image");
  if (!Number.isFinite(spec.resources.cpus) || spec.resources.cpus <= 0) throw new Error("Invalid CPU limit");
  if (!Number.isInteger(spec.resources.pids) || spec.resources.pids < 32) throw new Error("Invalid PID limit");
  if (!RESOURCE_SIZE.test(spec.resources.memory)) throw new Error("Invalid memory limit");
  if (!RESOURCE_SIZE.test(spec.resources.volumeSize)) throw new Error("Invalid volume size");
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
