import { basename, posix } from "node:path";
import {
  AttachOptionsBuilder,
  ExecOptionsBuilder,
  Sandbox,
} from "microsandbox/native";

import type { CommandSpec } from "../process/command.ts";
import { stableCapsuleName } from "./identity-capsule.ts";
import type {
  ProcessExecutionTarget,
  ProcessLaunchSpec,
  ProcessLauncher,
  RunningProcess,
} from "./protocol.ts";

const MICROSANDBOX_ATTACH_PROTOCOL = "altitude.microsandbox-attach/v1" as const;

type MicrosandboxAttachPayload = Readonly<{
  schema: typeof MICROSANDBOX_ATTACH_PROTOCOL;
  capsuleId: string;
  executable: string;
  args: readonly string[];
  cwd?: string;
  env?: Readonly<Record<string, string>>;
}>;

function requireNormalizedDirectory(directory: string, root: string, label: string): void {
  if (!directory.startsWith("/") || posix.normalize(directory) !== directory) {
    throw new Error(`${label} must be a normalized absolute capsule path`);
  }
  if (directory !== root && !directory.startsWith(`${root}/`)) {
    throw new Error(`${label} must remain inside ${root}`);
  }
}

function validateLaunch(spec: ProcessLaunchSpec): void {
  if (!spec.executable.trim() || spec.executable.includes("\0")) {
    throw new Error("Process executable is invalid");
  }
  if (spec.args.some((argument) => argument.includes("\0"))) {
    throw new Error("Process arguments cannot contain NUL bytes");
  }
  if (!spec.purpose.trim()) throw new Error("Process purpose is required");
  for (const [key, value] of Object.entries(spec.env ?? {})) {
    if (!/^[A-Z_][A-Z0-9_]*$/.test(key)) throw new Error(`Invalid environment key: ${key}`);
    if (value.includes("\0")) throw new Error(`Environment value ${key} contains a NUL byte`);
  }
  if (spec.target.kind === "identity-capsule") {
    if (spec.cwd !== undefined) requireNormalizedDirectory(spec.cwd, "/identity", "Identity process cwd");
    if (spec.env?.CLAUDE_CONFIG_DIR !== undefined) {
      requireNormalizedDirectory(spec.env.CLAUDE_CONFIG_DIR, "/identity", "CLAUDE_CONFIG_DIR");
    }
  } else if (spec.target.kind === "task-capsule") {
    if (spec.cwd !== undefined) requireNormalizedDirectory(spec.cwd, "/workspace", "Task process cwd");
    if (spec.env?.CLAUDE_CONFIG_DIR !== undefined) {
      throw new Error("Task capsules cannot receive a Claude identity directory");
    }
  }
}

function targetContainer(target: Exclude<ProcessExecutionTarget, { readonly kind: "host" }>): string {
  return target.kind === "identity-capsule"
    ? stableCapsuleName("identity", target.identityId)
    : stableCapsuleName("task", target.taskId);
}

function selfInvocation(): readonly [string, ...string[]] {
  const script = process.argv[1];
  return basename(process.execPath).startsWith("bun") && script !== undefined
    ? [process.execPath, script]
    : [process.execPath];
}

function microsandboxAttachCommand(spec: ProcessLaunchSpec & { readonly io: "interactive" }): CommandSpec {
  validateLaunch(spec);
  if (spec.target.kind === "host") {
    throw new Error("A capsule process launcher cannot silently execute on the host");
  }
  const payload: MicrosandboxAttachPayload = {
    schema: MICROSANDBOX_ATTACH_PROTOCOL,
    capsuleId: targetContainer(spec.target),
    executable: spec.executable,
    args: [...spec.args],
    ...(spec.cwd === undefined ? {} : { cwd: spec.cwd }),
    ...(spec.env === undefined ? {} : { env: { ...spec.env } }),
  };
  const [executable, ...prefix] = selfInvocation();
  return {
    executable,
    args: [
      ...prefix,
      "_microsandbox-attach",
      Buffer.from(JSON.stringify(payload), "utf8").toString("base64url"),
    ],
  };
}

function parseMicrosandboxAttachPayload(encoded: string | undefined): MicrosandboxAttachPayload {
  if (encoded === undefined || encoded.length > 256 * 1024) {
    throw new Error("Microsandbox attach payload is invalid");
  }
  let value: unknown;
  try {
    value = JSON.parse(Buffer.from(encoded, "base64url").toString("utf8")) as unknown;
  } catch (error) {
    throw new Error("Microsandbox attach payload is invalid", { cause: error });
  }
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error("Microsandbox attach payload is invalid");
  }
  const payload = value as Partial<MicrosandboxAttachPayload>;
  if (payload.schema !== MICROSANDBOX_ATTACH_PROTOCOL
    || typeof payload.capsuleId !== "string"
    || !/^[a-z0-9][a-z0-9-]{0,127}$/u.test(payload.capsuleId)
    || typeof payload.executable !== "string"
    || !Array.isArray(payload.args)
    || payload.args.some((argument) => typeof argument !== "string" || argument.includes("\0"))
    || (payload.cwd !== undefined && typeof payload.cwd !== "string")
    || (payload.env !== undefined && (typeof payload.env !== "object" || payload.env === null))) {
    throw new Error("Microsandbox attach payload is invalid");
  }
  return payload as MicrosandboxAttachPayload;
}

