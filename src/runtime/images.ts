import { isAbsolute, relative, resolve, sep } from "node:path";

import type { CommandRunner, CommandSpec } from "../process/command.ts";
import type { RuntimeKind } from "./capsule-runtime.ts";

const IMAGE = /^[^\s\u0000]+$/u;

export interface CapsuleImageBuild {
  readonly image: string;
  readonly contextDirectory: string;
  readonly containerfile: string;
  readonly buildArguments?: Readonly<Record<string, string>>;
}

function normalizedBuild(input: CapsuleImageBuild): CapsuleImageBuild {
  if (!IMAGE.test(input.image)) throw new Error("Capsule image name is invalid");
  if (!isAbsolute(input.contextDirectory)) throw new Error("Image build context must be an absolute path");
  const contextDirectory = resolve(input.contextDirectory);
  const containerfile = resolve(input.containerfile);
  const fromContext = relative(contextDirectory, containerfile);
  if (fromContext === "" || fromContext === ".." || fromContext.startsWith(`..${sep}`) || isAbsolute(fromContext)) {
    throw new Error("Containerfile must remain inside its build context");
  }
  for (const [key, value] of Object.entries(input.buildArguments ?? {})) {
    if (!/^[A-Z_][A-Z0-9_]*$/u.test(key) || value.includes("\0")) {
      throw new Error(`Image build argument ${key} is invalid`);
    }
  }
  return { ...input, contextDirectory, containerfile };
}

export function capsuleImageBuildCommand(runtime: RuntimeKind, input: CapsuleImageBuild): CommandSpec {
  const build = normalizedBuild(input);
  const buildArguments = Object.entries(build.buildArguments ?? {})
    .sort(([left], [right]) => left.localeCompare(right))
    .flatMap(([key, value]) => ["--build-arg", `${key}=${value}`]);
  return {
    executable: runtime === "apple-container" ? "container" : "docker",
    args: [
      "build",
      "--tag",
      build.image,
      "--file",
      build.containerfile,
      ...buildArguments,
      build.contextDirectory,
    ],
  };
}

export class CapsuleImageBuilder {
  constructor(
    private readonly runtime: RuntimeKind,
    private readonly runner: CommandRunner,
  ) {}

  async build(input: CapsuleImageBuild): Promise<void> {
    const result = await this.runner.run(capsuleImageBuildCommand(this.runtime, input));
    if (result.exitCode !== 0) {
      throw new Error(`Building ${input.image} failed (${result.exitCode}): ${result.stderr.trim()}`);
    }
  }
}
