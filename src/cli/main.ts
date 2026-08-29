import type { Writable } from "node:stream";
import { join, resolve } from "node:path";

import type { AttentionResolution, EventActor } from "../core/events.ts";
import { ClaudeCliAdapter } from "../agent/claude-cli.ts";
import type { AgentEvent } from "../agent/protocol.ts";
import {
  INTENT_LEVELS,
  TASK_STATUSES,
  isIntentLevel,
  isTaskStatus,
  type Task,
  type TaskStatus,
} from "../core/model.ts";
import { resolveAltitudePaths, ensureAltitudePaths } from "../config/paths.ts";
import {
  defaultLocalConfig,
  readLocalConfig,
  validateLocalConfig,
  writeLocalConfig,
  type AltitudeLocalConfig,
} from "../config/local.ts";
import { runDoctor, renderDoctor } from "../doctor.ts";
import { createAltitudeNodeService, nodeInstanceId, serveAltitudeNode } from "../node/server.ts";
import { NodeScheduledActionWorker } from "../node/worker.ts";
import { RedmarkerIntentTool } from "../intent/redmarker.ts";
import { IntentSubmissionStore } from "../intent/submission.ts";
import { BunCommandRunner, type CommandRunner } from "../process/command.ts";
import type { RuntimeKind } from "../runtime/capsule-runtime.ts";
import { AppleContainerRuntime } from "../runtime/apple-container.ts";
import { DockerCapsuleRuntime } from "../runtime/docker.ts";
import { CapsuleImageBuilder } from "../runtime/images.ts";
import { IdentityCapsuleManager } from "../runtime/identity-manager.ts";
import { identityVolumeName } from "../runtime/identity-capsule.ts";
import { CapsuleProcessLauncher } from "../runtime/process-launcher.ts";
import {
  TaskCapsuleBootstrapProvisioner,
  TaskCapsuleCheckpointProvisioner,
  TaskCapsuleManager,
} from "../runtime/task-manager.ts";
import { setupAltitude } from "../setup.ts";
import { SqliteAgentWriterLeaseStore } from "../storage/agent-writer-lease-store.ts";
import { TaskAgentSessionStore } from "../storage/task-agent-session-store.ts";
import { TaskStore } from "../storage/task-store.ts";
import { IdentityControlProvisioner, runIdentityControlStdio } from "../identity/control.ts";
import { runIdentityInit } from "../identity/init.ts";
import { TaskMcpGrantStore } from "../tools/grants.ts";
import { ProcessTaskToolExecutor } from "../tools/process-executor.ts";
import { runTaskToolStdio } from "../tools/task-helper.ts";
import { runTaskBootstrapStdio } from "../capsule/bootstrap.ts";
import { runTaskCheckpointStdio } from "../capsule/task-checkpoint.ts";
import { runTaskInit } from "../capsule/init.ts";
import { FilesystemCapsuleCheckpointStore } from "../capsule/filesystem.ts";
import { capsuleManifestDigest } from "../capsule/manifest.ts";
import { FilesystemRepositorySeedCache } from "../capsule/seed-cache.ts";
import { LocalCapsulePreparer } from "../run/local-capsule.ts";
import { LocalCapsuleCheckpointer } from "../run/local-checkpoint.ts";
import { portableTaskStateMember } from "../run/task-state.ts";
import { createManagedAgentRetryExecutor } from "../run/retry-executor.ts";
import { TaskRunCoordinator } from "../run/task-runner.ts";
import { VERSION } from "../version.ts";

