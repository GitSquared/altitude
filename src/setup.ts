import type { AgentAdapter, AgentProbe } from "./agent/protocol.ts";
import type { CommandRunner, CommandSpec } from "./process/command.ts";
import type { AltitudeLocalConfig } from "./config/local.ts";
import type { RuntimeProbe } from "./runtime/capsule-runtime.ts";
import type { IdentityCapsuleManager, IdentityCapsuleEnsureResult } from "./runtime/identity-manager.ts";

export type SetupProgress = Readonly<{
  step: "runtime" | "identity" | "claude_probe" | "claude_login";
  message: string;
}>;

export interface AltitudeSetupDependencies {
  readonly runtimeProbe: { probe(): Promise<RuntimeProbe> };
  readonly identity: Pick<IdentityCapsuleManager, "ensure">;
  readonly claude: Pick<AgentAdapter, "probe" | "loginLaunchSpec">;
  interactiveCommand(spec: ReturnType<AgentAdapter["loginLaunchSpec"]>): CommandSpec;
  readonly runner: CommandRunner;
}

export interface AltitudeSetupResult {
  readonly runtime: RuntimeProbe;
  readonly identity: IdentityCapsuleEnsureResult;
  readonly claude: AgentProbe;
}

/**
 * First-run setup deliberately authenticates the managed identity capsule. It
 * never imports or even probes the operator's host Claude profile.
 */
export async function setupAltitude(input: {
  readonly config: AltitudeLocalConfig;
  readonly progress?: (progress: SetupProgress) => void;
}, dependencies: AltitudeSetupDependencies): Promise<AltitudeSetupResult> {
  const report = (progress: SetupProgress) => input.progress?.(progress);
  report({ step: "runtime", message: "Checking Microsandbox" });
  const runtime = await dependencies.runtimeProbe.probe();
  if (!runtime.available) throw new Error(`Microsandbox is not ready: ${runtime.detail}`);

  report({ step: "identity", message: "Preparing the node-local identity capsule" });
  const identity = await dependencies.identity.ensure({
    identityId: input.config.identity.id,
    image: input.config.identity.image,
    environment: {
      CLAUDE_CONFIG_DIR: "/identity/claude",
      DISABLE_AUTOUPDATER: "1",
    },
  });

  report({ step: "claude_probe", message: "Checking Claude Code inside the managed identity" });
  let claude = await dependencies.claude.probe();
  if (!claude.installed) throw new Error("Claude Code is not installed in the managed identity image");
  if (claude.auth !== "authenticated") {
    report({
      step: "claude_login",
      message: "Opening Claude inside the persistent identity capsule; finish setup, then exit Claude",
    });
    const exitCode = await dependencies.runner.runInteractive(
      dependencies.interactiveCommand(dependencies.claude.loginLaunchSpec()),
    );
    if (exitCode !== 0) throw new Error(`Claude setup exited with status ${exitCode}`);
    claude = await dependencies.claude.probe();
    if (claude.auth !== "authenticated") {
      throw new Error("Claude login finished, but the managed identity is still unauthenticated");
    }
  }

  return { runtime, identity, claude };
}
