import { constants } from "node:fs";
import {
  lstat,
  mkdir,
  mkdtemp,
  open,
  readdir,
  realpath,
  rename,
  rm,
} from "node:fs/promises";
import { dirname, join, relative, resolve, sep } from "node:path";

import {
  MAX_CLAUDE_TRANSCRIPT_BYTES,
  MAX_CLAUDE_TRANSCRIPT_FILES,
  assertResumableClaudeTranscript,
  claudeTranscriptCapsuleMembers,
  createClaudeTranscriptBundle,
  decodeClaudeTranscriptCapsuleMembers,
  decodeClaudeTranscriptWire,
  encodeClaudeTranscriptWire,
  type ClaudeTranscriptBinding,
  type ClaudeTranscriptBundle,
  type ClaudeTranscriptFile,
} from "../agent/transcript.ts";
import type { CapsuleMemberContent } from "../capsule/manifest.ts";
import type {
  ProcessLaunchSpec,
  ProcessLauncher,
  RunningProcess,
} from "../runtime/protocol.ts";

export const IDENTITY_TRANSCRIPT_PROTOCOL =
  "altitude.identity-transcript/v1" as const;

const MAX_HELPER_OUTPUT_BYTES = MAX_CLAUDE_TRANSCRIPT_BYTES + 4 * 1024 * 1024 + 4;
const MAX_ERROR_BYTES = 256 * 1024;
const MAX_DEPTH = 64;

export type IdentityTranscriptImportState = "installed" | "already_present";

function isMissing(error: unknown): boolean {
  return typeof error === "object" && error !== null
    && "code" in error && error.code === "ENOENT";
}

function isAlreadyExists(error: unknown): boolean {
  return typeof error === "object" && error !== null
    && "code" in error && (error.code === "EEXIST" || error.code === "ENOTEMPTY");
}

function inside(root: string, candidate: string): boolean {
  const fromRoot = relative(root, candidate);
  return fromRoot === ""
    || (fromRoot !== ".." && !fromRoot.startsWith(".." + sep));
}

async function requireDirectory(path: string, label: string): Promise<string> {
  const stat = await lstat(path);
  if (stat.isSymbolicLink() || !stat.isDirectory()) {
    throw new Error(label + " is not a regular directory");
  }
  return await realpath(path);
}

function projectPath(identityRoot: string, projectDirectoryName: string): string {
  return join(identityRoot, "claude", "projects", projectDirectoryName);
}

async function readRegularFile(
  root: string,
  path: string,
): Promise<Uint8Array> {
  const actual = await realpath(path);
  if (!inside(root, actual)) {
    throw new Error("Claude transcript file escaped its project directory");
  }
  const file = await open(path, constants.O_RDONLY | constants.O_NOFOLLOW);
  try {
    const stat = await file.stat();
    if (!stat.isFile()) throw new Error("Claude transcript entry is not a regular file");
    if (stat.size > MAX_CLAUDE_TRANSCRIPT_BYTES) {
      throw new Error("Claude transcript file is too large");
    }
    return new Uint8Array(await file.readFile());
  } finally {
    await file.close();
  }
}

async function listTranscriptFiles(
  root: string,
  directory: string,
  segments: readonly string[],
  output: ClaudeTranscriptFile[],
): Promise<void> {
  if (segments.length > MAX_DEPTH) {
    throw new Error("Claude transcript directory nesting is too deep");
  }
  const entries = await readdir(directory, { withFileTypes: true });
  entries.sort((left, right) => left.name.localeCompare(right.name, "en"));
  for (const entry of entries) {
    if (output.length >= MAX_CLAUDE_TRANSCRIPT_FILES) {
      throw new Error("Claude transcript has too many files");
    }
    const path = join(directory, entry.name);
    if (entry.isSymbolicLink()) {
      throw new Error("Claude transcript cannot contain symbolic links");
    }
    if (entry.isDirectory()) {
      const actual = await requireDirectory(path, "Claude transcript directory");
      if (!inside(root, actual)) {
        throw new Error("Claude transcript directory escaped its project root");
      }
      await listTranscriptFiles(root, path, [...segments, entry.name], output);
      continue;
    }
    if (!entry.isFile()) {
      throw new Error("Claude transcript contains a non-regular entry");
    }
    output.push({
      path: [...segments, entry.name].join("/"),
      bytes: await readRegularFile(root, path),
    });
  }
}

export async function captureIdentityTranscript(
  identityRootInput: string,
  binding: ClaudeTranscriptBinding,
): Promise<ClaudeTranscriptBundle> {
  const validated = createClaudeTranscriptBundle({ ...binding, files: [] });
  const identityRoot = await requireDirectory(
    resolve(identityRootInput),
    "Identity root",
  );
  const project = projectPath(identityRoot, validated.projectDirectoryName);
  const projectRoot = await requireDirectory(
    project,
    "Claude transcript project",
  ).catch((error: unknown) => {
    if (isMissing(error)) {
      throw new Error("Claude transcript project does not exist", { cause: error });
    }
    throw error;
  });
  if (!inside(identityRoot, projectRoot)) {
    throw new Error("Claude transcript project escaped the identity root");
  }
  const files: ClaudeTranscriptFile[] = [];
  await listTranscriptFiles(projectRoot, projectRoot, [], files);
  return createClaudeTranscriptBundle({ ...validated, files });
}