const HELP = `altitude ${VERSION}

An intent control plane for delegated agentic development.

Usage:
  altitude <command> [options]

Commands:
  setup                       Build capsules and log in to managed Claude
  task create <intent>        Create a task at an explicit intention level
  task list                   List tasks
  task show <task-id>         Show task state and event history
  task queue <task-id>        Make a draft, stopped, or failed task runnable
  task run <task-id>          Run or resume a managed headless Claude turn
  task take <task-id>         Enter the same session in Claude Code's TUI
  task review <task-id>       Add visual intent with managed Redmarker
  attention answer <task-id>  Answer the active Decision
  attention accept <task-id>  Accept the active Review and complete the task
  attention revise <task-id>  Return a Review with revision instructions
  attention ack <task-id>     Acknowledge an Update by --id
  node serve                  Run the local task/MCP control endpoint
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
  altitude task run <task-id> [--repository <path>] [--model <model>] [--json]
  altitude task take <task-id>
  altitude task review <task-id> --url <loopback-url> [--redmarker <executable>] [--json]

Intent levels:
  ${INTENT_LEVELS.join(", ")}
`;

const ATTENTION_HELP = `Usage:
  altitude attention answer <task-id> <answer...> [--json]
  altitude attention accept <task-id> [note...] [--json]
  altitude attention revise <task-id> <instructions...> [--json]
  altitude attention ack <task-id> --id <attention-id> [note...] [--json]
`;

const NODE_HELP = `Usage:
  altitude node serve [--host <address>] [--port <port>] [--runtime <runtime>]

Runtimes:
  apple-container, docker

The listener defaults to loopback. Use --host deliberately when capsules on a
native Linux node need to reach the service through the host gateway.
With the saved runtime and listener settings, the node also resumes durable
rate-limit retries for the exact managed Claude session.
`;

const SETUP_HELP = `Usage:
  altitude setup [--runtime <runtime>] [--identity <id>] [--source <path>]
                 [--identity-image <image>] [--task-image <image>]
                 [--network <name>] [--node-url <url>]
                 [--node-host <address>] [--node-port <port>]
                 [--skip-build] [--json]

Setup builds the two OCI images, creates a source-free identity capsule, and
opens the official Claude Code login there when authentication is needed.
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
        return {
          task,
          events: store.getEvents(taskId),
          intentArtifacts: store.listIntentArtifacts(taskId),
        };
      });
      if (wantsJson(parsed)) writeJson(stdout, result);
      else {
        stdout.write(renderTask(result.task));
        stdout.write("History:\n");
        for (const event of result.events) {
          stdout.write(`  ${event.sequence}. ${event.type}  ${new Date(event.occurredAt).toISOString()}\n`);
        }
        if (result.intentArtifacts.length > 0) {
          stdout.write("Visual intent:\n");
          for (const artifact of result.intentArtifacts) {
            stdout.write(`  ${artifact.tool} ${artifact.artifactId}  ${artifact.createdAt}\n`);
          }
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
    case "run":
      return await runManagedTask(args.slice(1), options, stdout);
    case "take":
      return await takeManagedTask(args.slice(1), options, stdout);
    case "review":
      return await reviewManagedTask(args.slice(1), options, stdout);
    default:
      throw new CliUsageError(`Unknown task command: ${subcommand}`);
  }
}

type NodeUse = Readonly<{ external: boolean; close(): Promise<void> }>;

async function existingNode(port: number, expectedInstanceId: string): Promise<boolean> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), 500);
  try {
    const response = await fetch(`http://127.0.0.1:${port}/healthz`, { signal: controller.signal });
    if (!response.ok) return false;
    const value = await response.json() as { service?: string; instanceId?: string };
    return value.service === "altitude-node" && value.instanceId === expectedInstanceId;
  } catch {
    return false;
  } finally {
    clearTimeout(timer);
  }
}

async function useTaskNode(input: {
  readonly config: AltitudeLocalConfig;
  readonly store: TaskStore;
  readonly grants: TaskMcpGrantStore;
  readonly launcher: CapsuleProcessLauncher;
  readonly databasePath: string;
  readonly submissions?: IntentSubmissionStore;
}): Promise<NodeUse> {
  const instanceId = nodeInstanceId(input.databasePath);
  if (await existingNode(input.config.node.port, instanceId)) {
    return { external: true, close: async () => undefined };
  }
  const service = createAltitudeNodeService({
    runtime: input.config.runtime,
    store: input.store,
    grants: input.grants,
    executor: new ProcessTaskToolExecutor(input.launcher),
    ...(input.submissions === undefined ? {} : { submissions: input.submissions }),
    instanceId,
    allowedHostnames: [...new Set([
      "127.0.0.1",
      "localhost",
      "[::1]",
      "host.docker.internal",
      "host.container.internal",
      new URL(input.config.node.capsuleMcpEndpoint).hostname,
    ])],
  });
  const server = serveAltitudeNode({
    service,
    hostname: input.config.node.bindHost,
    port: input.config.node.port,
  });
  return { external: false, close: () => server.close() };
}