export async function runMicrosandboxAttach(encoded: string | undefined): Promise<number> {
  const payload = parseMicrosandboxAttachPayload(encoded);
  const sandbox = await (await Sandbox.get(payload.capsuleId)).connectOrStart(true);
  const options = new AttachOptionsBuilder()
    .args([...payload.args])
    .detachKeys("ctrl-g,d");
  if (payload.cwd !== undefined) options.cwd(payload.cwd);
  if (payload.env !== undefined) options.envs({ ...payload.env });
  return await sandbox.attachWithBuilder(payload.executable, options);
}

async function attachMicrosandboxProcess(
  spec: ProcessLaunchSpec & { readonly io: "interactive" },
): Promise<number> {
  validateLaunch(spec);
  if (spec.target.kind === "host") {
    throw new Error("A capsule process launcher cannot silently execute on the host");
  }
  const sandbox = await (await Sandbox.get(targetContainer(spec.target))).connectOrStart(true);
  const options = new AttachOptionsBuilder().args([...spec.args]);
  if (spec.cwd !== undefined) options.cwd(spec.cwd);
  if (spec.env !== undefined) options.envs({ ...spec.env });
  return await sandbox.attachWithBuilder(spec.executable, options);
}

class ByteQueue implements AsyncIterable<Uint8Array> {
  readonly #values: Uint8Array[] = [];
  readonly #waiters: Array<() => void> = [];
  #closed = false;
  #error: unknown;

  push(value: Uint8Array): void {
    if (this.#closed) return;
    this.#values.push(value);
    this.#wake();
  }

  close(error?: unknown): void {
    this.#closed = true;
    this.#error = error;
    this.#wake();
  }

  async *[Symbol.asyncIterator](): AsyncIterator<Uint8Array> {
    while (true) {
      const value = this.#values.shift();
      if (value !== undefined) {
        yield value;
        continue;
      }
      if (this.#closed) {
        if (this.#error !== undefined) throw this.#error;
        return;
      }
      await new Promise<void>((resolve) => this.#waiters.push(resolve));
    }
  }

  #wake(): void {
    for (const waiter of this.#waiters.splice(0)) waiter();
  }
}

async function launchMicrosandboxProcess(
  spec: ProcessLaunchSpec & { readonly io: "piped" },
): Promise<RunningProcess> {
  validateLaunch(spec);
  if (spec.target.kind === "host") {
    throw new Error("A capsule process launcher cannot silently execute on the host");
  }
  const sandbox = await (await Sandbox.get(targetContainer(spec.target)))
    .connectOrStart(true);
  const options = new ExecOptionsBuilder().args([...spec.args]).stdinPipe();
  if (spec.cwd !== undefined) options.cwd(spec.cwd);
  if (spec.env !== undefined) options.envs({ ...spec.env });
  const handle = await sandbox.execStreamWithBuilder(spec.executable, options);
  const stdin = await handle.takeStdin();
  if (stdin === null) throw new Error("Microsandbox did not expose process stdin");
  const stdout = new ByteQueue();
  const stderr = new ByteQueue();
  const exited = (async () => {
    let exitCode: number | undefined;
    try {
      while (true) {
        const event = await handle.recv();
        if (event === null) break;
        if (event.eventType === "stdout" && event.data !== undefined) stdout.push(event.data);
        else if (event.eventType === "stderr" && event.data !== undefined) stderr.push(event.data);
        else if (event.eventType === "exited" && event.code !== undefined) exitCode = event.code;
      }
      if (exitCode === undefined) exitCode = (await handle.wait()).code;
      stdout.close();
      stderr.close();
      return { exitCode };
    } catch (error) {
      stdout.close(error);
      stderr.close(error);
      throw error;
    }
  })();
  let stdinClosed = false;
  return {
    stdout,
    stderr,
    exited,
    async writeStdin(chunk) {
      if (stdinClosed) throw new Error("Process stdin is closed");
      await stdin.write(Buffer.from(chunk));
    },
    async closeStdin() {
      if (stdinClosed) return;
      stdinClosed = true;
      await stdin.close();
    },
    async terminate(signal = "SIGTERM") {
      if (signal === "SIGKILL") await handle.kill();
      else await handle.signal(signal === "SIGINT" ? 2 : 15);
    },
  };
}

export class CapsuleProcessLauncher implements ProcessLauncher {
  launch(spec: ProcessLaunchSpec & { readonly io: "piped" }): Promise<RunningProcess> {
    return launchMicrosandboxProcess(spec);
  }

  interactiveCommand(spec: ProcessLaunchSpec & { readonly io: "interactive" }): CommandSpec {
    return microsandboxAttachCommand(spec);
  }

  /**
   * Attach the current process directly to a capsule. Setup uses this path so
   * native login input does not cross Altitude's detached-session relay.
   */
  attach(spec: ProcessLaunchSpec & { readonly io: "interactive" }): Promise<number> {
    return attachMicrosandboxProcess(spec);
  }
}
