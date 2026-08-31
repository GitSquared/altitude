import { describe, expect, test } from "bun:test";

import type { Task } from "../src/core/model.ts";
import {
  decodeTuiKey,
  renderTaskBoard,
  renderWelcome,
  runAltitudeTui,
  type AltitudeTuiActions,
  type TuiKey,
  type TuiTerminal,
} from "../src/tui/index.ts";

class ScriptedTerminal implements TuiTerminal {
  readonly interactive = true;
  readonly frames: string[] = [];
  readonly prompts: string[] = [];
  externalEntries = 0;
  externalExits = 0;

  constructor(
    private readonly keys: TuiKey[],
    private readonly lines: Array<string | undefined> = [],
  ) {}

  draw(content: string): void {
    this.frames.push(content);
  }

  async readKey(): Promise<TuiKey> {
    const key = this.keys.shift();
    if (key === undefined) throw new Error("Scripted terminal ran out of keys");
    return key;
  }

  async readLine(prompt: string): Promise<string | undefined> {
    this.prompts.push(prompt);
    return this.lines.shift();
  }

  beforeExternal(): void {
    this.externalEntries += 1;
  }

  afterExternal(): void {
    this.externalExits += 1;
  }
}

function task(input: Partial<Task> & Pick<Task, "id" | "title" | "status">): Task {
  return {
    id: input.id,
    title: input.title,
    status: input.status,
    intent: input.intent ?? input.title,
    intentLevel: input.intentLevel ?? "principles",
    revision: input.revision ?? 1,
    createdAt: input.createdAt ?? 1,
    updatedAt: input.updatedAt ?? 1,
    activeAttention: input.activeAttention ?? null,
    openUpdates: input.openUpdates ?? [],
  };
}

describe("Altitude TUI", () => {
  test("teaches the capsule boundary before opening native Claude login", async () => {
    const terminal = new ScriptedTerminal(["enter", "quit"]);
    let configured = false;
    let setupCalls = 0;
    const actions: AltitudeTuiActions = {
      configured: async () => configured,
      configureClaude: async () => {
        setupCalls += 1;
        configured = true;
      },
      listTasks: async () => [],
      createTask: async () => { throw new Error("not used"); },
      runTask: async () => { throw new Error("not used"); },
      takeTask: async () => { throw new Error("not used"); },
      openShell: async () => { throw new Error("not used"); },
    };

    expect(await runAltitudeTui(terminal, actions)).toBe(0);
    expect(setupCalls).toBe(1);
    expect(terminal.frames[0]).toContain("isolated capsules");
    expect(terminal.frames[0]).toContain("do not inherit this");
    expect(terminal.frames[0]).toContain("Claude Code");
    expect(terminal.frames.at(-1)).toContain("The factory is quiet");
    expect(terminal.externalEntries).toBe(1);
    expect(terminal.externalExits).toBe(1);
  });

  test("creates and starts the first delegated task without asking for a host path", async () => {
    const terminal = new ScriptedTerminal(["new", "quit"], ["Build a tiny compiler"]);
    const tasks: Task[] = [];
    const takes: string[] = [];
    const actions: AltitudeTuiActions = {
      configured: async () => true,
      configureClaude: async () => { throw new Error("not used"); },
      listTasks: async () => tasks,
      createTask: async (intent) => {
        const created = task({ id: "task-1", title: intent, status: "queued", updatedAt: 2 });
        tasks.push(created);
        return created;
      },
      runTask: async () => { throw new Error("not used"); },
      takeTask: async (taskId) => {
        takes.push(taskId);
        tasks[0] = task({ ...tasks[0]!, status: "paused", revision: 2, updatedAt: 3 });
      },
      openShell: async () => { throw new Error("not used"); },
    };

    expect(await runAltitudeTui(terminal, actions)).toBe(0);
    expect(terminal.prompts).toEqual(["What would you like to delegate? "]);
    expect(takes).toEqual(["task-1"]);
    expect(terminal.frames.at(-1)).toContain("Build a tiny compiler");
    expect(terminal.frames.at(-1)).toContain("paused");
  });

  test("orders the overview by attention rather than pretending to be a rich tracker", () => {
    const content = renderTaskBoard([
      task({ id: "done", title: "Already shipped", status: "completed" }),
      task({ id: "ready", title: "Ready task", status: "queued" }),
      task({ id: "working", title: "Working task", status: "running" }),
      task({
        id: "decision",
        title: "Choose a path",
        status: "waiting_decision",
        activeAttention: {
          id: "attention-1",
          class: "decision",
          title: "Choose",
          summary: "Two valid directions",
          question: "Which one?",
          raisedAt: 1,
          resumeStatus: "running",
        },
      }),
    ], 0);
    expect(content.indexOf("NEEDS YOU")).toBeLessThan(content.indexOf("WORKING"));
    expect(content.indexOf("WORKING")).toBeLessThan(content.indexOf("READY"));
    expect(content.indexOf("READY")).toBeLessThan(content.indexOf("DONE"));
    expect(content).toContain("waiting_decision · decision");
  });

  test("restores checkpointed tasks and earns the remote-node suggestion", async () => {
    const terminal = new ScriptedTerminal(["quit"]);
    const paused = task({ id: "task-1", title: "Continue the compiler", status: "paused" });
    const actions: AltitudeTuiActions = {
      configured: async () => true,
      configureClaude: async () => { throw new Error("not used"); },
      listTasks: async () => [paused],
      createTask: async () => { throw new Error("not used"); },
      runTask: async () => { throw new Error("not used"); },
      takeTask: async () => { throw new Error("not used"); },
      openShell: async () => { throw new Error("not used"); },
    };

    expect(await runAltitudeTui(terminal, actions)).toBe(0);
    expect(terminal.frames[0]).toContain("Restored 1 checkpointed task");
    expect(terminal.frames[0]).toContain("altitude node add");
    expect(terminal.frames[0]).toContain("after Altitude closes");
  });

  test("opens explicit capsule setup from the overview", async () => {
    const terminal = new ScriptedTerminal(["shell", "quit"]);
    const setups: string[] = [];
    const ready = task({ id: "task-1", title: "Prepare the compiler", status: "queued" });
    const actions: AltitudeTuiActions = {
      configured: async () => true,
      configureClaude: async () => { throw new Error("not used"); },
      listTasks: async () => [ready],
      createTask: async () => { throw new Error("not used"); },
      runTask: async () => { throw new Error("not used"); },
      takeTask: async () => { throw new Error("not used"); },
      openShell: async (taskId) => { setups.push(taskId); return "Workbench is still running."; },
    };

    expect(await runAltitudeTui(terminal, actions)).toBe(0);
    expect(setups).toEqual(["task-1"]);
    expect(terminal.frames[0]).toContain("[s] setup");
    expect(terminal.frames.at(-1)).toContain("Workbench is still running.");
  });

  test("decodes the small stable navigation vocabulary", () => {
    expect(decodeTuiKey("\u001b[A")).toBe("up");
    expect(decodeTuiKey("j")).toBe("down");
    expect(decodeTuiKey("\r")).toBe("enter");
    expect(decodeTuiKey("s")).toBe("shell");
    expect(decodeTuiKey("\u0003")).toBe("quit");
    expect(decodeTuiKey("x")).toBe("unknown");
  });

  test("welcome names native interactive setup, not API-key configuration", () => {
    const welcome = renderWelcome();
    expect(welcome).toContain("native login");
    expect(welcome).toContain("inside a node-local identity capsule");
    expect(welcome).not.toContain("API key");
  });
});
