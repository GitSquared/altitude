import { describe, expect, test } from "bun:test";

import type {
  CapsuleExecSpec,
  CapsuleLaunchSpec,
  CapsuleRuntime,
} from "./capsule-runtime.ts";
import { identityVolumeName } from "./identity-capsule.ts";
import { IdentityCapsuleManager } from "./identity-manager.ts";

describe("identity capsule manager", () => {
  test("initializes only a persistent node-local Claude identity", async () => {
    const launches: CapsuleLaunchSpec[] = [];
    const executions: CapsuleExecSpec[] = [];
    const runtime: CapsuleRuntime = {
      probe: async () => ({ available: true, detail: "ready" }),
      prepare: async () => undefined,
      ensure: async (spec) => {
        launches.push(spec);
        return "created";
      },
      exec: async (spec) => {
        executions.push(spec);
        return { exitCode: 0, stdout: "", stderr: "" };
      },
      stop: async () => undefined,
      destroy: async () => undefined,
    };

    const result = await new IdentityCapsuleManager(runtime).ensure({
      identityId: "claude-personal",
      image: "altitude-identity:test",
    });

    expect(result.state).toBe("created");
    expect(launches[0]).toMatchObject({
      image: "altitude-identity:test",
      volume: {
        name: identityVolumeName("claude-personal"),
        mountPath: "/identity",
      },
      environment: {
        CLAUDE_CONFIG_DIR: "/identity/claude",
        HOME: "/identity/home",
      },
    });
    expect(executions[0]).toMatchObject({
      command: ["altitude", "_identity-init"],
      environment: { ALTITUDE_IDENTITY_INIT: "1" },
      workdir: "/identity",
    });
    expect(JSON.stringify(launches)).not.toMatch(/workspace|\.ssh|credential/iu);
  });

  test("fails closed when capsule initialization fails", async () => {
    const runtime: CapsuleRuntime = {
      probe: async () => ({ available: true, detail: "ready" }),
      prepare: async () => undefined,
      ensure: async () => "resumed",
      exec: async () => ({ exitCode: 17, stdout: "", stderr: "unsafe identity root" }),
      stop: async () => undefined,
      destroy: async () => undefined,
    };

    expect(new IdentityCapsuleManager(runtime).ensure({
      identityId: "claude-personal",
      image: "altitude-identity:test",
    })).rejects.toThrow("unsafe identity root");
  });
});
