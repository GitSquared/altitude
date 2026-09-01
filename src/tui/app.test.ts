import { describe, expect, test } from "bun:test";
import { createElement } from "react";
import { renderToString } from "ink";

import type { Task } from "../core/model.ts";
import {
  TaskBoardScreen,
  WelcomeScreen,
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
  test("teaches the capsule boundary before native Claude login", () => {
    const output = renderToString(createElement(WelcomeScreen, {}));
    expect(output).toContain("isolated capsules");
    expect(output).toContain("do not inherit this");
    expect(output).toContain("Claude Code");
    expect(output).toContain("node-local identity capsule");
  });

  test("renders the empty factory and native shortcuts", () => {
    const output = renderToString(createElement(TaskBoardScreen, {
      tasks: [],
      selectedIndex: 0,
    }));
    expect(output).toContain("The factory is quiet");
    expect(output).toContain("n new");
    expect(output).toContain("v visual review");
    expect(output).toContain("q quit");
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
    }));
    expect(output.indexOf("NEEDS YOU")).toBeLessThan(output.indexOf("WORKING"));
    expect(output).toContain("waiting_decision · decision");
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
