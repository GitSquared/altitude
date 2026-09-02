import { join, posix } from "node:path";

import type { ProcessExit, ProcessLauncher, ProcessLaunchSpec, RunningProcess } from "../runtime/protocol.ts";
import type {
  AgentAdapter,
  AgentCapabilities,
  AgentEvent,
  AgentInteractiveStartRequest,
  AgentInteractiveSession,
  AgentProbe,
  AgentRun,
  AgentRunCompletion,
  AgentRunRequest,
  AgentTakeRequest,
  AgentTaskSession,
  AgentTranscriptTransferPlan,
} from "./protocol.ts";
import { EXPERIMENTAL_AGENT_PROTOCOL } from "./protocol.ts";
import { encodeClaudeUserInput, parseClaudeStreamLine } from "./claude-stream.ts";
import {
  AgentWriterLeaseConflictError,
  type AgentWriterLease,
  type AgentWriterLeaseStore,
  type AgentWriterMode,
} from "./writer-lease.ts";

const SESSION_ID = /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i;
const PROJECT_DIRECTORY = /^[A-Za-z0-9._-]+$/;
const MAX_CAPTURE_BYTES = 64 * 1024;

const SAFE_IDENTITY_TOOLS = [
  "Agent",
  "AskUserQuestion",
  "CronCreate",
  "CronDelete",
  "CronList",
  "EndConversation",
  "EnterPlanMode",
  "ExitPlanMode",
  "ListAgents",
  "ListMcpResourcesTool",
  "ReadMcpResourceTool",
  "ReportFindings",
  "SendMessage",
  "Skill",
  "TaskCreate",
  "TaskGet",
  "TaskList",
  "TaskStop",
  "TaskUpdate",
  "ToolSearch",
  "WaitForMcpServers",
  "WebFetch",
  "WebSearch",
  "Workflow",
] as const;

const PREAPPROVED_IDENTITY_TOOLS = ["Skill", "WebFetch", "WebSearch", "Workflow"] as const;

function insideIdentity(path: string): boolean {
  const normalized = posix.normalize(path);
  return normalized === path && (path === "/identity" || path.startsWith("/identity/"));
}

export interface ClaudeIdentity {
  readonly id: string;
  /** Absolute path inside the identity/task agent capsule. */
  readonly configDirectory: string;
  /** An identity-only directory; never a source workspace. */
  readonly loginDirectory: string;
}

export interface ClaudeCliAdapterOptions {
  readonly identity: ClaudeIdentity;
  readonly launcher: ProcessLauncher;
  readonly leases: AgentWriterLeaseStore;
  readonly executable?: string;
  readonly now?: () => number;
  readonly createId?: () => string;
}

interface EventQueue<T> extends AsyncIterable<T> {
  push(value: T): void;
  close(): void;
}

interface CapturedProcess {
  readonly exit: ProcessExit;
  readonly stdout: string;
}

function eventQueue<T>(): EventQueue<T> {
  const values: T[] = [];
  const waiters: Array<(result: IteratorResult<T>) => void> = [];
  let closed = false;

  return {
    push(value) {
      if (closed) return;
      const waiter = waiters.shift();
      if (waiter === undefined) values.push(value);
      else waiter({ done: false, value });
    },
    close() {
      if (closed) return;
      closed = true;
      for (const waiter of waiters.splice(0)) waiter({ done: true, value: undefined });
    },
    [Symbol.asyncIterator]() {
      return {
        next(): Promise<IteratorResult<T>> {
          const value = values.shift();
          if (value !== undefined) return Promise.resolve({ done: false, value });
          if (closed) return Promise.resolve({ done: true, value: undefined });
          return new Promise((resolve) => waiters.push(resolve));
        },
      };
    },
  };
}

