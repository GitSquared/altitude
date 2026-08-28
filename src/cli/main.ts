import type { Writable } from "node:stream";

import type { AttentionResolution, EventActor } from "../core/events.ts";
import {
  INTENT_LEVELS,
  TASK_STATUSES,
  isIntentLevel,
  isTaskStatus,
  type Task,
  type TaskStatus,
} from "../core/model.ts";
import { resolveAltitudePaths, ensureAltitudePaths } from "../config/paths.ts";
import { runDoctor, renderDoctor } from "../doctor.ts";
import { BunCommandRunner, type CommandRunner } from "../process/command.ts";
import { TaskStore } from "../storage/task-store.ts";
import { runIdentityControlStdio } from "../identity/control.ts";
import { runTaskToolStdio } from "../tools/task-helper.ts";
import { VERSION } from "../version.ts";

const HELP = `altitude ${VERSION}

An intent control plane for delegated agentic development.

Usage:
  altitude <command> [options]

Commands:
  task create <intent>        Create a task at an explicit intention level
  task list                   List tasks
  task show <task-id>         Show task state and event history
  task queue <task-id>        Make a draft, stopped, or failed task runnable
  attention answer <task-id>  Answer the active Decision
  attention accept <task-id>  Accept the active Review and complete the task
  attention revise <task-id>  Return a Review with revision instructions
  attention ack <task-id>     Acknowledge an Update by --id
  status                      Summarize the local control plane
  doctor                      Check the local Altitude runtime boundary
  help                        Show this help
  version                     Print the Altitude version

Global output option:
  --json                      Emit stable machine-readable JSON
`;

const TASK_HELP = `Usage:
  altitude task create <intent...> [--title <title>] [--level <level>] [--json]
  altitude task list [--status <status>] [--json]
  altitude task show <task-id> [--json]
  altitude task queue <task-id> [--json]

Intent levels:
  ${INTENT_LEVELS.join(", ")}
`;

const ATTENTION_HELP = `Usage:
  altitude attention answer <task-id> <answer...> [--json]
  altitude attention accept <task-id> [note...] [--json]
  altitude attention revise <task-id> <instructions...> [--json]
  altitude attention ack <task-id> --id <attention-id> [note...] [--json]
`;

type Output = Pick<Writable, "write">;

export interface CliOptions {
  readonly env?: Readonly<Record<string, string | undefined>>;
  readonly stdout?: Output;
  readonly stderr?: Output;
  readonly clock?: () => number;
  readonly createId?: () => string;
  readonly commandRunner?: CommandRunner;
}

type ParsedArguments = Readonly<{
  positionals: readonly string[];
  options: ReadonlyMap<string, string | true>;
}>;

class CliUsageError extends Error {}

const OPERATOR: EventActor = { kind: "operator" };

function parseArguments(
  args: readonly string[],
  valueOptions: ReadonlySet<string> = new Set(),
  booleanOptions: ReadonlySet<string> = new Set(["json"]),
): ParsedArguments {
  const positionals: string[] = [];
  const options = new Map<string, string | true>();
  let literal = false;
  for (let index = 0; index < args.length; index += 1) {
    const argument = args[index]!;
    if (literal) {
      positionals.push(argument);
      continue;
    }
    if (argument === "--") {
      literal = true;
      continue;
    }
    if (!argument.startsWith("--")) {
      positionals.push(argument);
      continue;
    }
    const separator = argument.indexOf("=");
    const name = argument.slice(2, separator < 0 ? undefined : separator);
    if (booleanOptions.has(name)) {
      if (separator >= 0) throw new CliUsageError(`--${name} does not take a value`);
      options.set(name, true);
      continue;
    }
    if (!valueOptions.has(name)) throw new CliUsageError(`Unknown option: --${name}`);
    const value = separator >= 0 ? argument.slice(separator + 1) : args[++index];
    if (value === undefined || value === "") throw new CliUsageError(`--${name} requires a value`);
    options.set(name, value);
  }
  return { positionals, options };
}

function option(parsed: ParsedArguments, name: string): string | undefined {
  const value = parsed.options.get(name);
  return typeof value === "string" ? value : undefined;
}

function wantsJson(parsed: ParsedArguments): boolean {
  return parsed.options.get("json") === true;
}

function generatedTitle(intent: string): string {
  const firstLine = intent.trim().split(/\r?\n/, 1)[0]!.replace(/\s+/g, " ");
  return firstLine.length <= 72 ? firstLine : `${firstLine.slice(0, 69).trimEnd()}...`;
}

function writeJson(output: Output, value: unknown): void {
  output.write(`${JSON.stringify(value, null, 2)}\n`);
}

function renderTask(task: Task): string {
  const attention = task.activeAttention === null
    ? "none"
    : `${task.activeAttention.class}: ${task.activeAttention.title}`;
  return [
    `${task.id}  ${task.status}  ${task.intentLevel}`,
    task.title,
    `Intent: ${task.intent}`,
    `Blocking attention: ${attention}`,
    `Open updates: ${task.openUpdates.length}`,
    `Revision: ${task.revision}`,
    "",
  ].join("\n");
}

