import { randomUUID } from "node:crypto";
import {
  chmod,
  copyFile,
  lstat,
  mkdir,
  readFile,
  rm,
  writeFile,
} from "node:fs/promises";
import { join } from "node:path";

const MAX_EXECUTABLE_BYTES = 256 * 1024 * 1024;

export const STANDALONE_IDENTITY_CONTAINERFILE = `# syntax=docker/dockerfile:1.7

FROM node:24.6.0-bookworm-slim

ARG CLAUDE_CODE_VERSION=2.1.248

RUN apt-get update \\
    && apt-get install -y --no-install-recommends ca-certificates git \\
    && npm install --global "@anthropic-ai/claude-code@\${CLAUDE_CODE_VERSION}" \\
    && npm cache clean --force \\
    && rm -rf /var/lib/apt/lists/* \\
    && install -d -o 1000 -g 1000 -m 0700 /identity /identity/control /identity/claude \\
    && install -d -o 1000 -g 1000 -m 0755 /opt/altitude/bin

COPY --chown=1000:1000 --chmod=0755 altitude /opt/altitude/bin/altitude

ENV PATH="/opt/altitude/bin:\${PATH}" \\
    CLAUDE_CONFIG_DIR=/identity/claude \\
    DISABLE_AUTOUPDATER=1 \\
    HOME=/identity/home

USER 1000:1000
WORKDIR /identity

CMD ["sleep", "infinity"]
`;

export const STANDALONE_TASK_CONTAINERFILE = `# syntax=docker/dockerfile:1.7

FROM oven/bun:1.4.0-debian

USER root
RUN apt-get update \\
    && apt-get install -y --no-install-recommends ca-certificates gh git openssh-client ripgrep \\
    && rm -rf /var/lib/apt/lists/* \\
    && install -d -o 1000 -g 1000 -m 0755 /workspace /opt/altitude/bin

COPY --chown=1000:1000 --chmod=0755 altitude /opt/altitude/bin/altitude

ENV ALTITUDE_CAPSULE=task \\
    HOME=/workspace/.altitude/environment/home \\
    XDG_CONFIG_HOME=/workspace/.altitude/environment/home/.config \\
    XDG_DATA_HOME=/workspace/.altitude/environment/home/.local/share \\
    XDG_STATE_HOME=/workspace/.altitude/environment/home/.local/state \\
    XDG_CACHE_HOME=/tmp/altitude-cache \\
    BUN_INSTALL=/workspace/.altitude/environment/home/.bun \\
    BUN_INSTALL_CACHE_DIR=/tmp/bun-install-cache \\
    NPM_CONFIG_CACHE=/tmp/npm-cache \\
    GH_CONFIG_DIR=/workspace/.altitude/node-local/gh \\
    PATH=/workspace/.altitude/environment/bin:/workspace/.altitude/environment/home/.local/bin:/workspace/.altitude/environment/home/.bun/bin:/opt/altitude/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

USER 1000:1000
WORKDIR /workspace

CMD ["sleep", "infinity"]
`;

export interface StandaloneImageContext {
  readonly directory: string;
  cleanup(): Promise<void>;
}

export async function isAltitudeSourceCheckout(directory: string): Promise<boolean> {
  try {
    const root = await lstat(directory);
    if (root.isSymbolicLink() || !root.isDirectory()) return false;
    const required = [
      "package.json",
      "bun.lock",
      "tsconfig.json",
      join("src", "cli.ts"),
      join("images", "identity", "Containerfile"),
      join("images", "task", "Containerfile"),
    ];
    for (const relative of required) {
      const stat = await lstat(join(directory, relative));
      if (stat.isSymbolicLink() || !stat.isFile()) return false;
    }
    const manifest = JSON.parse(
      await readFile(join(directory, "package.json"), "utf8"),
    ) as { name?: unknown };
    return manifest.name === "altitude";
  } catch {
    return false;
  }
}

/**
 * Builds a minimal OCI context around the currently installed Altitude binary.
 * This keeps node setup independent from a source checkout without embedding an
 * identity, credential, or task workspace in the executable or image context.
 */
export async function prepareStandaloneImageContext(input: {
  readonly parentDirectory: string;
  readonly executable: string;
  readonly platform?: NodeJS.Platform;
}): Promise<StandaloneImageContext> {
  if ((input.platform ?? process.platform) !== "linux") {
    throw new Error(
      "Installed-binary capsule builds currently require Linux; use --source with an Altitude checkout on this platform",
    );
  }
  const source = await lstat(input.executable);
  if (source.isSymbolicLink() || !source.isFile()) {
    throw new Error("Standalone Altitude executable must be a regular file");
  }
  if (source.size < 1 || source.size > MAX_EXECUTABLE_BYTES) {
    throw new Error("Standalone Altitude executable size is invalid");
  }
  await mkdir(input.parentDirectory, { recursive: true, mode: 0o700 });
  const parent = await lstat(input.parentDirectory);
  if (parent.isSymbolicLink() || !parent.isDirectory()) {
    throw new Error("Standalone image cache must be a regular directory");
  }
  await chmod(input.parentDirectory, 0o700);
  const directory = join(
    input.parentDirectory,
    `.context-${randomUUID()}`,
  );
  await mkdir(join(directory, "images", "identity"), {
    recursive: true,
    mode: 0o700,
  });
  await mkdir(join(directory, "images", "task"), {
    recursive: true,
    mode: 0o700,
  });
  try {
    const executable = join(directory, "altitude");
    await copyFile(input.executable, executable);
    await chmod(executable, 0o700);
    await writeFile(
      join(directory, "images", "identity", "Containerfile"),
      STANDALONE_IDENTITY_CONTAINERFILE,
      { flag: "wx", mode: 0o600 },
    );
    await writeFile(
      join(directory, "images", "task", "Containerfile"),
      STANDALONE_TASK_CONTAINERFILE,
      { flag: "wx", mode: 0o600 },
    );
    await writeFile(join(directory, ".dockerignore"), [
      "*",
      "!altitude",
      "!images/",
      "!images/identity/",
      "!images/identity/Containerfile",
      "!images/task/",
      "!images/task/Containerfile",
      "",
    ].join("\n"), { flag: "wx", mode: 0o600 });
  } catch (error) {
    await rm(directory, { recursive: true, force: true });
    throw error;
  }
  let cleaned = false;
  return {
    directory,
    async cleanup() {
      if (cleaned) return;
      cleaned = true;
      await rm(directory, { recursive: true, force: true });
    },
  };
}
