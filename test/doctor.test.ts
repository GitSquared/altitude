import { describe, expect, test } from "bun:test";

import { renderDoctor, runDoctor } from "../src/doctor.ts";
import type { CommandResult, CommandRunner, CommandSpec } from "../src/process/command.ts";

class FakeRunner implements CommandRunner {
  constructor(private readonly available: ReadonlySet<string>) {}

  async run(spec: CommandSpec): Promise<CommandResult> {
    return {
      exitCode: this.available.has(spec.executable) ? 0 : 127,
      stdout: "",
      stderr: "",
    };
  }

  async runInteractive(): Promise<number> {
    throw new Error("not used");
  }
}

describe("doctor", () => {
  test("accepts the embedded Microsandbox runtime", async () => {
    const findings = await runDoctor(new FakeRunner(new Set(["git", "gh"])), {
      platform: "darwin",
      arch: "arm64",
      bunVersion: "1.4.0",
    }, { probe: async () => ({ available: true, detail: "ready" }) });

    expect(findings.find((finding) => finding.id === "runtime")?.severity).toBe("ok");
    expect(renderDoctor(findings)).toContain("Microsandbox is ready");
  });

  test("fails closed without a supported runtime", async () => {
    const findings = await runDoctor(new FakeRunner(new Set(["git"])), {
      platform: "darwin",
      arch: "arm64",
      bunVersion: "1.4.0",
    }, { probe: async () => ({ available: false, detail: "hypervisor unavailable" }) });

    expect(findings.find((finding) => finding.id === "runtime")?.severity).toBe("error");
  });
});
