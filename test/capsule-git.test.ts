import { describe, expect, test } from "bun:test";

import {
  CommandIdConflictError,
  InMemoryCommandReceiptStore,
  InvalidGitCaptureError,
  UnsafeCapsulePathError,
  executeCommandPlan,
  makeGitRepositorySnapshot,
  planGitCapture,
  planGitRestore,
  planUntrackedDirectoryArchive,
  sha256,
  utf8,
  type CapsuleMemberDescriptor,
  type CommandExecutor,
  type CommandPlan,
  type CommandResult,
  type PlannedCommand,
} from "../src/capsule/index.ts";

const BASE = "1".repeat(40);
const HEAD = "2".repeat(40);

const captureInput = {
  capsuleId: "capsule-1",
  repositoryPath: "/workspace",
  remoteUrl: "git@github.com:gitsquared/example.git",
  defaultBranch: "main",
  branch: "altitude/task-1",
  baseCommit: BASE,
  headCommit: HEAD,
} as const;

function descriptor(path: string, size: number): CapsuleMemberDescriptor {
  return { path: path as CapsuleMemberDescriptor["path"], size, sha256: sha256(utf8(path)) };
}

describe("Git capsule capture and restore planning", () => {
  test("plans a commit bundle, binary index/worktree patches, and NUL-safe untracked listing", () => {
    const plan = planGitCapture(captureInput);
    expect(plan.commands.commands.map(({ id }) => id)).toEqual([
      "git.bundle",
      "git.patch-staged",
      "git.patch-unstaged",
      "git.list-untracked",
    ]);
    expect(plan.commands.commands[0]?.arguments as unknown).toEqual([
      { kind: "literal", value: "bundle" },
      { kind: "literal", value: "create" },
      {
        kind: "capsule-member",
        access: "write",
        path: "git/repository.bundle",
      },
      { kind: "literal", value: "HEAD" },
      { kind: "literal", value: `^${BASE}` },
    ]);
    expect(plan.commands.commands[1]?.stdout as unknown).toEqual({
      kind: "capsule-member",
      path: "git/staged.patch",
    });
    expect(plan.commands.commands[3]?.arguments.at(-1)).toEqual({
      kind: "literal",
      value: "-z",
    });
  });

  test("uses a sorted no-follow representation for safe untracked files", () => {
    const archive = planUntrackedDirectoryArchive(
      utf8("z file.txt\0src/new.ts\0assets/icon.png\0"),
    );
    expect(archive as unknown).toEqual({
      format: "altitude.untracked-directory/v1",
      entries: [
        {
          repositoryPath: "assets/icon.png",
          memberPath: "git/untracked/assets/icon.png",
          copyMode: "regular-file-no-follow",
        },
        {
          repositoryPath: "src/new.ts",
          memberPath: "git/untracked/src/new.ts",
          copyMode: "regular-file-no-follow",
        },
        {
          repositoryPath: "z file.txt",
          memberPath: "git/untracked/z file.txt",
          copyMode: "regular-file-no-follow",
        },
      ],
    });
  });

  test("rejects traversal, Git internals, and duplicate untracked paths", () => {
    for (const output of [
      "../outside\0",
      "/absolute\0",
      ".git/config\0",
      "safe.txt\0safe.txt\0",
    ]) {
      expect(() => planUntrackedDirectoryArchive(utf8(output))).toThrow(
        output.includes("safe.txt")
          ? InvalidGitCaptureError
          : UnsafeCapsulePathError,
      );
    }
  });

  test("restores committed, staged, unstaged, then untracked state without overwriting files", () => {
    const capture = planGitCapture(captureInput);
    const untracked = planUntrackedDirectoryArchive(utf8("new.txt\0"));
    const snapshot = makeGitRepositorySnapshot(captureInput, capture, untracked);
    const members = [
      descriptor("git/repository.bundle", 100),
      descriptor("git/staged.patch", 20),
      descriptor("git/unstaged.patch", 30),
      descriptor("git/untracked/new.txt", 4),
    ];
    const restore = planGitRestore({
      capsuleId: "capsule-1",
      repositoryPath: "/workspace",
      snapshot,
      members,
    });
    expect(restore.workspaceMustBeDisposable).toBe(true);
    expect(restore.commands.commands.map(({ id }) => id)).toEqual([
      "git.reset-base",
      "git.fetch-bundle",
      "git.checkout-task",
      "git.apply-staged",
      "git.apply-unstaged",
    ]);
    expect(restore.files as unknown).toEqual([
      {
        kind: "restore-untracked-file",
        sourceMember: "git/untracked/new.txt",
        destinationRepositoryPath: "new.txt",
        overwrite: false,
        followSymlinks: false,
      },
    ]);
  });

  test("skips empty patches and bundle when HEAD equals the base", () => {
    const input = { ...captureInput, headCommit: BASE };
    const capture = planGitCapture(input);
    expect(capture.outputs.bundleMember).toBeNull();
    expect(capture.commands.commands.some(({ id }) => id === "git.bundle")).toBe(false);
    const snapshot = makeGitRepositorySnapshot(
      input,
      capture,
      planUntrackedDirectoryArchive(new Uint8Array()),
    );
    const restore = planGitRestore({
      capsuleId: "capsule-1",
      repositoryPath: "/workspace",
      snapshot,
      members: [
        descriptor("git/staged.patch", 0),
        descriptor("git/unstaged.patch", 0),
      ],
    });
    expect(restore.commands.commands.map(({ id }) => id)).toEqual([
      "git.reset-base",
      "git.checkout-task",
    ]);
  });
});

describe("injected command execution", () => {
  test("uses durable receipts to make repeated command plans idempotent", async () => {
    const calls: PlannedCommand[] = [];
    const result: CommandResult = {
      exitCode: 0,
      stdout: utf8("ok"),
      stderr: new Uint8Array(),
    };
    const executor: CommandExecutor = {
      async execute(command) {
        calls.push(command);
        return result;
      },
    };
    const command: PlannedCommand = {
      id: "git.status",
      program: "git",
      arguments: [{ kind: "literal", value: "status" }],
      cwd: "/workspace",
      stdin: { kind: "none" },
      stdout: { kind: "capture", key: "status" },
    };
    const plan: CommandPlan = { id: "plan-1", commands: [command] };
    const receipts = new InMemoryCommandReceiptStore();
    await executeCommandPlan(plan, executor, receipts);
    await executeCommandPlan(plan, executor, receipts);
    expect(calls).toHaveLength(1);
  });

  test("rejects reuse of an idempotency key for a different command", async () => {
    const executor: CommandExecutor = {
      async execute() {
        return { exitCode: 0, stdout: new Uint8Array(), stderr: new Uint8Array() };
      },
    };
    const receipts = new InMemoryCommandReceiptStore();
    const base: PlannedCommand = {
      id: "git.status",
      program: "git",
      arguments: [{ kind: "literal", value: "status" }],
      cwd: "/workspace",
      stdin: { kind: "none" },
      stdout: { kind: "discard" },
    };
    await executeCommandPlan({ id: "plan-1", commands: [base] }, executor, receipts);
    await expect(
      executeCommandPlan(
        {
          id: "plan-1",
          commands: [
            {
              ...base,
              arguments: [{ kind: "literal", value: "reset" }],
            },
          ],
        },
        executor,
        receipts,
      ),
    ).rejects.toBeInstanceOf(CommandIdConflictError);
  });
});
