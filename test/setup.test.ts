import { describe, expect, test } from "bun:test";

import type { AgentProbe } from "../src/agent/protocol.ts";
import { defaultLocalConfig } from "../src/config/local.ts";
import { setupAltitude, type SetupProgress } from "../src/setup.ts";

describe("Altitude first-run setup", () => {
  test("initializes an identity capsule and leaves login to the first task", async () => {
    const identities: unknown[] = [];
    const progress: SetupProgress[] = [];
    const probe: AgentProbe = { installed: true, version: "2.1.248", auth: "unauthenticated" };
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
        probe: async () => probe,
      },
    });
    expect(identities).toHaveLength(1);
    expect(JSON.stringify(identities[0])).not.toContain("workspace");
    expect(progress.map((item) => item.step)).toEqual([
      "runtime",
      "identity",
      "claude_probe",
    ]);
    expect(result.claude.auth).toBe("unauthenticated");
  });

  test("reports an already-authenticated managed identity without opening Claude", async () => {
    await setupAltitude({
      config: defaultLocalConfig(),
    }, {
      runtimeProbe: { probe: async () => ({ available: true, detail: "ready" }) },
      identity: { ensure: async () => ({ capsuleId: "altitude-i-test", state: "already_running" }) },
      claude: {
        probe: async () => ({ installed: true, auth: "authenticated" }),
      },
    });
  });

  test("stops before identity work when Microsandbox is unavailable", async () => {
    expect(setupAltitude({
      config: defaultLocalConfig(),
    }, {
      runtimeProbe: { probe: async () => ({ available: false, detail: "engine stopped" }) },
      identity: { ensure: async () => { throw new Error("must not create"); } },
      claude: {
        probe: async () => { throw new Error("must not probe"); },
      },
    })).rejects.toThrow("engine stopped");
  });
});