function startsWithBytes(value: Uint8Array, prefix: Uint8Array): boolean {
  if (value.byteLength < prefix.byteLength) return false;
  for (let index = 0; index < prefix.byteLength; index += 1) {
    if (value[index] !== prefix[index]) return false;
  }
  return true;
}

async function verifyExistingTranscript(
  project: string,
  incoming: ClaudeTranscriptBundle,
): Promise<void> {
  const root = await requireDirectory(project, "Existing Claude transcript project");
  const files: ClaudeTranscriptFile[] = [];
  await listTranscriptFiles(root, root, [], files);
  const existing = new Map(files.map((file) => [file.path, file.bytes] as const));
  for (const expected of incoming.files) {
    const actual = existing.get(expected.path);
    if (actual === undefined || !startsWithBytes(actual, expected.bytes)) {
      throw new Error(
        "Existing Claude transcript diverges at " + expected.path,
      );
    }
  }
}

async function safeProjectsDirectory(identityRoot: string): Promise<string> {
  const claude = join(identityRoot, "claude");
  const claudeRoot = await requireDirectory(claude, "Claude config directory");
  if (!inside(identityRoot, claudeRoot)) {
    throw new Error("Claude config directory escaped the identity root");
  }
  const projects = join(claudeRoot, "projects");
  try {
    const root = await requireDirectory(projects, "Claude projects directory");
    if (!inside(claudeRoot, root)) {
      throw new Error("Claude projects directory escaped the config directory");
    }
    return root;
  } catch (error) {
    if (!isMissing(error)) throw error;
    await mkdir(projects, { mode: 0o700 });
    const root = await requireDirectory(projects, "Claude projects directory");
    if (!inside(claudeRoot, root)) {
      throw new Error("Claude projects directory escaped the config directory");
    }
    return root;
  }
}

export async function restoreIdentityTranscript(
  identityRootInput: string,
  bundleInput: ClaudeTranscriptBundle,
): Promise<IdentityTranscriptImportState> {
  const bundle = createClaudeTranscriptBundle(bundleInput);
  const identityRoot = await requireDirectory(
    resolve(identityRootInput),
    "Identity root",
  );
  const projects = await safeProjectsDirectory(identityRoot);
  const destination = join(projects, bundle.projectDirectoryName);
  try {
    await verifyExistingTranscript(destination, bundle);
    return "already_present";
  } catch (error) {
    if (!isMissing(error)) throw error;
  }

  const staging = await mkdtemp(join(projects, ".altitude-transcript-"));
  let moved = false;
  try {
    for (const transcriptFile of bundle.files) {
      const destinationFile = resolve(staging, ...transcriptFile.path.split("/"));
      if (!inside(staging, destinationFile)) {
        throw new Error("Transcript import path escaped its staging directory");
      }
      await mkdir(dirname(destinationFile), { recursive: true, mode: 0o700 });
      const file = await open(
        destinationFile,
        constants.O_WRONLY | constants.O_CREAT | constants.O_EXCL | constants.O_NOFOLLOW,
        0o600,
      );
      try {
        await file.writeFile(transcriptFile.bytes);
        await file.sync();
      } finally {
        await file.close();
      }
    }
    try {
      await rename(staging, destination);
      moved = true;
      return "installed";
    } catch (error) {
      if (!isAlreadyExists(error)) throw error;
      await verifyExistingTranscript(destination, bundle);
      return "already_present";
    }
  } finally {
    if (!moved) await rm(staging, { recursive: true, force: true });
  }
}

async function collectBytes(
  stream: AsyncIterable<Uint8Array>,
  limit: number,
  label: string,
): Promise<Uint8Array> {
  const chunks: Uint8Array[] = [];
  let size = 0;
  for await (const chunk of stream) {
    size += chunk.byteLength;
    if (size > limit) throw new Error(label + " is too large");
    chunks.push(chunk);
  }
  const output = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) {
    output.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return output;
}

async function collectProcess(
  process: RunningProcess,
  stdoutLimit: number,
): Promise<{ readonly exitCode: number; readonly stdout: Uint8Array; readonly stderr: string }> {
  const [exit, stdout, stderrBytes] = await Promise.all([
    process.exited,
    collectBytes(process.stdout, stdoutLimit, "Identity transcript output"),
    collectBytes(process.stderr, MAX_ERROR_BYTES, "Identity transcript error output"),
  ]);
  return {
    exitCode: exit.exitCode,
    stdout,
    stderr: new TextDecoder().decode(stderrBytes).trim(),
  };
}

function sameBinding(
  bundle: ClaudeTranscriptBundle,
  binding: ClaudeTranscriptBinding,
): boolean {
  return bundle.taskId === binding.taskId
    && bundle.sessionId === binding.sessionId
    && bundle.projectDirectoryName === binding.projectDirectoryName;
}