async function withStore<T>(options: CliOptions, run: (store: TaskStore) => T | Promise<T>): Promise<T> {
  const paths = resolveAltitudePaths(options.env ?? process.env);
  await ensureAltitudePaths(paths);
  using store = new TaskStore(paths.database, {
    ...(options.clock === undefined ? {} : { clock: options.clock }),
    ...(options.createId === undefined ? {} : { createId: options.createId }),
  });
  return await run(store);
}

async function runTaskCommand(
  args: readonly string[],
  options: CliOptions,
  stdout: Output,
): Promise<number> {
  const subcommand = args[0] ?? "help";
  if (["help", "--help", "-h"].includes(subcommand)) {
    stdout.write(TASK_HELP);
    return 0;
  }

  switch (subcommand) {
    case "create": {
      const parsed = parseArguments(args.slice(1), new Set(["title", "level"]));
      const intent = parsed.positionals.join(" ").trim();
      if (!intent) throw new CliUsageError("task create requires an intent");
      const level = option(parsed, "level") ?? "principles";
      if (!isIntentLevel(level)) throw new CliUsageError(`Unknown intent level: ${level}`);
      const title = option(parsed, "title")?.trim() || generatedTitle(intent);
      const task = await withStore(options, (store) => store.append({
        taskId: options.createId?.() ?? crypto.randomUUID(),
        type: "task.created",
        payload: { title, intent, intentLevel: level },
        actor: OPERATOR,
      }).task);
      if (wantsJson(parsed)) writeJson(stdout, task);
      else stdout.write(`Created\n${renderTask(task)}`);
      return 0;
    }
    case "list": {
      const parsed = parseArguments(args.slice(1), new Set(["status"]));
      if (parsed.positionals.length !== 0) throw new CliUsageError("task list does not take positional arguments");
      const requestedStatus = option(parsed, "status");
      if (requestedStatus !== undefined && !isTaskStatus(requestedStatus)) {
        throw new CliUsageError(`Unknown task status: ${requestedStatus}`);
      }
      const tasks = await withStore(options, (store) => store.listTasks(requestedStatus));
      if (wantsJson(parsed)) writeJson(stdout, tasks);
      else if (tasks.length === 0) stdout.write("No tasks yet.\n");
      else {
        for (const task of tasks) {
          const attention = task.activeAttention === null ? "" : `  ${task.activeAttention.class}!`;
          stdout.write(`${task.id}  ${task.status.padEnd(16)}  ${task.intentLevel}${attention}\n  ${task.title}\n`);
        }
      }
      return 0;
    }
    case "show": {
      const parsed = parseArguments(args.slice(1));
      if (parsed.positionals.length !== 1) throw new CliUsageError("task show requires exactly one task id");
      const taskId = parsed.positionals[0]!;
      const result = await withStore(options, (store) => {
        const task = store.getTask(taskId);
        if (task === undefined) throw new Error(`Task ${taskId} does not exist`);
        return { task, events: store.getEvents(taskId) };
      });
      if (wantsJson(parsed)) writeJson(stdout, result);
      else {
        stdout.write(renderTask(result.task));
        stdout.write("History:\n");
        for (const event of result.events) {
          stdout.write(`  ${event.sequence}. ${event.type}  ${new Date(event.occurredAt).toISOString()}\n`);
        }
      }
      return 0;
    }
    case "queue": {
      const parsed = parseArguments(args.slice(1));
      if (parsed.positionals.length !== 1) throw new CliUsageError("task queue requires exactly one task id");
      const taskId = parsed.positionals[0]!;
      const task = await withStore(options, (store) => {
        const current = store.getTask(taskId);
        if (current === undefined) throw new Error(`Task ${taskId} does not exist`);
        if (current.status === "queued") return current;
        return store.append({
          taskId,
          type: "task.state_changed",
          payload: { to: "queued", reason: "Queued by operator" },
          actor: OPERATOR,
        }).task;
      });
      if (wantsJson(parsed)) writeJson(stdout, task);
      else stdout.write(`Queued ${task.id}\n`);
      return 0;
    }
    default:
      throw new CliUsageError(`Unknown task command: ${subcommand}`);
  }
}

function resolutionFor(
  command: string,
  task: Task,
  words: readonly string[],
): { attentionId: string; resolution: AttentionResolution } {
  const attention = task.activeAttention;
  if (attention === null) throw new Error(`Task ${task.id} has no blocking attention`);
  const text = words.join(" ").trim();
  switch (command) {
    case "answer":
      if (attention.class !== "decision") throw new Error(`Task ${task.id} is waiting for a Review, not a Decision`);
      if (!text) throw new CliUsageError("attention answer requires an answer");
      return { attentionId: attention.id, resolution: { kind: "answered", answer: text } };
    case "accept":
      if (attention.class !== "review") throw new Error(`Task ${task.id} is waiting for a Decision, not a Review`);
      return {
        attentionId: attention.id,
        resolution: { kind: "accepted", ...(text ? { note: text } : {}) },
      };
    case "revise":
      if (attention.class !== "review") throw new Error(`Task ${task.id} is waiting for a Decision, not a Review`);
      if (!text) throw new CliUsageError("attention revise requires instructions");
      return { attentionId: attention.id, resolution: { kind: "revision_requested", instructions: text } };
    default:
      throw new CliUsageError(`Unknown attention command: ${command}`);
  }
}

