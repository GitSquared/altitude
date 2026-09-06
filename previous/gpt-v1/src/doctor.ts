import type { CommandRunner } from "./process/command.ts";
import type { RuntimeProbe } from "./runtime/capsule-runtime.ts";
import { MicrosandboxCapsuleRuntime } from "./runtime/microsandbox.ts";

export type DoctorSeverity = "ok" | "warning" | "error";

export interface DoctorFinding {
  readonly id: string;
  readonly severity: DoctorSeverity;
  readonly summary: string;
  readonly detail?: string;
}

export interface DoctorPlatform {
  readonly platform: NodeJS.Platform;
  readonly arch: string;
  readonly bunVersion: string;
}

const OPTIONAL_TOOLS = ["gh"] as const;

async function probeExecutable(
  runner: CommandRunner,
  executable: string,
  args: readonly string[],
): Promise<boolean> {
  try {
    return (await runner.run({ executable, args })).exitCode === 0;
  } catch {
    return false;
  }
}

export async function runDoctor(
  runner: CommandRunner,
  platform: DoctorPlatform = {
    platform: process.platform,
    arch: process.arch,
    bunVersion: Bun.version,
  },
  runtime: { probe(): Promise<RuntimeProbe> } = new MicrosandboxCapsuleRuntime(),
): Promise<readonly DoctorFinding[]> {
  const findings: DoctorFinding[] = [];

  if (platform.platform === "darwin" && platform.arch === "arm64") {
    findings.push({ id: "platform", severity: "ok", summary: "Apple Silicon macOS is supported" });
  } else if (platform.platform === "linux") {
    findings.push({ id: "platform", severity: "ok", summary: `${platform.arch} Linux is supported as a node` });
  } else {
    findings.push({
      id: "platform",
      severity: "error",
      summary: `${platform.platform}/${platform.arch} is outside the v1 support boundary`,
    });
  }

  findings.push({ id: "bun", severity: "ok", summary: `Bun ${platform.bunVersion}` });

  const gitAvailable = await probeExecutable(runner, "git", ["--version"]);
  findings.push({
    id: "git",
    severity: gitAvailable ? "ok" : "error",
    summary: gitAvailable ? "Git is available" : "Git is required",
  });

  const capsuleRuntime = await runtime.probe();
  findings.push({
    id: "runtime",
    severity: capsuleRuntime.available ? "ok" : "error",
    summary: capsuleRuntime.available ? "Microsandbox is ready" : "Microsandbox is unavailable",
    ...(capsuleRuntime.available ? {} : { detail: capsuleRuntime.detail }),
  });

  for (const executable of OPTIONAL_TOOLS) {
    const available = await probeExecutable(runner, executable, ["version"]);
    findings.push({
      id: executable,
      severity: available ? "ok" : "warning",
      summary: available
        ? `${executable} is available`
        : `${executable} is not installed; its adapter will remain unavailable`,
    });
  }

  findings.push({
    id: "claude-identity",
    severity: "warning",
    summary: "Claude authentication has not been checked yet",
    detail: "Altitude checks the managed identity capsule, never the host Claude profile.",
  });

  return findings;
}

export function renderDoctor(findings: readonly DoctorFinding[]): string {
  const glyph: Record<DoctorSeverity, string> = { ok: "✓", warning: "!", error: "×" };
  return `${findings
    .map((finding) => {
      const line = `${glyph[finding.severity]} ${finding.summary}`;
      return finding.detail === undefined ? line : `${line}\n  ${finding.detail}`;
    })
    .join("\n")}\n`;
}
