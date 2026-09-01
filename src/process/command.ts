import { AsyncLocalStorage } from "node:async_hooks";
import type { ReadStream } from "node:tty";

export interface CommandSpec {
  readonly executable: string;
  readonly args?: readonly string[];
  readonly cwd?: string;
  readonly env?: Readonly<Record<string, string | undefined>>;
  readonly stdin?: string | Uint8Array;
}

export interface CommandResult {
  readonly exitCode: number;
  readonly stdout: string;
  readonly stderr: string;
}

export interface InteractiveCommandHooks {
  /** Called synchronously after the child has spawned successfully. */
  started?(): void;
}

export interface CommandRunner {
  run(spec: CommandSpec): Promise<CommandResult>;
  runInteractive(
    spec: Omit<CommandSpec, "stdin">,
    hooks?: InteractiveCommandHooks,
  ): Promise<number>;
}

const interactiveInput = new AsyncLocalStorage<ReadStream>();

/**
 * Bind the TUI's existing stdin reader to interactive children. The child gets
 * a pipe rather than racing Bun for the terminal's file descriptor.
 */
export async function withInteractiveInput<T>(
  input: ReadStream,
  action: () => Promise<T>,
): Promise<T> {
  return await interactiveInput.run(input, action);
}

function commandEnvironment(
  extra: Readonly<Record<string, string | undefined>> | undefined,
): Record<string, string | undefined> {
  return { ...process.env, ...extra };
}

export class BunCommandRunner implements CommandRunner {
  async run(spec: CommandSpec): Promise<CommandResult> {
    const subprocess = Bun.spawn([spec.executable, ...(spec.args ?? [])], {
      ...(spec.cwd === undefined ? {} : { cwd: spec.cwd }),
      env: commandEnvironment(spec.env),
      stdin: spec.stdin === undefined ? "ignore" : new Blob([spec.stdin]),
      stdout: "pipe",
      stderr: "pipe",
    });

    const [exitCode, stdout, stderr] = await Promise.all([
      subprocess.exited,
      new Response(subprocess.stdout).text(),
      new Response(subprocess.stderr).text(),
    ]);

    return { exitCode, stdout, stderr };
  }

  async runInteractive(
    spec: Omit<CommandSpec, "stdin">,
    hooks?: InteractiveCommandHooks,
  ): Promise<number> {
    const input = interactiveInput.getStore();
    const subprocess = Bun.spawn([spec.executable, ...(spec.args ?? [])], {
      ...(spec.cwd === undefined ? {} : { cwd: spec.cwd }),
      env: commandEnvironment(spec.env),
      stdin: input === undefined ? "inherit" : "pipe",
      stdout: "inherit",
      stderr: "inherit",
    });
    const wasRaw = input?.isRaw === true;
    let inputFailure: unknown;
    const sink = input === undefined ? undefined : subprocess.stdin;
    const forward = (chunk: Buffer | string): void => {
      if (sink === undefined || typeof sink === "number") return;
      try {
        sink.write(typeof chunk === "string" ? Buffer.from(chunk) : chunk);
        sink.flush();
      } catch (error) {
        inputFailure ??= error;
        subprocess.kill("SIGTERM");
      }
    };
    if (input !== undefined) {
      input.setRawMode(true);
      input.on("data", forward);
      input.resume();
    }
    try {
      try {
        hooks?.started?.();
      } catch (error) {
        subprocess.kill("SIGTERM");
        await subprocess.exited.catch(() => undefined);
        throw error;
      }

      const exitCode = await subprocess.exited;
      if (inputFailure !== undefined) throw inputFailure;
      return exitCode;
    } finally {
      if (input !== undefined) {
        input.removeListener("data", forward);
        input.setRawMode(wasRaw);
      }
      if (sink !== undefined && typeof sink !== "number") sink.end();
    }
  }
}

export function formatCommand(spec: Pick<CommandSpec, "executable" | "args">): string {
  return [spec.executable, ...(spec.args ?? [])]
    .map((part) => (/^[A-Za-z0-9_./:@%+=,-]+$/.test(part) ? part : JSON.stringify(part)))
    .join(" ");
}
