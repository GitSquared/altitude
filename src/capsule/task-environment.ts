import { constants } from "node:fs";
import {
  chmod,
  lstat,
  mkdir,
  open,
  readFile,
  readdir,
  readlink,
  symlink,
  writeFile,
} from "node:fs/promises";
import { dirname, join, relative, resolve, sep } from "node:path";

import { compareCanonicalStrings, stableJson, utf8 } from "./canonical.ts";
import type { CapsuleMemberContent } from "./manifest.ts";
import { portableCapsulePath, type PortableCapsulePath } from "./paths.ts";

export const TASK_ENVIRONMENT_ROOT = "/workspace/.altitude/environment" as const;
export const TASK_ENVIRONMENT_HOME = `${TASK_ENVIRONMENT_ROOT}/home` as const;
export const TASK_ENVIRONMENT_BIN = `${TASK_ENVIRONMENT_ROOT}/bin` as const;
export const TASK_NODE_LOCAL_ROOT = "/workspace/.altitude/node-local" as const;
export const TASK_ENVIRONMENT_METADATA_MEMBER = portableCapsulePath("workspace/environment.json");

const TASK_ENVIRONMENT_FORMAT = "altitude.task-environment/v1" as const;
const ENVIRONMENT_DIRECTORY = ".altitude/environment";
const ENVIRONMENT_FILES_PREFIX = "workspace/environment/files/";
const MAX_ENTRIES = 20_000;
const MAX_BYTES = 512 * 1024 * 1024;

type EnvironmentEntry =
  | Readonly<{ path: string; kind: "directory" }>
  | Readonly<{ path: string; kind: "file"; executable: boolean; member: PortableCapsulePath }>
  | Readonly<{ path: string; kind: "symlink"; destination: string }>;

type EnvironmentMetadata = Readonly<{
  format: typeof TASK_ENVIRONMENT_FORMAT;
  entries: readonly EnvironmentEntry[];
}>;

function inside(root: string, candidate: string): boolean {
  const fromRoot = relative(root, candidate);
  return fromRoot === "" || (fromRoot !== ".." && !fromRoot.startsWith(`..${sep}`));
}

function environmentMember(path: string): PortableCapsulePath {
  return portableCapsulePath(`${ENVIRONMENT_FILES_PREFIX}${path}`);
}

async function requireDirectory(path: string, label: string): Promise<void> {
  const stat = await lstat(path);
  if (stat.isSymbolicLink() || !stat.isDirectory()) throw new Error(`${label} is unsafe`);
}

async function ensureDirectory(path: string, mode = 0o700): Promise<void> {
  try {
    await mkdir(path, { mode });
  } catch (error) {
    if (typeof error !== "object" || error === null || !("code" in error) || error.code !== "EEXIST") throw error;
    await requireDirectory(path, "Task environment directory");
  }
  await chmod(path, mode);
}

async function ensureGitExclude(root: string): Promise<void> {
  const git = join(root, ".git");
  await requireDirectory(git, "Task Git directory");
  const info = join(git, "info");
  await ensureDirectory(info);
  const exclude = join(info, "exclude");
  let current = "";
  try {
    const stat = await lstat(exclude);
    if (stat.isSymbolicLink() || !stat.isFile()) throw new Error("Task Git exclude file is unsafe");
    current = await readFile(exclude, "utf8");
  } catch (error) {
    if (typeof error !== "object" || error === null || !("code" in error) || error.code !== "ENOENT") throw error;
  }
  if (current.split(/\r?\n/u).includes("/.altitude/")) return;
  const prefix = current === "" || current.endsWith("\n") ? current : `${current}\n`;
  await writeFile(exclude, `${prefix}/.altitude/\n`, { mode: 0o600 });
}

/** Establish the reserved, non-Git workbench directory inside a task volume. */
export async function initializeTaskEnvironment(rootInput: string): Promise<void> {
  const root = resolve(rootInput);
  await requireDirectory(root, "Task workspace");
  await ensureDirectory(join(root, ".altitude"));
  await ensureDirectory(join(root, ENVIRONMENT_DIRECTORY));
  await ensureDirectory(join(root, ENVIRONMENT_DIRECTORY, "home"));
  await ensureDirectory(join(root, ENVIRONMENT_DIRECTORY, "bin"));
  await ensureDirectory(join(root, ".altitude", "node-local"));
  await ensureGitExclude(root);
}

