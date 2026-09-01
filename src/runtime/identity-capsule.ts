import { createHash } from "node:crypto";

import {
  DEFAULT_CAPSULE_RESOURCES,
  type CapsuleLaunchSpec,
  type CapsuleResources,
} from "./capsule-runtime.ts";

const ID = /^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/u;

export interface IdentityCapsuleSpec {
  readonly identityId: string;
  readonly image: string;
  readonly resources?: CapsuleResources;
  readonly environment?: Readonly<Record<string, string>>;
}

export function stableCapsuleName(kind: "identity" | "task", id: string): string {
  if (!ID.test(id)) throw new Error(`Invalid ${kind} id`);
  const digest = createHash("sha256").update(id, "utf8").digest("hex").slice(0, 20);
  return `altitude-${kind === "identity" ? "i" : "t"}-${digest}`;
}

export function identityVolumeName(identityId: string): string {
  return `${stableCapsuleName("identity", identityId)}-data`;
}

export function identityCapsuleLaunchSpec(spec: IdentityCapsuleSpec): CapsuleLaunchSpec {
  const capsuleId = stableCapsuleName("identity", spec.identityId);
  return {
    capsuleId,
    image: spec.image,
    volume: { name: identityVolumeName(spec.identityId), mountPath: "/identity" },
    resources: spec.resources
      ?? { ...DEFAULT_CAPSULE_RESOURCES, cpus: 2, memory: "4G", volumeSize: "8G" },
    environment: {
      CLAUDE_CONFIG_DIR: "/identity/claude",
      DISABLE_AUTOUPDATER: "1",
      HOME: "/identity/home",
      ...spec.environment,
    },
  };
}
