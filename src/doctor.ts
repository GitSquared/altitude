import type { CommandRunner } from "./process/command.ts";

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

const OPTIONAL_TOOLS = ["gh", "pigeons"] as const;

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

  if (platform.platform === "darwin") {
    const appleRuntime = await probeExecutable(runner, "container", ["system", "status", "--format", "json"]);
    const dockerRuntime = await probeExecutable(runner, "docker", ["info"]);
    findings.push({
      id: "runtime",
      severity: appleRuntime || dockerRuntime ? "ok" : "error",
      summary: appleRuntime
        ? "Apple container runtime is ready"
        : dockerRuntime
          ? "Docker runtime is ready; Apple container remains the preferred v1 runtime"
          : "No supported local capsule runtime is ready",
      ...(appleRuntime || dockerRuntime
        ? {}
        : { detail: "Install Apple's container runtime before creating a capsule." }),
    });
  } else if (platform.platform === "linux") {
    const dockerRuntime = await probeExecutable(runner, "docker", ["info"]);
    findings.push({
      id: "runtime",
      severity: dockerRuntime ? "ok" : "error",
      summary: dockerRuntime ? "Docker Engine is ready" : "Docker Engine is required on a v1 node",
    });
  }

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
