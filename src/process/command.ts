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

export interface CommandRunner {
  run(spec: CommandSpec): Promise<CommandResult>;
  runInteractive(spec: Omit<CommandSpec, "stdin">): Promise<number>;
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

  async runInteractive(spec: Omit<CommandSpec, "stdin">): Promise<number> {
    const subprocess = Bun.spawn([spec.executable, ...(spec.args ?? [])], {
      ...(spec.cwd === undefined ? {} : { cwd: spec.cwd }),
      env: commandEnvironment(spec.env),
      stdin: "inherit",
      stdout: "inherit",
      stderr: "inherit",
    });

    return await subprocess.exited;
  }
}

export function formatCommand(spec: Pick<CommandSpec, "executable" | "args">): string {
  return [spec.executable, ...(spec.args ?? [])]
    .map((part) => (/^[A-Za-z0-9_./:@%+=,-]+$/.test(part) ? part : JSON.stringify(part)))
    .join(" ");
}
