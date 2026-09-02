import { describe, expect, test } from "bun:test";
import { createElement } from "react";
import { renderToString } from "ink";

import type { Task } from "../core/model.ts";
import {
  TaskBoardScreen,
  WelcomeScreen,
  delegateNewTask,
  orderedTasks,
  startupNotice,
} from "./app.tsx";

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

describe("Ink factory overview", () => {
  test("records a new task before delegating it", async () => {
    const calls: string[] = [];
    const result = await delegateNewTask({
      createTask: async (intent) => {
        calls.push(`create:${intent}`);
        return task({ id: "task-first", title: intent, status: "queued" });
      },
      delegateTask: async (taskId) => {
        calls.push(`delegate:${taskId}`);
        return "running";
      },
    }, "Build the first thing");
    expect(calls).toEqual(["create:Build the first thing", "delegate:task-first"]);
    expect(result).toBe("running");
  });

  test("teaches the capsule boundary before native Claude login", () => {
    const output = renderToString(createElement(WelcomeScreen, {}));
    expect(output).toContain("isolated capsules");
    expect(output).toContain("do not inherit this");
    expect(output).toContain("Claude Code");
    expect(output).toContain("persistent identity");
    expect(output).toContain("first real task opens interactively");
    expect(output).toContain("later tasks delegate in the background");
  });

  test("renders the empty factory and native shortcuts", () => {
    const output = renderToString(createElement(TaskBoardScreen, {
      tasks: [],
      selectedIndex: 0,
      width: 120,
      height: 30,
    }));
    expect(output).toContain("The factory is quiet");
    expect(output).toContain("n new");
    expect(output).toContain("v visual review");
    expect(output).toContain("q quit");
  });

  test("keeps the compact factory footer within an 80-column terminal", () => {
    const output = renderToString(createElement(TaskBoardScreen, {
      tasks: [task({ id: "amber-anchor", title: "A compact task", status: "running" })],
      selectedIndex: 0,
      width: 80,
      height: 24,
    }), { columns: 80 });
    expect(output).toContain("ctrl-a back from capsule");
    expect(output.split("\n").every((line) => [...line].length <= 80)).toBeTrue();
  });

  test("orders work by attention rather than tracker ceremony", () => {
    const tasks = orderedTasks([
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
    ]);
    expect(tasks.map((value) => value.id)).toEqual([
      "decision",
      "working",
      "ready",
      "done",
    ]);
    const output = renderToString(createElement(TaskBoardScreen, {
      tasks,
      selectedIndex: 0,
      notice: "Add an Iroh node for unattended work.",
      width: 120,
      height: 30,
    }));
    expect(output.indexOf("NEEDS YOU")).toBeLessThan(output.indexOf("WORKING"));
    expect(output).toContain("decision");
    expect(output).toContain("waiting_decision");
    expect(output).toContain("DECISION · Choose");
    expect(output).toContain("intent level · principles");
    expect(output).toContain("Add an Iroh node");
  });

  test("explains automatic local resume and the remote-node upgrade path", () => {
    expect(startupNotice({ resumed: 2, deferred: 0 }, [])).toContain(
      "Resumed 2 tasks interrupted when Altitude closed",
    );
    expect(startupNotice({ resumed: 2, deferred: 0 }, [])).toContain("Add an Iroh node");
    expect(startupNotice({ resumed: 0, deferred: 1 }, [])).toContain("retry next time");
  });
});