function validateSession(session: AgentTaskSession): void {
  if (!session.taskId.trim()) throw new Error("Agent task id is required");
  if (!SESSION_ID.test(session.sessionId)) throw new Error("Claude session id must be a UUID");
  const controlDirectory = posix.normalize(session.controlDirectory);
  if (controlDirectory !== session.controlDirectory || !controlDirectory.startsWith("/identity/control/tasks/")) {
    throw new Error("Agent control directory must be a normalized task directory inside the identity capsule");
  }
  if (!PROJECT_DIRECTORY.test(session.projectDirectoryName)) {
    throw new Error("Claude project directory name must be a portable path segment");
  }
  if (!session.mcp.configPath.startsWith("/")) throw new Error("Claude MCP config path must be absolute inside the capsule");
  if (session.mcp.allowedTools.length === 0) throw new Error("Claude requires at least one Altitude MCP tool");
  if (session.mcp.allowedTools.some((tool) => !tool.startsWith("mcp__altitude__"))) {
    throw new Error("Claude may only receive fully-qualified Altitude MCP tools");
  }
}

function claudeEnvironment(identity: ClaudeIdentity, session?: AgentTaskSession): Readonly<Record<string, string>> {
  return {
    CLAUDE_CONFIG_DIR: identity.configDirectory,
    // The pinned Claude CLI uses this project key for transcript storage. The
    // checkpoint exporter later verifies the expected session file exists.
    ...(session === undefined ? {} : { CLAUDE_CODE_PROJECT_DIR_NAME: session.projectDirectoryName }),
  };
}

function strictMcpArgs(session: AgentTaskSession): readonly string[] {
  return [
    "--mcp-config",
    session.mcp.configPath,
    "--strict-mcp-config",
    "--tools",
    SAFE_IDENTITY_TOOLS.join(","),
    "--allowedTools",
    [...session.mcp.allowedTools, ...PREAPPROVED_IDENTITY_TOOLS].join(","),
    "--permission-mode",
    "dontAsk",
    "--disable-slash-commands",
    "--no-chrome",
  ];
}

function versionFromOutput(stdout: string): string | undefined {
  const match = /(?:^|\s)(\d+\.\d+\.\d+(?:[-+][A-Za-z0-9.-]+)?)(?:\s|$)/.exec(stdout.trim());
  return match?.[1];
}

function authFromOutput(exitCode: number, stdout: string): AgentProbe["auth"] {
  if (exitCode !== 0) return "unauthenticated";
  try {
    const value = JSON.parse(stdout) as Record<string, unknown>;
    if (value.loggedIn === true || value.authenticated === true || value.status === "authenticated") {
      return "authenticated";
    }
    if (value.loggedIn === false || value.authenticated === false || value.status === "unauthenticated") {
      return "unauthenticated";
    }
  } catch {
    // A successful but unknown response is deliberately not treated as auth.
  }
  // Claude Code documents exit 0 as logged in; fields may evolve without
  // weakening that CLI contract.
  return "authenticated";
}

async function boundedText(stream: AsyncIterable<Uint8Array>): Promise<string> {
  const decoder = new TextDecoder();
  let result = "";
  let bytes = 0;
  for await (const chunk of stream) {
    if (bytes >= MAX_CAPTURE_BYTES) continue;
    const remaining = MAX_CAPTURE_BYTES - bytes;
    const slice = chunk.byteLength <= remaining ? chunk : chunk.slice(0, remaining);
    bytes += slice.byteLength;
    result += decoder.decode(slice, { stream: true });
  }
  return `${result}${decoder.decode()}`;
}

async function drain(stream: AsyncIterable<Uint8Array>): Promise<void> {
  for await (const _chunk of stream) {
    // Draining stderr prevents the child from blocking without persisting it.
  }
}

async function acquireLease(
  store: AgentWriterLeaseStore,
  taskId: string,
  sessionId: string,
  mode: AgentWriterMode,
  holderId: string,
  acquiredAtMs: number,
): Promise<AgentWriterLease> {
  const lease = await store.acquire({ taskId, sessionId, mode, holderId, acquiredAtMs });
  if (lease !== undefined) return lease;
  const current = await store.current(taskId);
  if (current === undefined) throw new Error(`Task ${taskId} writer lease changed while acquiring it`);
  throw new AgentWriterLeaseConflictError(current);
}

