import { createHash, randomUUID } from "node:crypto";
import {
  lstat,
  mkdir,
  readFile,
  readdir,
  realpath,
  rename,
  rm,
  writeFile,
} from "node:fs/promises";
import { isAbsolute, join, posix, relative, resolve, sep } from "node:path";

import { safeRepositoryPath } from "../capsule/paths.ts";
import {
  TASK_TOOL_PROTOCOL,
  type TaskToolRequest,
  type TaskToolResponse,
} from "./protocol.ts";

const MAX_FILE_BYTES = 2 * 1024 * 1024;
const MAX_OUTPUT_BYTES = 2 * 1024 * 1024;
const DEFAULT_TIMEOUT_MS = 120_000;

class TaskToolError extends Error {
  constructor(readonly code: string, message: string) {
    super(message);
  }
}

function sha256(bytes: Uint8Array): string {
  return `sha256:${createHash("sha256").update(bytes).digest("hex")}`;
}

function validateRelativePath(value: string | undefined, allowRoot: boolean): string {
  if ((value === undefined || value === ".") && allowRoot) return ".";
  if (value === undefined) throw new TaskToolError("invalid_path", "A repository-relative path is required");
  try {
    return safeRepositoryPath(value);
  } catch (error) {
    throw new TaskToolError("invalid_path", error instanceof Error ? error.message : String(error));
  }
}

function within(root: string, candidate: string): boolean {
  const fromRoot = relative(root, candidate);
  return fromRoot === "" || (!isAbsolute(fromRoot) && fromRoot !== ".." && !fromRoot.startsWith(`..${sep}`));
}

async function noFollowPath(root: string, path: string, allowMissingLeaf = false): Promise<string> {
  const absoluteRoot = await realpath(root);
  const segments = path === "." ? [] : path.split("/");
  let current = absoluteRoot;
  for (let index = 0; index < segments.length; index += 1) {
    current = join(current, segments[index]!);
    if (!within(absoluteRoot, current)) throw new TaskToolError("invalid_path", "Path escapes the task workspace");
    try {
      const stat = await lstat(current);
      if (stat.isSymbolicLink()) throw new TaskToolError("symlink_rejected", `Path ${path} traverses a symbolic link`);
    } catch (error) {
      if (allowMissingLeaf && index === segments.length - 1 && isNotFound(error)) return current;
      throw error;
    }
  }
  return current;
}

function isNotFound(error: unknown): boolean {
  return typeof error === "object" && error !== null && "code" in error && error.code === "ENOENT";
}

function requireInteger(value: number | undefined, fallback: number, minimum: number, maximum: number, label: string): number {
  const result = value ?? fallback;
  if (!Number.isSafeInteger(result) || result < minimum || result > maximum) {
    throw new TaskToolError("invalid_argument", `${label} must be between ${minimum} and ${maximum}`);
  }
  return result;
}

async function readTextFile(root: string, request: Extract<TaskToolRequest, { operation: "read_file" }>): Promise<unknown> {
  const path = validateRelativePath(request.path, false);
  const absolute = await noFollowPath(root, path);
  const bytes = await readFile(absolute);
  if (bytes.byteLength > MAX_FILE_BYTES) {
    throw new TaskToolError("file_too_large", `File ${path} exceeds ${MAX_FILE_BYTES} bytes`);
  }
  let content: string;
  try {
    content = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  } catch {
    throw new TaskToolError("not_text", `File ${path} is not valid UTF-8 text`);
  }
  const lines = content.split("\n");
  const startLine = requireInteger(request.startLine, 1, 1, Math.max(1, lines.length), "startLine");
  const endLine = requireInteger(request.endLine, Math.min(lines.length, startLine + 1_999), startLine, lines.length, "endLine");
  return {
    path,
    content: lines.slice(startLine - 1, endLine).join("\n"),
    startLine,
    endLine,
    totalLines: lines.length,
    size: bytes.byteLength,
    sha256: sha256(bytes),
  };
}

