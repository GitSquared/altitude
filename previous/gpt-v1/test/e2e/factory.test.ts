import { afterEach, describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { defaultLocalConfig, writeLocalConfig } from "../../src/config/local.ts";
import { TaskStore } from "../../src/storage/task-store.ts";
import { TerminalSessionManager } from "../../src/terminal/session.ts";

const roots: string[] = [];
const children: Array<ReturnType<typeof Bun.spawn>> = [];

afterEach(async () => {
  for (const child of children.splice(0)) {
    if (child.exitCode === null) child.kill("SIGTERM");
    await child.exited.catch(() => undefined);
    child.terminal?.close();
  }
  for (const root of roots.splice(0)) {
    const terminals = new TerminalSessionManager(join(root, "terminals"));
    for (const taskId of ["amber-anchor", "birch-brook"]) {
      if ((await terminals.status(taskId, "agent"))?.state === "running") {
        await terminals.terminate(taskId, "agent");
      }
      await terminals.finalize(taskId, "agent");
    }
    rmSync(root, { recursive: true, force: true });
  }
});

function temporaryRoot(): string {
  const root = mkdtempSync(join(tmpdir(), "altitude-e2e-"));
  roots.push(root);
  return root;
}

function spawnPty(command: readonly string[], env: Record<string, string | undefined> = {}): {
  readonly process: ReturnType<typeof Bun.spawn>;
  readonly output: () => string;
} {
  let output = "";
  const decoder = new TextDecoder();
  const child = Bun.spawn([...command], {
    cwd: join(import.meta.dir, "..", ".."),
    env: { ...process.env, TERM: "xterm-256color", ...env },
    terminal: {
      cols: 100,
      rows: 30,
      data(_terminal, data) {
        output += decoder.decode(data);
      },
    },
  });
  children.push(child);
  return { process: child, output: () => output };
}

async function waitFor(read: () => string, expected: string, timeoutMs = 8_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!read().includes(expected) && Date.now() < deadline) await Bun.sleep(20);
  expect(read()).toContain(expected);
}

async function waitForOutputAfter(
  read: () => string,
  offset: number,
  expected: string,
  timeoutMs = 8_000,
): Promise<void> {
  await waitFor(() => read().slice(offset), expected, timeoutMs);
}

async function waitForFile(path: string, expected: string): Promise<void> {
  await waitFor(() => existsSync(path) ? readFileSync(path, "utf8") : "", expected);
}

async function waitForFileOccurrences(path: string, expected: string, count: number): Promise<void> {
  const occurrences = () => existsSync(path)
    ? readFileSync(path, "utf8").split(expected).length - 1
    : 0;
  const deadline = Date.now() + 8_000;
  while (occurrences() < count && Date.now() < deadline) await Bun.sleep(20);
  expect(occurrences()).toBeGreaterThanOrEqual(count);
}

describe("Altitude operator journeys", () => {
  test("drives the compiled factory with real terminal keys", async () => {
    const root = temporaryRoot();
    const config = defaultLocalConfig();
    await writeLocalConfig(join(root, "config.json"), {
      ...config,
      node: { ...config.node, port: 45_000 + Math.floor(Math.random() * 10_000) },
    });
    using store = new TaskStore(join(root, "altitude.db"));
    store.append({
      taskId: "amber-anchor",
      type: "task.created",
      payload: { title: "Check the controls", intent: "Check the controls", intentLevel: "principles" },
      actor: { kind: "operator", id: "e2e" },
    });
    store.append({
      taskId: "amber-anchor",
      type: "task.state_changed",
      payload: { to: "queued" },
      actor: { kind: "operator", id: "e2e" },
    });
    store.close();

    const session = spawnPty([join(process.cwd(), "dist", "altitude")], { ALTITUDE_HOME: root });
    await waitFor(session.output, "factory overview");
    await waitFor(session.output, "amber-anchor");

    let outputOffset = session.output().length;
    session.process.terminal!.write("n");
    await waitForOutputAfter(session.output, outputOffset, "What would you like to delegate?");
    outputOffset = session.output().length;
    session.process.terminal!.write("\u001b");
    await waitForOutputAfter(session.output, outputOffset, "factory overview");
    session.process.terminal!.write("q");

    expect(await session.process.exited).toBe(0);
    expect(session.output()).toContain("\u001b[?1049h");
    expect(session.output()).toContain("\u001b[?1049l");
  }, 20_000);

  test("opens, detaches, and resumes the same task across a TUI restart", async () => {
    const root = temporaryRoot();
    const events = join(root, "events.log");
    const fixture = join(import.meta.dir, "tui-fixture.ts");
    const first = spawnPty(
      [process.execPath, fixture, "run", root],
      { ALTITUDE_E2E_KEEP_AGENT: "1" },
    );

    await waitFor(first.output, "Exercise the factory controls");
    let outputOffset = first.output().length;
    first.process.terminal!.write("\u001b[B");
    await waitForOutputAfter(first.output, outputOffset, "› birch-brook");
    first.process.terminal!.write("\r");
    await waitFor(first.output, "AGENT READY");
    await waitForFile(events, "open:birch-brook");

    outputOffset = first.output().length;
    first.process.terminal!.write(new Uint8Array([0x01]));
    await waitForFile(events, "detached:birch-brook");
    await waitForOutputAfter(first.output, outputOffset, "factory overview");
    first.process.terminal!.write("q");
    expect(await first.process.exited).toBe(0);

    const terminals = new TerminalSessionManager(join(root, "terminals"));
    expect(await terminals.status("birch-brook", "agent")).toMatchObject({ state: "running" });

    const session = spawnPty([process.execPath, fixture, "run", root]);
    await waitFor(session.output, "Exercise the factory controls");
    outputOffset = session.output().length;
    session.process.terminal!.write("\u001b[B");
    await waitForOutputAfter(session.output, outputOffset, "› birch-brook");
    session.process.terminal!.write("\r");
    await waitForFileOccurrences(events, "open:birch-brook", 2);
    await waitFor(session.output, "AGENT READY");
    outputOffset = session.output().length;
    session.process.terminal!.write(new Uint8Array([0x01]));
    await waitForFileOccurrences(events, "detached:birch-brook", 2);
    await waitForOutputAfter(session.output, outputOffset, "factory overview");

    outputOffset = session.output().length;
    session.process.terminal!.write("r");
    await waitForFile(events, "run:birch-brook");
    await waitForOutputAfter(session.output, outputOffset, "factory overview");
    expect(session.output().slice(outputOffset)).not.toContain("\u001b[?1049l");
    outputOffset = session.output().length;
    session.process.terminal!.write("n");
    await waitForOutputAfter(session.output, outputOffset, "What would you like to delegate?");
    outputOffset = session.output().length;
    session.process.terminal!.write("A second capsule");
    await waitForOutputAfter(session.output, outputOffset, "A second capsule");
    session.process.terminal!.write("\r");
    await waitForFile(events, "create:A second capsule");
    await waitForFile(events, "delegate:cedar-delta");
    session.process.terminal!.write("q");

    expect(await session.process.exited).toBe(0);
    expect(readFileSync(events, "utf8")).toContain("resume");
  }, 20_000);
});
