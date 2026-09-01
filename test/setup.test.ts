import { describe, expect, test } from "bun:test";

import type { AgentAdapter, AgentProbe } from "../src/agent/protocol.ts";
import { defaultLocalConfig } from "../src/config/local.ts";
import { setupAltitude, type SetupProgress } from "../src/setup.ts";

class InteractiveLogin {
  interactive = 0;
  run(spec: ReturnType<AgentAdapter["loginLaunchSpec"]>): Promise<number> {
    this.interactive += 1;
    expect(spec.target).toEqual({ kind: "identity-capsule", identityId: "claude-personal" });
    expect(spec.args).toEqual([]);
    return Promise.resolve(0);
  }
}

describe("Altitude first-run setup", () => {
  test("initializes an identity capsule and logs in only inside it", async () => {
    const identities: unknown[] = [];
    const progress: SetupProgress[] = [];
    const login = new InteractiveLogin();
    const probes: AgentProbe[] = [
      { installed: true, version: "2.1.248", auth: "unauthenticated" },
      { installed: true, version: "2.1.248", auth: "authenticated" },
    ];
    const result = await setupAltitude({
      config: defaultLocalConfig(),
      progress: (value) => progress.push(value),
    }, {
      runtimeProbe: { probe: async () => ({ available: true, detail: "ready" }) },
      identity: {
        ensure: async (input) => {
          identities.push(input);
          return { capsuleId: "altitude-i-test", state: "created" };
        },
      },
      claude: {
        probe: async () => probes.shift()!,
        loginLaunchSpec: () => ({
          target: { kind: "identity-capsule", identityId: "claude-personal" },
          executable: "claude",
          args: [],
          cwd: "/identity",
          env: { CLAUDE_CONFIG_DIR: "/identity/claude" },
          io: "interactive",
          purpose: "login",
        }),
      },
      login: (spec) => login.run(spec),
    });
    expect(identities).toHaveLength(1);
    expect(JSON.stringify(identities[0])).not.toContain("workspace");
    expect(login.interactive).toBe(1);
    expect(progress.map((item) => item.step)).toEqual([
      "runtime",
      "identity",
      "claude_probe",
      "claude_login",
    ]);
    expect(result.claude.auth).toBe("authenticated");
  });

  test("does not prompt again when the managed identity is already authenticated", async () => {
    const login = new InteractiveLogin();
    await setupAltitude({
      config: defaultLocalConfig(),
    }, {
      runtimeProbe: { probe: async () => ({ available: true, detail: "ready" }) },
      identity: { ensure: async () => ({ capsuleId: "altitude-i-test", state: "already_running" }) },
      claude: {
        probe: async () => ({ installed: true, auth: "authenticated" }),
        loginLaunchSpec: () => { throw new Error("must not login"); },
      },
      login: () => { throw new Error("must not open login"); },
    });
    expect(login.interactive).toBe(0);
  });

  test("stops before identity work when Microsandbox is unavailable", async () => {
    expect(setupAltitude({
      config: defaultLocalConfig(),
    }, {
      runtimeProbe: { probe: async () => ({ available: false, detail: "engine stopped" }) },
      identity: { ensure: async () => { throw new Error("must not create"); } },
      claude: {
        probe: async () => { throw new Error("must not probe"); },
        loginLaunchSpec: () => { throw new Error("must not login"); },
      },
      login: () => { throw new Error("must not open login"); },
    })).rejects.toThrow("engine stopped");
  });
});
