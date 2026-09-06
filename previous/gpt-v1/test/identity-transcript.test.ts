import { afterEach, describe, expect, test } from "bun:test";
import {
  mkdirSync,
  mkdtempSync,
  readFileSync,
  rmSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  createClaudeTranscriptBundle,
  decodeClaudeTranscriptCapsuleMembers,
  encodeClaudeTranscriptWire,
} from "../src/agent/transcript.ts";
import {
  IdentityTranscriptProvisioner,
  captureIdentityTranscript,
  restoreIdentityTranscript,
} from "../src/identity/transcript.ts";
import type {
  ProcessExit,
  ProcessLauncher,
  ProcessLaunchSpec,
  RunningProcess,
} from "../src/runtime/protocol.ts";

const SESSION_ID = "01900000-0000-7000-8000-000000000001";
const BINDING = {
  taskId: "task-1",
  sessionId: SESSION_ID,
  projectDirectoryName: "altitude-task-1",
} as const;
const roots: string[] = [];

afterEach(() => {
  for (const root of roots.splice(0)) {
    rmSync(root, { recursive: true, force: true });
  }
});

function identity(label: string): string {
  const root = mkdtempSync(join(tmpdir(), "altitude-identity-transcript-" + label + "-"));
  roots.push(root);
  mkdirSync(join(root, "claude", "projects"), { recursive: true, mode: 0o700 });
  return root;
}

async function* bytes(value: Uint8Array | string): AsyncIterable<Uint8Array> {
  yield typeof value === "string" ? new TextEncoder().encode(value) : value;
}

class ImmediateProcess implements RunningProcess {
  readonly writes: Array<string | Uint8Array> = [];
  readonly stdout: AsyncIterable<Uint8Array>;
  readonly stderr: AsyncIterable<Uint8Array>;
  readonly exited: Promise<ProcessExit>;

  constructor(output: Uint8Array | string, exitCode = 0, error = "") {
    this.stdout = bytes(output);
    this.stderr = bytes(error);
    this.exited = Promise.resolve({ exitCode });
  }

  writeStdin(chunk: string | Uint8Array): Promise<void> {
    this.writes.push(typeof chunk === "string" ? chunk : chunk.slice());
    return Promise.resolve();
  }

  closeStdin(): Promise<void> {
    return Promise.resolve();
  }

  terminate(): Promise<void> {
    return Promise.resolve();
  }
}

describe("identity-local Claude transcript boundary", () => {
  test("copies only one project transcript into a fresh authenticated identity", async () => {
    const source = identity("source");
    const sourceProject = join(source, "claude", "projects", BINDING.projectDirectoryName);
    mkdirSync(join(sourceProject, "subagents"), { recursive: true });
    writeFileSync(join(sourceProject, SESSION_ID + ".jsonl"), "turn one\n");
    writeFileSync(join(sourceProject, "subagents", "worker.jsonl"), "worker\n");
    writeFileSync(join(source, "claude", ".credentials.json"), "source-secret");

    const transcript = await captureIdentityTranscript(source, BINDING);
    expect(transcript.files.map((file) => file.path)).toEqual([
      SESSION_ID + ".jsonl",
      "subagents/worker.jsonl",
    ]);

    const target = identity("target");
    writeFileSync(join(target, "claude", ".credentials.json"), "target-secret");
    expect(await restoreIdentityTranscript(target, transcript)).toBe("installed");
    expect(readFileSync(
      join(target, "claude", "projects", BINDING.projectDirectoryName, SESSION_ID + ".jsonl"),
      "utf8",
    )).toBe("turn one\n");
    expect(readFileSync(join(target, "claude", ".credentials.json"), "utf8"))
      .toBe("target-secret");
  });

  test("keeps a newer local transcript but rejects divergence and symlinks", async () => {
    const target = identity("existing");
    const project = join(target, "claude", "projects", BINDING.projectDirectoryName);
    mkdirSync(project);
    writeFileSync(join(project, SESSION_ID + ".jsonl"), "old\nnew\n");
    const incoming = createClaudeTranscriptBundle({
      ...BINDING,
      files: [{ path: SESSION_ID + ".jsonl", bytes: new TextEncoder().encode("old\n") }],
    });
    expect(await restoreIdentityTranscript(target, incoming))
      .toBe("already_present");

    const divergent = createClaudeTranscriptBundle({
      ...BINDING,
      files: [{ path: SESSION_ID + ".jsonl", bytes: new TextEncoder().encode("other\n") }],
    });
    expect(restoreIdentityTranscript(target, divergent)).rejects.toThrow(
      "diverges",
    );

    const source = identity("symlink");
    const sourceProject = join(source, "claude", "projects", BINDING.projectDirectoryName);
    mkdirSync(sourceProject);
    symlinkSync(join(source, "claude", ".credentials.json"), join(sourceProject, "leak.jsonl"));
    expect(captureIdentityTranscript(source, BINDING)).rejects.toThrow(
      "symbolic links",
    );
  });

  test("moves transcript bytes through a task-bound identity helper process", async () => {
    const transcript = createClaudeTranscriptBundle({
      ...BINDING,
      files: [{ path: SESSION_ID + ".jsonl", bytes: new TextEncoder().encode("turn\n") }],
    });
    const exportProcess = new ImmediateProcess(encodeClaudeTranscriptWire(transcript));
    const importProcess = new ImmediateProcess(JSON.stringify({
      schema: "altitude.identity-transcript/v1",
      state: "installed",
    }) + "\n");
    const launches: Array<ProcessLaunchSpec & { readonly io: "piped" }> = [];
    const queue = [exportProcess, importProcess];
    const launcher: ProcessLauncher = {
      launch(spec) {
        launches.push(spec);
        const process = queue.shift();
        if (process === undefined) throw new Error("No helper process queued");
        return Promise.resolve(process);
      },
    };
    const provisioner = new IdentityTranscriptProvisioner(
      launcher,
      "claude-personal",
    );
    const members = await provisioner.export(BINDING);
    expect(decodeClaudeTranscriptCapsuleMembers(members)).toEqual(transcript);
    expect(await provisioner.import(BINDING, members)).toBe("installed");
    expect(launches.map((launch) => launch.args)).toEqual([
      ["_identity-transcript", "export"],
      ["_identity-transcript", "import"],
    ]);
    expect(launches.every((launch) =>
      launch.target.kind === "identity-capsule"
      && launch.target.taskId === BINDING.taskId
      && launch.cwd === "/identity"
      && launch.env?.ALTITUDE_IDENTITY_TRANSCRIPT === "1"
    )).toBeTrue();
    expect(JSON.stringify(launches)).not.toContain("credentials");
  });
});