export class ClaudeCliAdapter implements AgentAdapter {
  readonly capabilities: AgentCapabilities = {
    protocol: EXPERIMENTAL_AGENT_PROTOCOL,
    adapter: "claude-code-cli",
    fixedSessions: true,
    resumableSessions: true,
    cooperativeYield: true,
    nativeInteractiveTake: true,
    nodeLocalIdentity: true,
    portableTaskTranscripts: true,
  };

  readonly #identity: ClaudeIdentity;
  readonly #launcher: ProcessLauncher;
  readonly #leases: AgentWriterLeaseStore;
  readonly #executable: string;
  readonly #now: () => number;
  readonly #createId: () => string;

  constructor(options: ClaudeCliAdapterOptions) {
    if (!/^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/u.test(options.identity.id)) {
      throw new Error("Claude identity id is invalid");
    }
    if (!insideIdentity(options.identity.configDirectory)) {
      throw new Error("Claude config must remain inside the identity capsule");
    }
    if (!insideIdentity(options.identity.loginDirectory)) {
      throw new Error("Claude login directory must remain inside the identity capsule");
    }
    this.#identity = options.identity;
    this.#launcher = options.launcher;
    this.#leases = options.leases;
    this.#executable = options.executable ?? "claude";
    this.#now = options.now ?? Date.now;
    this.#createId = options.createId ?? (() => crypto.randomUUID());
  }

  async probe(): Promise<AgentProbe> {
    let version: CapturedProcess;
    try {
      version = await this.#capture(["--version"]);
    } catch {
      return { installed: false, auth: "unknown" };
    }
    if (version.exit.exitCode !== 0) return { installed: false, auth: "unknown" };

    const parsedVersion = versionFromOutput(version.stdout);
    let auth: AgentProbe["auth"] = "unknown";
    try {
      // Current Claude Code emits JSON by default and uses its exit status as
      // the authenticated/unauthenticated contract.
      const status = await this.#capture(["auth", "status"]);
      auth = authFromOutput(status.exit.exitCode, status.stdout);
    } catch {
      // Claude is installed, but a broken identity capsule must not be called logged out.
    }
    return {
      installed: true,
      ...(parsedVersion === undefined ? {} : { version: parsedVersion }),
      auth,
    };
  }

  async start(request: AgentRunRequest): Promise<AgentRun> {
    validateSession(request);
    if (!request.prompt.trim()) throw new Error("Claude run prompt is required");
    const holderId = `claude-headless:${this.#createId()}`;
    const lease = await acquireLease(
      this.#leases,
      request.taskId,
      request.sessionId,
      "headless",
      holderId,
      this.#now(),
    );
    const launch = this.#headlessLaunch(request);
    let process: RunningProcess;
    try {
      process = await this.#launcher.launch(launch);
    } catch (error) {
      await this.#leases.release(lease);
      throw error;
    }
    try {
      await process.writeStdin(encodeClaudeUserInput(request.prompt));
    } catch (error) {
      await process.terminate("SIGTERM").catch(() => undefined);
      await this.#leases.release(lease);
      throw error;
    }

    const events = eventQueue<AgentEvent>();
    const state = { yieldRequested: false, stopRequested: false, terminal: false };
    const completion = this.#monitor(process, request, lease, events, state);

    return {
      taskId: request.taskId,
      sessionId: request.sessionId,
      launch,
      events,
      completion,
      requestYield: async (reason?: string): Promise<boolean> => {
        if (state.terminal || state.stopRequested) return false;
        if (state.yieldRequested) return true;
        state.yieldRequested = true;
        const detail = reason?.trim() ? ` Reason: ${reason.trim()}` : "";
        try {
          await process.writeStdin(encodeClaudeUserInput(
            `Altitude is requesting a cooperative yield.${detail} Reach the next safe boundary, preserve durable work, do not begin another unit of work, record the checkpoint with the configured Altitude lifecycle tool, and finish this turn.`,
          ));
          return true;
        } catch {
          state.yieldRequested = false;
          return false;
        }
      },
      stop: async (): Promise<void> => {
        if (state.terminal || state.stopRequested) return;
        state.stopRequested = true;
        await process.terminate("SIGTERM");
      },
    };
  }

