import {
  literal,
  readMember,
  writeMember,
  type CommandPlan,
  type PlannedCommand,
} from "./commands.ts";
import { compareCanonicalStrings } from "./canonical.ts";
import type { CapsuleMemberDescriptor, GitRepositorySnapshot, UntrackedMember } from "./manifest.ts";
import {
  portableCapsulePath,
  safeRepositoryPath,
  untrackedMemberPath,
  type PortableCapsulePath,
} from "./paths.ts";

export const GIT_BUNDLE_MEMBER = portableCapsulePath("git/repository.bundle");
export const GIT_STAGED_PATCH_MEMBER = portableCapsulePath("git/staged.patch");
export const GIT_UNSTAGED_PATCH_MEMBER = portableCapsulePath("git/unstaged.patch");

export interface GitCaptureInput {
  readonly capsuleId: string;
  readonly repositoryPath: string;
  readonly remoteUrl: string;
  readonly defaultBranch: string;
  readonly branch: string;
  readonly baseCommit: string;
  readonly headCommit: string;
}

export interface GitCapturePlan {
  readonly commands: CommandPlan;
  readonly outputs: {
    readonly bundleMember: PortableCapsulePath | null;
    readonly stagedPatchMember: PortableCapsulePath;
    readonly unstagedPatchMember: PortableCapsulePath;
    readonly untrackedListCaptureKey: "git.untracked-list";
  };
}

export interface UntrackedArchiveEntry extends UntrackedMember {
  readonly copyMode: "regular-file-no-follow";
}

export interface UntrackedDirectoryArchivePlan {
  readonly format: "altitude.untracked-directory/v1";
  readonly entries: readonly UntrackedArchiveEntry[];
}

export interface GitRestoreInput {
  readonly capsuleId: string;
  readonly repositoryPath: string;
  readonly snapshot: GitRepositorySnapshot;
  readonly members: readonly CapsuleMemberDescriptor[];
}

export interface GitRestoreFileOperation {
  readonly kind: "restore-untracked-file";
  readonly sourceMember: PortableCapsulePath;
  readonly destinationRepositoryPath: string;
  readonly overwrite: false;
  readonly followSymlinks: false;
}

export interface GitRestorePlan {
  readonly workspaceMustBeDisposable: true;
  readonly commands: CommandPlan;
  readonly files: readonly GitRestoreFileOperation[];
}

export class InvalidGitCaptureError extends Error {
  override readonly name = "InvalidGitCaptureError";
}

export function planGitCapture(input: GitCaptureInput): GitCapturePlan {
  validateCapsuleId(input.capsuleId);
  validateGitObjectId(input.baseCommit, "baseCommit");
  validateGitObjectId(input.headCommit, "headCommit");
  if (input.repositoryPath.length === 0 || input.repositoryPath.includes("\0")) {
    throw new InvalidGitCaptureError("repositoryPath is invalid");
  }

  const commands: PlannedCommand[] = [];
  const bundleMember = input.baseCommit === input.headCommit
    ? null
    : GIT_BUNDLE_MEMBER;
  if (bundleMember !== null) {
    commands.push({
      id: "git.bundle",
      program: "git",
      arguments: [
        literal("bundle"),
        literal("create"),
        writeMember(bundleMember),
        literal("HEAD"),
        literal(`^${input.baseCommit}`),
      ],
      cwd: input.repositoryPath,
      stdin: { kind: "none" },
      stdout: { kind: "discard" },
    });
  }
  commands.push(
    {
      id: "git.patch-staged",
      program: "git",
      arguments: [
        literal("diff"),
        literal("--cached"),
        literal("--binary"),
        literal("--full-index"),
        literal("--no-ext-diff"),
        literal("--no-renames"),
      ],
      cwd: input.repositoryPath,
      stdin: { kind: "none" },
      stdout: { kind: "capsule-member", path: GIT_STAGED_PATCH_MEMBER },
    },
    {
      id: "git.patch-unstaged",
      program: "git",
      arguments: [
        literal("diff"),
        literal("--binary"),
        literal("--full-index"),
        literal("--no-ext-diff"),
        literal("--no-renames"),
      ],
      cwd: input.repositoryPath,
      stdin: { kind: "none" },
      stdout: { kind: "capsule-member", path: GIT_UNSTAGED_PATCH_MEMBER },
    },
    {
      id: "git.list-untracked",
      program: "git",
      arguments: [
        literal("ls-files"),
        literal("--others"),
        literal("--exclude-standard"),
        literal("-z"),
      ],
      cwd: input.repositoryPath,
      stdin: { kind: "none" },
      stdout: { kind: "capture", key: "git.untracked-list" },
    },
  );

  return {
    commands: { id: `capture:${input.capsuleId}`, commands },
    outputs: {
      bundleMember,
      stagedPatchMember: GIT_STAGED_PATCH_MEMBER,
      unstagedPatchMember: GIT_UNSTAGED_PATCH_MEMBER,
      untrackedListCaptureKey: "git.untracked-list",
    },
  };
}