async function managedContext(options: CliOptions): Promise<{
  readonly paths: ReturnType<typeof resolveAltitudePaths>;
  readonly config: AltitudeLocalConfig;
  readonly runner: CommandRunner;
}> {
  const paths = resolveAltitudePaths(options.env ?? process.env);
  await ensureAltitudePaths(paths);
  const config = await readLocalConfig(paths.config);
  if (config === undefined) throw new Error("Altitude is not set up yet. Run `altitude setup` first.");
  return { paths, config, runner: options.commandRunner ?? new BunCommandRunner() };
}

async function ensureManagedIdentity(
  config: AltitudeLocalConfig,
  runner: CommandRunner,
  launcher: CapsuleProcessLauncher,
  store: TaskStore,
  options: CliOptions,
): Promise<ClaudeCliAdapter> {
  await new IdentityCapsuleManager(config.runtime, runner).ensure({
    identityId: config.identity.id,
    image: config.identity.image,
    identityVolume: identityVolumeName(config.identity.id),
    network: config.network,
    command: ["sleep", "infinity"],
    environment: { CLAUDE_CONFIG_DIR: "/identity/claude", DISABLE_AUTOUPDATER: "1" },
  });
  const agent = new ClaudeCliAdapter({
    identity: { id: config.identity.id, configDirectory: "/identity/claude", loginDirectory: "/identity" },
    launcher,
    leases: new SqliteAgentWriterLeaseStore(store.database),
    ...(options.clock === undefined ? {} : { now: options.clock }),
    ...(options.createId === undefined ? {} : { createId: options.createId }),
  });
  const probe = await agent.probe();
  if (!probe.installed || probe.auth !== "authenticated") {
    throw new Error("Managed Claude is not authenticated. Run `altitude setup` to log in inside the identity capsule.");
  }
  return agent;
}

async function prepareTaskCapsule(input: {
  readonly task: Task;
  readonly store: TaskStore;
  readonly repositoryPath?: string;
  readonly paths: ReturnType<typeof resolveAltitudePaths>;
  readonly config: AltitudeLocalConfig;
  readonly runner: CommandRunner;
  readonly launcher: CapsuleProcessLauncher;
  readonly clock?: () => number;
}): Promise<LocalCapsuleCheckpointer> {
  const needsInitialRepository = input.task.status === "queued";
  if (needsInitialRepository && input.repositoryPath === undefined) {
    throw new CliUsageError("The first task run requires --repository <worktree-root>");
  }
  if (!needsInitialRepository && input.repositoryPath !== undefined) {
    throw new CliUsageError("--repository is only accepted for the first queued run; resumes use the task capsule");
  }
  const checkpoints = new FilesystemCapsuleCheckpointStore(input.paths.capsules);
  const seeds = new FilesystemRepositorySeedCache(join(input.paths.cache, "repository-seeds"));
  const checkpointProvisioner = new TaskCapsuleCheckpointProvisioner(input.launcher);
  const checkpointer = new LocalCapsuleCheckpointer(
    checkpoints,
    seeds,
    checkpointProvisioner,
    input.store,
    input.clock ?? Date.now,
  );
  const preparer = new LocalCapsulePreparer(
    checkpoints,
    seeds,
    new TaskCapsuleManager(input.config.runtime, input.runner),
    new TaskCapsuleBootstrapProvisioner(input.launcher),
    checkpointer,
  );
  await preparer.prepare({
    taskId: input.task.id,
    taskImage: input.config.taskImage,
    network: input.config.network,
    nodeId: "local",
    ...(input.repositoryPath === undefined ? {} : { repositoryPath: resolve(input.repositoryPath) }),
    additionalMembers: [portableTaskStateMember(input.store, input.task.id)],
  });
  return checkpointer;
}