async function readRegularFile(path: string, expectedSize: number): Promise<Uint8Array> {
  const handle = await open(path, constants.O_RDONLY | constants.O_NOFOLLOW);
  try {
    const stat = await handle.stat();
    if (!stat.isFile() || stat.size !== expectedSize) throw new Error("Task environment file changed during capture");
    return new Uint8Array(await handle.readFile());
  } finally {
    await handle.close();
  }
}

/** Capture only portable workbench material; known credential locations fail closed. */
export async function captureTaskEnvironment(rootInput: string): Promise<readonly CapsuleMemberContent[]> {
  const root = resolve(rootInput);
  await initializeTaskEnvironment(root);
  const environment = join(root, ENVIRONMENT_DIRECTORY);
  const entries: EnvironmentEntry[] = [];
  const members: CapsuleMemberContent[] = [];
  let totalBytes = 0;

  const visit = async (directory: string, relativeDirectory: string): Promise<void> => {
    const children = await readdir(directory, { withFileTypes: true });
    children.sort((left, right) => compareCanonicalStrings(left.name, right.name));
    for (const child of children) {
      const path = relativeDirectory === "" ? child.name : `${relativeDirectory}/${child.name}`;
      environmentMember(path); // validates the portable and credential boundary for every entry
      if (++totalEntries > MAX_ENTRIES) throw new Error(`Task environment exceeds ${MAX_ENTRIES} entries`);
      const absolute = join(directory, child.name);
      const stat = await lstat(absolute);
      if (stat.isSymbolicLink()) {
        const target = await readlink(absolute);
        const destinationAbsolute = resolve(dirname(absolute), target);
        if (!inside(environment, destinationAbsolute) || destinationAbsolute === environment) {
          throw new Error(`Task environment symlink ${path} escapes the portable workbench`);
        }
        const destination = relative(environment, destinationAbsolute).split(sep).join("/");
        environmentMember(destination);
        entries.push({ path, kind: "symlink", destination });
      } else if (stat.isDirectory()) {
        entries.push({ path, kind: "directory" });
        await visit(absolute, path);
      } else if (stat.isFile()) {
        totalBytes += stat.size;
        if (totalBytes > MAX_BYTES) throw new Error(`Task environment exceeds ${MAX_BYTES} bytes`);
        const member = environmentMember(path);
        entries.push({ path, kind: "file", executable: (stat.mode & 0o111) !== 0, member });
        members.push({ path: member, bytes: await readRegularFile(absolute, stat.size) });
      } else {
        throw new Error(`Task environment entry ${path} is not a regular file, directory, or symlink`);
      }
    }
  };

  let totalEntries = 0;
  await visit(environment, "");
  entries.sort((left, right) => compareCanonicalStrings(left.path, right.path));
  const metadata: EnvironmentMetadata = { format: TASK_ENVIRONMENT_FORMAT, entries };
  return [
    { path: TASK_ENVIRONMENT_METADATA_MEMBER, bytes: utf8(`${stableJson(metadata)}\n`) },
    ...members.sort((left, right) => compareCanonicalStrings(left.path, right.path)),
  ];
}

function parseMetadata(bytes: Uint8Array): EnvironmentMetadata {
  let value: unknown;
  try {
    value = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes)) as unknown;
  } catch (error) {
    throw new Error("Task environment metadata is not valid UTF-8 JSON", { cause: error });
  }
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw new Error("Task environment metadata is invalid");
  const metadata = value as { format?: unknown; entries?: unknown };
  if (metadata.format !== TASK_ENVIRONMENT_FORMAT || !Array.isArray(metadata.entries)) {
    throw new Error("Task environment metadata schema is invalid");
  }
  const entries: EnvironmentEntry[] = [];
  let previous: string | undefined;
  for (const raw of metadata.entries) {
    if (typeof raw !== "object" || raw === null || Array.isArray(raw)) throw new Error("Task environment entry is invalid");
    const entry = raw as Record<string, unknown>;
    if (typeof entry.path !== "string") throw new Error("Task environment entry path is invalid");
    environmentMember(entry.path);
    if (previous !== undefined && compareCanonicalStrings(previous, entry.path) >= 0) {
      throw new Error("Task environment entries must be uniquely sorted");
    }
    previous = entry.path;
    if (entry.kind === "directory") {
      entries.push({ path: entry.path, kind: "directory" });
    } else if (entry.kind === "file" && typeof entry.member === "string" && typeof entry.executable === "boolean") {
      const member = portableCapsulePath(entry.member);
      if (member !== environmentMember(entry.path)) throw new Error(`Task environment file member for ${entry.path} is invalid`);
      entries.push({ path: entry.path, kind: "file", executable: entry.executable, member });
    } else if (entry.kind === "symlink" && typeof entry.destination === "string") {
      environmentMember(entry.destination);
      entries.push({ path: entry.path, kind: "symlink", destination: entry.destination });
    } else {
      throw new Error(`Task environment entry ${entry.path} is invalid`);
    }
  }
  if (entries.length > MAX_ENTRIES) throw new Error(`Task environment exceeds ${MAX_ENTRIES} entries`);
  return { format: TASK_ENVIRONMENT_FORMAT, entries };
}

