import { describe, expect, test } from "bun:test";

import { ClaudeCliAdapter } from "../src/agent/claude-cli.ts";
import { AgentWriterLeaseConflictError, InMemoryAgentWriterLeaseStore } from "../src/agent/writer-lease.ts";
import type {
  ProcessExit,
  ProcessLauncher,
  ProcessLaunchSpec,
  RunningProcess,
} from "../src/runtime/protocol.ts";

class AsyncByteQueue implements AsyncIterable<Uint8Array> {
  readonly #values: Uint8Array[] = [];
  readonly #waiters: Array<(result: IteratorResult<Uint8Array>) => void> = [];
  #closed = false;

  push(value: string): void {
    const bytes = new TextEncoder().encode(value);
    const waiter = this.#waiters.shift();
    if (waiter === undefined) this.#values.push(bytes);
    else waiter({ done: false, value: bytes });
  }

  close(): void {
    this.#closed = true;
    for (const waiter of this.#waiters.splice(0)) waiter({ done: true, value: undefined });
  }

  [Symbol.asyncIterator](): AsyncIterator<Uint8Array> {
    return {
      next: () => {
        const value = this.#values.shift();
        if (value !== undefined) return Promise.resolve({ done: false, value });
        if (this.#closed) return Promise.resolve({ done: true, value: undefined });
        return new Promise((resolve) => this.#waiters.push(resolve));
      },
    };
  }
}

class FakeProcess implements RunningProcess {
  readonly stdout = new AsyncByteQueue();
  readonly stderr = new AsyncByteQueue();
  readonly writes: string[] = [];
  readonly exited: Promise<ProcessExit>;
  stdinClosed = false;
  terminated = false;
  rejectWrites = false;
  #resolveExit!: (exit: ProcessExit) => void;
  #finished = false;

  constructor(output?: { readonly stdout?: string; readonly stderr?: string; readonly exitCode?: number }) {
    this.exited = new Promise((resolve) => { this.#resolveExit = resolve; });
    if (output !== undefined) {
      if (output.stdout !== undefined) this.stdout.push(output.stdout);
      if (output.stderr !== undefined) this.stderr.push(output.stderr);
      this.finish(output.exitCode ?? 0);
    }
  }

  writeStdin(chunk: string | Uint8Array): Promise<void> {
    if (this.rejectWrites) return Promise.reject(new Error("stdin failed"));
    if (this.stdinClosed) return Promise.reject(new Error("stdin is closed"));
    this.writes.push(typeof chunk === "string" ? chunk : new TextDecoder().decode(chunk));
    return Promise.resolve();
  }

  closeStdin(): Promise<void> {
    this.stdinClosed = true;
    return Promise.resolve();
  }

  terminate(): Promise<void> {
    this.terminated = true;
    this.finish(143, "SIGTERM");
    return Promise.resolve();
  }

  emitStdout(value: string): void {
    this.stdout.push(value);
  }

  finish(exitCode: number, signal?: string): void {
    if (this.#finished) return;
    this.#finished = true;
    this.stdout.close();
    this.stderr.close();
    this.#resolveExit({ exitCode, ...(signal === undefined ? {} : { signal }) });
  }
}

class FakeLauncher implements ProcessLauncher {
  readonly launches: Array<ProcessLaunchSpec & { readonly io: "piped" }> = [];

  constructor(readonly processes: FakeProcess[]) {}

  launch(spec: ProcessLaunchSpec & { readonly io: "piped" }): Promise<RunningProcess> {
    this.launches.push(spec);
    const process = this.processes.shift();
    if (process === undefined) throw new Error("No fake process queued");
    return Promise.resolve(process);
  }
}

const SESSION_ID = "01900000-0000-7000-8000-000000000001";

function request(resume = false) {
  return {
    taskId: "task-1",
    sessionId: SESSION_ID,
    controlDirectory: "/identity/control/tasks/task-1",
    projectDirectoryName: "altitude-task-1",
    mcp: {
      configPath: "/run/altitude/task-1.mcp.json",
      allowedTools: ["mcp__altitude__bash", "mcp__altitude__request_review"],
    },
    displayName: "Task one",
    prompt: "Implement the first slice.",
    resume,
  } as const;
}

function adapter(
  launcher: ProcessLauncher,
  leases = new InMemoryAgentWriterLeaseStore(),
): ClaudeCliAdapter {
  let id = 0;
  return new ClaudeCliAdapter({
    identity: {
      id: "claude-personal",
      configDirectory: "/identity/claude",
      loginDirectory: "/identity",
    },
    launcher,
    leases,
    now: () => 123,
    createId: () => `holder-${++id}`,
  });
}

async function collect<T>(values: AsyncIterable<T>): Promise<T[]> {
  const result: T[] = [];
  for await (const value of values) result.push(value);
  return result;
}

describe("Claude CLI adapter", () => {
  test("reports an absent managed CLI as unavailable", async () => {
    expect(await adapter(new FakeLauncher([])).probe()).toEqual({ installed: false, auth: "unknown" });
  });

  test("probes the managed identity without inspecting credential files", async () => {
    const launcher = new FakeLauncher([
      new FakeProcess({ stdout: "2.1.241 (Claude Code)\n" }),
      new FakeProcess({ stdout: '{"loggedIn":true,"email":"private@example.test"}\n' }),
    ]);

    expect(await adapter(launcher).probe()).toEqual({
      installed: true,
      version: "2.1.241",
      auth: "authenticated",
    });
    expect(launcher.launches.map((launch) => launch.args)).toEqual([
      ["--version"],
      ["auth", "status"],
    ]);
    expect(launcher.launches.every((launch) => launch.target.kind === "identity-capsule")).toBe(true);
  });

  test("uses the documented auth-status exit contract when JSON fields evolve", async () => {
    const launcher = new FakeLauncher([
      new FakeProcess({ stdout: "2.1.241 (Claude Code)\n" }),
      new FakeProcess({ stdout: '{"account":"present"}\n' }),
    ]);
    expect((await adapter(launcher).probe()).auth).toBe("authenticated");
  });

  test("builds official login and strict native take specs", async () => {
    const leases = new InMemoryAgentWriterLeaseStore();
    const claude = adapter(new FakeLauncher([]), leases);
    const login = claude.loginLaunchSpec();
    expect(login.target).toEqual({ kind: "identity-capsule", identityId: "claude-personal" });
    expect(login.args).toEqual([]);
    expect(login.env).toEqual({ CLAUDE_CONFIG_DIR: "/identity/claude" });

    const take = await claude.prepareInteractiveTake(request());
    expect(take.launch.target).toEqual({
      kind: "identity-capsule",
      identityId: "claude-personal",
      taskId: "task-1",
    });
    expect(take.launch.args).toContain("--resume");
    expect(take.launch.args).toContain("--strict-mcp-config");
    expect(take.launch.args).toContain("--dangerously-skip-permissions");
    expect(take.launch.args).toContain("mcp__altitude__bash,mcp__altitude__request_review");
    expect(take.launch.env).toEqual({
      CLAUDE_CONFIG_DIR: "/identity/claude",
      CLAUDE_CODE_PROJECT_DIR_NAME: "altitude-task-1",
    });
    expect((await leases.current("task-1"))?.mode).toBe("interactive");
    await take.release();
    expect(await leases.current("task-1")).toBeUndefined();

    const start = await claude.prepareInteractiveStart(request());
    expect(start.launch.args).toContain("--session-id");
    expect(start.launch.args).not.toContain("--resume");
    expect(start.launch.args).toContain("Implement the first slice.");
    expect(start.launch.purpose).toContain("Start this task");
    await start.release();
  });

  test("starts a fixed stream-json session and normalizes fragmented output", async () => {
    const process = new FakeProcess();
    const launcher = new FakeLauncher([process]);
    const claude = adapter(launcher);
    const run = await claude.start(request());
    const observed = collect(run.events);

    expect(run.launch.args).toContain("-p");
    expect(run.launch.args).toContain("--session-id");
    expect(run.launch.args).toContain(SESSION_ID);
    expect(run.launch.args).not.toContain("--resume");
    expect(run.launch.args).toContain("stream-json");
    expect(run.launch.target).toEqual({
      kind: "identity-capsule",
      identityId: "claude-personal",
      taskId: "task-1",
    });
    expect(JSON.parse(process.writes[0]!)).toMatchObject({ type: "user" });

    process.emitStdout('{"type":"system","subtype":"init","session_id":"01900000-0000-7000');
    process.emitStdout('-8000-000000000001","model":"sonnet","tools":[]}\n');
    process.emitStdout('{"type":"result","subtype":"success","is_error":false,"session_id":"01900000-0000-7000-8000-000000000001","result":"Ready."}\n');
    process.finish(0);

    expect(await run.completion).toMatchObject({
      outcome: "success",
      exit: { exitCode: 0 },
      result: { type: "agent.result", text: "Ready." },
    });
    expect((await observed).map((event) => event.type)).toEqual(["agent.init", "agent.result"]);
    expect(process.stdinClosed).toBe(true);
  });

  test("cooperatively yields, then transfers the writer lease to the native TUI", async () => {
    const process = new FakeProcess();
    const leases = new InMemoryAgentWriterLeaseStore();
    const claude = adapter(new FakeLauncher([process]), leases);
    const run = await claude.start(request(true));

    await expect(claude.prepareInteractiveTake(request())).rejects.toBeInstanceOf(AgentWriterLeaseConflictError);
    expect(await run.requestYield("The operator selected Take.")).toBe(true);
    expect(process.writes).toHaveLength(2);
    expect(process.writes[1]).toContain("cooperative yield");

    process.emitStdout(`${JSON.stringify({
      type: "result",
      subtype: "success",
      is_error: false,
      session_id: SESSION_ID,
      result: "Checkpointed.",
    })}\n`);
    process.finish(0);
    expect((await run.completion).outcome).toBe("yielded");
    expect(await leases.current("task-1")).toBeUndefined();

    const take = await claude.prepareInteractiveTake(request());
    expect((await leases.current("task-1"))?.mode).toBe("interactive");
    await take.release();
  });

  test("reports rejected quota and releases the lease for a later exact resume", async () => {
    const process = new FakeProcess();
    const leases = new InMemoryAgentWriterLeaseStore();
    const claude = adapter(new FakeLauncher([process]), leases);
    const run = await claude.start(request(true));

    process.emitStdout(`${JSON.stringify({
      type: "rate_limit_event",
      rate_limit_info: { status: "rejected", rate_limit_type: "five_hour", resets_at: 1_900_000_000 },
    })}\n`);
    process.emitStdout(`${JSON.stringify({
      type: "result",
      subtype: "error_during_execution",
      is_error: true,
      session_id: SESSION_ID,
    })}\n`);
    process.finish(1);

    expect(await run.completion).toMatchObject({
      outcome: "rate_limited",
      rateLimit: { status: "rejected", resetsAtMs: 1_900_000_000_000 },
    });
    expect(await leases.current("task-1")).toBeUndefined();
  });

  test("fails closed on a mismatched resumed session and frees the writer", async () => {
    const process = new FakeProcess();
    const leases = new InMemoryAgentWriterLeaseStore();
    const claude = adapter(new FakeLauncher([process]), leases);
    const run = await claude.start(request(true));

    process.emitStdout(`${JSON.stringify({
      type: "system",
      subtype: "init",
      session_id: "01900000-0000-7000-8000-000000000099",
      tools: [],
    })}\n`);

    expect(await run.completion).toMatchObject({ outcome: "protocol_error", exit: { signal: "SIGTERM" } });
    expect(process.terminated).toBe(true);
    expect(await leases.current("task-1")).toBeUndefined();
  });

  test("stops the child and releases ownership when initial stream input fails", async () => {
    const process = new FakeProcess();
    process.rejectWrites = true;
    const leases = new InMemoryAgentWriterLeaseStore();
    const claude = adapter(new FakeLauncher([process]), leases);

    await expect(claude.start(request())).rejects.toThrow("stdin failed");
    expect(process.terminated).toBe(true);
    expect(await leases.current("task-1")).toBeUndefined();
  });

  test("marks an operator stop distinctly and releases ownership", async () => {
    const process = new FakeProcess();
    const leases = new InMemoryAgentWriterLeaseStore();
    const claude = adapter(new FakeLauncher([process]), leases);
    const run = await claude.start(request());

    await run.stop();
    expect(await run.completion).toMatchObject({ outcome: "stopped", exit: { signal: "SIGTERM" } });
    expect(await leases.current("task-1")).toBeUndefined();
  });

  test("moves only the task transcript, never the identity root", () => {
    const claude = adapter(new FakeLauncher([]));
    const exported = claude.transcriptExportPlan(request());
    const imported = claude.transcriptImportPlan(request());

    expect(exported).toEqual({
      direction: "export",
      taskId: "task-1",
      from: { scope: "identity", path: "/identity/claude/projects/altitude-task-1" },
      to: { scope: "task-capsule", path: "session/claude/files" },
      identityMaterial: "excluded",
    });
    expect(imported).toEqual({
      direction: "import",
      taskId: "task-1",
      from: { scope: "task-capsule", path: "session/claude/files" },
      to: { scope: "identity", path: "/identity/claude/projects/altitude-task-1" },
      identityMaterial: "excluded",
    });
    expect(JSON.stringify([exported, imported])).not.toContain("credentials");
  });
});
