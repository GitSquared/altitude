import { afterEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { ProcessTaskToolExecutor } from "../src/tools/process-executor.ts";
import { TASK_TOOL_PROTOCOL, type TaskToolRequest } from "../src/tools/protocol.ts";
import { executeTaskTool } from "../src/tools/task-helper.ts";
import type { ProcessExit, ProcessLaunchSpec, ProcessLauncher, RunningProcess } from "../src/runtime/protocol.ts";

const temporaryDirectories: string[] = [];

afterEach(() => {
  for (const directory of temporaryDirectories.splice(0)) rmSync(directory, { recursive: true, force: true });
});

function workspace(): string {
  const root = mkdtempSync(join(tmpdir(), "altitude-task-tool-test-"));
  temporaryDirectories.push(root);
  mkdirSync(join(root, "src"));
  writeFileSync(join(root, "src", "hello.ts"), "one\ntwo\nthree\n");
  return root;
}

function request<T extends Omit<TaskToolRequest, "schema">>(value: T): T & { schema: typeof TASK_TOOL_PROTOCOL } {
  return { schema: TASK_TOOL_PROTOCOL, ...value };
}

class ByteStream implements AsyncIterable<Uint8Array> {
  constructor(private readonly value: string) {}
  async *[Symbol.asyncIterator](): AsyncIterator<Uint8Array> {
    yield new TextEncoder().encode(this.value);
  }
}

class CapturedProcess implements RunningProcess {
  readonly stdout: AsyncIterable<Uint8Array>;
  readonly stderr = new ByteStream("");
  readonly exited: Promise<ProcessExit>;
  input = "";
  closed = false;
  terminated = false;

  constructor(response: unknown, exitCode = 0) {
    this.stdout = new ByteStream(`${JSON.stringify(response)}\n`);
    this.exited = Promise.resolve({ exitCode });
  }

  writeStdin(chunk: string | Uint8Array): Promise<void> {
    this.input += typeof chunk === "string" ? chunk : new TextDecoder().decode(chunk);
    return Promise.resolve();
  }

  closeStdin(): Promise<void> {
    this.closed = true;
    return Promise.resolve();
  }

  terminate(): Promise<void> {
    this.terminated = true;
    return Promise.resolve();
  }
}

describe("task capsule tools", () => {
  test("reads line ranges and uses content hashes for conflict-safe writes", async () => {
    const root = workspace();
    const read = await executeTaskTool(root, request({ operation: "read_file", path: "src/hello.ts", startLine: 2, endLine: 3 }));
    expect(read.ok).toBeTrue();
    if (!read.ok) return;
    expect(read.result).toMatchObject({ content: "two\nthree", startLine: 2, endLine: 3 });
    const digest = (read.result as { sha256: string }).sha256;

    const write = await executeTaskTool(root, request({
      operation: "write_file",
      path: "src/hello.ts",
      content: "changed\n",
      expectedSha256: digest,
    }));
    expect(write.ok).toBeTrue();
    const stale = await executeTaskTool(root, request({
      operation: "write_file",
      path: "src/hello.ts",
      content: "lost update\n",
      expectedSha256: digest,
    }));
    expect(stale).toMatchObject({ ok: false, error: { code: "write_conflict" } });
  });

  test("never follows workspace symlinks or exposes Git internals", async () => {
    const root = workspace();
    mkdirSync(join(root, ".git"));
    writeFileSync(join(root, ".git", "config"), "secret-ish metadata");
    symlinkSync("/etc/passwd", join(root, "outside"));

    const read = await executeTaskTool(root, request({ operation: "read_file", path: "outside" }));
    expect(read).toMatchObject({ ok: false, error: { code: "symlink_rejected" } });
    const traversal = await executeTaskTool(root, request({ operation: "read_file", path: "../outside" }));
    expect(traversal).toMatchObject({ ok: false, error: { code: "invalid_path" } });
    const listed = await executeTaskTool(root, request({ operation: "list_files", depth: 3 }));
    expect(listed.ok).toBeTrue();
    if (listed.ok) {
      const paths = (listed.result as { entries: Array<{ path: string }> }).entries.map((entry) => entry.path);
      expect(paths).not.toContain(".git");
      expect(paths).toContain("outside");
    }
  });

  test("runs argv commands without a shell and searches with fixed strings", async () => {
    const root = workspace();
    const init = await executeTaskTool(root, request({
      operation: "run_command",
      executable: "git",
      args: ["init", "--quiet"],
    }));
    expect(init).toMatchObject({ ok: true, result: { exitCode: 0 } });

    const search = await executeTaskTool(root, request({
      operation: "search_text",
      query: "two",
      path: "src",
      glob: "*.ts",
    }));
    expect(search).toMatchObject({ ok: true, result: { matches: ["src/hello.ts:2:1:two"] } });
    const shell = await executeTaskTool(root, request({
      operation: "run_command",
      executable: "sh -c",
      args: ["touch escaped"],
    }));
    expect(shell).toMatchObject({ ok: false, error: { code: "invalid_command" } });
  });

  test("creates requested parent directories but rejects symlink parents", async () => {
    const root = workspace();
    const created = await executeTaskTool(root, request({
      operation: "write_file",
      path: "new/nested/file.ts",
      content: "export {};\n",
      expectedSha256: null,
      createDirectories: true,
    }));
    expect(created).toMatchObject({ ok: true, result: { created: true } });
    symlinkSync("/tmp", join(root, "linked"));
    const rejected = await executeTaskTool(root, request({
      operation: "write_file",
      path: "linked/file.ts",
      content: "nope",
    }));
    expect(rejected).toMatchObject({ ok: false });
  });

  test("creates only draft PRs and evaluates normalized GitHub readiness", async () => {
    const root = workspace();
    const created: unknown[] = [];
    const snapshot = {
      url: "https://github.com/example/project/pull/42",
      number: 42,
      title: "Capsule work",
      state: "open" as const,
      draft: true,
      mergeState: "CLEAN",
      reviewDecision: "",
      headRefName: "agent/capsule",
      headSha: "a".repeat(40),
      checks: [{ name: "test", state: "success" as const }],
      reviews: [],
      observedAt: "2026-08-31T12:00:00.000Z",
    };
    const dependencies = {
      github: {
        authenticated: async () => true,
        createDraftPullRequest: async (input: unknown) => {
          created.push(input);
          return snapshot.url;
        },
        inspectPullRequest: async () => snapshot,
      },
    };
    const create = await executeTaskTool(root, request({
      operation: "github_pr_create",
      repository: "example/project",
      head: "agent/capsule",
      base: "main",
      title: "Capsule work",
      body: "Checked by Altitude",
    }), dependencies);
    expect(create).toMatchObject({ ok: true, result: { url: snapshot.url, draft: true } });
    expect(created).toHaveLength(1);

    const inspect = await executeTaskTool(root, request({
      operation: "github_pr_inspect",
      url: snapshot.url,
      evidence: {
        worktreeClean: true,
        headPushed: true,
        pathExercised: false,
        feedbackAddressed: true,
      },
    }), dependencies);
    expect(inspect).toMatchObject({
      ok: true,
      result: {
        snapshot: { number: 42 },
        readiness: { ready: false, blockers: ["The changed path has not been exercised"] },
      },
    });

    const loggedOut = await executeTaskTool(root, request({
      operation: "github_pr_inspect",
      url: snapshot.url,
      evidence: {
        worktreeClean: true,
        headPushed: true,
        pathExercised: true,
        feedbackAddressed: true,
      },
    }), {
      github: { ...dependencies.github, authenticated: async () => false },
    });
    expect(loggedOut).toMatchObject({ ok: false, error: { code: "github_auth_required" } });
  });
});

describe("task tool process boundary", () => {
  test("binds the helper to one task capsule and exchanges one typed request", async () => {
    const response = { schema: TASK_TOOL_PROTOCOL, ok: true, result: { path: "README.md" } } as const;
    const child = new CapturedProcess(response);
    const launches: Array<ProcessLaunchSpec & { readonly io: "piped" }> = [];
    const launcher: ProcessLauncher = {
      launch(spec) {
        launches.push(spec);
        return Promise.resolve(child);
      },
    };
    const executor = new ProcessTaskToolExecutor(launcher);
    const toolRequest = request({ operation: "read_file", path: "README.md" });
    expect(await executor.execute("task-1", toolRequest)).toEqual(response);
    expect(launches[0]).toMatchObject({
      target: { kind: "task-capsule", taskId: "task-1" },
      executable: "altitude",
      args: ["_task-tool"],
      cwd: "/workspace",
      env: { ALTITUDE_TASK_TOOL: "1" },
    });
    expect(JSON.parse(child.input)).toEqual(toolRequest);
    expect(child.closed).toBeTrue();
  });
});
