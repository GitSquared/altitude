import { describe, expect, test } from "bun:test";

import type { CommandResult, CommandRunner, CommandSpec } from "../src/process/command.ts";
import {
  taskVolumeName,
  TaskCapsuleBootstrapProvisioner,
  TaskCapsuleCheckpointProvisioner,
  TaskCapsuleManager,
} from "../src/runtime/task-manager.ts";
import type { ProcessExit, ProcessLaunchSpec, ProcessLauncher, RunningProcess } from "../src/runtime/protocol.ts";
import { createCapsuleCheckpoint, capsuleManifestDigest } from "../src/capsule/manifest.ts";
import { portableCapsulePath } from "../src/capsule/paths.ts";
import { TASK_CHECKPOINT_PROTOCOL, decodeTaskCheckpointRequest } from "../src/capsule/task-checkpoint.ts";
import { encodeCapsuleWire } from "../src/capsule/wire.ts";

const ok: CommandResult = { exitCode: 0, stdout: "", stderr: "" };

class SequenceRunner implements CommandRunner {
  readonly commands: CommandSpec[] = [];
  constructor(private readonly results: CommandResult[]) {}
  run(spec: CommandSpec): Promise<CommandResult> {
    this.commands.push(spec);
    return Promise.resolve(this.results.shift()!);
  }
  runInteractive(): Promise<number> { throw new Error("unexpected"); }
}

class Bytes implements AsyncIterable<Uint8Array> {
  constructor(private readonly text: string) {}
  async *[Symbol.asyncIterator]() { yield new TextEncoder().encode(this.text); }
}

class CapturedProcess implements RunningProcess {
  readonly stdout = new Bytes('{"schema":"altitude.task-bootstrap/v1","ok":true,"result":{"reused":false}}\n');
  readonly stderr = new Bytes("");
  readonly exited: Promise<ProcessExit> = Promise.resolve({ exitCode: 0 });
  input?: Uint8Array;
  writeStdin(chunk: string | Uint8Array): Promise<void> {
    this.input = typeof chunk === "string" ? new TextEncoder().encode(chunk) : new Uint8Array(chunk);
    return Promise.resolve();
  }
  closeStdin(): Promise<void> { return Promise.resolve(); }
  terminate(): Promise<void> { return Promise.resolve(); }
}

class BinaryProcess implements RunningProcess {
  readonly stdout: AsyncIterable<Uint8Array>;
  readonly stderr = new Bytes("");
  readonly exited: Promise<ProcessExit>;
  input = new Uint8Array();

  constructor(output: Uint8Array | string, exitCode = 0) {
    const bytes = typeof output === "string" ? new TextEncoder().encode(output) : output;
    this.stdout = { async *[Symbol.asyncIterator]() { yield bytes; } };
    this.exited = Promise.resolve({ exitCode });
  }

  writeStdin(chunk: string | Uint8Array): Promise<void> {
    this.input = typeof chunk === "string" ? new TextEncoder().encode(chunk) : new Uint8Array(chunk);
    return Promise.resolve();
  }
  closeStdin(): Promise<void> { return Promise.resolve(); }
  terminate(): Promise<void> { return Promise.resolve(); }
}