async function absent(path: string): Promise<boolean> {
  try {
    await lstat(path);
    return false;
  } catch (error) {
    if (typeof error === "object" && error !== null && "code" in error && error.code === "ENOENT") return true;
    throw error;
  }
}

/** Restore a verified workbench snapshot into a newly cloned task workspace. */
export async function restoreTaskEnvironment(
  rootInput: string,
  membersInput: readonly CapsuleMemberContent[],
): Promise<void> {
  const root = resolve(rootInput);
  const byPath = new Map(membersInput.map((member) => [member.path, member.bytes] as const));
  const metadataBytes = byPath.get(TASK_ENVIRONMENT_METADATA_MEMBER);
  await initializeTaskEnvironment(root);
  if (metadataBytes === undefined) {
    const orphan = membersInput.find((member) => member.path.startsWith(ENVIRONMENT_FILES_PREFIX));
    if (orphan !== undefined) throw new Error(`Task environment member ${orphan.path} has no metadata`);
    return;
  }
  const metadata = parseMetadata(metadataBytes);
  const environment = join(root, ENVIRONMENT_DIRECTORY);
  const expectedMembers = new Set<string>([TASK_ENVIRONMENT_METADATA_MEMBER]);
  let totalBytes = 0;

  for (const entry of metadata.entries) {
    const destination = resolve(environment, ...entry.path.split("/"));
    if (!inside(environment, destination)) throw new Error(`Task environment entry ${entry.path} escaped its root`);
    if (entry.kind === "directory") {
      await ensureDirectory(destination);
      continue;
    }
    await ensureDirectory(dirname(destination));
    if (!(await absent(destination))) throw new Error(`Task environment entry ${entry.path} would overwrite restored content`);
    if (entry.kind === "file") {
      expectedMembers.add(entry.member);
      const bytes = byPath.get(entry.member);
      if (bytes === undefined) throw new Error(`Missing task environment member ${entry.member}`);
      totalBytes += bytes.byteLength;
      if (totalBytes > MAX_BYTES) throw new Error(`Task environment exceeds ${MAX_BYTES} bytes`);
      await writeFile(destination, bytes, { flag: "wx", mode: entry.executable ? 0o700 : 0o600 });
      continue;
    }
    const target = resolve(environment, ...entry.destination.split("/"));
    if (!inside(environment, target) || target === environment) throw new Error(`Task environment symlink ${entry.path} escapes its root`);
    const linkTarget = relative(dirname(destination), target) || ".";
    await symlink(linkTarget, destination);
  }

  for (const member of membersInput) {
    if (member.path.startsWith(ENVIRONMENT_FILES_PREFIX) && !expectedMembers.has(member.path)) {
      throw new Error(`Unexpected task environment member ${member.path}`);
    }
  }
}

export function taskEnvironmentVariables(): Readonly<Record<string, string>> {
  return {
    ALTITUDE_CAPSULE: "task",
    HOME: TASK_ENVIRONMENT_HOME,
    XDG_CONFIG_HOME: `${TASK_ENVIRONMENT_HOME}/.config`,
    XDG_DATA_HOME: `${TASK_ENVIRONMENT_HOME}/.local/share`,
    XDG_STATE_HOME: `${TASK_ENVIRONMENT_HOME}/.local/state`,
    XDG_CACHE_HOME: "/tmp/altitude-cache",
    BUN_INSTALL: `${TASK_ENVIRONMENT_HOME}/.bun`,
    BUN_INSTALL_CACHE_DIR: "/tmp/bun-install-cache",
    NPM_CONFIG_CACHE: "/tmp/npm-cache",
    GH_CONFIG_DIR: `${TASK_NODE_LOCAL_ROOT}/gh`,
    PATH: [
      TASK_ENVIRONMENT_BIN,
      `${TASK_ENVIRONMENT_HOME}/.local/bin`,
      `${TASK_ENVIRONMENT_HOME}/.bun/bin`,
      "/opt/altitude/bin",
      "/usr/local/sbin",
      "/usr/local/bin",
      "/usr/sbin",
      "/usr/bin",
      "/sbin",
      "/bin",
    ].join(":"),
  };
}
