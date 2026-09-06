import { mkdir, mkdtemp, rm } from "node:fs/promises";
import { join } from "node:path";

import { BunCommandRunner, type CommandRunner, type CommandSpec } from "../process/command.ts";

export interface FactoryGitRepository {
  readonly path: string;
  cleanup(): Promise<void>;
}

const FACTORY_AUTHOR = "Altitude Factory";
const FACTORY_EMAIL = "factory@altitude.local";

function factoryEnvironment(): Readonly<Record<string, string>> {
  return {
    GIT_CONFIG_NOSYSTEM: "1",
    GIT_CONFIG_GLOBAL: "/dev/null",
    GIT_TERMINAL_PROMPT: "0",
    GIT_AUTHOR_NAME: FACTORY_AUTHOR,
    GIT_AUTHOR_EMAIL: FACTORY_EMAIL,
    GIT_COMMITTER_NAME: FACTORY_AUTHOR,
    GIT_COMMITTER_EMAIL: FACTORY_EMAIL,
    LC_ALL: "C",
  };
}

async function required(
  runner: Pick<CommandRunner, "run">,
  spec: CommandSpec,
  action: string,
): Promise<void> {
  const result = await runner.run(spec);
  if (result.exitCode !== 0) {
    throw new Error(`${action} failed (${result.exitCode}): ${result.stderr.trim()}`);
  }
}

/**
 * Create the seed for a task that begins inside Altitude rather than from a
 * host checkout. The repository is intentionally content-free and all Git
 * identity/configuration used to create it is isolated from the host.
 */
export async function createFactoryGitRepository(input: {
  readonly parentDirectory: string;
  readonly taskId: string;
  readonly runner?: Pick<CommandRunner, "run">;
}): Promise<FactoryGitRepository> {
  if (!/^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/u.test(input.taskId)) {
    throw new Error("Factory task id is invalid");
  }
  await mkdir(input.parentDirectory, { recursive: true, mode: 0o700 });
  const directory = await mkdtemp(join(input.parentDirectory, "task-"));
  const runner = input.runner ?? new BunCommandRunner();
  const env = factoryEnvironment();
  let ready = false;
  try {
    await required(runner, {
      executable: "git",
      args: ["init", "--initial-branch", "main", "."],
      cwd: directory,
      env,
    }, "Initializing the factory repository");
    await required(runner, {
      executable: "git",
      args: ["config", "--local", "commit.gpgsign", "false"],
      cwd: directory,
      env,
    }, "Disabling inherited commit signing");
    await required(runner, {
      executable: "git",
      args: ["commit", "--allow-empty", "--no-gpg-sign", "-m", "Initialize task capsule"],
      cwd: directory,
      env,
    }, "Creating the factory root commit");
    await required(runner, {
      executable: "git",
      args: ["remote", "add", "origin", `altitude://unbound/${encodeURIComponent(input.taskId)}`],
      cwd: directory,
      env,
    }, "Binding the unconfigured factory origin");
    ready = true;
    let cleaned = false;
    return {
      path: directory,
      cleanup: async () => {
        if (cleaned) return;
        cleaned = true;
        await rm(directory, { recursive: true, force: true });
      },
    };
  } finally {
    if (!ready) await rm(directory, { recursive: true, force: true });
  }
}