async function writeTextFile(root: string, request: Extract<TaskToolRequest, { operation: "write_file" }>): Promise<unknown> {
  const path = validateRelativePath(request.path, false);
  const bytes = new TextEncoder().encode(request.content);
  if (bytes.byteLength > MAX_FILE_BYTES) {
    throw new TaskToolError("file_too_large", `Content exceeds ${MAX_FILE_BYTES} bytes`);
  }
  const rootReal = await realpath(root);
  const parentPath = posix.dirname(path);
  if (request.createDirectories === true && parentPath !== ".") {
    const parentSegments = parentPath.split("/");
    let cursor = rootReal;
    for (const segment of parentSegments) {
      cursor = join(cursor, segment);
      try {
        const stat = await lstat(cursor);
        if (stat.isSymbolicLink() || !stat.isDirectory()) {
          throw new TaskToolError("invalid_path", `Parent of ${path} is not a regular directory`);
        }
      } catch (error) {
        if (!isNotFound(error)) throw error;
        await mkdir(cursor, { mode: 0o755 });
      }
    }
  }
  const parent = await noFollowPath(rootReal, parentPath);
  const absolute = await noFollowPath(rootReal, path, true);
  if (!within(rootReal, absolute) || !within(rootReal, parent)) {
    throw new TaskToolError("invalid_path", "Write path escapes the task workspace");
  }
  let existing: Uint8Array | undefined;
  try {
    existing = await readFile(absolute);
  } catch (error) {
    if (!isNotFound(error)) throw error;
  }
  if (request.expectedSha256 === null && existing !== undefined) {
    throw new TaskToolError("write_conflict", `File ${path} already exists`);
  }
  if (typeof request.expectedSha256 === "string") {
    const actual = existing === undefined ? null : sha256(existing);
    if (actual !== request.expectedSha256) {
      throw new TaskToolError("write_conflict", `File ${path} changed since it was read`);
    }
  }
  const temporary = join(parent, `.altitude-write-${randomUUID()}`);
  try {
    await writeFile(temporary, bytes, { flag: "wx", mode: 0o644 });
    await rename(temporary, absolute);
  } finally {
    await rm(temporary, { force: true }).catch(() => undefined);
  }
  return { path, size: bytes.byteLength, sha256: sha256(bytes), created: existing === undefined };
}

async function listFiles(root: string, request: Extract<TaskToolRequest, { operation: "list_files" }>): Promise<unknown> {
  const path = validateRelativePath(request.path, true);
  const depth = requireInteger(request.depth, 2, 0, 8, "depth");
  const limit = requireInteger(request.limit, 250, 1, 1_000, "limit");
  const start = await noFollowPath(root, path);
  const entries: Array<{ path: string; type: "file" | "directory" | "symlink" | "other"; size?: number }> = [];

  const visit = async (directory: string, relativeDirectory: string, remainingDepth: number): Promise<void> => {
    const children = await readdir(directory, { withFileTypes: true });
    children.sort((left, right) => left.name.localeCompare(right.name));
    for (const child of children) {
      if (entries.length >= limit) return;
      const childRelative = relativeDirectory === "." ? child.name : `${relativeDirectory}/${child.name}`;
      if (childRelative === ".git" || childRelative.startsWith(".git/")) continue;
      const childAbsolute = join(directory, child.name);
      const type = child.isSymbolicLink() ? "symlink"
        : child.isDirectory() ? "directory"
          : child.isFile() ? "file" : "other";
      const entry: { path: string; type: typeof type; size?: number } = { path: childRelative, type };
      if (type === "file") entry.size = (await lstat(childAbsolute)).size;
      entries.push(entry);
      if (type === "directory" && remainingDepth > 0) {
        await visit(childAbsolute, childRelative, remainingDepth - 1);
      }
    }
  };
  await visit(start, path, depth);
  return { path, entries, truncated: entries.length >= limit };
}

async function bounded(stream: ReadableStream<Uint8Array>, limit: number, onOverflow: () => void): Promise<string> {
  const reader = stream.getReader();
  const decoder = new TextDecoder();
  let text = "";
  let size = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      size += value.byteLength;
      if (size > limit) {
        onOverflow();
        throw new TaskToolError("output_too_large", `Command output exceeds ${limit} bytes`);
      }
      text += decoder.decode(value, { stream: true });
    }
    return `${text}${decoder.decode()}`;
  } finally {
    reader.releaseLock();
  }
}

