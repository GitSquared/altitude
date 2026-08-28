import { posix } from "node:path";

import type { CommandSpec } from "../process/command.ts";
import { appleExecSpec } from "./apple-container.ts";
import type { RuntimeKind } from "./capsule-runtime.ts";
import { dockerExecSpec } from "./docker.ts";
import { stableCapsuleName } from "./identity-capsule.ts";
import type {
  ProcessExecutionTarget,
  ProcessLaunchSpec,
  ProcessLauncher,
  RunningProcess,
} from "./protocol.ts";

export interface PipedCommandSpawner {
  spawn(command: CommandSpec): RunningProcess;
}

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

/**
 * Translate a typed process boundary into a shell-free runtime CLI command.
 * `exec` cannot add mounts, so identity and source boundaries are fixed when
 * their capsules are created rather than being weakened per process.
 */
export function runtimeProcessCommand(runtime: RuntimeKind, spec: ProcessLaunchSpec): CommandSpec {
  validateLaunch(spec);
  if (spec.target.kind === "host") {
    throw new Error("A capsule process launcher cannot silently execute on the host");
  }
  const execSpec = {
    capsuleId: targetContainer(spec.target),
    command: [spec.executable, ...spec.args],
    ...(spec.cwd === undefined ? {} : { workdir: spec.cwd }),
    ...(spec.env === undefined ? {} : { environment: spec.env }),
    interactive: spec.io === "interactive",
  };
  return runtime === "apple-container" ? appleExecSpec(execSpec) : dockerExecSpec(execSpec);
}

export class BunPipedCommandSpawner implements PipedCommandSpawner {
  spawn(command: CommandSpec): RunningProcess {
    const child = Bun.spawn([command.executable, ...(command.args ?? [])], {
      ...(command.cwd === undefined ? {} : { cwd: command.cwd }),
      env: { ...process.env, ...command.env },
      stdin: "pipe",
      stdout: "pipe",
      stderr: "pipe",
    });
    let stdinClosed = false;
    return {
      stdout: streamBytes(child.stdout),
      stderr: streamBytes(child.stderr),
      exited: child.exited.then((exitCode) => ({
        exitCode,
        ...(child.signalCode === null ? {} : { signal: child.signalCode }),
      })),
      async writeStdin(chunk) {
        if (stdinClosed) throw new Error("Process stdin is closed");
        child.stdin.write(chunk);
        await child.stdin.flush();
      },
      async closeStdin() {
        if (stdinClosed) return;
        stdinClosed = true;
        await child.stdin.end();
      },
      async terminate(signal = "SIGTERM") {
        child.kill(signal);
      },
    };
  }
}

async function* streamBytes(stream: ReadableStream<Uint8Array>): AsyncIterable<Uint8Array> {
  const reader = stream.getReader();
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) return;
      yield value;
    }
  } finally {
    reader.releaseLock();
  }
}

export class CapsuleProcessLauncher implements ProcessLauncher {
  constructor(
    readonly runtime: RuntimeKind,
    private readonly spawner: PipedCommandSpawner = new BunPipedCommandSpawner(),
  ) {}

  launch(spec: ProcessLaunchSpec & { readonly io: "piped" }): Promise<RunningProcess> {
    return Promise.resolve(this.spawner.spawn(runtimeProcessCommand(this.runtime, spec)));
  }

  interactiveCommand(spec: ProcessLaunchSpec & { readonly io: "interactive" }): CommandSpec {
    return runtimeProcessCommand(this.runtime, spec);
  }
}
