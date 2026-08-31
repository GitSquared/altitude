import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { createConnection } from "node:net";
import { EventEmitter } from "node:events";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { ReadStream, WriteStream } from "node:tty";

import { TerminalSessionManager } from "../src/terminal/session.ts";

const roots: string[] = [];

class FakeInput extends EventEmitter {
  readonly isTTY = true;
  isRaw = false;
  setRawMode(value: boolean): void { this.isRaw = value; }
  resume(): void {}
  pause(): void {}
}

class FakeOutput extends EventEmitter {
  readonly isTTY = true;
  readonly columns = 80;
  readonly rows = 24;
  value = "";
  write(chunk: Uint8Array | string): boolean {
    this.value += typeof chunk === "string" ? chunk : Buffer.from(chunk).toString("utf8");
    return true;
  }
}

afterEach(() => {
  for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true });
});

describe("detachable terminal supervisor", () => {
  test("keeps a PTY alive across client detach and records its exact exit", async () => {
    const root = mkdtempSync(join(tmpdir(), "altitude-terminal-"));
    roots.push(root);
    const manager = new TerminalSessionManager(root, [
      process.execPath,
      join(import.meta.dir, "..", "src", "cli.ts"),
    ]);
    const started = await manager.start({
      taskId: "task-1",
      command: {
        executable: "/bin/sh",
        args: ["-c", "printf 'READY\\n'; IFS= read -r line; printf 'GOT:%s\\n' \"$line\""],
        env: { TERM: "xterm-256color" },
      },
    });
    if (started.state === "failed") throw new Error(started.error);
    expect(started.state).toBe("running");

    const input = new FakeInput();
    const output = new FakeOutput();
    const attaching = manager.attach(
      "task-1",
      input as unknown as ReadStream,
      output as unknown as WriteStream,
    );
    for (let attempt = 0; attempt < 100 && !output.value.includes("READY"); attempt += 1) await Bun.sleep(10);
    input.emit("data", Buffer.from([0x07, 0x64]));
    expect(await attaching).toMatchObject({ outcome: "detached", status: { state: "running" } });
    expect(output.value).toContain("READY");
    expect(input.isRaw).toBeFalse();
    expect(await manager.status("task-1")).toMatchObject({ state: "running" });

    const socket = createConnection(manager.socketPath("task-1"));
    let resumedOutput = "";
    let answered = false;
    socket.on("data", (chunk) => {
      resumedOutput += chunk.toString("utf8");
      if (!answered && resumedOutput.includes("READY")) {
        answered = true;
        socket.write("hello\n");
      }
    });
    await new Promise<void>((resolve, reject) => {
      socket.once("connect", () => socket.write(`${JSON.stringify({ mode: "attach", cols: 80, rows: 24 })}\n`));
      socket.once("close", resolve);
      socket.once("error", reject);
    });
    expect(resumedOutput).toContain("READY");
    expect(resumedOutput).toContain("GOT:hello");
    expect(await manager.status("task-1")).toMatchObject({ state: "exited", exitCode: 0 });
    await manager.finalize("task-1");
    expect(await manager.status("task-1")).toBeUndefined();
  });

  test("isolates agent terminals and terminates them through the supervisor", async () => {
    const root = mkdtempSync(join(tmpdir(), "altitude-agent-terminal-"));
    roots.push(root);
    const manager = new TerminalSessionManager(root, [
      process.execPath,
      join(import.meta.dir, "..", "src", "cli.ts"),
    ]);
    const started = await manager.start({
      taskId: "task-2",
      kind: "agent",
      command: {
        executable: "/bin/sh",
        args: ["-c", "trap 'exit 0' TERM; while :; do sleep 1; done"],
      },
    });
    if (started.state === "failed") throw new Error(started.error);
    expect(started.state).toBe("running");
    expect(await manager.status("task-2")).toBeUndefined();
    expect(await manager.status("task-2", "agent")).toMatchObject({ state: "running" });
    expect(await manager.terminate("task-2", "agent")).toMatchObject({ state: "exited" });
    await manager.finalize("task-2", "agent");
  });

  test("records a supervisor that cannot start without launching a terminal child", async () => {
    const root = mkdtempSync(join(tmpdir(), "altitude-failed-terminal-"));
    roots.push(root);
    const manager = new TerminalSessionManager(root, ["/definitely/missing/altitude-supervisor"]);
    expect(await manager.start({
      taskId: "task-3",
      command: { executable: "/bin/sh", args: ["-c", "exit 42"] },
    })).toMatchObject({ state: "failed" });
    await manager.finalize("task-3");
  });

  test("injects routed operator input into a detached agent terminal", async () => {
    const root = mkdtempSync(join(tmpdir(), "altitude-agent-input-"));
    roots.push(root);
    const manager = new TerminalSessionManager(root, [
      process.execPath,
      join(import.meta.dir, "..", "src", "cli.ts"),
    ]);
    expect(await manager.start({
      taskId: "task-4",
      kind: "agent",
      command: {
        executable: "/bin/sh",
        args: ["-c", "IFS= read -r line; test \"$line\" = continue"],
      },
    })).toMatchObject({ state: "running" });
    await manager.writeInput("task-4", "continue\n", "agent");
    let status = await manager.status("task-4", "agent");
    for (let attempt = 0; attempt < 100 && status?.state === "running"; attempt += 1) {
      await Bun.sleep(10);
      status = await manager.status("task-4", "agent");
    }
    expect(status).toMatchObject({ state: "exited", exitCode: 0 });
    await manager.finalize("task-4", "agent");
  });
});
