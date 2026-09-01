import { existsSync, readdirSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import {
  AttachOptionsBuilder,
  ExecOptionsBuilder,
  NetworkPolicyBuilder,
  Sandbox,
  SandboxBuilder,
  setRuntimeLibkrunfwPath,
  setRuntimeMsbPath,
  VolumeBuilder,
} from "microsandbox/native";

import type { CommandResult } from "../process/command.ts";
import {
  assertCapsuleLaunchSpec,
  type CapsuleExecSpec,
  type CapsuleLaunchSpec,
  type CapsuleRuntime,
  type RuntimeProbe,
} from "./capsule-runtime.ts";

const SIZE = /^([1-9][0-9]*)(K|M|G|T)$/iu;
let runtimeConfigured = false;

export const MICROSANDBOX_GUEST_IPV4 = "172.16.0.6";
export const MICROSANDBOX_HOST_IPV4 = "172.16.0.5";

function platformPackage(): string {
  if (process.platform === "darwin" && process.arch === "arm64") return "darwin-arm64";
  if (process.platform === "linux" && process.arch === "x64") return "linux-x64-gnu";
  if (process.platform === "linux" && process.arch === "arm64") return "linux-arm64-gnu";
  if (process.platform === "win32" && process.arch === "x64") return "win32-x64-msvc";
  if (process.platform === "win32" && process.arch === "arm64") return "win32-arm64-msvc";
  throw new Error(`Microsandbox does not support ${process.platform}/${process.arch}`);
}

/**
 * The raw native entrypoint is compatible with Bun's standalone compiler, but
 * unlike the Node wrapper it does not locate the platform package's runtime
 * sidecars. Resolve those once without mutating PATH or requiring a daemon.
 */
export function configureMicrosandboxRuntime(): void {
  if (runtimeConfigured) return;
  const packageName = `@superradcompany/microsandbox-${platformPackage()}/package.json`;
  const bases = [import.meta.url, `file://${process.execPath}`, `file://${process.cwd()}/`];
  let packagePath: string | undefined;
  for (const base of bases) {
    try {
      packagePath = createRequire(base).resolve(packageName);
      break;
    } catch {
      // An installed compiled binary resolves from its own package; source
      // execution usually resolves from import.meta.url or the working tree.
    }
  }
  if (packagePath === undefined) {
    throw new Error(`Microsandbox platform package is missing: ${packageName}`);
  }
  const root = dirname(packagePath.startsWith("file:") ? fileURLToPath(packagePath) : packagePath);
  const msb = join(root, "bin", process.platform === "win32" ? "msb.exe" : "msb");
  if (!existsSync(msb)) throw new Error(`Microsandbox runtime binary is missing from ${root}`);
  setRuntimeMsbPath(msb);
  const libraryDirectory = join(root, "lib");
  const libkrunfw = existsSync(libraryDirectory)
    ? readdirSync(libraryDirectory).find((name) => name.startsWith("libkrunfw"))
    : undefined;
  if (libkrunfw !== undefined) setRuntimeLibkrunfwPath(join(libraryDirectory, libkrunfw));
  runtimeConfigured = true;
}

export function sizeInMebibytes(value: string): number {
  const match = SIZE.exec(value);
  if (match === null) throw new Error(`Invalid Microsandbox size: ${value}`);
  const amount = Number(match[1]);
  const multiplier = match[2]!.toUpperCase() === "K" ? 1 / 1024
    : match[2]!.toUpperCase() === "M" ? 1
      : match[2]!.toUpperCase() === "G" ? 1024
        : 1024 * 1024;
  const result = Math.ceil(amount * multiplier);
  if (!Number.isSafeInteger(result) || result < 1) throw new Error(`Invalid Microsandbox size: ${value}`);
  return result;
}

async function runningSandbox(name: string): Promise<Sandbox> {
  const handle = await Sandbox.get(name);
  return await handle.connectOrStart(true);
}

export function isMicrosandboxError(error: unknown, name: string): boolean {
  if (typeof error !== "object" || error === null) return false;
  const normalized = name.toLowerCase();
  return ("name" in error && typeof error.name === "string" && error.name.toLowerCase().includes(normalized))
    || ("code" in error && typeof error.code === "string" && error.code.toLowerCase().includes(normalized))
    || ("message" in error && typeof error.message === "string"
      && error.message.toLowerCase().includes(`[${normalized}]`));
}

/**
 * Altitude's only v1 isolation backend. Microsandbox embeds the runtime in the
 * application process, so a node does not need Docker, a daemon, or a
 * platform-specific container branch.
 */
export class MicrosandboxCapsuleRuntime implements CapsuleRuntime {
  async probe(): Promise<RuntimeProbe> {
    try {
      configureMicrosandboxRuntime();
      await Sandbox.list();
      return { available: true, detail: "Microsandbox runtime is ready" };
    } catch (error) {
      return {
        available: false,
        detail: error instanceof Error ? error.message : String(error),
      };
    }
  }

  async prepare(spec: CapsuleLaunchSpec): Promise<void> {
    configureMicrosandboxRuntime();
    assertCapsuleLaunchSpec(spec);
    try {
      await new VolumeBuilder(spec.volume.name)
        .directory()
        .quota(sizeInMebibytes(spec.resources.volumeSize))
        .label("dev.altitude.volume", spec.volume.name)
        .create();
    } catch (error) {
      if (!isMicrosandboxError(error, "VolumeAlreadyExists")) throw error;
    }
  }

  async ensure(spec: CapsuleLaunchSpec): Promise<"created" | "resumed" | "already_running"> {
    configureMicrosandboxRuntime();
    assertCapsuleLaunchSpec(spec);
    await this.prepare(spec);
    try {
      const handle = await Sandbox.get(spec.capsuleId);
      const status = handle.status;
      await handle.connectOrStart(true);
      return status === "running" ? "already_running" : "resumed";
    } catch (error) {
      if (!isMicrosandboxError(error, "SandboxNotFound")) throw error;
    }
    const builder = new SandboxBuilder(spec.capsuleId)
      .image(spec.image)
      .cpus(spec.resources.cpus)
      .memory(sizeInMebibytes(spec.resources.memory))
      .detached(true)
      .ephemeral(false)
      .workdir(spec.volume.mountPath)
      .security("restricted")
      .network((network) => network
        .enabled(true)
        .interface((networkInterface) => networkInterface.ipv4(MICROSANDBOX_GUEST_IPV4))
        .policyFromBuilder(new NetworkPolicyBuilder().defaultAllow()))
      .rlimit("nproc", spec.resources.pids)
      .labels({
        "dev.altitude.capsule": spec.capsuleId,
        "dev.altitude.runtime": "microsandbox",
      })
      .volume(spec.volume.mountPath, (mount) => mount
        .named(spec.volume.name)
        .owner(1000, 1000));
    if (spec.environment !== undefined) builder.envs({ ...spec.environment });
    await builder.create();
    return "created";
  }

  async exec(spec: CapsuleExecSpec): Promise<CommandResult | number> {
    configureMicrosandboxRuntime();
    if (spec.command.length === 0) throw new Error("Exec command is required");
    const [executable, ...args] = spec.command;
    const sandbox = await runningSandbox(spec.capsuleId);
    if (spec.interactive === true) {
      const options = new AttachOptionsBuilder().args(args).detachKeys("ctrl-g,d");
      if (spec.workdir !== undefined) options.cwd(spec.workdir);
      if (spec.environment !== undefined) options.envs({ ...spec.environment });
      return await sandbox.attachWithBuilder(executable!, options);
    }
    const options = new ExecOptionsBuilder().args(args);
    if (spec.workdir !== undefined) options.cwd(spec.workdir);
    if (spec.environment !== undefined) options.envs({ ...spec.environment });
    const output = await sandbox.execWithBuilder(executable!, options);
    return {
      exitCode: output.code,
      stdout: output.stdout(),
      stderr: output.stderr(),
    };
  }

  async stop(capsuleId: string): Promise<void> {
    configureMicrosandboxRuntime();
    try {
      await (await Sandbox.get(capsuleId)).stopWithTimeout(30_000);
    } catch (error) {
      if (!isMicrosandboxError(error, "SandboxNotFound")) throw error;
    }
  }

  async destroy(capsuleId: string): Promise<void> {
    configureMicrosandboxRuntime();
    try {
      await (await Sandbox.get(capsuleId)).destroy();
    } catch (error) {
      if (!isMicrosandboxError(error, "SandboxNotFound")) throw error;
    }
  }
}
