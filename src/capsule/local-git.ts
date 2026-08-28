import { createHash } from "node:crypto";
import { lstat, mkdtemp, readFile, realpath, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

import {
  GIT_BUNDLE_MEMBER,
  GIT_STAGED_PATCH_MEMBER,
  GIT_UNSTAGED_PATCH_MEMBER,
  makeGitRepositorySnapshot,
  planGitCapture,
  planUntrackedDirectoryArchive,
} from "./git.ts";
import {
  createCapsuleCheckpoint,
  type ActiveOwnershipLease,
  type CapsuleCheckpoint,
  type CapsuleMemberContent,
} from "./manifest.ts";

const MAX_GIT_OUTPUT_BYTES = 512 * 1024 * 1024;

export interface CapturedLocalGitCapsule {
  readonly checkpoint: CapsuleCheckpoint;
  /** Node-local seed material. It is not part of the portable capsule. */
  readonly repositorySeedBundle: Uint8Array;
  readonly repositoryPath: string;
}

type GitResult = Readonly<{
  exitCode: number;
  stdout: Uint8Array;
  stderr: string;
}>;

async function boundedBytes(stream: ReadableStream<Uint8Array>, limit: number): Promise<Uint8Array> {
  const reader = stream.getReader();
  const chunks: Uint8Array[] = [];
  let size = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      size += value.byteLength;
      if (size > limit) throw new Error(`Git output exceeds ${limit} bytes`);
      chunks.push(value);
    }
  } finally {
    reader.releaseLock();
  }
  const result = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) {
    result.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return result;
}

async function git(repository: string, args: readonly string[], allowFailure = false): Promise<GitResult> {
  const child = Bun.spawn(["git", ...args], {
    cwd: repository,
    env: {
      ...process.env,
      GIT_CONFIG_NOSYSTEM: "1",
      GIT_TERMINAL_PROMPT: "0",
    },
    stdin: "ignore",
    stdout: "pipe",
    stderr: "pipe",
  });
  const [exitCode, stdout, stderr] = await Promise.all([
    child.exited,
    boundedBytes(child.stdout, MAX_GIT_OUTPUT_BYTES),
    new Response(child.stderr).text(),
  ]);
  if (exitCode !== 0 && !allowFailure) {
    throw new Error(`git ${args[0] ?? "command"} failed (${exitCode}): ${stderr.trim()}`);
  }
  return { exitCode, stdout, stderr };
}

function text(result: GitResult): string {
  return new TextDecoder("utf-8", { fatal: true }).decode(result.stdout).trim();
}

async function regularUntrackedFile(repository: string, path: string): Promise<Uint8Array> {
  const absolute = join(repository, ...path.split("/"));
  const stat = await lstat(absolute);
  if (stat.isSymbolicLink() || !stat.isFile()) {
    throw new Error(`Untracked path ${path} is not a regular no-follow file`);
  }
  if (stat.size > MAX_GIT_OUTPUT_BYTES) throw new Error(`Untracked file ${path} is too large`);
  return new Uint8Array(await readFile(absolute));
}

function fallbackBranch(taskId: string): string {
  const digest = createHash("sha256").update(taskId, "utf8").digest("hex").slice(0, 16);
  return `altitude/task-${digest}`;
}

async function readBundle(path: string): Promise<Uint8Array> {
  const stat = await lstat(path);
  if (!stat.isFile() || stat.isSymbolicLink()) throw new Error("Git bundle output is not a regular file");
  if (stat.size > MAX_GIT_OUTPUT_BYTES) throw new Error("Git bundle exceeds the v1 size boundary");
  return new Uint8Array(await readFile(path));
}