  async prepareInteractiveTake(request: AgentTakeRequest): Promise<AgentInteractiveSession> {
    return await this.#prepareInteractive(request, true);
  }

  async prepareInteractiveStart(request: AgentInteractiveStartRequest): Promise<AgentInteractiveSession> {
    if (!request.prompt.trim()) throw new Error("Claude interactive prompt is required");
    return await this.#prepareInteractive(request, false, request.prompt);
  }

  async #prepareInteractive(
    request: AgentTakeRequest,
    resume: boolean,
    initialPrompt?: string,
  ): Promise<AgentInteractiveSession> {
    validateSession(request);
    const lease = await acquireLease(
      this.#leases,
      request.taskId,
      request.sessionId,
      "interactive",
      `claude-interactive:${this.#createId()}`,
      this.#now(),
    );
    let released = false;
    const launch: ProcessLaunchSpec & { readonly io: "interactive" } = {
      target: { kind: "identity-capsule", identityId: this.#identity.id, taskId: request.taskId },
      executable: this.#executable,
      args: [
        resume ? "--resume" : "--session-id",
        request.sessionId,
        ...strictMcpArgs(request),
        ...(request.displayName === undefined ? [] : ["--name", request.displayName]),
        ...(initialPrompt === undefined ? [] : [initialPrompt]),
      ],
      cwd: request.controlDirectory,
      env: claudeEnvironment(this.#identity, request),
      io: "interactive",
      purpose: resume
        ? "Take this task in the native Claude Code TUI"
        : "Start this task in the native Claude Code TUI",
    };
    return {
      taskId: request.taskId,
      sessionId: request.sessionId,
      launch,
      release: async () => {
        if (released) return;
        released = true;
        await this.#leases.release(lease);
      },
    };
  }

  transcriptExportPlan(session: AgentTaskSession): AgentTranscriptTransferPlan {
    validateSession(session);
    const taskPath = this.#taskTranscriptPath(session);
    return {
      direction: "export",
      taskId: session.taskId,
      from: { scope: "identity", path: taskPath },
      to: { scope: "task-capsule", path: join("session", "claude", "files") },
      identityMaterial: "excluded",
    };
  }

  transcriptImportPlan(session: AgentTaskSession): AgentTranscriptTransferPlan {
    validateSession(session);
    const taskPath = this.#taskTranscriptPath(session);
    return {
      direction: "import",
      taskId: session.taskId,
      from: { scope: "task-capsule", path: join("session", "claude", "files") },
      to: { scope: "identity", path: taskPath },
      identityMaterial: "excluded",
    };
  }

  #headlessLaunch(request: AgentRunRequest): ProcessLaunchSpec & { readonly io: "piped" } {
    const sessionArgs = request.resume
      ? ["--resume", request.sessionId]
      : ["--session-id", request.sessionId];
    return {
      target: { kind: "identity-capsule", identityId: this.#identity.id, taskId: request.taskId },
      executable: this.#executable,
      args: [
        "-p",
        ...sessionArgs,
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        ...strictMcpArgs(request),
        ...(request.model === undefined ? [] : ["--model", request.model]),
        ...(request.displayName === undefined ? [] : ["--name", request.displayName]),
      ],
      cwd: request.controlDirectory,
      env: claudeEnvironment(this.#identity, request),
      io: "piped",
      purpose: request.resume ? "Resume a headless Claude task" : "Start a fixed headless Claude task",
    };
  }

  async #capture(args: readonly string[]): Promise<CapturedProcess> {
    const process = await this.#launcher.launch({
      target: { kind: "identity-capsule", identityId: this.#identity.id },
      executable: this.#executable,
      args,
      cwd: this.#identity.loginDirectory,
      env: claudeEnvironment(this.#identity),
      io: "piped",
      purpose: "Probe Claude Code in the node-local identity",
    });
    await process.closeStdin();
    const [exit, stdout] = await Promise.all([process.exited, boundedText(process.stdout), drain(process.stderr)]);
    return { exit, stdout };
  }

  async #monitor(
    process: RunningProcess,
    request: AgentRunRequest,
    lease: AgentWriterLease,
    events: EventQueue<AgentEvent>,
    state: { yieldRequested: boolean; stopRequested: boolean; terminal: boolean },
  ): Promise<AgentRunCompletion> {
    let result: Extract<AgentEvent, { readonly type: "agent.result" }> | undefined;
    let rejectedRateLimit: Extract<AgentEvent, { readonly type: "agent.rate_limit" }> | undefined;
    let protocolError: string | undefined;

    const readStdout = async () => {
      const decoder = new TextDecoder();
      let pending = "";
      const acceptLine = async (line: string) => {
        if (!line.trim()) return;
        let event: AgentEvent;
        try {
          event = parseClaudeStreamLine(line, this.#now());
        } catch (error) {
          protocolError = error instanceof Error ? error.message : String(error);
          events.push({ type: "agent.protocol_error", observedAtMs: this.#now(), message: protocolError });
          await process.terminate("SIGTERM");
          return;
        }
        if ((event.type === "agent.init" || event.type === "agent.result")
          && event.sessionId !== undefined
          && event.sessionId !== request.sessionId) {
          protocolError = `Claude returned session ${event.sessionId} while ${request.sessionId} owns the writer lease`;
          events.push({ type: "agent.protocol_error", observedAtMs: this.#now(), message: protocolError });
          await process.terminate("SIGTERM");
          return;
        }
        events.push(event);
        if (event.type === "agent.result") {
          result = event;
          await process.closeStdin();
        } else if (event.type === "agent.rate_limit" && event.status === "rejected") {
          rejectedRateLimit = event;
        }
      };

      for await (const chunk of process.stdout) {
        pending += decoder.decode(chunk, { stream: true });
        let newline = pending.indexOf("\n");
        while (newline >= 0) {
          const line = pending.slice(0, newline).replace(/\r$/, "");
          pending = pending.slice(newline + 1);
          await acceptLine(line);
          if (protocolError !== undefined) return;
          newline = pending.indexOf("\n");
        }
      }
      pending += decoder.decode();
      if (pending.trim() && protocolError === undefined) await acceptLine(pending);
    };

    try {
      const [exit] = await Promise.all([process.exited, readStdout(), drain(process.stderr)]);
      state.terminal = true;
      if (protocolError !== undefined) {
        return { outcome: "protocol_error", exit, detail: protocolError };
      }
      if (state.stopRequested) return { outcome: "stopped", exit, ...(result === undefined ? {} : { result }) };
      if (rejectedRateLimit !== undefined) {
        return {
          outcome: "rate_limited",
          exit,
          rateLimit: rejectedRateLimit,
          ...(result === undefined ? {} : { result }),
        };
      }
      if (result === undefined) {
        return { outcome: "error", exit, detail: "Claude exited without a result event" };
      }
      if (!result.success || exit.exitCode !== 0) return { outcome: "error", exit, result };
      return { outcome: state.yieldRequested ? "yielded" : "success", exit, result };
    } finally {
      state.terminal = true;
      events.close();
      await this.#leases.release(lease);
    }
  }

  #taskTranscriptPath(session: AgentTaskSession): string {
    // By construction this selects one Claude project transcript directory,
    // never the identity root where .credentials.json and settings live.
    return join(this.#identity.configDirectory, "projects", session.projectDirectoryName);
  }
}