async function runAttentionCommand(
  args: readonly string[],
  options: CliOptions,
  stdout: Output,
): Promise<number> {
  const subcommand = args[0] ?? "help";
  if (["help", "--help", "-h"].includes(subcommand)) {
    stdout.write(ATTENTION_HELP);
    return 0;
  }
  if (!["answer", "accept", "revise", "ack"].includes(subcommand)) {
    throw new CliUsageError(`Unknown attention command: ${subcommand}`);
  }
  const parsed = parseArguments(args.slice(1), subcommand === "ack" ? new Set(["id"]) : new Set());
  const [taskId, ...words] = parsed.positionals;
  if (taskId === undefined) throw new CliUsageError(`attention ${subcommand} requires a task id`);

  const task = await withStore(options, (store) => {
    const current = store.getTask(taskId);
    if (current === undefined) throw new Error(`Task ${taskId} does not exist`);
    let attentionId: string;
    let resolution: AttentionResolution;
    if (subcommand === "ack") {
      attentionId = option(parsed, "id") ?? "";
      if (!attentionId) throw new CliUsageError("attention ack requires --id <attention-id>");
      if (!current.openUpdates.some((update) => update.id === attentionId)) {
        throw new Error(`Update ${attentionId} is not open on task ${taskId}`);
      }
      const note = words.join(" ").trim();
      resolution = { kind: "acknowledged", ...(note ? { note } : {}) };
    } else {
      ({ attentionId, resolution } = resolutionFor(subcommand, current, words));
    }
    return store.append({
      taskId,
      type: "attention.resolved",
      payload: { attentionId, resolution },
      actor: OPERATOR,
    }).task;
  });
  if (wantsJson(parsed)) writeJson(stdout, task);
  else stdout.write(`Resolved ${subcommand} for ${task.id}; task is ${task.status}.\n`);
  return 0;
}

async function runStatus(options: CliOptions, stdout: Output, json: boolean): Promise<number> {
  const summary = await withStore(options, (store) => {
    const tasks = store.listTasks();
    const byStatus = Object.fromEntries(TASK_STATUSES.map((status) => [status, 0])) as Record<TaskStatus, number>;
    let openUpdates = 0;
    let blockingAttention = 0;
    for (const task of tasks) {
      byStatus[task.status] += 1;
      openUpdates += task.openUpdates.length;
      if (task.activeAttention !== null) blockingAttention += 1;
    }
    return { total: tasks.length, blockingAttention, openUpdates, byStatus };
  });
  if (json) writeJson(stdout, summary);
  else {
    const active = summary.byStatus.running + summary.byStatus.taken + summary.byStatus.provisioning;
    stdout.write(`${summary.total} tasks · ${active} active · ${summary.blockingAttention} need attention · ${summary.openUpdates} updates\n`);
    const nonzero = TASK_STATUSES.filter((status) => summary.byStatus[status] > 0);
    if (nonzero.length > 0) stdout.write(`${nonzero.map((status) => `${status} ${summary.byStatus[status]}`).join(" · ")}\n`);
  }
  return 0;
}

export async function runCli(args: readonly string[], options: CliOptions = {}): Promise<number> {
  const stdout = options.stdout ?? process.stdout;
  const stderr = options.stderr ?? process.stderr;
  const command = args[0] ?? "help";

  try {
    switch (command) {
      case "help":
      case "--help":
      case "-h":
        stdout.write(HELP);
        return 0;
      case "version":
      case "--version":
      case "-v":
        stdout.write(`${VERSION}\n`);
        return 0;
      case "doctor": {
        const parsed = parseArguments(args.slice(1));
        const findings = await runDoctor(options.commandRunner ?? new BunCommandRunner());
        if (wantsJson(parsed)) writeJson(stdout, findings);
        else stdout.write(renderDoctor(findings));
        return findings.some((finding) => finding.severity === "error") ? 1 : 0;
      }
      case "task":
        return await runTaskCommand(args.slice(1), options, stdout);
      case "attention":
        return await runAttentionCommand(args.slice(1), options, stdout);
      case "status": {
        const parsed = parseArguments(args.slice(1));
        if (parsed.positionals.length !== 0) throw new CliUsageError("status does not take positional arguments");
        return await runStatus(options, stdout, wantsJson(parsed));
      }
      case "_task-tool":
        return await runTaskToolStdio();
      case "_identity-control":
        return await runIdentityControlStdio();
      default:
        throw new CliUsageError(`Unknown command: ${command}`);
    }
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    stderr.write(`${message}\n`);
    if (error instanceof CliUsageError) stderr.write(`\n${HELP}`);
    return error instanceof CliUsageError ? 2 : 1;
  }
}