/** Capture a host checkout without carrying its Git, SSH, or hosting credentials. */
export async function captureLocalGitCapsule(input: {
  readonly capsuleId: string;
  readonly taskId: string;
  readonly repositoryPath: string;
  readonly revision: number;
  readonly ownership: ActiveOwnershipLease;
  readonly checkpointedAt?: string;
}): Promise<CapturedLocalGitCapsule> {
  const requested = resolve(input.repositoryPath);
  const requestedStat = await lstat(requested);
  if (requestedStat.isSymbolicLink() || !requestedStat.isDirectory()) {
    throw new Error("Repository path must be a regular directory");
  }
  const repository = await realpath(requested);
  const topLevel = text(await git(repository, ["rev-parse", "--show-toplevel"]));
  if (await realpath(topLevel) !== repository) {
    throw new Error("Repository path must name the Git worktree root");
  }
  const remoteUrl = text(await git(repository, ["remote", "get-url", "origin"]));
  const headCommit = text(await git(repository, ["rev-parse", "HEAD"]));
  const branchResult = await git(repository, ["symbolic-ref", "--quiet", "--short", "HEAD"], true);
  const branch = branchResult.exitCode === 0 ? text(branchResult) : fallbackBranch(input.taskId);
  const defaultResult = await git(repository, ["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"], true);
  const defaultBranch = defaultResult.exitCode === 0
    ? text(defaultResult).replace(/^origin\//u, "")
    : branch;
  const baseResult = await git(repository, ["merge-base", "HEAD", `refs/remotes/origin/${defaultBranch}`], true);
  const baseCommit = baseResult.exitCode === 0 ? text(baseResult) : headCommit;
  const capture = planGitCapture({
    capsuleId: input.capsuleId,
    repositoryPath: repository,
    remoteUrl,
    defaultBranch,
    branch,
    baseCommit,
    headCommit,
  });

  const temporary = await mkdtemp(join(tmpdir(), "altitude-git-capture-"));
  try {
    const seedPath = join(temporary, "repository-seed.bundle");
    await git(repository, ["bundle", "create", seedPath, "--all"]);
    const members: CapsuleMemberContent[] = [];
    if (capture.outputs.bundleMember !== null) {
      const deltaPath = join(temporary, "repository-delta.bundle");
      await git(repository, ["bundle", "create", deltaPath, "HEAD", `^${baseCommit}`]);
      members.push({ path: GIT_BUNDLE_MEMBER, bytes: await readBundle(deltaPath) });
    }
    const staged = await git(repository, [
      "diff", "--cached", "--binary", "--full-index", "--no-ext-diff", "--no-renames",
    ]);
    const unstaged = await git(repository, [
      "diff", "--binary", "--full-index", "--no-ext-diff", "--no-renames",
    ]);
    members.push(
      { path: GIT_STAGED_PATCH_MEMBER, bytes: staged.stdout },
      { path: GIT_UNSTAGED_PATCH_MEMBER, bytes: unstaged.stdout },
    );
    const untrackedOutput = await git(repository, ["ls-files", "--others", "--exclude-standard", "-z"]);
    const untracked = planUntrackedDirectoryArchive(untrackedOutput.stdout);
    for (const entry of untracked.entries) {
      members.push({
        path: entry.memberPath,
        bytes: await regularUntrackedFile(repository, entry.repositoryPath),
      });
    }
    const repositorySnapshot = makeGitRepositorySnapshot({
      capsuleId: input.capsuleId,
      repositoryPath: repository,
      remoteUrl,
      defaultBranch,
      branch,
      baseCommit,
      headCommit,
    }, capture, untracked);
    return {
      checkpoint: createCapsuleCheckpoint({
        capsuleId: input.capsuleId,
        taskId: input.taskId,
        revision: input.revision,
        checkpointedAt: input.checkpointedAt ?? new Date().toISOString(),
        ownership: input.ownership,
        repository: repositorySnapshot,
        members,
      }),
      repositorySeedBundle: await readBundle(seedPath),
      repositoryPath: repository,
    };
  } finally {
    await rm(temporary, { recursive: true, force: true });
  }
}