export function planUntrackedDirectoryArchive(
  nulSeparatedGitOutput: Uint8Array,
): UntrackedDirectoryArchivePlan {
  let decoded: string;
  try {
    decoded = new TextDecoder("utf-8", { fatal: true }).decode(
      nulSeparatedGitOutput,
    );
  } catch (error) {
    throw new InvalidGitCaptureError("Git returned a non-UTF-8 untracked path list", {
      cause: error,
    });
  }
  if (decoded !== "" && !decoded.endsWith("\0")) {
    throw new InvalidGitCaptureError("Git returned a truncated untracked path list");
  }
  const rawPaths = decoded === "" ? [] : decoded.split("\0");
  if (rawPaths.at(-1) === "") rawPaths.pop();
  if (rawPaths.some((path) => path === "")) {
    throw new InvalidGitCaptureError("Git returned an empty untracked path");
  }
  const seen = new Set<string>();
  const entries = rawPaths.map((rawPath): UntrackedArchiveEntry => {
    const repositoryPath = safeRepositoryPath(rawPath);
    if (seen.has(repositoryPath)) {
      throw new InvalidGitCaptureError(
        `Git returned duplicate untracked path ${repositoryPath}`,
      );
    }
    seen.add(repositoryPath);
    return {
      repositoryPath,
      memberPath: untrackedMemberPath(repositoryPath),
      copyMode: "regular-file-no-follow",
    };
  });
  entries.sort((left, right) =>
    compareCanonicalStrings(left.repositoryPath, right.repositoryPath)
  );
  return { format: "altitude.untracked-directory/v1", entries };
}

export function makeGitRepositorySnapshot(
  input: GitCaptureInput,
  capture: GitCapturePlan,
  untracked: UntrackedDirectoryArchivePlan,
): GitRepositorySnapshot {
  return {
    kind: "git",
    remoteUrl: input.remoteUrl,
    defaultBranch: input.defaultBranch,
    branch: input.branch,
    baseCommit: input.baseCommit,
    headCommit: input.headCommit,
    bundleMember: capture.outputs.bundleMember,
    stagedPatchMember: capture.outputs.stagedPatchMember,
    unstagedPatchMember: capture.outputs.unstagedPatchMember,
    untracked: untracked.entries.map(({ repositoryPath, memberPath }) => ({
      repositoryPath,
      memberPath,
    })),
  };
}