describe("task capsule manager", () => {
  test("initializes a private volume and launches a constrained disposable workspace", async () => {
    const runner = new SequenceRunner([ok, ok, ok, { exitCode: 1, stdout: "", stderr: "missing" }, ok, ok]);
    const result = await new TaskCapsuleManager("docker", runner).ensure({
      taskId: "task-1",
      image: "altitude-task:dev",
      network: "altitude-control",
    });
    expect(result).toMatchObject({ state: "created", volume: taskVolumeName("task-1") });
    expect(runner.commands[2]?.args).toContain("ALTITUDE_TASK_INIT=1");
    expect(runner.commands[4]?.args).toContain("host.docker.internal:host-gateway");
    expect(runner.commands[4]?.args).toContain("HOME=/workspace/.altitude/environment/home");
    expect(runner.commands[4]?.args).toContain("XDG_CACHE_HOME=/tmp/altitude-cache");
    expect(runner.commands[4]?.args?.join(" ")).not.toContain("docker.sock");
  });

  test("streams a typed checkpoint only to the bound task capsule", async () => {
    const child = new CapturedProcess();
    const launches: Array<ProcessLaunchSpec & { readonly io: "piped" }> = [];
    const launcher: ProcessLauncher = {
      launch(spec) { launches.push(spec); return Promise.resolve(child); },
    };
    const empty = new Uint8Array();
    const checkpoint = createCapsuleCheckpoint({
      capsuleId: "capsule-1",
      taskId: "task-1",
      revision: 1,
      checkpointedAt: "2026-08-29T12:00:00.000Z",
      ownership: { epoch: 1, leaseId: "lease-1", ownerNodeId: "local", status: "active" },
      repository: {
        kind: "git",
        remoteUrl: "git@github.com:gitsquared/fixture.git",
        defaultBranch: "main",
        branch: "main",
        baseCommit: "a".repeat(40),
        headCommit: "a".repeat(40),
        bundleMember: null,
        stagedPatchMember: portableCapsulePath("git/staged.patch"),
        unstagedPatchMember: portableCapsulePath("git/unstaged.patch"),
        untracked: [],
      },
      members: [
        { path: portableCapsulePath("git/staged.patch"), bytes: empty },
        { path: portableCapsulePath("git/unstaged.patch"), bytes: empty },
      ],
    });
    expect(await new TaskCapsuleBootstrapProvisioner(launcher).bootstrap({
      taskId: "task-1",
      checkpoint,
      repositorySeedBundle: new Uint8Array([1, 2, 3]),
    })).toEqual({ reused: false });
    expect(launches[0]).toMatchObject({
      target: { kind: "task-capsule", taskId: "task-1" },
      args: ["_task-bootstrap"],
      env: { ALTITUDE_TASK_BOOTSTRAP: "1" },
    });
    expect(child.input?.byteLength).toBeGreaterThan(3);
  });

  test("captures, recovers, and activates task checkpoints through the bound capsule", async () => {
    const state = { path: portableCapsulePath("state/task.json"), bytes: new TextEncoder().encode("{}\n") };
    const next = createCapsuleCheckpoint({
      capsuleId: "task-1",
      taskId: "task-1",
      revision: 2,
      checkpointedAt: "2026-08-29T12:05:00.000Z",
      ownership: { epoch: 1, leaseId: "lease-1", ownerNodeId: "local", status: "active" },
      repository: {
        kind: "git",
        remoteUrl: "git@github.com:gitsquared/fixture.git",
        defaultBranch: "main",
        branch: "main",
        baseCommit: "a".repeat(40),
        headCommit: "a".repeat(40),
        bundleMember: null,
        stagedPatchMember: portableCapsulePath("git/staged.patch"),
        unstagedPatchMember: portableCapsulePath("git/unstaged.patch"),
        untracked: [],
      },
      members: [
        { path: portableCapsulePath("git/staged.patch"), bytes: new Uint8Array() },
        { path: portableCapsulePath("git/unstaged.patch"), bytes: new Uint8Array() },
        state,
      ],
    });
    const captureChild = new BinaryProcess(encodeCapsuleWire(next));
    const children = [
      captureChild,
      new BinaryProcess(encodeCapsuleWire(next)),
      new BinaryProcess(`${JSON.stringify({ schema: TASK_CHECKPOINT_PROTOCOL, ok: true })}\n`),
    ];
    const launches: Array<ProcessLaunchSpec & { readonly io: "piped" }> = [];
    const launcher: ProcessLauncher = {
      launch(spec) { launches.push(spec); return Promise.resolve(children.shift()!); },
    };
    const provisioner = new TaskCapsuleCheckpointProvisioner(launcher);
    const request = {
      schema: TASK_CHECKPOINT_PROTOCOL,
      capsuleId: "task-1",
      taskId: "task-1",
      revision: 2,
      checkpointedAt: "2026-08-29T12:05:00.000Z",
      ownership: next.manifest.ownership,
      previousCheckpointDigest: `sha256:${"1".repeat(64)}`,
      repositoryBindingDigest: `sha256:${"2".repeat(64)}`,
      additionalMembers: [state],
    } as const;
    expect(await provisioner.capture(request)).toEqual(next);
    expect(await provisioner.pending("task-1")).toEqual(next);
    await provisioner.activate("task-1", capsuleManifestDigest(next.manifest));

    expect(decodeTaskCheckpointRequest(captureChild.input)).toEqual(request);
    expect(launches.map((launch) => launch.args)).toEqual([
      ["_task-checkpoint", "capture"],
      ["_task-checkpoint", "pending"],
      ["_task-checkpoint", "activate"],
    ]);
    expect(launches.every((launch) => launch.target.kind === "task-capsule")).toBeTrue();
  });
});
