import { createHash } from "node:crypto";

import type { CommandSpec } from "../process/command.ts";
import {
  DEFAULT_CAPSULE_RESOURCES,
  environmentArgs,
  type CapsuleResources,
  type RuntimeKind,
} from "./capsule-runtime.ts";

const IDENTITY_ID = /^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/u;
const IMAGE = /^[^\s\u0000]+$/u;
const VOLUME = /^[a-z0-9][a-z0-9-]{0,62}$/u;

export interface IdentityCapsuleLaunchSpec {
  readonly identityId: string;
  readonly image: string;
  readonly identityVolume: string;
  readonly network: string;
  readonly command: readonly string[];
  readonly resources?: CapsuleResources;
  readonly environment?: Readonly<Record<string, string>>;
}

export function stableCapsuleName(kind: "identity" | "task", id: string): string {
  if (!IDENTITY_ID.test(id)) throw new Error(`Invalid ${kind} id`);
  const digest = createHash("sha256").update(id, "utf8").digest("hex").slice(0, 20);
  return `altitude-${kind === "identity" ? "i" : "t"}-${digest}`;
}

export function identityVolumeName(identityId: string): string {
  return `${stableCapsuleName("identity", identityId)}-data`;
}

function resources(spec: IdentityCapsuleLaunchSpec): CapsuleResources {
  return spec.resources ?? { ...DEFAULT_CAPSULE_RESOURCES, cpus: 2, memory: "4G", volumeSize: "8G" };
}

function validate(spec: IdentityCapsuleLaunchSpec): void {
  stableCapsuleName("identity", spec.identityId);
  if (!IMAGE.test(spec.image)) throw new Error("Invalid identity capsule image");
  if (!VOLUME.test(spec.identityVolume)) throw new Error("Invalid identity volume name");
  if (!VOLUME.test(spec.network)) throw new Error("Invalid identity network name");
  if (spec.command.length === 0 || spec.command.some((value) => value.includes("\0"))) {
    throw new Error("Identity capsule command is invalid");
  }
  const limits = resources(spec);
  if (!Number.isFinite(limits.cpus) || limits.cpus <= 0) throw new Error("Invalid CPU limit");
  if (!Number.isInteger(limits.pids) || limits.pids < 32) throw new Error("Invalid PID limit");
  if (!/^[1-9][0-9]*(?:K|M|G|T)$/i.test(limits.memory)) throw new Error("Invalid memory limit");
  if (!/^[1-9][0-9]*(?:K|M|G|T)$/i.test(limits.volumeSize)) throw new Error("Invalid volume size");
}

export function identityVolumeCreateCommand(
  runtime: RuntimeKind,
  spec: IdentityCapsuleLaunchSpec,
): CommandSpec {
  validate(spec);
  if (runtime === "apple-container") {
    return {
      executable: "container",
      args: [
        "volume",
        "create",
        "--label",
        `dev.altitude.identity=${spec.identityId}`,
        "-s",
        resources(spec).volumeSize,
        spec.identityVolume,
      ],
    };
  }
  return {
    executable: "docker",
    args: ["volume", "create", "--label", `dev.altitude.identity=${spec.identityId}`, spec.identityVolume],
  };
}

export function identityLaunchCommand(
  runtime: RuntimeKind,
  spec: IdentityCapsuleLaunchSpec,
): CommandSpec {
  validate(spec);
  const name = stableCapsuleName("identity", spec.identityId);
  const limits = resources(spec);
  const common = [
    "run",
    "--detach",
    "--name",
    name,
    "--label",
    `dev.altitude.identity=${spec.identityId}`,
    "--init",
    "--read-only",
    "--cap-drop",
    "ALL",
    "--cpus",
    String(limits.cpus),
    "--memory",
    limits.memory,
  ];
  if (runtime === "apple-container") {
    return {
      executable: "container",
      args: [
        ...common,
        "--user",
        "altitude",
        "--workdir",
        "/identity",
        "--volume",
        `${spec.identityVolume}:/identity`,
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
  return {
    executable: "docker",
    args: [
      ...common,
      "--security-opt",
      "no-new-privileges",
      "--pids-limit",
      String(limits.pids),
      "--user",
      "1000:1000",
      "--workdir",
      "/identity",
      "--mount",
      `type=volume,src=${spec.identityVolume},dst=/identity`,
      "--tmpfs",
      "/tmp:rw,noexec,nosuid,size=512m",
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
