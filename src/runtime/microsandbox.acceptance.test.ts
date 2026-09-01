import { describe, expect, test } from "bun:test";

import { Volume } from "microsandbox/native";

import { ClaudeCliAdapter } from "../agent/claude-cli.ts";
import { defaultLocalConfig, DEFAULT_IDENTITY_IMAGE, DEFAULT_TASK_IMAGE } from "../config/local.ts";
import { setupAltitude } from "../setup.ts";
import { SqliteAgentWriterLeaseStore } from "../storage/agent-writer-lease-store.ts";
import { TaskStore } from "../storage/task-store.ts";
import { identityVolumeName, stableCapsuleName } from "./identity-capsule.ts";
import { IdentityCapsuleManager } from "./identity-manager.ts";
import { MicrosandboxCapsuleRuntime } from "./microsandbox.ts";
import { CapsuleProcessLauncher } from "./process-launcher.ts";
import type { ProcessLaunchSpec } from "./protocol.ts";
import { taskVolumeName, TaskCapsuleManager } from "./task-manager.ts";

const enabled = process.env.ALTITUDE_ACCEPTANCE_MICROSANDBOX === "1";

async function output(
  runtime: MicrosandboxCapsuleRuntime,
  capsuleId: string,
  command: readonly string[],
  workdir: string,
): Promise<string> {
  const result = await runtime.exec({ capsuleId, command, workdir });
  if (typeof result === "number") throw new Error(`Unexpected interactive exit status ${result}`);
  expect(result.exitCode).toBe(0);
  expect(result.stderr).toBe("");
  return result.stdout.trim();
}

describe("Microsandbox capsule images", () => {
  test.skipIf(!enabled)("boot the real identity and task images without host inheritance", async () => {
    const suffix = `${process.pid}-${Date.now()}`;
    const identityId = `acceptance-${suffix}`;
    const taskId = `acceptance-${suffix}`;
    const identityCapsuleId = stableCapsuleName("identity", identityId);
    const taskCapsuleId = stableCapsuleName("task", taskId);
    const runtime = new MicrosandboxCapsuleRuntime();

    try {
      expect(await runtime.probe()).toEqual({
        available: true,
        detail: "Microsandbox runtime is ready",
      });

      expect(await new IdentityCapsuleManager(runtime).ensure({
        identityId,
        image: DEFAULT_IDENTITY_IMAGE,
      })).toMatchObject({ capsuleId: identityCapsuleId, state: "created" });
      expect(await output(runtime, identityCapsuleId, ["id", "-u"], "/identity")).toBe("1000");
      expect(await output(runtime, identityCapsuleId, ["altitude", "version"], "/identity")).toBe("0.0.0");
      expect(await output(runtime, identityCapsuleId, ["claude", "--version"], "/identity")).toMatch(/^2\./u);
      expect(await output(runtime, identityCapsuleId, ["printenv", "HOME"], "/identity")).toBe("/identity/home");
      expect(await output(runtime, identityCapsuleId, ["printenv", "CLAUDE_CONFIG_DIR"], "/identity"))
        .toBe("/identity/claude");

      using store = new TaskStore(":memory:");
      const claude = new ClaudeCliAdapter({
        identity: {
          id: identityId,
          configDirectory: "/identity/claude",
          loginDirectory: "/identity",
        },
        launcher: new CapsuleProcessLauncher(),
        leases: new SqliteAgentWriterLeaseStore(store.database),
      });
      expect(await claude.probe()).toMatchObject({
        installed: true,
        auth: "unauthenticated",
      });
      const loginLaunches: ProcessLaunchSpec[] = [];
      const progress: string[] = [];
      await expect(setupAltitude({
        config: {
          ...defaultLocalConfig(),
          identity: { id: identityId, image: DEFAULT_IDENTITY_IMAGE },
        },
        progress: (event) => progress.push(event.step),
      }, {
        runtimeProbe: runtime,
        identity: new IdentityCapsuleManager(runtime),
        claude,
        login: async (launch) => {
          loginLaunches.push(launch);
          return 17;
        },
      })).rejects.toThrow("Claude setup exited with status 17");
      expect(progress).toEqual(["runtime", "identity", "claude_probe", "claude_login"]);
      expect(loginLaunches).toEqual([expect.objectContaining({
        target: { kind: "identity-capsule", identityId },
        executable: "claude",
        args: [],
        cwd: "/identity",
        io: "interactive",
      })]);

      expect(await new TaskCapsuleManager(runtime).ensure({
        taskId,
        image: DEFAULT_TASK_IMAGE,
      })).toMatchObject({ capsuleId: taskCapsuleId, state: "created" });
      expect(await output(runtime, taskCapsuleId, ["id", "-u"], "/workspace")).toBe("1000");
      expect(await output(runtime, taskCapsuleId, ["altitude", "version"], "/workspace")).toBe("0.0.0");
      expect(await output(runtime, taskCapsuleId, ["git", "--version"], "/workspace")).toMatch(/^git version /u);
      expect(await output(runtime, taskCapsuleId, ["gh", "--version"], "/workspace")).toMatch(/^gh version /u);
      expect(await output(runtime, taskCapsuleId, ["rg", "--version"], "/workspace")).toMatch(/^ripgrep /u);
      expect(await output(runtime, taskCapsuleId, ["stat", "-c", "%u:%g:%a", "/workspace"], "/workspace"))
        .toBe("1000:1000:700");
    } finally {
      await runtime.destroy(taskCapsuleId).catch(() => undefined);
      await runtime.destroy(identityCapsuleId).catch(() => undefined);
      await Volume.remove(taskVolumeName(taskId)).catch(() => undefined);
      await Volume.remove(identityVolumeName(identityId)).catch(() => undefined);
    }
  }, 60_000);
});