function renderAgentEvent(event: AgentEvent): string | undefined {
  if (event.type === "agent.message" && event.partial === false && typeof event.text === "string") return `${event.text}\n`;
  if (event.type === "agent.api_retry") return "↻ Claude API retry\n";
  if (event.type === "agent.rate_limit") return `! Claude rate limit: ${String(event.status)}\n`;
  if (event.type === "agent.protocol_error") return `! Agent protocol error: ${String(event.message)}\n`;
  return undefined;
}

async function runManagedTask(args: readonly string[], options: CliOptions, stdout: Output): Promise<number> {
  const parsed = parseArguments(args, new Set(["repository", "model"]));
  if (parsed.positionals.length !== 1) throw new CliUsageError("task run requires exactly one task id");
  const taskId = parsed.positionals[0]!;
  const { paths, config, runner } = await managedContext(options);
  using store = new TaskStore(paths.database, {
    ...(options.clock === undefined ? {} : { clock: options.clock }),
    ...(options.createId === undefined ? {} : { createId: options.createId }),
  });
  const task = store.getTask(taskId);
  if (task === undefined) throw new Error(`Task ${taskId} does not exist`);
  const launcher = new CapsuleProcessLauncher(config.runtime);
  const agent = await ensureManagedIdentity(config, runner, launcher, store, options);
  const checkpointer = await prepareTaskCapsule({
    task,
    store,
    ...(option(parsed, "repository") === undefined ? {} : { repositoryPath: option(parsed, "repository")! }),
    paths,
    config,
    runner,
    launcher,
    ...(options.clock === undefined ? {} : { clock: options.clock }),
  });
  const grants = new TaskMcpGrantStore(store.database, options.clock);
  const node = await useTaskNode({ config, store, grants, launcher, databasePath: paths.database });
  try {
    const coordinator = new TaskRunCoordinator(
      store,
      new TaskAgentSessionStore(store.database, options.clock),
      grants,
      new IdentityControlProvisioner(launcher),
      agent,
      options.clock ?? Date.now,
    );
    const managed = await coordinator.start({
      taskId,
      identityId: config.identity.id,
      mcpEndpoint: config.node.capsuleMcpEndpoint,
      ...(option(parsed, "model") === undefined ? {} : { model: option(parsed, "model")! }),
    });
    const json = wantsJson(parsed);
    const events = (async () => {
      for await (const event of managed.run.events) {
        if (json) stdout.write(`${JSON.stringify({ type: "agent_event", event })}\n`);
        else {
          const rendered = renderAgentEvent(event);
          if (rendered !== undefined) stdout.write(rendered);
        }
      }
    })();
    const [completion] = await Promise.all([managed.settle(), events]);
    const checkpoint = await checkpointer.capture(taskId);
    const current = store.getTask(taskId)!;
    if (json) stdout.write(`${JSON.stringify({ type: "completion", completion, task: current, checkpoint })}\n`);
    else stdout.write(`Task ${taskId} is ${current.status} (${completion.outcome}); capsule revision ${checkpoint.revision} is durable.\n`);
    return completion.outcome === "error" || completion.outcome === "protocol_error" ? 1
      : completion.outcome === "rate_limited" ? 75 : 0;
  } finally {
    await node.close();
  }
}

