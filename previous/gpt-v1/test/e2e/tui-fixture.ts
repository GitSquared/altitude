import { appendFileSync } from "node:fs";
import { join } from "node:path";

import type { Task } from "../../src/core/model.ts";
import { runTerminalSupervisor, TerminalSessionManager } from "../../src/terminal/session.ts";
import { runAltitudeTui, type AltitudeTuiActions } from "../../src/tui/app.tsx";

const [command, root] = process.argv.slice(2);

if (command === "_terminal-supervisor") {
  process.exit(await runTerminalSupervisor(root));
}
if (command !== "run" || root === undefined) process.exit(2);

const eventsPath = join(root, "events.log");
const terminals = new TerminalSessionManager(join(root, "terminals"), [process.execPath, import.meta.path]);
const now = Date.now();
let tasks: readonly Task[] = [
  {
    id: "amber-anchor",
    title: "Exercise the factory controls",
    intent: "Prove the real keyboard journey",
    intentLevel: "principles",
    status: "queued",
    revision: 1,
    createdAt: now,
    updatedAt: now + 2,
    activeAttention: null,
    openUpdates: [],
  },
  {
    id: "birch-brook",
    title: "Prove arrow-key selection",
    intent: "Open the task selected with the down arrow",
    intentLevel: "principles",
    status: "queued",
    revision: 1,
    createdAt: now,
    updatedAt: now + 1,
    activeAttention: null,
    openUpdates: [],
  },
];

function record(event: string): void {
  appendFileSync(eventsPath, `${event}\n`, { encoding: "utf8", mode: 0o600 });
}

const actions: AltitudeTuiActions = {
  configured: async () => true,
  configureClaude: async () => undefined,
  resumeInterruptedTasks: async () => {
    record("resume");
    return { resumed: 0, deferred: 0 };
  },
  listTasks: async () => tasks,
  createTask: async (intent) => {
    record(`create:${intent}`);
    const task: Task = {
      id: "cedar-delta",
      title: intent,
      intent,
      intentLevel: "principles",
      status: "queued",
      revision: 1,
      createdAt: now + 1,
      updatedAt: now + 1,
      activeAttention: null,
      openUpdates: [],
    };
    tasks = [...tasks, task];
    return task;
  },
  delegateTask: async (taskId) => {
    record(`delegate:${taskId}`);
    return "delegated";
  },
  runTask: async (taskId) => {
    record(`run:${taskId}`);
  },
  takeTask: async (taskId) => {
    record(`open:${taskId}`);
    let status = await terminals.status(taskId, "agent");
    if (status === undefined) {
      status = await terminals.start({
        taskId,
        kind: "agent",
        command: {
          executable: "/bin/sh",
          args: ["-c", "printf 'AGENT READY\\n'; trap 'exit 0' TERM; while :; do sleep 1; done"],
          env: { TERM: "xterm-256color" },
        },
      });
    }
    if (status.state !== "running") throw new Error("Fixture agent did not start");
    const attached = await terminals.attach(taskId, process.stdin, process.stdout, "agent", true);
    record(`${attached.outcome}:${taskId}`);
    return attached.outcome;
  },
  openShell: async (taskId) => {
    record(`shell:${taskId}`);
    return undefined;
  },
  reviewTask: async (taskId) => {
    record(`review:${taskId}`);
    return undefined;
  },
  resolveAttention: async () => undefined,
};

try {
  process.exitCode = await runAltitudeTui(actions);
} finally {
  if (process.env.ALTITUDE_E2E_KEEP_AGENT !== "1") {
    for (const taskId of ["amber-anchor", "birch-brook"]) {
      if ((await terminals.status(taskId, "agent"))?.state === "running") {
        await terminals.terminate(taskId, "agent");
      }
      await terminals.finalize(taskId, "agent");
    }
  }
}
