import { createHash, randomUUID } from "node:crypto";
import {
  appendFileSync,
  chmodSync,
  existsSync,
  mkdirSync,
  readFileSync,
  renameSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { chmod, lstat, mkdir, readFile, rename, rm, writeFile } from "node:fs/promises";
import { createConnection, createServer, type Socket } from "node:net";
import { basename, dirname, join, resolve } from "node:path";
import type { ReadStream, WriteStream } from "node:tty";

import type { CommandSpec } from "../process/command.ts";

export const TERMINAL_SESSION_PROTOCOL = "altitude.terminal-session/v1" as const;
export type TerminalSessionKind = "workbench" | "agent";
const MAX_REPLAY_BYTES = 1024 * 1024;
const MAX_LOG_BYTES = 4 * 1024 * 1024;
const MAX_HANDSHAKE_BYTES = 4096;
const STARTING_STALE_MS = 60_000;

export type TerminalSessionStatus =
  | Readonly<{ schema: typeof TERMINAL_SESSION_PROTOCOL; state: "starting"; updatedAt: string }>
  | Readonly<{ schema: typeof TERMINAL_SESSION_PROTOCOL; state: "running"; supervisorPid: number; childPid: number; updatedAt: string }>
  | Readonly<{ schema: typeof TERMINAL_SESSION_PROTOCOL; state: "exited"; exitCode: number; signal: string | null; updatedAt: string }>
  | Readonly<{
      schema: typeof TERMINAL_SESSION_PROTOCOL;
      state: "failed";
      error: string;
      childPid?: number;
      childAlive?: boolean;
      updatedAt: string;
    }>;

type TerminalLaunch = Readonly<{
  schema: typeof TERMINAL_SESSION_PROTOCOL;
  sessionId: string;
  taskId: string;
  kind: TerminalSessionKind;
  command: CommandSpec;
}>;

export type TerminalAttachResult = Readonly<{
  outcome: "detached" | "exited";
  status: TerminalSessionStatus;
}>;

function validId(value: string): boolean {
  return /^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/u.test(value);
}

function sessionId(taskId: string, kind: TerminalSessionKind): string {
  if (!validId(taskId)) throw new Error("Terminal task id is invalid");
  const digest = createHash("sha256").update(`${kind}\0${taskId}`, "utf8").digest("hex").slice(0, 24);
  return `${kind}-${digest}`;
}

function shortSocketPath(root: string, id: string): string {
  const owner = typeof process.getuid === "function" ? process.getuid() : process.pid;
  // Darwin's Unix-domain socket path is only 104 bytes; /tmp keeps this bounded.
  const directory = join("/tmp", `altitude-terminals-${owner}`);
  const digest = createHash("sha256").update(`${resolve(root)}\0${id}`, "utf8").digest("hex").slice(0, 32);
  return join(directory, `${digest}.sock`);
}

function validateCommand(command: CommandSpec): CommandSpec {
  if (!command.executable.trim() || command.executable.includes("\0")) throw new Error("Terminal executable is invalid");
  if (command.args?.some((argument) => argument.includes("\0"))) throw new Error("Terminal argument is invalid");
  if (command.stdin !== undefined) throw new Error("Terminal sessions own their stdin");
  for (const [key, value] of Object.entries(command.env ?? {})) {
    if (!/^[A-Z_][A-Z0-9_]*$/u.test(key) || value?.includes("\0")) throw new Error(`Terminal environment ${key} is invalid`);
  }
  return {
    executable: command.executable,
    ...(command.args === undefined ? {} : { args: [...command.args] }),
    ...(command.cwd === undefined ? {} : { cwd: command.cwd }),
    ...(command.env === undefined ? {} : { env: { ...command.env } }),
  };
}

function parseLaunch(value: unknown): TerminalLaunch {
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw new Error("Terminal launch record is invalid");
  const launch = value as Partial<TerminalLaunch>;
  if (launch.schema !== TERMINAL_SESSION_PROTOCOL || typeof launch.sessionId !== "string"
    || typeof launch.taskId !== "string" || (launch.kind !== "workbench" && launch.kind !== "agent") || !validId(launch.taskId)
    || typeof launch.command !== "object" || launch.command === null) {
    throw new Error("Terminal launch record schema is invalid");
  }
  if (launch.sessionId !== sessionId(launch.taskId, launch.kind)) throw new Error("Terminal launch record binding is invalid");
  return { ...launch, command: validateCommand(launch.command) } as TerminalLaunch;
}

function parseStatus(value: unknown): TerminalSessionStatus {
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw new Error("Terminal status is invalid");
  const status = value as Record<string, unknown>;
  if (status.schema !== TERMINAL_SESSION_PROTOCOL || typeof status.updatedAt !== "string") throw new Error("Terminal status schema is invalid");
  if (status.state === "starting") return status as TerminalSessionStatus;
  if (status.state === "running" && Number.isSafeInteger(status.supervisorPid) && Number.isSafeInteger(status.childPid)) {
    return status as TerminalSessionStatus;
  }
  if (status.state === "exited" && Number.isSafeInteger(status.exitCode)
    && (typeof status.signal === "string" || status.signal === null)) return status as TerminalSessionStatus;
  if (status.state === "failed" && typeof status.error === "string"
    && ((status.childPid === undefined && status.childAlive === undefined)
      || (Number.isSafeInteger(status.childPid) && typeof status.childAlive === "boolean"))) {
    return status as TerminalSessionStatus;
  }
  throw new Error("Terminal status state is invalid");
}

function processAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

function atomicStatus(directory: string, status: TerminalSessionStatus): void {
  const temporary = join(directory, `.status-${randomUUID()}.tmp`);
  writeFileSync(temporary, `${JSON.stringify(status)}\n`, { mode: 0o600 });
  renameSync(temporary, join(directory, "status.json"));
}

function nowStatus<T extends Omit<TerminalSessionStatus, "schema" | "updatedAt">>(status: T): TerminalSessionStatus {
  return { schema: TERMINAL_SESSION_PROTOCOL, ...status, updatedAt: new Date().toISOString() } as TerminalSessionStatus;
}

function selfInvocation(): string[] {
  const executable = process.execPath;
  const script = process.argv[1];
  return basename(executable).startsWith("bun") && script !== undefined
    ? [executable, script]
    : [executable];
}

async function regularDirectory(path: string, label: string): Promise<void> {
  const stat = await lstat(path);
  if (stat.isSymbolicLink() || !stat.isDirectory()) throw new Error(`${label} is unsafe`);
}

async function ensurePrivateDirectory(path: string): Promise<void> {
  await mkdir(path, { recursive: true, mode: 0o700 });
  await regularDirectory(path, "Terminal session directory");
  await chmod(path, 0o700);
}

export class TerminalSessionManager {
  constructor(
    readonly root: string,
    private readonly supervisorCommand: readonly string[] = selfInvocation(),
  ) {
    if (supervisorCommand.length === 0) throw new Error("Terminal supervisor command is required");
  }

  directory(taskId: string, kind: TerminalSessionKind = "workbench"): string {
    return join(resolve(this.root), sessionId(taskId, kind));
  }

  socketPath(taskId: string, kind: TerminalSessionKind = "workbench"): string {
    return shortSocketPath(this.root, sessionId(taskId, kind));
  }

  async status(taskId: string, kind: TerminalSessionKind = "workbench"): Promise<TerminalSessionStatus | undefined> {
    try {
      const bytes = await readFile(join(this.directory(taskId, kind), "status.json"), "utf8");
      let status = parseStatus(JSON.parse(bytes) as unknown);
      if (status.state === "starting") {
        let supervisorPid: number | undefined;
        try {
          const value = Number.parseInt(await readFile(join(this.directory(taskId, kind), "supervisor.pid"), "utf8"), 10);
          if (Number.isSafeInteger(value) && value > 0) supervisorPid = value;
        } catch (error) {
          if (typeof error !== "object" || error === null || !("code" in error) || error.code !== "ENOENT") throw error;
        }
        if ((supervisorPid !== undefined && !processAlive(supervisorPid))
          || (supervisorPid === undefined && Date.now() - Date.parse(status.updatedAt) > STARTING_STALE_MS)) {
          status = nowStatus({ state: "failed", error: "Terminal supervisor did not start" });
          atomicStatus(this.directory(taskId, kind), status);
        }
      } else if (status.state === "running") {
        if (!processAlive(status.supervisorPid) || !existsSync(this.socketPath(taskId, kind))) {
          const childAlive = processAlive(status.childPid);
          const failed = nowStatus({
            state: "failed",
            error: childAlive
              ? "Terminal supervisor was lost while its child still appears alive"
              : "Terminal supervisor and child are no longer alive",
            childPid: status.childPid,
            childAlive,
          });
          atomicStatus(this.directory(taskId, kind), failed);
          return failed;
        }
      } else if (status.state === "failed" && status.childPid !== undefined && status.childAlive === true
        && !processAlive(status.childPid)) {
        status = nowStatus({
          state: "failed",
          error: "Terminal supervisor and child are no longer alive",
          childPid: status.childPid,
          childAlive: false,
        });
        atomicStatus(this.directory(taskId, kind), status);
      }
      return status;
    } catch (error) {
      if (typeof error === "object" && error !== null && "code" in error && error.code === "ENOENT") return undefined;
      throw error;
    }
  }

  async start(input: { readonly taskId: string; readonly kind?: TerminalSessionKind; readonly command: CommandSpec }): Promise<TerminalSessionStatus> {
    const kind = input.kind ?? "workbench";
    const directory = this.directory(input.taskId, kind);
    await ensurePrivateDirectory(directory);
    const existing = await this.status(input.taskId, kind);
    if (existing?.state === "running" || existing?.state === "starting") return existing;
    if (existing !== undefined) throw new Error(`Terminal session ${input.taskId} must be finalized before it can restart`);
    const launch: TerminalLaunch = {
      schema: TERMINAL_SESSION_PROTOCOL,
      sessionId: sessionId(input.taskId, kind),
      taskId: input.taskId,
      kind,
      command: validateCommand(input.command),
    };
    const launchPath = join(directory, "launch.json");
    const temporary = join(directory, `.launch-${randomUUID()}.tmp`);
    await writeFile(temporary, `${JSON.stringify(launch)}\n`, { mode: 0o600, flag: "wx" });
    await rename(temporary, launchPath);
    atomicStatus(directory, nowStatus({ state: "starting" }));
    let child: ReturnType<typeof Bun.spawn>;
    try {
      child = Bun.spawn([...this.supervisorCommand, "_terminal-supervisor", launch.sessionId], {
        env: { ...process.env, ALTITUDE_TERMINAL_ROOT: resolve(this.root) },
        detached: true,
        stdin: "ignore",
        stdout: "ignore",
        stderr: "ignore",
      });
    } catch (error) {
      const failed = nowStatus({
        state: "failed",
        error: `Terminal supervisor could not launch: ${error instanceof Error ? error.message : String(error)}`,
      });
      atomicStatus(directory, failed);
      return failed;
    }
    await writeFile(join(directory, "supervisor.pid"), `${child.pid}\n`, { mode: 0o600 });
    child.unref();
    for (let attempt = 0; attempt < 100; attempt += 1) {
      const status = await this.status(input.taskId, kind);
      if (status?.state !== "starting") return status!;
      await Bun.sleep(25);
    }
    return await this.status(input.taskId, kind) ?? nowStatus({ state: "starting" });
  }

  async finalize(taskId: string, kind: TerminalSessionKind = "workbench"): Promise<void> {
    const status = await this.status(taskId, kind);
    if (status?.state === "running" || status?.state === "starting") throw new Error("A running terminal session cannot be finalized");
    if (status?.state === "failed" && status.childAlive === true) {
      throw new Error("A terminal session with a live child cannot be finalized");
    }
    await rm(this.socketPath(taskId, kind), { force: true });
    await rm(this.directory(taskId, kind), { recursive: true, force: true });
  }

  async attach(
    taskId: string,
    input: ReadStream = process.stdin,
    output: WriteStream = process.stdout,
    kind: TerminalSessionKind = "workbench",
    allowDetach = true,
  ): Promise<TerminalAttachResult> {
    if (input.isTTY !== true || output.isTTY !== true) throw new Error("Attaching a terminal session requires an interactive terminal");
    const directory = this.directory(taskId, kind);
    const status = await this.status(taskId, kind);
    if (status === undefined) throw new Error(`Task ${taskId} has no terminal session`);
    if (status.state !== "running") return { outcome: "exited", status };
    const socketPath = this.socketPath(taskId, kind);
    const socket = createConnection(socketPath);
    const wasRaw = input.isRaw;
    let detached = false;
    let prefix = false;
    const dimensions = () => ({ cols: output.columns ?? 80, rows: output.rows ?? 24 });
    const resize = () => {
      const connection = createConnection(socketPath, () => {
        connection.end(`${JSON.stringify({ mode: "resize", ...dimensions() })}\n`);
      });
      connection.on("error", () => undefined);
    };
    const onInput = (chunk: Buffer | string) => {
      const bytes = Buffer.from(chunk);
      const forwarded: number[] = [];
      for (const byte of bytes) {
        if (prefix) {
          prefix = false;
          if (byte === 0x64 && allowDetach) {
            detached = true;
            socket.end();
            break;
          }
          forwarded.push(0x07, byte);
        } else if (byte === 0x07) {
          prefix = true;
        } else {
          forwarded.push(byte);
        }
      }
      if (forwarded.length > 0) socket.write(Buffer.from(forwarded));
    };
    try {
      await new Promise<void>((resolveConnection, reject) => {
        socket.once("connect", () => {
          socket.write(`${JSON.stringify({ mode: "attach", ...dimensions() })}\n`);
          resolveConnection();
        });
        socket.once("error", reject);
      });
      input.setRawMode(true);
      input.resume();
      input.on("data", onInput);
      output.on("resize", resize);
      socket.on("data", (chunk) => output.write(chunk));
      await new Promise<void>((resolveSocket, reject) => {
        socket.once("close", resolveSocket);
        socket.once("error", reject);
      });
    } finally {
      input.off("data", onInput);
      output.off("resize", resize);
      input.setRawMode(wasRaw);
      if (!wasRaw) input.pause();
      socket.destroy();
    }
    const next = await this.status(taskId, kind) ?? status;
    if (!detached && next.state === "running") {
      throw new Error("Terminal session is already attached or its connection closed unexpectedly");
    }
    return { outcome: detached ? "detached" : "exited", status: next };
  }

  async terminate(taskId: string, kind: TerminalSessionKind): Promise<TerminalSessionStatus | undefined> {
    const status = await this.status(taskId, kind);
    if (status?.state !== "running") return status;
    const socket = createConnection(this.socketPath(taskId, kind));
    await new Promise<void>((resolveConnection, reject) => {
      socket.once("connect", () => {
        socket.end(`${JSON.stringify({ mode: "terminate", cols: 80, rows: 24 })}\n`);
        resolveConnection();
      });
      socket.once("error", reject);
    });
    for (let attempt = 0; attempt < 200; attempt += 1) {
      const next = await this.status(taskId, kind);
      if (next?.state !== "running") return next;
      await Bun.sleep(25);
    }
    throw new Error(`Terminal session ${taskId} did not stop`);
  }
}

function replayLog(directory: string): Buffer {
  const path = join(directory, "terminal.log");
  if (!existsSync(path)) return Buffer.alloc(0);
  const bytes = readFileSync(path);
  return bytes.byteLength <= MAX_REPLAY_BYTES ? bytes : bytes.subarray(bytes.byteLength - MAX_REPLAY_BYTES);
}

function dimensions(value: unknown): { cols: number; rows: number } {
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw new Error("Terminal handshake is invalid");
  const input = value as Record<string, unknown>;
  if (!Number.isSafeInteger(input.cols) || !Number.isSafeInteger(input.rows)
    || (input.cols as number) < 20 || (input.cols as number) > 1000
    || (input.rows as number) < 5 || (input.rows as number) > 1000) throw new Error("Terminal dimensions are invalid");
  return { cols: input.cols as number, rows: input.rows as number };
}

export async function runTerminalSupervisor(requestedSessionId: string | undefined): Promise<number> {
  const root = process.env.ALTITUDE_TERMINAL_ROOT;
  if (root === undefined || requestedSessionId === undefined || !/^(?:workbench|agent)-[a-f0-9]{24}$/u.test(requestedSessionId)) return 2;
  const directory = join(resolve(root), requestedSessionId);
  try {
    mkdirSync(directory, { recursive: true, mode: 0o700 });
    chmodSync(directory, 0o700);
    const launch = parseLaunch(JSON.parse(readFileSync(join(directory, "launch.json"), "utf8")) as unknown);
    if (launch.sessionId !== requestedSessionId) throw new Error("Terminal supervisor session binding is invalid");
    const socketPath = shortSocketPath(root, requestedSessionId);
    mkdirSync(dirname(socketPath), { recursive: true, mode: 0o700 });
    chmodSync(dirname(socketPath), 0o700);
    rmSync(socketPath, { force: true });
    let attached: Socket | undefined;
    const logPath = join(directory, "terminal.log");
    let logBytes = existsSync(logPath) ? statSync(logPath).size : 0;
    const terminal = new Bun.Terminal({
      cols: 80,
      rows: 24,
      data(_terminal, data) {
        if (logBytes + data.byteLength > MAX_LOG_BYTES) {
          const replay = replayLog(directory);
          writeFileSync(logPath, replay, { mode: 0o600 });
          logBytes = replay.byteLength;
        }
        appendFileSync(logPath, data, { mode: 0o600 });
        logBytes += data.byteLength;
        attached?.write(data);
      },
    });
    let child: ReturnType<typeof Bun.spawn> | undefined;
    const server = createServer((socket) => {
      let handshake = Buffer.alloc(0);
      const receiveHandshake = (chunk: Buffer) => {
        handshake = Buffer.concat([handshake, chunk]);
        if (handshake.byteLength > MAX_HANDSHAKE_BYTES) return socket.destroy();
        const newline = handshake.indexOf(0x0a);
        if (newline < 0) return;
        socket.off("data", receiveHandshake);
        let value: Record<string, unknown>;
        try {
          value = JSON.parse(handshake.subarray(0, newline).toString("utf8")) as Record<string, unknown>;
          const size = dimensions(value);
          terminal.resize(size.cols, size.rows);
        } catch {
          return socket.destroy();
        }
        if (value.mode === "resize") return socket.end();
        if (value.mode === "terminate") {
          child?.kill("SIGTERM");
          return socket.end();
        }
        if (value.mode !== "attach" || attached !== undefined) return socket.destroy();
        attached = socket;
        socket.write(replayLog(directory));
        const remainder = handshake.subarray(newline + 1);
        if (remainder.byteLength > 0) terminal.write(remainder);
        socket.on("data", (data) => terminal.write(data));
        socket.on("close", () => { if (attached === socket) attached = undefined; });
      };
      socket.on("data", receiveHandshake);
    });
    await new Promise<void>((resolveListen, reject) => {
      server.once("error", reject);
      server.listen(socketPath, () => resolveListen());
    });
    chmodSync(socketPath, 0o600);
    child = Bun.spawn([launch.command.executable, ...(launch.command.args ?? [])], {
      ...(launch.command.cwd === undefined ? {} : { cwd: launch.command.cwd }),
      env: { ...process.env, ...launch.command.env },
      terminal,
    });
    atomicStatus(directory, nowStatus({ state: "running", supervisorPid: process.pid, childPid: child.pid }));
    const exitCode = await child.exited;
    atomicStatus(directory, nowStatus({ state: "exited", exitCode, signal: child.signalCode }));
    attached?.end();
    await new Promise<void>((resolveClose) => server.close(() => resolveClose()));
    terminal.close();
    rmSync(socketPath, { force: true });
    return 0;
  } catch (error) {
    atomicStatus(directory, nowStatus({ state: "failed", error: error instanceof Error ? error.message : String(error) }));
    return 1;
  }
}
