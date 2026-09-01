import type { CapsuleRuntime } from "./capsule-runtime.ts";
import {
  identityCapsuleLaunchSpec,
  type IdentityCapsuleSpec,
} from "./identity-capsule.ts";

export type IdentityCapsuleEnsureResult = Readonly<{
  capsuleId: string;
  state: "already_running" | "resumed" | "created";
}>;

export class IdentityCapsuleManager {
  constructor(private readonly runtime: CapsuleRuntime) {}

  async ensure(input: IdentityCapsuleSpec): Promise<IdentityCapsuleEnsureResult> {
    const spec = identityCapsuleLaunchSpec(input);
    const state = await this.runtime.ensure(spec);
    const initialized = await this.runtime.exec({
      capsuleId: spec.capsuleId,
      command: ["altitude", "_identity-init"],
      environment: { ALTITUDE_IDENTITY_INIT: "1" },
      workdir: "/identity",
    });
    if (typeof initialized === "number" || initialized.exitCode !== 0) {
      const detail = typeof initialized === "number" ? `status ${initialized}` : initialized.stderr.trim();
      throw new Error(`Initializing identity capsule failed: ${detail}`);
    }
    return { capsuleId: spec.capsuleId, state };
  }
}