async function takeManagedTask(args: readonly string[], options: CliOptions, stdout: Output): Promise<number> {
  const parsed = parseArguments(args, new Set(), new Set());
  if (parsed.positionals.length !== 1) throw new CliUsageError("task take requires exactly one task id");
  const taskId = parsed.positionals[0]!;
  const { paths, config, runner } = await managedContext(options);
  using store = new TaskStore(paths.database, {
    ...(options.clock === undefined ? {} : { clock: options.clock }),
    ...(options.createId === undefined ? {} : { createId: options.createId }),
  });
  const task = store.getTask(taskId);
  if (task === undefined) throw new Error(`Task ${taskId} does not exist`);
  const launcher = new CapsuleProcessLauncher(config.runtime);
  const agent = await ensureManagedIdentity(config, runner, launcher, store, options);
  const checkpointer = await prepareTaskCapsule({
    task,
    store,
    paths,
    config,
    runner,
    launcher,
    ...(options.clock === undefined ? {} : { clock: options.clock }),
  });
  const grants = new TaskMcpGrantStore(store.database, options.clock);
  const node = await useTaskNode({ config, store, grants, launcher, databasePath: paths.database });
  try {
    const coordinator = new TaskRunCoordinator(
      store,
      new TaskAgentSessionStore(store.database, options.clock),
      grants,
      new IdentityControlProvisioner(launcher),
      agent,
      options.clock ?? Date.now,
    );
    const take = await coordinator.take({
      taskId,
      identityId: config.identity.id,
      mcpEndpoint: config.node.capsuleMcpEndpoint,
    });
    let exitCode = 1;
    let interactiveError: unknown;
    try {
      exitCode = await runner.runInteractive(launcher.interactiveCommand(take.interactive.launch));
    } catch (error) {
      interactiveError = error;
    } finally {
      await take.finish();
    }
    const checkpoint = await checkpointer.capture(taskId);
    if (interactiveError !== undefined) throw interactiveError;
    stdout.write(`Claude Code session closed; task ${taskId} is ${store.getTask(taskId)?.status}; capsule revision ${checkpoint.revision} is durable.\n`);
    return exitCode;
  } finally {
    await node.close();
  }
}