export function planGitRestore(input: GitRestoreInput): GitRestorePlan {
  validateCapsuleId(input.capsuleId);
  validateGitObjectId(input.snapshot.baseCommit, "snapshot.baseCommit");
  validateGitObjectId(input.snapshot.headCommit, "snapshot.headCommit");
  validateGitRefName(input.snapshot.branch, "snapshot.branch");
  const memberSizes = new Map(
    input.members.map((member) => [member.path, member.size] as const),
  );
  const requireMember = (path: PortableCapsulePath): number => {
    const size = memberSizes.get(path);
    if (size === undefined) {
      throw new InvalidGitCaptureError(`Restore member ${path} is missing`);
    }
    return size;
  };

  const commands: PlannedCommand[] = [
    {
      id: "git.reset-base",
      program: "git",
      arguments: [literal("reset"), literal("--hard"), literal(input.snapshot.baseCommit)],
      cwd: input.repositoryPath,
      stdin: { kind: "none" },
      stdout: { kind: "discard" },
    },
  ];
  if (input.snapshot.bundleMember !== null) {
    requireMember(input.snapshot.bundleMember);
    const importRef = `refs/altitude/capsules/${input.capsuleId}`;
    commands.push(
      {
        id: "git.fetch-bundle",
        program: "git",
        arguments: [
          literal("fetch"),
          readMember(input.snapshot.bundleMember),
          literal(`HEAD:${importRef}`),
        ],
        cwd: input.repositoryPath,
        stdin: { kind: "none" },
        stdout: { kind: "discard" },
      },
    );
  } else if (input.snapshot.headCommit !== input.snapshot.baseCommit) {
    throw new InvalidGitCaptureError(
      "A snapshot with commits beyond its base requires a Git bundle",
    );
  }

  commands.push({
    id: "git.checkout-task",
    program: "git",
    arguments: [
      literal("switch"),
      literal("--force-create"),
      literal(input.snapshot.branch),
      literal(input.snapshot.headCommit),
    ],
    cwd: input.repositoryPath,
    stdin: { kind: "none" },
    stdout: { kind: "discard" },
  });

  if (requireMember(input.snapshot.stagedPatchMember) > 0) {
    commands.push({
      id: "git.apply-staged",
      program: "git",
      arguments: [
        literal("apply"),
        literal("--index"),
        literal("--binary"),
        literal("--whitespace=nowarn"),
        literal("-"),
      ],
      cwd: input.repositoryPath,
      stdin: { kind: "capsule-member", path: input.snapshot.stagedPatchMember },
      stdout: { kind: "discard" },
    });
  }
  if (requireMember(input.snapshot.unstagedPatchMember) > 0) {
    commands.push({
      id: "git.apply-unstaged",
      program: "git",
      arguments: [
        literal("apply"),
        literal("--binary"),
        literal("--whitespace=nowarn"),
        literal("-"),
      ],
      cwd: input.repositoryPath,
      stdin: { kind: "capsule-member", path: input.snapshot.unstagedPatchMember },
      stdout: { kind: "discard" },
    });
  }

  const files = input.snapshot.untracked.map(
    (entry): GitRestoreFileOperation => {
      requireMember(entry.memberPath);
      return {
        kind: "restore-untracked-file",
        sourceMember: entry.memberPath,
        destinationRepositoryPath: safeRepositoryPath(entry.repositoryPath),
        overwrite: false,
        followSymlinks: false,
      };
    },
  );
  return {
    workspaceMustBeDisposable: true,
    commands: { id: `restore:${input.capsuleId}`, commands },
    files,
  };
}

function validateCapsuleId(value: string): void {
  if (!/^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/u.test(value)) {
    throw new InvalidGitCaptureError("capsuleId is invalid");
  }
}

function validateGitObjectId(value: string, label: string): void {
  if (!/^[a-f0-9]{40}(?:[a-f0-9]{24})?$/u.test(value)) {
    throw new InvalidGitCaptureError(`${label} must be a full Git object ID`);
  }
}

function validateGitRefName(value: string, label: string): void {
  if (
    value.length === 0 ||
    value.startsWith("-") ||
    value.startsWith("/") ||
    value.endsWith("/") ||
    value.includes("..") ||
    value.includes("//") ||
    value.includes("@{") ||
    /[\u0000-\u0020~^:?*[\\\u007f]/u.test(value)
  ) {
    throw new InvalidGitCaptureError(`${label} is not a safe Git ref name`);
  }
}
