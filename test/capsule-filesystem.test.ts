import { afterEach, describe, expect, test } from "bun:test";
import {
  chmodSync,
  lstatSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  symlinkSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";

import {
  CapsuleCheckpointLimitError,
  CapsuleCheckpointNotFoundError,
  FilesystemCapsuleCheckpointStore,
  UnsafeCapsuleFilesystemError,
  capsuleManifestDigest,
  createCapsuleCheckpoint,
  portableCapsulePath,
  safeRepositoryPath,
  stableJson,
  untrackedMemberPath,
  utf8,
  type CapsuleCheckpoint,
  type CapsuleMemberContent,
  type GitRepositorySnapshot,
} from "../src/capsule/index.ts";

const temporaryRoots: string[] = [];
const BASE = "1".repeat(40);
const HEAD = "2".repeat(40);

afterEach(() => {
  for (const root of temporaryRoots.splice(0)) {
    rmSync(root, { force: true, recursive: true });
  }
});

function temporaryRoot(label: string): string {
  const root = mkdtempSync(join(tmpdir(), `altitude-${label}-`));
  temporaryRoots.push(root);
  return root;
}

function member(path: string, value: string | Uint8Array): CapsuleMemberContent {
  return {
    path: portableCapsulePath(path),
    bytes: typeof value === "string" ? utf8(value) : value,
  };
}

function makeCheckpoint(input: {
  readonly revision?: number;
  readonly members?: readonly CapsuleMemberContent[];
  readonly repository?: GitRepositorySnapshot;
} = {}): CapsuleCheckpoint {
  const members = input.members ?? [
    member("git/repository.bundle", "bundle"),
    member("git/staged.patch", "staged"),
    member("git/unstaged.patch", "unstaged"),
    member("git/untracked/notes/idea.txt", "small capsule, big idea"),
    member("state/task.sqlite", "sqlite bytes"),
  ];
  const repository = input.repository ?? {
    kind: "git",
    remoteUrl: "git@github.com:gitsquared/example.git",
    defaultBranch: "main",
    branch: "altitude/task-1",
    baseCommit: BASE,
    headCommit: HEAD,
    bundleMember: portableCapsulePath("git/repository.bundle"),
    stagedPatchMember: portableCapsulePath("git/staged.patch"),
    unstagedPatchMember: portableCapsulePath("git/unstaged.patch"),
    untracked: [
      {
        repositoryPath: safeRepositoryPath("notes/idea.txt"),
        memberPath: untrackedMemberPath(safeRepositoryPath("notes/idea.txt")),
      },
    ],
  } satisfies GitRepositorySnapshot;
  return createCapsuleCheckpoint({
    capsuleId: "capsule-1",
    taskId: "task-1",
    revision: input.revision ?? 7,
    checkpointedAt: "2026-08-28T08:30:00.000Z",
    ownership: {
      epoch: 4,
      ownerNodeId: "macbook",
      leaseId: "lease-4",
      status: "active",
    },
    repository,
    members,
  });
}

describe("filesystem capsule checkpoints", () => {
  test("publishes atomically, persists private permissions, activates, and survives restart", async () => {
    const root = join(temporaryRoot("checkpoint-restart"), "capsules");
    const original = makeCheckpoint();
    const writer = new FilesystemCapsuleCheckpointStore(root);
    const reference = await writer.publish(original);
    expect(reference).toEqual({
      capsuleId: "capsule-1",
      revision: 7,
      digest: capsuleManifestDigest(original.manifest),
    });

    const directory = writer.checkpointDirectory(reference);
    expect(lstatSync(directory).mode & 0o777).toBe(0o700);
    expect(lstatSync(join(directory, "capsule.json")).mode & 0o777).toBe(0o600);
    expect(
      lstatSync(join(directory, "members", "state", "task.sqlite")).mode & 0o777,
    ).toBe(0o600);

    await writer.activate(reference);
    const restarted = new FilesystemCapsuleCheckpointStore(root);
    const restored = await restarted.readActive("capsule-1");
    expect(restored.manifest).toEqual(original.manifest);
    expect(
      new TextDecoder().decode(
        restored.members.find(({ path }) => path === "state/task.sqlite")?.bytes,
      ),
    ).toBe("sqlite bytes");
    expect(await restarted.publish(original)).toEqual(reference);
  });

  test("never publishes or activates a partial checkpoint", async () => {
    const root = join(temporaryRoot("checkpoint-partial"), "capsules");
    const store = new FilesystemCapsuleCheckpointStore(root);
    const checkpoint = makeCheckpoint();
    const reference = {
      capsuleId: checkpoint.manifest.capsuleId,
      revision: checkpoint.manifest.revision,
      digest: capsuleManifestDigest(checkpoint.manifest),
    } as const;
    const directory = store.checkpointDirectory(reference);
    mkdirSync(join(directory, "members"), { recursive: true, mode: 0o700 });
    writeFileSync(
      join(directory, "capsule.json"),
      `${stableJson(checkpoint.manifest)}\n`,
      { mode: 0o600 },
    );

    await expect(store.activate(reference)).rejects.toThrow(
      "missing member git/repository.bundle",
    );
    await expect(store.readActive("capsule-1")).rejects.toBeInstanceOf(
      CapsuleCheckpointNotFoundError,
    );
  });

  test("rejects entries that are not committed by the canonical manifest", async () => {
    const root = join(temporaryRoot("checkpoint-extra"), "capsules");
    const store = new FilesystemCapsuleCheckpointStore(root);
    const reference = await store.publish(makeCheckpoint());
    const directory = store.checkpointDirectory(reference);
    writeFileSync(join(directory, "node-local-secret"), "not portable");
    await expect(store.read(reference)).rejects.toThrow(
      "unexpected entry node-local-secret",
    );
    unlinkSync(join(directory, "node-local-secret"));
    mkdirSync(join(directory, "members", "state", "empty-extra"));
    await expect(store.read(reference)).rejects.toThrow(
      "unexpected member directory state/empty-extra",
    );
  });

  test("rejects traversal references and no-follow symlink entries", async () => {
    const root = join(temporaryRoot("checkpoint-symlink"), "capsules");
    const store = new FilesystemCapsuleCheckpointStore(root);
    const reference = await store.publish(makeCheckpoint());
    expect(() =>
      store.checkpointDirectory({ ...reference, capsuleId: "../outside" })
    ).toThrow(UnsafeCapsuleFilesystemError);

    const external = join(temporaryRoot("checkpoint-outside"), "secret.txt");
    writeFileSync(external, "node-local secret");
    const memberPath = join(
      store.checkpointDirectory(reference),
      "members",
      "state",
      "task.sqlite",
    );
    unlinkSync(memberPath);
    symlinkSync(external, memberPath);
    await expect(store.read(reference)).rejects.toThrow("is a symlink");
  });

  test("rejects a symlinked checkpoint root", async () => {
    const parent = temporaryRoot("checkpoint-root-link");
    const target = join(parent, "target");
    mkdirSync(target);
    const link = join(parent, "link");
    symlinkSync(target, link);
    const store = new FilesystemCapsuleCheckpointStore(link);
    await expect(store.publish(makeCheckpoint())).rejects.toThrow(
      "not a no-follow regular directory",
    );
  });

  test("rejects a symlinked capsule directory instead of following it outside the store", async () => {
    const root = join(temporaryRoot("checkpoint-parent-link"), "capsules");
    const store = new FilesystemCapsuleCheckpointStore(root);
    const reference = await store.publish(makeCheckpoint());
    const capsuleDirectory = join(root, "capsules", "capsule-1");
    const outside = temporaryRoot("checkpoint-parent-outside");
    rmSync(capsuleDirectory, { force: true, recursive: true });
    symlinkSync(outside, capsuleDirectory);
    await expect(store.read(reference)).rejects.toThrow(
      "capsule directory is not a no-follow regular directory",
    );
  });

  test("detects corruption before changing the active checkpoint", async () => {
    const root = join(temporaryRoot("checkpoint-corruption"), "capsules");
    const store = new FilesystemCapsuleCheckpointStore(root);
    const first = await store.publish(makeCheckpoint());
    await store.activate(first);
    const second = await store.publish(makeCheckpoint({ revision: 8 }));
    const corruptPath = join(
      store.checkpointDirectory(second),
      "members",
      "state",
      "task.sqlite",
    );
    writeFileSync(corruptPath, "corrupt data");
    chmodSync(corruptPath, 0o600);

    await expect(store.activate(second)).rejects.toThrow(/mismatch/u);
    expect((await store.readActive("capsule-1")).manifest.revision).toBe(7);
  });

  test("enforces member and aggregate limits before publication", async () => {
    const root = join(temporaryRoot("checkpoint-limits"), "capsules");
    const store = new FilesystemCapsuleCheckpointStore(root, {
      maxMemberBytes: 8,
      maxTotalBytes: 24,
    });
    await expect(store.publish(makeCheckpoint())).rejects.toBeInstanceOf(
      CapsuleCheckpointLimitError,
    );
  });

  test("round-trips patches and untracked bytes captured from a real Git repository", async () => {
    const repositoryRoot = temporaryRoot("checkpoint-git");
    runGit(repositoryRoot, "init", "--initial-branch=main");
    runGit(repositoryRoot, "config", "user.name", "Altitude Test");
    runGit(repositoryRoot, "config", "user.email", "altitude@example.test");
    writeFileSync(join(repositoryRoot, "tracked.txt"), "base\n");
    runGit(repositoryRoot, "add", "tracked.txt");
    runGit(repositoryRoot, "-c", "commit.gpgsign=false", "commit", "-m", "base");
    const head = runGit(repositoryRoot, "rev-parse", "HEAD").trim();
    writeFileSync(join(repositoryRoot, "tracked.txt"), "staged\n");
    runGit(repositoryRoot, "add", "tracked.txt");
    writeFileSync(join(repositoryRoot, "tracked.txt"), "unstaged\n");
    writeFileSync(join(repositoryRoot, "new file.txt"), "untracked\n");

    const staged = runGitBytes(
      repositoryRoot,
      "diff",
      "--cached",
      "--binary",
      "--full-index",
      "--no-ext-diff",
      "--no-renames",
    );
    const unstaged = runGitBytes(
      repositoryRoot,
      "diff",
      "--binary",
      "--full-index",
      "--no-ext-diff",
      "--no-renames",
    );
    const untrackedPath = safeRepositoryPath("new file.txt");
    const snapshot: GitRepositorySnapshot = {
      kind: "git",
      remoteUrl: "git@github.com:gitsquared/example.git",
      defaultBranch: "main",
      branch: "altitude/git-fixture",
      baseCommit: head,
      headCommit: head,
      bundleMember: null,
      stagedPatchMember: portableCapsulePath("git/staged.patch"),
      unstagedPatchMember: portableCapsulePath("git/unstaged.patch"),
      untracked: [
        {
          repositoryPath: untrackedPath,
          memberPath: untrackedMemberPath(untrackedPath),
        },
      ],
    };
    const checkpoint = makeCheckpoint({
      repository: snapshot,
      members: [
        member("git/staged.patch", staged),
        member("git/unstaged.patch", unstaged),
        member(
          "git/untracked/new file.txt",
          readFileSync(join(repositoryRoot, "new file.txt")),
        ),
      ],
    });
    const checkpointRoot = join(temporaryRoot("checkpoint-git-store"), "capsules");
    const reference = await new FilesystemCapsuleCheckpointStore(
      checkpointRoot,
    ).publish(checkpoint);
    const restored = await new FilesystemCapsuleCheckpointStore(
      checkpointRoot,
    ).read(reference);
    expect(restored.members.map(({ path }) => String(path))).toEqual([
      "git/staged.patch",
      "git/unstaged.patch",
      "git/untracked/new file.txt",
    ]);
    expect(restored.members[0]?.bytes).toEqual(staged);
    expect(restored.members[1]?.bytes).toEqual(unstaged);
  });
});

function runGit(repository: string, ...arguments_: readonly string[]): string {
  return new TextDecoder().decode(runGitBytes(repository, ...arguments_));
}

function runGitBytes(
  repository: string,
  ...arguments_: readonly string[]
): Uint8Array {
  const result = Bun.spawnSync({
    cmd: ["git", ...arguments_],
    cwd: repository,
    env: { ...process.env, GIT_CONFIG_NOSYSTEM: "1" },
    stdout: "pipe",
    stderr: "pipe",
  });
  if (result.exitCode !== 0) {
    throw new Error(new TextDecoder().decode(result.stderr));
  }
  return result.stdout;
}