async function reviewManagedTask(
  args: readonly string[],
  options: CliOptions,
  stdout: Output,
): Promise<number> {
  const parsed = parseArguments(args, new Set(["url", "redmarker"]));
  if (parsed.positionals.length !== 1) {
    throw new CliUsageError("task review requires exactly one task id");
  }
  const targetUrl = option(parsed, "url");
  if (targetUrl === undefined) throw new CliUsageError("task review requires --url <loopback-url>");
  const taskId = parsed.positionals[0]!;
  const { paths, config, runner } = await managedContext(options);
  using store = new TaskStore(paths.database, {
    ...(options.clock === undefined ? {} : { clock: options.clock }),
    ...(options.createId === undefined ? {} : { createId: options.createId }),
  });
  const task = store.getTask(taskId);
  if (task === undefined) throw new Error(`Task ${taskId} does not exist`);
  if (task.status === "draft" || task.status === "queued") {
    throw new Error(`Task ${taskId} has no reviewable capsule yet; run it once before visual review`);
  }

  const launcher = new CapsuleProcessLauncher(config.runtime);
  const checkpointer = await prepareTaskCapsule({
    task,
    store,
    paths,
    config,
    runner,
    launcher,
    ...(options.clock === undefined ? {} : { clock: options.clock }),
  });
  const checkpoint = await new FilesystemCapsuleCheckpointStore(paths.capsules).readActive(taskId);
  if (checkpoint.manifest.taskId !== taskId) {
    throw new Error(`Active capsule for ${taskId} has a mismatched task binding`);
  }
  const checkpointDigest = capsuleManifestDigest(checkpoint.manifest);
  const submissions = new IntentSubmissionStore(store, {
    ...(options.clock === undefined ? {} : { clock: options.clock }),
  });
  const grant = submissions.issue({
    taskId,
    capsuleId: checkpoint.manifest.capsuleId,
    baseUrl: targetUrl,
    headSha: checkpoint.manifest.repository.headCommit,
    checkpointDigest,
    checkpointRevision: checkpoint.manifest.revision,
  });
  const grants = new TaskMcpGrantStore(store.database, options.clock);
  let node: NodeUse | undefined;
  try {
    node = await useTaskNode({
      config,
      store,
      grants,
      launcher,
      databasePath: paths.database,
      submissions,
    });
    const submissionUrl = new URL(
      `/v1/intent-tools/redmarker/${grant.sessionId}`,
      `http://127.0.0.1:${config.node.port}`,
    ).toString();
    const launch = new RedmarkerIntentTool(
      option(parsed, "redmarker")
        ?? options.env?.ALTITUDE_REDMARKER_BIN
        ?? process.env.ALTITUDE_REDMARKER_BIN
        ?? "redmarker",
    ).launch({
      taskId,
      capsuleId: checkpoint.manifest.capsuleId,
      targetUrl,
      submissionUrl,
      submissionToken: grant.token,
    });
    const exitCode = await runner.runInteractive({
      executable: launch.executable,
      args: launch.args,
      env: launch.environment,
    });
    const artifact = submissions.artifactForSession(grant.sessionId);
    if (artifact === undefined) {
      if (wantsJson(parsed)) {
        writeJson(stdout, {
          taskId,
          exitCode,
          submitted: false,
          message: "Redmarker exited without a managed artifact; its local copy fallback remains authoritative.",
        });
      } else {
        stdout.write(
          "No managed visual artifact was received; keep the Redmarker clipboard fallback if you made one.\n",
        );
      }
      return exitCode;
    }
    const durableCheckpoint = await checkpointer.capture(taskId);
    if (wantsJson(parsed)) {
      writeJson(stdout, {
        taskId,
        exitCode,
        submitted: true,
        artifact,
        checkpoint: durableCheckpoint,
      });
    } else {
      stdout.write(
        `Added Redmarker artifact ${artifact.artifactId} to task ${taskId}; capsule revision ${durableCheckpoint.revision} is durable.\n`,
      );
    }
    return exitCode;
  } finally {
    submissions.revoke(grant.sessionId);
    await node?.close();
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
    const scheduledRetries = store.listScheduledActions("scheduled")
      .filter((action) => action.kind === "agent.retry").length;
    return { total: tasks.length, blockingAttention, openUpdates, scheduledRetries, byStatus };
  });
  if (json) writeJson(stdout, summary);
  else {
    const active = summary.byStatus.running + summary.byStatus.taken + summary.byStatus.provisioning;
    stdout.write(`${summary.total} tasks · ${active} active · ${summary.blockingAttention} need attention · ${summary.openUpdates} updates · ${summary.scheduledRetries} retries queued\n`);
    const nonzero = TASK_STATUSES.filter((status) => summary.byStatus[status] > 0);
    if (nonzero.length > 0) stdout.write(`${nonzero.map((status) => `${status} ${summary.byStatus[status]}`).join(" · ")}\n`);
  }
  return 0;
}

function runtimeKind(value: string | undefined): RuntimeKind {
  const runtime = value ?? (process.platform === "darwin" ? "apple-container" : "docker");
  if (runtime !== "apple-container" && runtime !== "docker") {
    throw new CliUsageError(`Unknown capsule runtime: ${runtime}`);
  }
  return runtime;
}

function portNumber(value: string | undefined): number | undefined {
  if (value === undefined) return undefined;
  if (!/^[0-9]+$/u.test(value)) throw new CliUsageError("--port must be an integer");
  const port = Number(value);
  if (!Number.isSafeInteger(port) || port < 1 || port > 65_535) {
    throw new CliUsageError("--port must be between 1 and 65535");
  }
  return port;
}

async function waitForShutdownSignal(): Promise<void> {
  await new Promise<void>((resolve) => {
    const shutdown = () => {
      process.off("SIGINT", shutdown);
      process.off("SIGTERM", shutdown);
      resolve();
    };
    process.once("SIGINT", shutdown);
    process.once("SIGTERM", shutdown);
  });
}