export class IdentityTranscriptProvisioner {
  constructor(
    private readonly launcher: ProcessLauncher,
    readonly identityId: string,
    private readonly executable = "altitude",
  ) {}

  async export(
    binding: ClaudeTranscriptBinding,
  ): Promise<readonly CapsuleMemberContent[]> {
    const process = await this.#launch(binding.taskId, "export");
    try {
      await process.writeStdin(JSON.stringify(binding) + "\n");
      await process.closeStdin();
      const result = await collectProcess(process, MAX_HELPER_OUTPUT_BYTES);
      if (result.exitCode !== 0) {
        throw new Error(result.stderr || "Identity transcript export failed");
      }
      const bundle = decodeClaudeTranscriptWire(result.stdout);
      if (!sameBinding(bundle, binding)) {
        throw new Error("Identity transcript export returned another session");
      }
      assertResumableClaudeTranscript(bundle);
      return claudeTranscriptCapsuleMembers(bundle);
    } catch (error) {
      await process.terminate("SIGTERM").catch(() => undefined);
      throw error;
    }
  }

  async import(
    binding: ClaudeTranscriptBinding,
    members: readonly CapsuleMemberContent[],
  ): Promise<IdentityTranscriptImportState | "absent"> {
    const bundle = decodeClaudeTranscriptCapsuleMembers(members);
    if (bundle === undefined) return "absent";
    if (!sameBinding(bundle, binding)) {
      throw new Error("Portable Claude transcript belongs to another session");
    }
    assertResumableClaudeTranscript(bundle);
    const process = await this.#launch(binding.taskId, "import");
    try {
      await process.writeStdin(encodeClaudeTranscriptWire(bundle));
      await process.closeStdin();
      const result = await collectProcess(process, MAX_ERROR_BYTES);
      if (result.exitCode !== 0) {
        throw new Error(result.stderr || "Identity transcript import failed");
      }
      let value: unknown;
      try {
        value = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(
          result.stdout,
        )) as unknown;
      } catch (error) {
        throw new Error("Identity transcript import returned invalid JSON", {
          cause: error,
        });
      }
      if (typeof value !== "object" || value === null
        || (value as Record<string, unknown>).schema !== IDENTITY_TRANSCRIPT_PROTOCOL
        || !["installed", "already_present"].includes(
          String((value as Record<string, unknown>).state),
        )) {
        throw new Error("Identity transcript import returned an invalid receipt");
      }
      return (value as { readonly state: IdentityTranscriptImportState }).state;
    } catch (error) {
      await process.terminate("SIGTERM").catch(() => undefined);
      throw error;
    }
  }

  #launch(
    taskId: string,
    operation: "export" | "import",
  ): Promise<RunningProcess> {
    const spec: ProcessLaunchSpec & { readonly io: "piped" } = {
      target: {
        kind: "identity-capsule",
        identityId: this.identityId,
        taskId,
      },
      executable: this.executable,
      args: ["_identity-transcript", operation],
      cwd: "/identity",
      env: { ALTITUDE_IDENTITY_TRANSCRIPT: "1" },
      io: "piped",
      purpose: operation === "export"
        ? "Export one portable Claude task transcript"
        : "Import one portable Claude task transcript",
    };
    return this.launcher.launch(spec);
  }
}

async function readStdinBounded(limit: number): Promise<Uint8Array> {
  return await collectBytes(
    Bun.stdin.stream() as AsyncIterable<Uint8Array>,
    limit,
    "Identity transcript input",
  );
}

export async function runIdentityTranscriptStdio(
  operation: string | undefined,
  identityRoot = "/identity",
): Promise<number> {
  if (process.env.ALTITUDE_IDENTITY_TRANSCRIPT !== "1") {
    process.stderr.write(
      "Identity transcript helper may only run through an Altitude identity capsule binding.\n",
    );
    return 1;
  }
  try {
    if (operation === "export") {
      const bytes = await readStdinBounded(64 * 1024);
      const value = JSON.parse(
        new TextDecoder("utf-8", { fatal: true }).decode(bytes),
      ) as ClaudeTranscriptBinding;
      const transcript = await captureIdentityTranscript(identityRoot, value);
      process.stdout.write(encodeClaudeTranscriptWire(transcript));
      return 0;
    }
    if (operation === "import") {
      const transcript = decodeClaudeTranscriptWire(
        await readStdinBounded(MAX_HELPER_OUTPUT_BYTES),
      );
      const state = await restoreIdentityTranscript(identityRoot, transcript);
      process.stdout.write(JSON.stringify({
        schema: IDENTITY_TRANSCRIPT_PROTOCOL,
        state,
      }) + "\n");
      return 0;
    }
    throw new Error("Identity transcript operation must be export or import");
  } catch (error) {
    process.stderr.write(
      (error instanceof Error ? error.message : String(error)) + "\n",
    );
    return 1;
  }
}