async function runCommand(
  root: string,
  input: {
    executable: string;
    args?: readonly string[] | undefined;
    cwd?: string | undefined;
    env?: Readonly<Record<string, string>> | undefined;
    timeoutMs?: number | undefined;
  },
): Promise<{ exitCode: number; stdout: string; stderr: string }> {
  if (!/^(?:[A-Za-z0-9_.+-]+|\.\/[A-Za-z0-9_./+-]+)$/u.test(input.executable) || input.executable.includes("..")) {
    throw new TaskToolError("invalid_command", "Executable must be a command name or normalized workspace-relative path");
  }
  const args = input.args ?? [];
  if (args.length > 256 || args.some((argument) => argument.includes("\0"))) {
    throw new TaskToolError("invalid_command", "Command arguments are invalid");
  }
  const cwdPath = validateRelativePath(input.cwd, true);
  const cwd = await noFollowPath(root, cwdPath);
  const timeoutMs = requireInteger(input.timeoutMs, DEFAULT_TIMEOUT_MS, 1_000, 600_000, "timeoutMs");
  const env = { ...process.env };
  for (const [key, value] of Object.entries(input.env ?? {})) {
    if (!/^[A-Z_][A-Z0-9_]*$/.test(key) || value.includes("\0")) {
      throw new TaskToolError("invalid_command", `Environment entry ${key} is invalid`);
    }
    env[key] = value;
  }
  const child = Bun.spawn([input.executable, ...args], { cwd, env, stdin: "ignore", stdout: "pipe", stderr: "pipe" });
  let timedOut = false;
  const timer = setTimeout(() => {
    timedOut = true;
    child.kill("SIGTERM");
  }, timeoutMs);
  try {
    const [exitCode, stdout, stderr] = await Promise.all([
      child.exited,
      bounded(child.stdout, MAX_OUTPUT_BYTES, () => child.kill("SIGTERM")),
      bounded(child.stderr, MAX_OUTPUT_BYTES, () => child.kill("SIGTERM")),
    ]);
    if (timedOut) throw new TaskToolError("command_timeout", `Command exceeded ${timeoutMs} ms`);
    return { exitCode, stdout, stderr };
  } finally {
    clearTimeout(timer);
  }
}

async function searchText(root: string, request: Extract<TaskToolRequest, { operation: "search_text" }>): Promise<unknown> {
  if (!request.query || request.query.length > 1_024 || request.query.includes("\0")) {
    throw new TaskToolError("invalid_argument", "Search query must contain 1 to 1024 characters");
  }
  const path = validateRelativePath(request.path, true);
  const limit = requireInteger(request.limit, 200, 1, 1_000, "limit");
  const args = ["--line-number", "--column", "--no-heading", "--color", "never", "--fixed-strings"];
  if (request.glob !== undefined) {
    if (!request.glob || request.glob.length > 256 || request.glob.includes("\0")) {
      throw new TaskToolError("invalid_argument", "Search glob is invalid");
    }
    args.push("--glob", request.glob);
  }
  args.push("--", request.query, path);
  const result = await runCommand(root, { executable: "rg", args });
  if (result.exitCode !== 0 && result.exitCode !== 1) {
    throw new TaskToolError("search_failed", result.stderr.trim() || `rg exited ${result.exitCode}`);
  }
  const matches = result.stdout.split("\n").filter(Boolean).slice(0, limit);
  return { path, matches, truncated: result.stdout.split("\n").filter(Boolean).length > limit };
}

export async function executeTaskTool(root: string, request: TaskToolRequest): Promise<TaskToolResponse> {
  try {
    if (request.schema !== TASK_TOOL_PROTOCOL) throw new TaskToolError("unsupported_protocol", "Unsupported task tool protocol");
    const resolvedRoot = resolve(root);
    const result = request.operation === "read_file" ? await readTextFile(resolvedRoot, request)
      : request.operation === "write_file" ? await writeTextFile(resolvedRoot, request)
        : request.operation === "list_files" ? await listFiles(resolvedRoot, request)
          : request.operation === "search_text" ? await searchText(resolvedRoot, request)
            : request.operation === "run_command" ? await runCommand(resolvedRoot, request)
              : request.operation === "git_status" ? await runCommand(resolvedRoot, {
                  executable: "git",
                  args: ["status", "--short", "--branch", "--porcelain=v2"],
                })
                : (() => { throw new TaskToolError("unsupported_operation", "Unsupported task tool operation"); })();
    return { schema: TASK_TOOL_PROTOCOL, ok: true, result };
  } catch (error) {
    return {
      schema: TASK_TOOL_PROTOCOL,
      ok: false,
      error: {
        code: error instanceof TaskToolError ? error.code : isNotFound(error) ? "not_found" : "tool_failed",
        message: error instanceof Error ? error.message : String(error),
      },
    };
  }
}

export async function runTaskToolStdio(root = process.cwd()): Promise<number> {
  if (process.env.ALTITUDE_TASK_TOOL !== "1") {
    process.stderr.write("Task tool helper may only run through an Altitude task capsule binding.\n");
    return 1;
  }
  let value: unknown;
  try {
    const body = await new Response(Bun.stdin.stream()).text();
    if (new TextEncoder().encode(body).byteLength > MAX_FILE_BYTES) throw new Error("Task tool request is too large");
    value = JSON.parse(body) as unknown;
  } catch (error) {
    process.stdout.write(`${JSON.stringify({
      schema: TASK_TOOL_PROTOCOL,
      ok: false,
      error: { code: "invalid_request", message: error instanceof Error ? error.message : String(error) },
    } satisfies TaskToolResponse)}\n`);
    return 1;
  }
  const response = await executeTaskTool(root, value as TaskToolRequest);
  process.stdout.write(`${JSON.stringify(response)}\n`);
  return response.ok ? 0 : 1;
}