async function runNodeCommand(
  args: readonly string[],
  options: CliOptions,
  stdout: Output,
): Promise<number> {
  const subcommand = args[0] ?? "help";
  if (["help", "--help", "-h"].includes(subcommand)) {
    stdout.write(NODE_HELP);
    return 0;
  }
  if (subcommand !== "serve") throw new CliUsageError(`Unknown node command: ${subcommand}`);
  const parsed = parseArguments(args.slice(1), new Set(["host", "port", "runtime"]), new Set());
  if (parsed.positionals.length !== 0) throw new CliUsageError("node serve does not take positional arguments");
  const paths = resolveAltitudePaths(options.env ?? process.env);
  await ensureAltitudePaths(paths);
  const config = await readLocalConfig(paths.config);
  const runtime = runtimeKind(option(parsed, "runtime") ?? config?.runtime);
  const port = portNumber(option(parsed, "port")) ?? config?.node.port;
  const hostname = option(parsed, "host") ?? config?.node.bindHost;
  using store = new TaskStore(paths.database, {
    ...(options.clock === undefined ? {} : { clock: options.clock }),
    ...(options.createId === undefined ? {} : { createId: options.createId }),
  });
  const grants = new TaskMcpGrantStore(store.database, options.clock);
  const launcher = new CapsuleProcessLauncher(runtime);
  const service = createAltitudeNodeService({
    runtime,
    store,
    grants,
    executor: new ProcessTaskToolExecutor(launcher),
    submissions: new IntentSubmissionStore(store, {
      ...(options.clock === undefined ? {} : { clock: options.clock }),
    }),
    instanceId: nodeInstanceId(paths.database),
    ...(config === undefined ? {} : {
      allowedHostnames: [...new Set([
        "127.0.0.1",
        "localhost",
        "[::1]",
        "host.docker.internal",
        "host.container.internal",
        new URL(config.node.capsuleMcpEndpoint).hostname,
      ])],
    }),
  });
  const server = serveAltitudeNode({
    service,
    ...(hostname === undefined ? {} : { hostname }),
    ...(port === undefined ? {} : { port }),
  });
  const workerCompatible = config !== undefined
    && runtime === config.runtime
    && port === config.node.port
    && hostname === config.node.bindHost;
  const retrySessions = new TaskAgentSessionStore(store.database, options.clock);
  const worker = !workerCompatible ? undefined : new NodeScheduledActionWorker(
    store,
    createManagedAgentRetryExecutor({
      tasks: store,
      session: (taskId) => retrySessions.get(taskId),
      run: async (taskId, model) => await runManagedTask(
        [taskId, ...(model === undefined ? [] : ["--model", model])],
        options,
        stdout,
      ),
    }),
    {
      ...(options.clock === undefined ? {} : { now: options.clock }),
      onError: (error) => (options.stderr ?? process.stderr).write(
        `Altitude retry worker: ${error instanceof Error ? error.message : String(error)}\n`,
      ),
    },
  );
  stdout.write(`Altitude node listening at ${server.url.toString()} (${runtime}${worker === undefined ? "" : ", retry worker active"})\n`);
  if (config !== undefined && !workerCompatible) {
    stdout.write("Automatic retries are inactive because node runtime/listener overrides differ from the saved setup.\n");
  }
  try {
    worker?.start();
    await waitForShutdownSignal();
  } finally {
    await worker?.close();
    await server.close();
  }
  return 0;
}

function setupConfig(
  parsed: ParsedArguments,
  existing: AltitudeLocalConfig | undefined,
): AltitudeLocalConfig {
  const runtime = runtimeKind(option(parsed, "runtime") ?? existing?.runtime);
  const base = existing?.runtime === runtime ? existing : defaultLocalConfig(runtime);
  const nodePort = portNumber(option(parsed, "node-port")) ?? base.node.port;
  const capsuleMcpEndpoint = option(parsed, "node-url")
    ?? (option(parsed, "node-port") === undefined
      ? base.node.capsuleMcpEndpoint
      : (() => {
          const url = new URL(base.node.capsuleMcpEndpoint);
          url.port = String(nodePort);
          return url.toString();
        })());
  return validateLocalConfig({
    ...base,
    runtime,
    identity: {
      id: option(parsed, "identity") ?? base.identity.id,
      image: option(parsed, "identity-image") ?? base.identity.image,
    },
    taskImage: option(parsed, "task-image") ?? base.taskImage,
    network: option(parsed, "network") ?? base.network,
    node: {
      bindHost: option(parsed, "node-host") ?? base.node.bindHost,
      port: nodePort,
      capsuleMcpEndpoint,
    },
  });
}

