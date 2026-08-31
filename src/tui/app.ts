import type { Task, TaskStatus } from "../core/model.ts";
import type { TuiTerminal } from "./terminal.ts";

export interface AltitudeTuiActions {
  configured(): Promise<boolean>;
  configureClaude(): Promise<void>;
  listTasks(): Promise<readonly Task[]>;
  createTask(intent: string): Promise<Task>;
  runTask(taskId: string): Promise<void>;
  takeTask(taskId: string): Promise<void>;
  openShell(taskId: string): Promise<string | undefined>;
}

type TaskGroup = "Needs you" | "Working" | "Ready" | "Done";

const GROUP_ORDER: readonly TaskGroup[] = ["Needs you", "Working", "Ready", "Done"];

function taskGroup(status: TaskStatus): TaskGroup {
  if (status === "waiting_decision" || status === "waiting_review") return "Needs you";
  if (status === "provisioning" || status === "running" || status === "waiting_external"
    || status === "taken" || status === "pausing" || status === "transferring") return "Working";
  if (status === "completed" || status === "archived") return "Done";
  return "Ready";
}

function orderedTasks(tasks: readonly Task[]): readonly Task[] {
  return [...tasks].sort((left, right) => {
    const group = GROUP_ORDER.indexOf(taskGroup(left.status)) - GROUP_ORDER.indexOf(taskGroup(right.status));
    return group !== 0 ? group : right.updatedAt - left.updatedAt || left.id.localeCompare(right.id);
  });
}

export function renderWelcome(notice?: string): string {
  return [
    "ALTITUDE",
    "delegated software factory",
    "",
    "Your agents work inside isolated capsules. They do not inherit this",
    "machine's files, tools, credentials, configuration, or current directory.",
    "You oversee the work; they operate the factory floor.",
    "",
    "Choose a coding harness",
    "  > Claude Code   managed, resumable, sandboxed",
    "",
    "Claude's native login will open inside a node-local identity capsule.",
    ...(notice === undefined ? [] : ["", `! ${notice}`]),
    "",
    "[enter] continue    [q] quit",
    "",
  ].join("\n");
}

export function renderTaskBoard(
  tasksInput: readonly Task[],
  selectedIndex: number,
  notice?: string,
): string {
  const tasks = orderedTasks(tasksInput);
  const lines = [
    "ALTITUDE",
    "factory overview",
    "",
  ];
  if (tasks.length === 0) {
    lines.push("The factory is quiet.", "Press n to delegate your first task.", "");
  } else {
    for (const group of GROUP_ORDER) {
      const grouped = tasks
        .map((task, index) => ({ task, index }))
        .filter(({ task }) => taskGroup(task.status) === group);
      if (grouped.length === 0) continue;
      lines.push(group.toUpperCase());
      for (const { task, index } of grouped) {
        const cursor = index === selectedIndex ? ">" : " ";
        const attention = task.activeAttention === null ? "" : ` · ${task.activeAttention.class}`;
        lines.push(`${cursor} ${task.title}`, `    ${task.status}${attention}`);
      }
      lines.push("");
    }
  }
  if (notice !== undefined) lines.push(`! ${notice}`, "");
  lines.push("[j/k or arrows] select   [enter] open   [r] run   [s] setup   [n] new   [q] quit", "");
  return lines.join("\n");
}

function canTake(task: Task): boolean {
  return task.status === "paused" || task.status === "running" || task.status === "waiting_external";
}

async function external(terminal: TuiTerminal, action: () => Promise<void>): Promise<string | undefined> {
  terminal.beforeExternal();
  try {
    await action();
    return undefined;
  } catch (error) {
    return error instanceof Error ? error.message : String(error);
  } finally {
    terminal.afterExternal();
  }
}

export async function runAltitudeTui(
  terminal: TuiTerminal,
  actions: AltitudeTuiActions,
): Promise<number> {
  if (!terminal.interactive) throw new Error("Altitude's TUI requires an interactive terminal");

  if (!(await actions.configured())) {
    let notice: string | undefined;
    while (!(await actions.configured())) {
      terminal.draw(renderWelcome(notice));
      const key = await terminal.readKey();
      if (key === "quit") return 0;
      if (key !== "enter") continue;
      notice = await external(terminal, () => actions.configureClaude());
    }
  }

  let selectedIndex = 0;
  const restoredTasks = await actions.listTasks();
  const resumable = restoredTasks.filter((task) => task.status === "paused").length;
  const scheduled = restoredTasks.filter((task) => task.status === "waiting_external").length;
  let notice = resumable > 0
    ? `Restored ${resumable} checkpointed task${resumable === 1 ? "" : "s"}. Press enter to resume. Want work to continue after Altitude closes? Add a remote node with \`altitude node add\`.`
    : scheduled > 0
      ? `Resumed local workers for ${scheduled} scheduled task${scheduled === 1 ? "" : "s"}. Keep Altitude open, or add a remote node for unattended work.`
      : undefined;
  while (true) {
    const tasks = orderedTasks(await actions.listTasks());
    selectedIndex = tasks.length === 0 ? 0 : Math.min(selectedIndex, tasks.length - 1);
    terminal.draw(renderTaskBoard(tasks, selectedIndex, notice));
    notice = undefined;
    const key = await terminal.readKey();
    if (key === "quit") return 0;
    if (key === "up" && tasks.length > 0) {
      selectedIndex = (selectedIndex + tasks.length - 1) % tasks.length;
      continue;
    }
    if (key === "down" && tasks.length > 0) {
      selectedIndex = (selectedIndex + 1) % tasks.length;
      continue;
    }
    if (key === "new") {
      terminal.beforeExternal();
      const intent = (await terminal.readLine("What would you like to delegate? "))?.trim();
      terminal.afterExternal();
      if (!intent) {
        notice = "No task created.";
        continue;
      }
      try {
        const task = await actions.createTask(intent);
        notice = await external(terminal, () => actions.takeTask(task.id));
      } catch (error) {
        notice = error instanceof Error ? error.message : String(error);
      }
      continue;
    }
    if ((key === "enter" || key === "run") && tasks.length > 0) {
      const task = tasks[selectedIndex]!;
      if (task.status === "waiting_decision" || task.status === "waiting_review") {
        notice = `${task.activeAttention?.class ?? "Attention"} needs an answer; use the attention command for now.`;
        continue;
      }
      notice = await external(
        terminal,
        () => key === "enter" && canTake(task)
          ? actions.takeTask(task.id)
          : actions.runTask(task.id),
      );
      continue;
    }
    if (key === "shell" && tasks.length > 0) {
      terminal.beforeExternal();
      try {
        notice = await actions.openShell(tasks[selectedIndex]!.id);
      } catch (error) {
        notice = error instanceof Error ? error.message : String(error);
      } finally {
        terminal.afterExternal();
      }
    }
  }
}