async function runSetupCommand(
  args: readonly string[],
  options: CliOptions,
  stdout: Output,
): Promise<number> {
  if (["help", "--help", "-h"].includes(args[0] ?? "")) {
    stdout.write(SETUP_HELP);
    return 0;
  }
  const parsed = parseArguments(
    args,
    new Set([
      "runtime",
      "identity",
      "source",
      "identity-image",
      "task-image",
      "network",
      "node-url",
      "node-host",
      "node-port",
    ]),
    new Set(["json", "skip-build"]),
  );
  if (parsed.positionals.length !== 0) throw new CliUsageError("setup does not take positional arguments");
  const paths = resolveAltitudePaths(options.env ?? process.env);
  await ensureAltitudePaths(paths);
  const config = setupConfig(parsed, await readLocalConfig(paths.config));
  const runner = options.commandRunner ?? new BunCommandRunner();
  const runtime = config.runtime === "apple-container"
    ? new AppleContainerRuntime(runner)
    : new DockerCapsuleRuntime(runner);
  const launcher = new CapsuleProcessLauncher(config.runtime);
  using store = new TaskStore(paths.database, {
    ...(options.clock === undefined ? {} : { clock: options.clock }),
    ...(options.createId === undefined ? {} : { createId: options.createId }),
  });
  const claude = new ClaudeCliAdapter({
    identity: {
      id: config.identity.id,
      configDirectory: "/identity/claude",
      loginDirectory: "/identity",
    },
    launcher,
    leases: new SqliteAgentWriterLeaseStore(store.database),
    ...(options.clock === undefined ? {} : { now: options.clock }),
    ...(options.createId === undefined ? {} : { createId: options.createId }),
  });
  const json = wantsJson(parsed);
  const result = await setupAltitude({
    config,
    sourceDirectory: resolve(option(parsed, "source") ?? process.cwd()),
    buildImages: parsed.options.get("skip-build") !== true,
    ...(json ? {} : { progress: (item) => stdout.write(`→ ${item.message}\n`) }),
  }, {
    runtimeProbe: runtime,
    images: new CapsuleImageBuilder(config.runtime, runner),
    identity: new IdentityCapsuleManager(config.runtime, runner),
    claude,
    interactiveCommand: (spec) => launcher.interactiveCommand(spec),
    runner,
  });
  await writeLocalConfig(paths.config, config);
  if (json) writeJson(stdout, { config, result });
  else {
    stdout.write(`Altitude is ready with Claude Code ${result.claude.version ?? "(version unknown)"}.\n`);
    if (config.runtime === "apple-container" && new URL(config.node.capsuleMcpEndpoint).hostname === "host.container.internal") {
      stdout.write("! Apple container host access still needs its explicit host.container.internal DNS rule before the first task run.\n");
    }
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
      case "setup":
        return await runSetupCommand(args.slice(1), options, stdout);
      case "task":
        return await runTaskCommand(args.slice(1), options, stdout);
      case "attention":
        return await runAttentionCommand(args.slice(1), options, stdout);
      case "node":
        return await runNodeCommand(args.slice(1), options, stdout);
      case "status": {
        const parsed = parseArguments(args.slice(1));
        if (parsed.positionals.length !== 0) throw new CliUsageError("status does not take positional arguments");
        return await runStatus(options, stdout, wantsJson(parsed));
      }
      case "_task-tool":
        return await runTaskToolStdio();
      case "_task-bootstrap":
        return await runTaskBootstrapStdio();
      case "_task-checkpoint":
        return await runTaskCheckpointStdio(args[1]);
      case "_task-init":
        return await runTaskInit();
      case "_identity-control":
        return await runIdentityControlStdio();
      case "_identity-init":
        return await runIdentityInit();
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
