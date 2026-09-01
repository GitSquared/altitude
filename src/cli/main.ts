import type { Writable } from "node:stream";
import { join, resolve } from "node:path";

import type { AttentionResolution, EventActor, NewEvent } from "../core/events.ts";
import { ClaudeCliAdapter } from "../agent/claude-cli.ts";
import type { AgentEvent } from "../agent/protocol.ts";
import {
  assertResumableClaudeTranscript,
  decodeClaudeTranscriptCapsuleMembers,
} from "../agent/transcript.ts";
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
  capsuleMcpEndpoint,
  defaultLocalConfig,
  readLocalConfig,
  validateLocalConfig,
  writeLocalConfig,
  type AltitudeLocalConfig,
} from "../config/local.ts";
import { runDoctor, renderDoctor } from "../doctor.ts";
import { createAltitudeNodeService, nodeInstanceId, serveAltitudeNode } from "../node/server.ts";
import {
  NodeOutboxTopicWorker,
  NodeScheduledActionKindWorker,
  AgentRetryWorker,
} from "../node/worker.ts";
import { RedmarkerIntentTool } from "../intent/redmarker.ts";
import { IntentSubmissionStore } from "../intent/submission.ts";
import { BunCommandRunner, type CommandRunner } from "../process/command.ts";
import { IdentityCapsuleManager } from "../runtime/identity-manager.ts";
import {
  MICROSANDBOX_HOST_IPV4,
  MicrosandboxCapsuleRuntime,
} from "../runtime/microsandbox.ts";
import {
  CapsuleProcessLauncher,
  runMicrosandboxAttach,
} from "../runtime/process-launcher.ts";
import {
  TaskCapsuleBootstrapProvisioner,
  TaskCapsuleCheckpointProvisioner,
  TaskCapsuleManager,
} from "../runtime/task-manager.ts";
import { setupAltitude } from "../setup.ts";
import { SqliteAgentWriterLeaseStore } from "../storage/agent-writer-lease-store.ts";
import { TaskAgentSessionStore } from "../storage/task-agent-session-store.ts";
import { TaskStore } from "../storage/task-store.ts";
import { LinearTrackerStateStore } from "../tracker/state.ts";
import {
  LINEAR_INPUT_ROUTE_TOPIC,
  parseLinearInputRoutePayload,
} from "../tracker/webhook.ts";
import {
  planLinearProjection,
} from "../tracker/projection.ts";
import {
  createLinearProjectionWorker,
  linearAccessToken,
} from "../tracker/worker.ts";
import { IdentityControlProvisioner, runIdentityControlStdio } from "../identity/control.ts";
import { runIdentityInit } from "../identity/init.ts";
import {
  IdentityTranscriptProvisioner,
  runIdentityTranscriptStdio,
} from "../identity/transcript.ts";
import { TaskMcpGrantStore } from "../tools/grants.ts";
import { ProcessTaskToolExecutor } from "../tools/process-executor.ts";
import { runTaskToolStdio } from "../tools/task-helper.ts";
import { runTaskBootstrapStdio } from "../capsule/bootstrap.ts";
import { runTaskCheckpointStdio } from "../capsule/task-checkpoint.ts";
import { runTaskInit } from "../capsule/init.ts";
import { createFactoryGitRepository } from "../capsule/factory-git.ts";
import { taskEnvironmentVariables } from "../capsule/task-environment.ts";
import { FilesystemCapsuleCheckpointStore } from "../capsule/filesystem.ts";
import { capsuleManifestDigest } from "../capsule/manifest.ts";
import {
  FilesystemCapsuleTransferReceiver,
  LocalToRemoteCapsuleTransfer,
  type PreparedCapsuleTransfer,
} from "../capsule/transfer-runtime.ts";
import {
  bindPersistentIrohIdentity,
  createIrohNodeClient,
  nodeTicketFor,
  parseAltitudeNodeTicket,
  serveIrohNode,
  type AltitudeNodeClient,
  type IrohNodeHandlers,
  type NodeConnection,
} from "../node/iroh.ts";
import {
  FilesystemNodeRegistry,
  registeredNode,
  type RegisteredNode,
} from "../node/registry.ts";
import { FilesystemRepositorySeedCache } from "../capsule/seed-cache.ts";
import { LocalCapsulePreparer } from "../run/local-capsule.ts";
import { LocalCapsuleCheckpointer } from "../run/local-checkpoint.ts";
import {
  PORTABLE_TASK_STATE_MEMBER,
  decodePortableTaskState,
  portableTaskStateMember,
} from "../run/task-state.ts";
import { createManagedAgentRetryExecutor } from "../run/retry-executor.ts";
import {
  planLocalResume,
  resumeLocalTasksOnOpen,
} from "../run/local-resume.ts";
import {
  TASK_DISPATCH_ACTION_KIND,
  parseTaskDispatchPayload,
} from "../run/dispatch.ts";
import { activateTransferredTask } from "../run/transfer-activation.ts";
import { TaskRunCoordinator } from "../run/task-runner.ts";
import { reconcileExitedInteractiveAgent } from "../run/interactive-terminal.ts";
import { VERSION } from "../version.ts";
import {
  runAltitudeTui,
  type AltitudeTuiActions,
} from "../tui/index.ts";
import { runTerminalSupervisor, TerminalSessionManager } from "../terminal/session.ts";

const HELP = `altitude ${VERSION}

An intent control plane for delegated agentic development.

Usage:
  altitude <command> [options]

Commands:
  setup                       Create the identity capsule and log in to Claude
  task create <intent>        Create a task at an explicit intention level
  task list                   List tasks
  task show <task-id>         Show task state and event history
  task queue <task-id>        Make a draft, stopped, or failed task runnable
  task run <task-id>          Run or resume a managed headless Claude turn
  task take <task-id>         Enter the same session in Claude Code's TUI
  task shell <task-id>        Set up tools and config inside the task capsule
  task review <task-id>       Add visual intent with managed Redmarker
  task send <task-id>         Move a paused capsule to a registered node
  attention answer <task-id>  Answer the active Decision
  attention accept <task-id>  Accept the active Review and complete the task
  attention revise <task-id>  Return a Review with revision instructions
  attention ack <task-id>     Acknowledge an Update by --id
  tracker linear bind         Connect a task capsule to a Linear issue
  tracker linear show         Show a task's Linear projection binding
  node add                    Remember a remote node connection
  node list                   List remembered remote nodes
  node check                  Verify a remote Altitude connection
  node serve                  Run the local task/MCP control endpoint
  config get                  Read non-secret Altitude configuration
  config set                  Change one explicit configuration value
  config path                 Print the configuration file path
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
  altitude task shell <task-id>
  altitude task review <task-id> --url <loopback-url> [--redmarker <executable>] [--json]
  altitude task send <task-id> --node <registered-id> [--json]

Intent levels:
  ${INTENT_LEVELS.join(", ")}

The first run starts in a clean Altitude-created Git capsule by default.
--repository explicitly imports that host worktree instead; Altitude never
infers or mounts the current directory.
`;

const ATTENTION_HELP = `Usage:
  altitude attention answer <task-id> <answer...> [--json]
  altitude attention accept <task-id> [note...] [--json]
  altitude attention revise <task-id> <instructions...> [--json]
  altitude attention ack <task-id> --id <attention-id> [note...] [--json]
`;

const TRACKER_HELP = `Usage:
  altitude tracker linear bind <task-id> --issue-id <uuid>
                               [--issue-identifier <key>]
                               [--agent-session-id <uuid>]
                               [--app-user-id <uuid>]
                               [--mention-token <token>] [--json]
  altitude tracker linear show <task-id> [--json]

The binding is node-local projection metadata; the task capsule remains the
operational source of truth. Set ALTITUDE_LINEAR_WEBHOOK_SECRET only in the
environment of the Altitude node process to enable the signed webhook endpoint;
set ALTITUDE_LINEAR_ACCESS_TOKEN there as well to publish additive issue history
and native Decision/Review prompts. Neither secret is written to capsule state.
`;

const NODE_HELP = `Usage:
  altitude node add <id> --ticket <altitude-node-ticket> [--json]
  altitude node list [--json]
  altitude node show <id> [--json]
  altitude node check <id> [--json]
  altitude node ticket [--json]
  altitude node serve [--host <address>] [--port <port>]

The listener defaults to loopback and remains reachable from its Microsandbox
capsules. Use --host deliberately when placing a webhook listener behind a
trusted reverse proxy.
With the saved listener settings, the node also resumes durable
rate-limit retries and starts tasks received from another node.
Altitude does not install or manage an operating-system service; run this command
under whichever process supervisor belongs to the node.
Node pairing and capsule transfer use Altitude's native Iroh route.
`;

const SETUP_HELP = `Usage:
  altitude setup [--identity <id>] [--identity-image <image>]
                 [--task-image <image>]
                 [--node-id <id>] [--node-host <address>] [--node-port <port>]
                 [--json]

Setup pulls the configured OCI image through Microsandbox, creates a private
identity capsule, and opens Claude Code login inside it when needed. It never
imports credentials, configuration, or source code from the host.
`;

const CONFIG_HELP = `Usage:
  altitude config get [key] [--json]
  altitude config set <key> <value> [--json]
  altitude config path

Keys:
  identity.id
  identity.image
  taskImage
  node.id
  node.bindHost
  node.port

This file contains only non-secret control-plane settings. Native logins and
credentials stay inside their node-local identity capsules.
`;

type Output = Pick<Writable, "write">;

export interface CliOptions {
  readonly env?: Readonly<Record<string, string | undefined>>;
  readonly stdout?: Output;
  readonly stderr?: Output;
  readonly clock?: () => number;
  readonly createId?: () => string;
  readonly commandRunner?: CommandRunner;
  readonly createNodeClient?: (
    connection: NodeConnection,
  ) => Promise<AltitudeNodeClient>;
  readonly startTui?: (actions: AltitudeTuiActions) => Promise<number>;
  readonly startTuiControlPlane?: () => Promise<Pick<NodeUse, "close">>;
}

type ParsedArguments = Readonly<{
  positionals: readonly string[];
  options: ReadonlyMap<string, string | true>;
}>;

class CliUsageError extends Error {}

const OPERATOR: EventActor = { kind: "operator" };

function linearWebhookSecret(options: CliOptions): string | undefined {
  const secret = (options.env ?? process.env).ALTITUDE_LINEAR_WEBHOOK_SECRET;
  return secret?.trim() ? secret : undefined;
}

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

const CONFIG_KEYS = [
  "identity.id",
  "identity.image",
  "taskImage",
  "node.id",
  "node.bindHost",
  "node.port",
] as const;

type ConfigKey = (typeof CONFIG_KEYS)[number];

function isConfigKey(value: string): value is ConfigKey {
  return (CONFIG_KEYS as readonly string[]).includes(value);
}

function configValue(config: AltitudeLocalConfig, key: ConfigKey): string | number {
  switch (key) {
    case "identity.id": return config.identity.id;
    case "identity.image": return config.identity.image;
    case "taskImage": return config.taskImage;
    case "node.id": return config.node.id;
    case "node.bindHost": return config.node.bindHost;
    case "node.port": return config.node.port;
  }
}

function withConfigValue(
  config: AltitudeLocalConfig,
  key: ConfigKey,
  rawValue: string,
): AltitudeLocalConfig {
  const value: string | number = key === "node.port"
    ? (() => {
        if (!/^\d+$/u.test(rawValue)) throw new CliUsageError("node.port must be an integer");
        return Number(rawValue);
      })()
    : rawValue;
  switch (key) {
    case "identity.id": return validateLocalConfig({ ...config, identity: { ...config.identity, id: value } });
    case "identity.image": return validateLocalConfig({ ...config, identity: { ...config.identity, image: value } });
    case "taskImage": return validateLocalConfig({ ...config, taskImage: value });
    case "node.id": return validateLocalConfig({ ...config, node: { ...config.node, id: value } });
    case "node.bindHost": return validateLocalConfig({ ...config, node: { ...config.node, bindHost: value } });
    case "node.port": return validateLocalConfig({ ...config, node: { ...config.node, port: value } });
  }
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
    case "shell":
      return await openTaskShell(args.slice(1), options, stdout);
    case "review":
      return (await reviewManagedTask(args.slice(1), options, stdout)).exitCode;
    case "send":
      return await sendManagedTask(args.slice(1), options, stdout);
    default:
      throw new CliUsageError(`Unknown task command: ${subcommand}`);
  }
}

type NodeUse = Readonly<{ external: boolean; close(): Promise<void> }>;

async function requireSettledWorkbench(
  paths: ReturnType<typeof resolveAltitudePaths>,
  taskId: string,
): Promise<void> {
  const status = await new TerminalSessionManager(paths.terminals).status(taskId);
  if (status === undefined) return;
  if (status.state === "running" || status.state === "starting") {
    throw new Error(`Task ${taskId} has a live workbench; reopen it with \`altitude task shell ${taskId}\` and exit before continuing`);
  }
  throw new Error(`Task ${taskId} has an uncheckpointed workbench; run \`altitude task shell ${taskId}\` once to finalize it`);
}

async function requireSettledAgentTerminal(
  paths: ReturnType<typeof resolveAltitudePaths>,
  taskId: string,
): Promise<void> {
  const status = await new TerminalSessionManager(paths.terminals).status(taskId, "agent");
  if (status === undefined) return;
  if (status.state === "running" || status.state === "starting") {
    throw new Error(`Task ${taskId} has a live Claude terminal; open the task and leave Claude before continuing`);
  }
  throw new Error(`Task ${taskId} has a finished Claude terminal awaiting reconciliation; open the task once to checkpoint it`);
}

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
  readonly linearWebhookSecret?: string;
  readonly linearAccessToken?: string;
}): Promise<NodeUse> {
  const instanceId = nodeInstanceId(input.databasePath);
  if (await existingNode(input.config.node.port, instanceId)) {
    return { external: true, close: async () => undefined };
  }
  const service = createAltitudeNodeService({
    store: input.store,
    grants: input.grants,
    executor: new ProcessTaskToolExecutor(input.launcher),
    ...(input.submissions === undefined ? {} : { submissions: input.submissions }),
    ...(input.linearWebhookSecret === undefined
      ? {}
      : { linearWebhookSecret: input.linearWebhookSecret }),
    instanceId,
    allowedHostnames: [...new Set([
      "127.0.0.1",
      "localhost",
      "[::1]",
      MICROSANDBOX_HOST_IPV4,
    ])],
  });
  const server = serveAltitudeNode({
    service,
    hostname: input.config.node.bindHost,
    port: input.config.node.port,
  });
  const projectionWorker = createLinearProjectionWorker({
    store: input.store,
    ...(input.linearAccessToken === undefined ? {} : { accessToken: input.linearAccessToken }),
  });
  projectionWorker?.start();
  return {
    external: false,
    close: async () => {
      await projectionWorker?.close();
      await server.close();
    },
  };
}

async function useTuiControlPlane(input: {
  readonly paths: ReturnType<typeof resolveAltitudePaths>;
  readonly config: AltitudeLocalConfig;
  readonly options: CliOptions;
  readonly stdout: Output;
}): Promise<NodeUse> {
  const instanceId = nodeInstanceId(input.paths.database);
  if (await existingNode(input.config.node.port, instanceId)) {
    return { external: true, close: async () => undefined };
  }
  const store = new TaskStore(input.paths.database, {
    ...(input.options.clock === undefined ? {} : { clock: input.options.clock }),
    ...(input.options.createId === undefined ? {} : { createId: input.options.createId }),
  });
  const launcher = new CapsuleProcessLauncher();
  const grants = new TaskMcpGrantStore(store.database, input.options.clock);
  let server: ReturnType<typeof serveAltitudeNode> | undefined;
  try {
    const service = createAltitudeNodeService({
      store,
      grants,
      executor: new ProcessTaskToolExecutor(launcher),
      submissions: new IntentSubmissionStore(store, {
        ...(input.options.clock === undefined ? {} : { clock: input.options.clock }),
      }),
      instanceId,
      ...(linearWebhookSecret(input.options) === undefined
        ? {}
        : { linearWebhookSecret: linearWebhookSecret(input.options)! }),
      allowedHostnames: [...new Set([
        "127.0.0.1",
        "localhost",
        "[::1]",
        MICROSANDBOX_HOST_IPV4,
      ])],
    });
    server = serveAltitudeNode({
      service,
      hostname: input.config.node.bindHost,
      port: input.config.node.port,
    });
  } catch (error) {
    store.close();
    throw error;
  }

  const retrySessions = new TaskAgentSessionStore(store.database, input.options.clock);
  const onError = (component: string) => (error: unknown) => (
    input.options.stderr ?? process.stderr
  ).write(`Altitude ${component}: ${error instanceof Error ? error.message : String(error)}\n`);
  const retryWorker = new AgentRetryWorker(
    store,
    createManagedAgentRetryExecutor({
      tasks: store,
      session: (taskId) => retrySessions.get(taskId),
      run: async (taskId, model) => await runManagedTask(
        [taskId, ...(model === undefined ? [] : ["--model", model])],
        input.options,
        input.stdout,
      ),
    }),
    {
      ...(input.options.clock === undefined ? {} : { now: input.options.clock }),
      onError: onError("retry worker"),
    },
  );
  const dispatchWorker = new NodeScheduledActionKindWorker(store, {
    kind: TASK_DISPATCH_ACTION_KIND,
    parse: (value) => {
      const payload = parseTaskDispatchPayload(value);
      if (payload.targetNodeId !== input.config.node.id) {
        throw new Error(
          `Task dispatch targets node ${payload.targetNodeId}, not ${input.config.node.id}`,
        );
      }
      return payload;
    },
    execute: async (taskId, payload) => {
      if (store.getTask(taskId)?.status !== "queued") return "skipped";
      const active = await new FilesystemCapsuleCheckpointStore(input.paths.capsules).readActive(taskId);
      if (active.manifest.ownership.ownerNodeId !== input.config.node.id) return "skipped";
      if (active.manifest.revision !== payload.checkpointRevision
        || capsuleManifestDigest(active.manifest) !== payload.checkpointDigest) {
        throw new Error(`Task ${taskId} active capsule changed before remote dispatch`);
      }
      await runManagedTask([taskId], input.options, input.stdout);
      if (store.getTask(taskId)?.status === "queued") {
        throw new Error(`Dispatched task ${taskId} did not leave the queue`);
      }
      return "executed";
    },
  }, {
    ...(input.options.clock === undefined ? {} : { now: input.options.clock }),
    onError: onError("dispatch worker"),
  });
  const trackerInputWorker = new NodeOutboxTopicWorker(store, {
    topic: LINEAR_INPUT_ROUTE_TOPIC,
    parse: parseLinearInputRoutePayload,
    execute: async (taskId, payload) => {
      const task = store.getTask(taskId);
      if (task === undefined) throw new Error(`Task ${taskId} does not exist`);
      const continued = await continueAfterOperatorInput({
        task,
        ...(payload.resolution === undefined ? {} : { resolution: payload.resolution }),
        options: input.options,
        stdout: input.stdout,
      });
      if (continued.exitCode !== 0 && continued.exitCode !== 75) {
        throw new Error(`Claude exited with status ${continued.exitCode} after Linear input was recorded`);
      }
      return "executed";
    },
  }, {
    ...(input.options.clock === undefined ? {} : { now: input.options.clock }),
    onError: onError("Linear input worker"),
  });
  const projectionWorker = createLinearProjectionWorker({
    store,
    ...(linearAccessToken(input.options.env ?? process.env) === undefined
      ? {}
      : { accessToken: linearAccessToken(input.options.env ?? process.env)! }),
    ...(input.options.clock === undefined ? {} : { now: input.options.clock }),
    onError: onError("Linear projection worker"),
  });
  try {
    retryWorker.start();
    dispatchWorker.start();
    trackerInputWorker.start();
    projectionWorker?.start();
  } catch (error) {
    await Promise.allSettled([
      retryWorker.close(),
      dispatchWorker.close(),
      trackerInputWorker.close(),
      projectionWorker?.close(),
      server.close(),
    ]);
    store.close();
    throw error;
  }

  let closed = false;
  return {
    external: false,
    close: async () => {
      if (closed) return;
      closed = true;
      try {
        await Promise.all([
          retryWorker.close(),
          dispatchWorker.close(),
          trackerInputWorker.close(),
          projectionWorker?.close(),
          server.close(),
        ]);
      } finally {
        store.close();
      }
    },
  };
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

async function nodeClientFor(
  paths: ReturnType<typeof resolveAltitudePaths>,
  connection: NodeConnection,
  options: CliOptions,
): Promise<AltitudeNodeClient> {
  if (options.createNodeClient !== undefined) {
    return await options.createNodeClient(connection);
  }
  const identity = await bindPersistentIrohIdentity(join(paths.nodes, "identity"));
  return await createIrohNodeClient(
    identity.endpoint,
    connection,
    async () => await identity.endpoint.close(),
  );
}

async function ensureManagedIdentity(
  config: AltitudeLocalConfig,
  launcher: CapsuleProcessLauncher,
  store: TaskStore,
  options: CliOptions,
): Promise<ClaudeCliAdapter> {
  await new IdentityCapsuleManager(new MicrosandboxCapsuleRuntime()).ensure({
    identityId: config.identity.id,
    image: config.identity.image,
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
  readonly launcher: CapsuleProcessLauncher;
  readonly agentSessions?: TaskAgentSessionStore;
  readonly transcripts?: IdentityTranscriptProvisioner;
  readonly clock?: () => number;
}): Promise<LocalCapsuleCheckpointer> {
  const needsInitialRepository = input.task.status === "queued";
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
    input.agentSessions,
    input.transcripts,
  );
  const preparer = new LocalCapsulePreparer(
    checkpoints,
    seeds,
    new TaskCapsuleManager(new MicrosandboxCapsuleRuntime()),
    new TaskCapsuleBootstrapProvisioner(input.launcher),
    checkpointer,
  );
  const factory = needsInitialRepository && input.repositoryPath === undefined
    ? await createFactoryGitRepository({
        parentDirectory: join(input.paths.cache, "factory-repositories"),
        taskId: input.task.id,
      })
    : undefined;
  try {
    await preparer.prepare({
      taskId: input.task.id,
      taskImage: input.config.taskImage,
      nodeId: input.config.node.id,
      ...(input.repositoryPath === undefined && factory === undefined
        ? {}
        : { repositoryPath: factory?.path ?? resolve(input.repositoryPath!) }),
      additionalMembers: [portableTaskStateMember(
        input.store,
        input.task.id,
        input.agentSessions,
      )],
    });
  } finally {
    await factory?.cleanup();
  }
  if (input.agentSessions !== undefined && input.transcripts !== undefined) {
    const session = input.agentSessions.get(input.task.id);
    if (session !== undefined && session.runCount > 0) {
      const checkpoint = await checkpoints.readActive(input.task.id);
      const imported = await input.transcripts.import({
        taskId: session.taskId,
        sessionId: session.sessionId,
        projectDirectoryName: session.projectDirectoryName,
      }, checkpoint.members);
      if (imported === "absent") {
        throw new Error(
          `Task ${input.task.id} has a resumable Claude session but its capsule has no portable transcript`,
        );
      }
    }
  }
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
  const { paths, config } = await managedContext(options);
  await requireSettledWorkbench(paths, taskId);
  await requireSettledAgentTerminal(paths, taskId);
  using store = new TaskStore(paths.database, {
    ...(options.clock === undefined ? {} : { clock: options.clock }),
    ...(options.createId === undefined ? {} : { createId: options.createId }),
  });
  const task = store.getTask(taskId);
  if (task === undefined) throw new Error(`Task ${taskId} does not exist`);
  const launcher = new CapsuleProcessLauncher();
  const agent = await ensureManagedIdentity(config, launcher, store, options);
  const agentSessions = new TaskAgentSessionStore(store.database, options.clock);
  const transcripts = new IdentityTranscriptProvisioner(
    launcher,
    config.identity.id,
  );
  const checkpointer = await prepareTaskCapsule({
    task,
    store,
    ...(option(parsed, "repository") === undefined ? {} : { repositoryPath: option(parsed, "repository")! }),
    paths,
    config,
    launcher,
    agentSessions,
    transcripts,
    ...(options.clock === undefined ? {} : { clock: options.clock }),
  });
  const grants = new TaskMcpGrantStore(store.database, options.clock);
  const node = await useTaskNode({
    config,
    store,
    grants,
    launcher,
    databasePath: paths.database,
    ...(linearWebhookSecret(options) === undefined
      ? {}
      : { linearWebhookSecret: linearWebhookSecret(options)! }),
    ...(linearAccessToken(options.env ?? process.env) === undefined
      ? {}
      : { linearAccessToken: linearAccessToken(options.env ?? process.env)! }),
  });
  try {
    const coordinator = new TaskRunCoordinator(
      store,
      agentSessions,
      grants,
      new IdentityControlProvisioner(launcher),
      agent,
      options.clock ?? Date.now,
    );
    const managed = await coordinator.start({
      taskId,
      identityId: config.identity.id,
      mcpEndpoint: capsuleMcpEndpoint(config),
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

async function reconcileAgentTerminal(input: {
  readonly taskId: string;
  readonly status: Awaited<ReturnType<TerminalSessionManager["status"]>>;
  readonly paths: ReturnType<typeof resolveAltitudePaths>;
  readonly config: AltitudeLocalConfig;
  readonly runner: CommandRunner;
  readonly store: TaskStore;
  readonly launcher: CapsuleProcessLauncher;
  readonly sessions: TaskAgentSessionStore;
  readonly grants: TaskMcpGrantStore;
  readonly options: CliOptions;
}): Promise<{ readonly revision: number; readonly exitCode: number }> {
  if (input.status === undefined || input.status.state === "running" || input.status.state === "starting") {
    throw new Error("A live Claude terminal cannot be reconciled");
  }
  const leases = new SqliteAgentWriterLeaseStore(input.store.database);
  await reconcileExitedInteractiveAgent({
    taskId: input.taskId,
    status: input.status,
    tasks: input.store,
    sessions: input.sessions,
    leases,
    grants: input.grants,
  });
  const transcripts = new IdentityTranscriptProvisioner(input.launcher, input.config.identity.id);
  const checkpointer = await prepareTaskCapsule({
    task: input.store.getTask(input.taskId)!,
    store: input.store,
    paths: input.paths,
    config: input.config,
    launcher: input.launcher,
    agentSessions: input.sessions,
    transcripts,
    ...(input.options.clock === undefined ? {} : { clock: input.options.clock }),
  });
  const checkpoint = await checkpointer.capture(input.taskId);
  await new TerminalSessionManager(input.paths.terminals).finalize(input.taskId, "agent");
  return {
    revision: checkpoint.revision,
    exitCode: input.status.state === "exited" ? input.status.exitCode : 1,
  };
}

async function takeManagedTask(
  args: readonly string[],
  options: CliOptions,
  stdout: Output,
  behavior: Readonly<{ detachable?: boolean; attach?: boolean }> = {},
): Promise<number> {
  const parsed = parseArguments(args, new Set(), new Set());
  if (parsed.positionals.length !== 1) throw new CliUsageError("task take requires exactly one task id");
  const taskId = parsed.positionals[0]!;
  const { paths, config, runner } = await managedContext(options);
  await requireSettledWorkbench(paths, taskId);
  using store = new TaskStore(paths.database, {
    ...(options.clock === undefined ? {} : { clock: options.clock }),
    ...(options.createId === undefined ? {} : { createId: options.createId }),
  });
  const task = store.getTask(taskId);
  if (task === undefined) throw new Error(`Task ${taskId} does not exist`);
  const launcher = new CapsuleProcessLauncher();
  const agentSessions = new TaskAgentSessionStore(store.database, options.clock);
  const grants = new TaskMcpGrantStore(store.database, options.clock);
  const terminals = new TerminalSessionManager(paths.terminals);
  let terminalStatus = await terminals.status(taskId, "agent");
  if (terminalStatus?.state === "starting") throw new Error("Claude terminal is still starting; try opening the task again");
  if (terminalStatus !== undefined && terminalStatus.state !== "running") {
    const reconciled = await reconcileAgentTerminal({
      taskId,
      status: terminalStatus,
      paths,
      config,
      runner,
      store,
      launcher,
      sessions: agentSessions,
      grants,
      options,
    });
    stdout.write(`Claude terminal reconciled; capsule revision ${reconciled.revision} is durable.\n`);
    return reconciled.exitCode;
  }
  const node = await useTaskNode({
    config,
    store,
    grants,
    launcher,
    databasePath: paths.database,
    ...(linearWebhookSecret(options) === undefined
      ? {}
      : { linearWebhookSecret: linearWebhookSecret(options)! }),
    ...(linearAccessToken(options.env ?? process.env) === undefined
      ? {}
      : { linearAccessToken: linearAccessToken(options.env ?? process.env)! }),
  });
  try {
    if (terminalStatus === undefined) {
      const agent = await ensureManagedIdentity(config, launcher, store, options);
      const transcripts = new IdentityTranscriptProvisioner(launcher, config.identity.id);
      await prepareTaskCapsule({
        task,
        store,
        paths,
        config,
        launcher,
        agentSessions,
        transcripts,
        ...(options.clock === undefined ? {} : { clock: options.clock }),
      });
      const coordinator = new TaskRunCoordinator(
        store,
        agentSessions,
        grants,
        new IdentityControlProvisioner(launcher),
        agent,
        options.clock ?? Date.now,
      );
      const take = await coordinator.take({
        taskId,
        identityId: config.identity.id,
        mcpEndpoint: capsuleMcpEndpoint(config),
      });
      try {
        terminalStatus = await terminals.start({
          taskId,
          kind: "agent",
          command: launcher.interactiveCommand(take.interactive.launch),
        });
      } catch (error) {
        await take.finish();
        throw error;
      }
      if (terminalStatus.state === "failed") {
        await terminals.finalize(taskId, "agent");
        await take.finish();
        throw new Error(`Claude terminal failed: ${terminalStatus.error}`);
      }
      if (terminalStatus.state === "starting") throw new Error("Claude terminal is still starting");
      take.started();
    }
    if (terminalStatus.state === "exited") {
      const reconciled = await reconcileAgentTerminal({
        taskId, status: terminalStatus, paths, config, runner, store, launcher,
        sessions: agentSessions, grants, options,
      });
      stdout.write(`Claude Code session closed; capsule revision ${reconciled.revision} is durable.\n`);
      return reconciled.exitCode;
    }
    if (behavior.attach === false) return 0;
    const canDetach = behavior.detachable === true && node.external;
    if (canDetach) stdout.write("Press Ctrl-G, then d to return to Altitude without stopping Claude.\n");
    const attached = await terminals.attach(
      taskId,
      process.stdin,
      process.stdout,
      "agent",
      canDetach,
    );
    if (attached.outcome === "detached") {
      stdout.write(`Claude is still working on ${taskId}; open the task again to reattach.\n`);
      return 0;
    }
    const reconciled = await reconcileAgentTerminal({
      taskId, status: attached.status, paths, config, runner, store, launcher,
      sessions: agentSessions, grants, options,
    });
    stdout.write(`Claude Code session closed; task ${taskId} is ${store.getTask(taskId)?.status}; capsule revision ${reconciled.revision} is durable.\n`);
    return reconciled.exitCode;
  } finally {
    await node.close();
  }
}

async function openTaskShell(args: readonly string[], options: CliOptions, stdout: Output): Promise<number> {
  const parsed = parseArguments(args);
  if (parsed.positionals.length !== 1) throw new CliUsageError("task shell requires exactly one task id");
  const taskId = parsed.positionals[0]!;
  const { paths, config } = await managedContext(options);
  await requireSettledAgentTerminal(paths, taskId);
  using store = new TaskStore(paths.database, {
    ...(options.clock === undefined ? {} : { clock: options.clock }),
    ...(options.createId === undefined ? {} : { createId: options.createId }),
  });
  const task = store.getTask(taskId);
  if (task === undefined) throw new Error(`Task ${taskId} does not exist`);
  const launcher = new CapsuleProcessLauncher();
  const agentSessions = new TaskAgentSessionStore(store.database, options.clock);
  const checkpointer = await prepareTaskCapsule({
    task,
    store,
    paths,
    config,
    launcher,
    agentSessions,
    ...(options.clock === undefined ? {} : { clock: options.clock }),
  });
  stdout.write([
    `Entering the portable workbench for ${taskId}.`,
    "Tools and non-secret config saved under $HOME will travel with this task.",
    "Run `gh auth login` here when this task needs GitHub; that login stays on this node.",
    "Other credentials and signing identities also belong in the node-local area, never portable state.",
    "Exit the shell to checkpoint the workbench.",
    "",
  ].join("\n"));
  const command = launcher.interactiveCommand({
    target: { kind: "task-capsule", taskId },
    executable: "/bin/sh",
    args: ["-i"],
    cwd: "/workspace",
    env: taskEnvironmentVariables(),
    io: "interactive",
    purpose: "Set up portable tools and configuration in this task capsule",
  });
  const terminals = new TerminalSessionManager(paths.terminals);
  let status = await terminals.status(taskId);
  if (status === undefined) status = await terminals.start({ taskId, command });
  if (status.state === "starting") throw new Error("Task workbench is still starting");
  if (status.state === "failed") {
    await terminals.finalize(taskId);
    throw new Error(`Task workbench failed: ${status.error}`);
  }
  if (status.state === "exited") {
    const checkpoint = await checkpointer.capture(taskId);
    await terminals.finalize(taskId);
    stdout.write(`Workbench checkpointed in capsule revision ${checkpoint.revision}.\n`);
    return status.exitCode;
  }
  stdout.write("Press Ctrl-G, then d to return to Altitude without stopping this workbench.\n");
  const attached = await terminals.attach(taskId);
  if (attached.outcome === "detached") {
    stdout.write(`Workbench for ${taskId} is still running; open it again with [s] or \`altitude task shell ${taskId}\`.\n`);
    return 0;
  }
  const checkpoint = await checkpointer.capture(taskId);
  await terminals.finalize(taskId);
  stdout.write(`Workbench checkpointed in capsule revision ${checkpoint.revision}.\n`);
  return attached.status.state === "exited" ? attached.status.exitCode : 1;
}

async function reviewManagedTask(
  args: readonly string[],
  options: CliOptions,
  stdout: Output,
): Promise<Readonly<{ exitCode: number; submitted: boolean }>> {
  const parsed = parseArguments(args, new Set(["url", "redmarker"]));
  if (parsed.positionals.length !== 1) {
    throw new CliUsageError("task review requires exactly one task id");
  }
  const targetUrl = option(parsed, "url");
  if (targetUrl === undefined) throw new CliUsageError("task review requires --url <loopback-url>");
  const taskId = parsed.positionals[0]!;
  const { paths, config, runner } = await managedContext(options);
  await requireSettledWorkbench(paths, taskId);
  await requireSettledAgentTerminal(paths, taskId);
  using store = new TaskStore(paths.database, {
    ...(options.clock === undefined ? {} : { clock: options.clock }),
    ...(options.createId === undefined ? {} : { createId: options.createId }),
  });
  const task = store.getTask(taskId);
  if (task === undefined) throw new Error(`Task ${taskId} does not exist`);
  if (task.status === "draft" || task.status === "queued") {
    throw new Error(`Task ${taskId} has no reviewable capsule yet; run it once before visual review`);
  }

  const launcher = new CapsuleProcessLauncher();
  const agentSessions = new TaskAgentSessionStore(store.database, options.clock);
  const checkpointer = await prepareTaskCapsule({
    task,
    store,
    paths,
    config,
    launcher,
    agentSessions,
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
      ...(linearWebhookSecret(options) === undefined
        ? {}
        : { linearWebhookSecret: linearWebhookSecret(options)! }),
      ...(linearAccessToken(options.env ?? process.env) === undefined
        ? {}
        : { linearAccessToken: linearAccessToken(options.env ?? process.env)! }),
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
      return { exitCode, submitted: false };
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
    return { exitCode, submitted: true };
  } finally {
    submissions.revoke(grant.sessionId);
    await node?.close();
  }
}

async function sendManagedTask(
  args: readonly string[],
  options: CliOptions,
  stdout: Output,
): Promise<number> {
  const parsed = parseArguments(args, new Set(["node"]));
  if (parsed.positionals.length !== 1) {
    throw new CliUsageError("task send requires exactly one task id");
  }
  const required = (name: string): string => {
    const value = option(parsed, name);
    if (value === undefined) throw new CliUsageError("task send requires --" + name);
    return value;
  };
  const targetNodeId = required("node");
  const { paths, config } = await managedContext(options);
  const taskId = parsed.positionals[0]!;
  await requireSettledWorkbench(paths, taskId);
  await requireSettledAgentTerminal(paths, taskId);
  const remembered = await new FilesystemNodeRegistry(paths.nodes).get(
    targetNodeId,
  );
  if (remembered === undefined) {
    throw new Error(
      `Node ${targetNodeId} is not registered. Run \`altitude node add ${targetNodeId} --ticket <ticket>\` first.`,
    );
  }
  using store = new TaskStore(paths.database, {
    ...(options.clock === undefined ? {} : { clock: options.clock }),
    ...(options.createId === undefined ? {} : { createId: options.createId }),
  });
  const task = store.getTask(taskId);
  if (task === undefined) throw new Error("Task " + taskId + " does not exist");
  if (task.status !== "paused") {
    throw new Error(
      "Task " + taskId + " must be paused before it can move to another node",
    );
  }
  const checkpoints = new FilesystemCapsuleCheckpointStore(paths.capsules);
  const activeCheckpoint = await checkpoints.readActive(taskId);
  const agentSessions = new TaskAgentSessionStore(store.database, options.clock);
  const agentSession = agentSessions.get(taskId);
  const transcript = decodeClaudeTranscriptCapsuleMembers(
    activeCheckpoint.members,
  );
  if (agentSession !== undefined && agentSession.runCount > 0) {
    if (transcript === undefined
      || transcript.taskId !== agentSession.taskId
      || transcript.sessionId !== agentSession.sessionId
      || transcript.projectDirectoryName !== agentSession.projectDirectoryName) {
      throw new Error(
        `Task ${taskId} cannot move until its Claude transcript is durably checkpointed`,
      );
    }
    assertResumableClaudeTranscript(transcript);
  } else if (transcript !== undefined) {
    throw new Error(
      `Task ${taskId} has transcript bytes without a resumable agent session`,
    );
  }
  store.append({
    taskId,
    type: "task.state_changed",
    payload: {
      to: "transferring",
      reason: "Preparing capsule transfer to node " + targetNodeId,
    },
    actor: OPERATOR,
  });
  const seeds = new FilesystemRepositorySeedCache(
    join(paths.cache, "repository-seeds"),
  );
  const nodeClient = await nodeClientFor(paths, remembered.connection, options);
  try {
    const coordinator = new LocalToRemoteCapsuleTransfer(
      checkpoints,
      seeds,
      config.node.id,
      nodeClient,
      {
        ...(options.clock === undefined ? {} : { now: options.clock }),
        ...(options.createId === undefined ? {} : { createId: options.createId }),
      },
    );
    let prepared: PreparedCapsuleTransfer;
    try {
      prepared = await coordinator.prepare({
        taskId,
        targetNodeId,
        additionalMembers: [portableTaskStateMember(
          store,
          taskId,
          agentSessions,
        )],
      });
    } catch (error) {
      if (store.getTask(taskId)?.status === "transferring") {
        store.append({
          taskId,
          type: "task.state_changed",
          payload: { to: "paused", reason: "Capsule transfer preparation failed" },
          actor: { kind: "system", component: "capsule-transfer" },
        });
      }
      throw error;
    }
    const committed = await coordinator.commit(prepared);
    if (wantsJson(parsed)) {
      writeJson(stdout, { task: store.getTask(taskId), transfer: committed });
    } else {
      stdout.write(
        "Moved task " + taskId + " to " + targetNodeId
          + " at capsule revision " + committed.targetCheckpoint.revision + ".\n",
      );
    }
  } finally {
    await nodeClient.close();
  }
  return 0;
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

async function persistAttentionResolution(
  options: CliOptions,
  taskId: string,
  attentionId: string,
  resolution: AttentionResolution,
): Promise<Task> {
  return await withStore(options, (store) => {
    const event = {
      taskId,
      type: "attention.resolved",
      payload: { attentionId, resolution },
      actor: OPERATOR,
    } satisfies NewEvent<"attention.resolved">;
    return store.append(event, planLinearProjection(store, event)).task;
  });
}

async function continueAfterOperatorInput(input: {
  readonly task: Task;
  readonly resolution?: AttentionResolution;
  readonly options: CliOptions;
  readonly stdout: Output;
}): Promise<{ readonly exitCode: number; readonly notice: string }> {
  const paths = resolveAltitudePaths(input.options.env ?? process.env);
  const terminals = new TerminalSessionManager(paths.terminals);
  let terminal = await terminals.status(input.task.id, "agent");
  const accepted = input.resolution?.kind === "accepted";

  if (accepted) {
    if (terminal?.state === "running") {
      terminal = await terminals.terminate(input.task.id, "agent");
    }
    if (terminal !== undefined && terminal.state !== "starting") {
      await takeManagedTask([input.task.id], input.options, input.stdout);
    }
    return { exitCode: 0, notice: "Review accepted. The task is complete and its capsule is durable." };
  }

  if (terminal?.state === "running") {
    await terminals.writeInput(
      input.task.id,
      "Altitude recorded new operator input in the durable task ledger. Call get_task now, read the latest operator input or resolved attention, and continue from that intent.\r",
      "agent",
    );
    return { exitCode: 0, notice: "Response recorded and routed into the live Claude session." };
  }
  if (terminal?.state === "starting") {
    return { exitCode: 0, notice: "Response recorded. Claude is still starting; reopen the task to continue." };
  }
  if (terminal !== undefined) {
    await takeManagedTask([input.task.id], input.options, input.stdout);
  }

  let current = await withStore(input.options, (store) => store.getTask(input.task.id));
  if (current?.status === "taken") {
    current = await withStore(input.options, (store) => store.append({
      taskId: input.task.id,
      type: "task.state_changed",
      payload: { to: "paused", reason: "Native Claude session ended before routed input arrived" },
      actor: { kind: "system", component: "attention-router" },
    }).task);
  }
  if (current?.status !== "running" && current?.status !== "paused") {
    return {
      exitCode: 0,
      notice: `Response recorded; task is ${current?.status ?? "unavailable"}.`,
    };
  }
  if (await readLocalConfig(paths.config) === undefined) {
    return {
      exitCode: 0,
      notice: "Response recorded. Set up Altitude before resuming Claude.",
    };
  }
  const exitCode = await runManagedTask([input.task.id], input.options, input.stdout);
  const resumed = await withStore(input.options, (store) => store.getTask(input.task.id));
  return {
    exitCode,
    notice: `Response recorded; Claude resumed and the task is ${resumed?.status ?? "unavailable"}.`,
  };
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

  const resolved = await withStore(options, (store) => {
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
    const event = {
      taskId,
      type: "attention.resolved",
      payload: { attentionId, resolution },
      actor: OPERATOR,
    } satisfies NewEvent<"attention.resolved">;
    const task = store.append(event, planLinearProjection(store, event)).task;
    return { task, resolution };
  });
  const json = wantsJson(parsed);
  if (!json) stdout.write(`Resolved ${subcommand} for ${resolved.task.id}; task is ${resolved.task.status}.\n`);
  if (subcommand === "ack") {
    if (json) writeJson(stdout, resolved.task);
    return 0;
  }
  const continuationOutput: Output = json ? { write: () => true } : stdout;
  const continued = await continueAfterOperatorInput({
    task: resolved.task,
    resolution: resolved.resolution,
    options,
    stdout: continuationOutput,
  });
  if (json) {
    writeJson(stdout, {
      task: await withStore(options, (store) => store.getTask(resolved.task.id)),
      continuation: continued,
    });
  } else {
    stdout.write(`${continued.notice}\n`);
  }
  return continued.exitCode;
}

async function runTrackerCommand(
  args: readonly string[],
  options: CliOptions,
  stdout: Output,
): Promise<number> {
  const provider = args[0] ?? "help";
  if (["help", "--help", "-h"].includes(provider)) {
    stdout.write(TRACKER_HELP);
    return 0;
  }
  if (provider !== "linear") throw new CliUsageError(`Unknown tracker provider: ${provider}`);
  const subcommand = args[1] ?? "help";
  if (["help", "--help", "-h"].includes(subcommand)) {
    stdout.write(TRACKER_HELP);
    return 0;
  }
  if (subcommand !== "bind" && subcommand !== "show") {
    throw new CliUsageError(`Unknown Linear tracker command: ${subcommand}`);
  }
  const parsed = parseArguments(
    args.slice(2),
    subcommand === "bind"
      ? new Set([
          "issue-id",
          "issue-identifier",
          "agent-session-id",
          "app-user-id",
          "mention-token",
        ])
      : new Set(),
  );
  if (parsed.positionals.length !== 1) {
    throw new CliUsageError(`tracker linear ${subcommand} requires exactly one task id`);
  }
  const taskId = parsed.positionals[0]!;
  const binding = await withStore(options, (store) => {
    if (store.getTask(taskId) === undefined) throw new Error(`Task ${taskId} does not exist`);
    const state = new LinearTrackerStateStore(store.database, options.clock);
    if (subcommand === "show") {
      const existing = state.getByTaskId(taskId);
      if (existing === undefined) throw new Error(`Task ${taskId} has no Linear binding`);
      return existing;
    }
    const issueId = option(parsed, "issue-id");
    if (issueId === undefined) throw new CliUsageError("tracker linear bind requires --issue-id <uuid>");
    return state.bind({
      taskId,
      issueId,
      ...(option(parsed, "issue-identifier") === undefined
        ? {}
        : { issueIdentifier: option(parsed, "issue-identifier")! }),
      ...(option(parsed, "agent-session-id") === undefined
        ? {}
        : { agentSessionId: option(parsed, "agent-session-id")! }),
      ...(option(parsed, "app-user-id") === undefined
        ? {}
        : { appUserId: option(parsed, "app-user-id")! }),
      ...(option(parsed, "mention-token") === undefined
        ? {}
        : { mentionTokens: [option(parsed, "mention-token")!] }),
    });
  });
  if (wantsJson(parsed)) writeJson(stdout, binding);
  else if (subcommand === "bind") {
    stdout.write(
      `Bound task ${binding.taskId} to Linear ${binding.issueIdentifier ?? binding.issueId}.\n`
        + "Run the node with ALTITUDE_LINEAR_WEBHOOK_SECRET set to accept signed replies and mentions.\n",
    );
  } else {
    stdout.write(
      `Task ${binding.taskId} projects to Linear ${binding.issueIdentifier ?? binding.issueId}`
        + (binding.agentSessionId === undefined ? ".\n" : ` via Agent Session ${binding.agentSessionId}.\n`),
    );
  }
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

function irohTransferHandlers(
  config: AltitudeLocalConfig,
  paths: ReturnType<typeof resolveAltitudePaths>,
  store: TaskStore,
  options: CliOptions,
): IrohNodeHandlers {
  const checkpoints = new FilesystemCapsuleCheckpointStore(paths.capsules);
  const receiver = new FilesystemCapsuleTransferReceiver(
    checkpoints,
    new FilesystemRepositorySeedCache(join(paths.cache, "repository-seeds")),
    config.node.id,
  );
  return {
    async health() {
      return { nodeId: config.node.id, version: VERSION };
    },
    async prepare(payload) {
      return await receiver.prepare(payload);
    },
    async activate(input) {
      const activation = await receiver.activate(input);
      const checkpoint = await checkpoints.read(activation.checkpoint);
      const stateMember = checkpoint.members.find(
        (member) => member.path === PORTABLE_TASK_STATE_MEMBER,
      );
      if (stateMember === undefined) {
        throw new Error("Transferred capsule has no portable task state");
      }
      activateTransferredTask({
        store,
        sessions: new TaskAgentSessionStore(store.database, options.clock),
        portableState: decodePortableTaskState(stateMember.bytes),
        targetIdentityId: config.identity.id,
        targetNodeId: config.node.id,
        transferId: activation.transferId,
        checkpoint: activation.checkpoint,
        activatedAt: (options.clock ?? Date.now)(),
      });
      return activation;
    },
  };
}

function registeredNodeRoute(record: RegisteredNode): string {
  const ticket = parseAltitudeNodeTicket(record.connection.ticket);
  return `iroh:${ticket.endpointId.slice(0, 12)}`;
}

async function runNodeRegistryCommand(
  subcommand: "add" | "list" | "show" | "check",
  args: readonly string[],
  options: CliOptions,
  stdout: Output,
): Promise<number> {
  const paths = resolveAltitudePaths(options.env ?? process.env);
  await ensureAltitudePaths(paths);
  const registry = new FilesystemNodeRegistry(paths.nodes);
  if (subcommand === "list") {
    const parsed = parseArguments(args);
    if (parsed.positionals.length !== 0) {
      throw new CliUsageError("node list does not take positional arguments");
    }
    const nodes = await registry.list();
    if (wantsJson(parsed)) writeJson(stdout, nodes);
    else if (nodes.length === 0) stdout.write("No remote nodes registered.\n");
    else {
      for (const node of nodes) {
        stdout.write(
          `${node.connection.nodeId}  ${registeredNodeRoute(node)}\n`,
        );
      }
    }
    return 0;
  }

  if (subcommand === "show") {
    const parsed = parseArguments(args);
    if (parsed.positionals.length !== 1) {
      throw new CliUsageError("node show requires exactly one node id");
    }
    const id = parsed.positionals[0]!;
    const node = await registry.get(id);
    if (node === undefined) throw new Error(`Node ${id} is not registered`);
    if (wantsJson(parsed)) writeJson(stdout, node);
    else {
      stdout.write(`${node.connection.nodeId}\n`);
      stdout.write(`Route: ${registeredNodeRoute(node)}\n`);
      stdout.write(`Pairing ticket: ${node.connection.ticket}\n`);
    }
    return 0;
  }

  if (subcommand === "check") {
    const parsed = parseArguments(args);
    if (parsed.positionals.length !== 1) {
      throw new CliUsageError("node check requires exactly one node id");
    }
    const id = parsed.positionals[0]!;
    const node = await registry.get(id);
    if (node === undefined) throw new Error(`Node ${id} is not registered`);
    const client = await nodeClientFor(paths, node.connection, options);
    try {
      const health = await client.health();
      if (wantsJson(parsed)) writeJson(stdout, health);
      else stdout.write(`${id} is reachable with Altitude ${health.version}.\n`);
    } finally {
      await client.close();
    }
    return 0;
  }

  const parsed = parseArguments(args, new Set(["ticket"]));
  if (parsed.positionals.length !== 1) {
    throw new CliUsageError("node add requires exactly one node id");
  }
  const id = parsed.positionals[0]!;
  const ticket = option(parsed, "ticket");
  if (ticket === undefined) throw new CliUsageError("node add requires --ticket");
  const node = await registry.put(registeredNode({
    nodeId: id,
    ticket,
  }));
  if (wantsJson(parsed)) writeJson(stdout, node);
  else {
    stdout.write(
      `Registered node ${node.connection.nodeId} at ${registeredNodeRoute(node)}.\n`,
    );
  }
  return 0;
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
  if (subcommand === "add" || subcommand === "list"
    || subcommand === "show" || subcommand === "check") {
    return await runNodeRegistryCommand(subcommand, args.slice(1), options, stdout);
  }
  if (subcommand === "ticket") {
    const parsed = parseArguments(args.slice(1));
    if (parsed.positionals.length !== 0) throw new CliUsageError("node ticket does not take positional arguments");
    const paths = resolveAltitudePaths(options.env ?? process.env);
    await ensureAltitudePaths(paths);
    const config = await readLocalConfig(paths.config);
    if (config === undefined) throw new Error("Altitude is not set up yet. Run `altitude setup` first.");
    const identity = await bindPersistentIrohIdentity(join(paths.nodes, "identity"));
    try {
      const ticket = await nodeTicketFor(config.node.id, identity);
      if (wantsJson(parsed)) writeJson(stdout, { nodeId: config.node.id, ticket });
      else stdout.write(`${ticket}\n`);
    } finally {
      await identity.endpoint.close();
    }
    return 0;
  }
  if (subcommand !== "serve") throw new CliUsageError(`Unknown node command: ${subcommand}`);
  const parsed = parseArguments(args.slice(1), new Set(["host", "port"]), new Set());
  if (parsed.positionals.length !== 0) throw new CliUsageError("node serve does not take positional arguments");
  const paths = resolveAltitudePaths(options.env ?? process.env);
  await ensureAltitudePaths(paths);
  const config = await readLocalConfig(paths.config);
  if (config === undefined) throw new Error("Altitude is not set up yet. Run `altitude setup` first.");
  const port = portNumber(option(parsed, "port")) ?? config?.node.port;
  const hostname = option(parsed, "host") ?? config?.node.bindHost;
  using store = new TaskStore(paths.database, {
    ...(options.clock === undefined ? {} : { clock: options.clock }),
    ...(options.createId === undefined ? {} : { createId: options.createId }),
  });
  const grants = new TaskMcpGrantStore(store.database, options.clock);
  const launcher = new CapsuleProcessLauncher();
  const service = createAltitudeNodeService({
    store,
    grants,
    executor: new ProcessTaskToolExecutor(launcher),
    submissions: new IntentSubmissionStore(store, {
      ...(options.clock === undefined ? {} : { clock: options.clock }),
    }),
    instanceId: nodeInstanceId(paths.database),
    ...(linearWebhookSecret(options) === undefined
      ? {}
      : { linearWebhookSecret: linearWebhookSecret(options)! }),
    ...(config === undefined ? {} : {
      allowedHostnames: [...new Set([
        "127.0.0.1",
        "localhost",
        "[::1]",
        MICROSANDBOX_HOST_IPV4,
      ])],
    }),
  });
  const server = serveAltitudeNode({
    service,
    ...(hostname === undefined ? {} : { hostname }),
    ...(port === undefined ? {} : { port }),
  });
  const irohIdentity = await bindPersistentIrohIdentity(join(paths.nodes, "identity"));
  let irohNode;
  try {
    irohNode = await serveIrohNode({
      nodeId: config.node.id,
      identity: irohIdentity,
      handlers: irohTransferHandlers(config, paths, store, options),
    });
  } catch (error) {
    await irohIdentity.endpoint.close().catch(() => undefined);
    await server.close();
    throw error;
  }
  const workersEnabled = port === config.node.port
    && hostname === config.node.bindHost;
  const retrySessions = new TaskAgentSessionStore(store.database, options.clock);
  const retryWorker = !workersEnabled ? undefined : new AgentRetryWorker(
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
  const dispatchWorker = !workersEnabled
    ? undefined
    : new NodeScheduledActionKindWorker(store, {
        kind: TASK_DISPATCH_ACTION_KIND,
        parse: (value) => {
          const payload = parseTaskDispatchPayload(value);
          if (payload.targetNodeId !== config.node.id) {
            throw new Error(
              `Task dispatch targets node ${payload.targetNodeId}, not ${config.node.id}`,
            );
          }
          return payload;
        },
        execute: async (taskId, payload) => {
          if (store.getTask(taskId)?.status !== "queued") return "skipped";
          const active = await new FilesystemCapsuleCheckpointStore(
            paths.capsules,
          ).readActive(taskId);
          if (active.manifest.ownership.ownerNodeId !== config.node.id) {
            return "skipped";
          }
          if (active.manifest.revision !== payload.checkpointRevision
            || capsuleManifestDigest(active.manifest) !== payload.checkpointDigest) {
            throw new Error(
              `Task ${taskId} active capsule changed before remote dispatch`,
            );
          }
          await runManagedTask([taskId], options, stdout);
          if (store.getTask(taskId)?.status === "queued") {
            throw new Error(`Remote task ${taskId} did not leave the queue`);
          }
          return "executed";
        },
      }, {
        ...(options.clock === undefined ? {} : { now: options.clock }),
        onError: (error) => (options.stderr ?? process.stderr).write(
          `Altitude dispatch worker: ${error instanceof Error ? error.message : String(error)}\n`,
        ),
      });
  const trackerInputWorker = !workersEnabled
    ? undefined
    : new NodeOutboxTopicWorker(store, {
        topic: LINEAR_INPUT_ROUTE_TOPIC,
        parse: parseLinearInputRoutePayload,
        execute: async (taskId, payload) => {
          const task = store.getTask(taskId);
          if (task === undefined) throw new Error(`Task ${taskId} does not exist`);
          const continued = await continueAfterOperatorInput({
            task,
            ...(payload.resolution === undefined ? {} : { resolution: payload.resolution }),
            options,
            stdout,
          });
          if (continued.exitCode !== 0 && continued.exitCode !== 75) {
            throw new Error(
              `Claude exited with status ${continued.exitCode} after Linear input was recorded`,
            );
          }
          return "executed";
        },
      }, {
        ...(options.clock === undefined ? {} : { now: options.clock }),
        onError: (error) => (options.stderr ?? process.stderr).write(
          `Altitude Linear input worker: ${error instanceof Error ? error.message : String(error)}\n`,
        ),
      });
  const projectionWorker = !workersEnabled
    ? undefined
    : createLinearProjectionWorker({
        store,
        ...(linearAccessToken(options.env ?? process.env) === undefined
          ? {}
          : { accessToken: linearAccessToken(options.env ?? process.env)! }),
        onError: (error) => (options.stderr ?? process.stderr).write(
          `Altitude Linear projection worker: ${error instanceof Error ? error.message : String(error)}\n`,
        ),
      });
  stdout.write(
    `Altitude node listening at ${server.url.toString()} (microsandbox`
      + (retryWorker === undefined ? "" : ", retry, dispatch, and tracker-input workers active")
      + (projectionWorker === undefined ? "" : ", Linear projection active")
      + ")\n",
  );
  stdout.write(`Iroh route active for ${config.node.id}; run \`altitude node ticket\` to pair it.\n`);
  if (!workersEnabled) {
    stdout.write("Automatic retries are inactive because listener overrides differ from the saved setup.\n");
  }
  try {
    retryWorker?.start();
    dispatchWorker?.start();
    trackerInputWorker?.start();
    projectionWorker?.start();
    await waitForShutdownSignal();
  } finally {
    await Promise.all([
      retryWorker?.close(),
      dispatchWorker?.close(),
      trackerInputWorker?.close(),
      projectionWorker?.close(),
      irohNode.close(),
    ]);
    await server.close();
  }
  return 0;
}

function setupConfig(
  parsed: ParsedArguments,
  existing: AltitudeLocalConfig | undefined,
): AltitudeLocalConfig {
  const base = existing ?? defaultLocalConfig();
  const nodePort = portNumber(option(parsed, "node-port")) ?? base.node.port;
  return validateLocalConfig({
    ...base,
    identity: {
      id: option(parsed, "identity") ?? base.identity.id,
      image: option(parsed, "identity-image") ?? base.identity.image,
    },
    taskImage: option(parsed, "task-image") ?? base.taskImage,
    node: {
      id: option(parsed, "node-id") ?? base.node.id,
      bindHost: option(parsed, "node-host") ?? base.node.bindHost,
      port: nodePort,
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
      "identity",
      "identity-image",
      "task-image",
      "node-id",
      "node-host",
      "node-port",
    ]),
    new Set(["json"]),
  );
  if (parsed.positionals.length !== 0) throw new CliUsageError("setup does not take positional arguments");
  const paths = resolveAltitudePaths(options.env ?? process.env);
  await ensureAltitudePaths(paths);
  const config = setupConfig(parsed, await readLocalConfig(paths.config));
  const runner = options.commandRunner ?? new BunCommandRunner();
  const runtime = new MicrosandboxCapsuleRuntime();
  const launcher = new CapsuleProcessLauncher();
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
    ...(json ? {} : { progress: (item) => stdout.write(`→ ${item.message}\n`) }),
  }, {
    runtimeProbe: runtime,
    identity: new IdentityCapsuleManager(runtime),
    claude,
    interactiveCommand: (spec) => launcher.interactiveCommand(spec),
    runner,
  });
  await writeLocalConfig(paths.config, config);
  if (json) writeJson(stdout, { config, result });
  else {
    stdout.write(`Altitude is ready with Claude Code ${result.claude.version ?? "(version unknown)"}.\n`);
  }
  return 0;
}

async function makeTaskRunnable(taskId: string, options: CliOptions): Promise<void> {
  await withStore(options, (store) => {
    const task = store.getTask(taskId);
    if (task === undefined) throw new Error(`Task ${taskId} does not exist`);
    if (task.status === "draft" || task.status === "stopped" || task.status === "failed") {
      store.append({
        taskId,
        type: "task.state_changed",
        payload: { to: "queued", reason: "Queued from the Altitude factory overview" },
        actor: OPERATOR,
      });
      return;
    }
    if (task.status === "completed" || task.status === "archived") {
      throw new Error(`Task ${taskId} is ${task.status} and cannot be run`);
    }
  });
}

async function runConfigCommand(
  args: readonly string[],
  options: CliOptions,
  stdout: Output,
): Promise<number> {
  const subcommand = args[0] ?? "help";
  if (["help", "--help", "-h"].includes(subcommand)) {
    stdout.write(CONFIG_HELP);
    return 0;
  }
  const paths = resolveAltitudePaths(options.env ?? process.env);
  if (subcommand === "path") {
    const parsed = parseArguments(args.slice(1), new Set(), new Set());
    if (parsed.positionals.length !== 0) throw new CliUsageError("config path does not take arguments");
    stdout.write(`${paths.config}\n`);
    return 0;
  }

  const config = await readLocalConfig(paths.config);
  if (config === undefined) {
    throw new Error("Altitude has no local configuration yet. Run `altitude` to enter the factory.");
  }
  if (subcommand === "get") {
    const parsed = parseArguments(args.slice(1));
    if (parsed.positionals.length > 1) throw new CliUsageError("config get accepts at most one key");
    const key = parsed.positionals[0];
    if (key !== undefined && !isConfigKey(key)) throw new CliUsageError(`Unknown config key: ${key}`);
    const value = key === undefined ? config : configValue(config, key);
    if (wantsJson(parsed)) writeJson(stdout, value);
    else if (key === undefined) stdout.write(`${JSON.stringify(config, null, 2)}\n`);
    else stdout.write(`${key} = ${String(value)}\n`);
    return 0;
  }
  if (subcommand === "set") {
    const parsed = parseArguments(args.slice(1));
    if (parsed.positionals.length !== 2) throw new CliUsageError("config set requires a key and value");
    const [key, value] = parsed.positionals as [string, string];
    if (!isConfigKey(key)) throw new CliUsageError(`Unknown config key: ${key}`);
    const updated = withConfigValue(config, key, value);
    await writeLocalConfig(paths.config, updated);
    const stored = configValue(updated, key);
    if (wantsJson(parsed)) writeJson(stdout, { key, value: stored });
    else stdout.write(`Set ${key} = ${String(stored)}\n`);
    return 0;
  }
  throw new CliUsageError(`Unknown config command: ${subcommand}`);
}

async function resumeTasksInterruptedByTui(
  options: CliOptions,
  stdout: Output,
): Promise<{ readonly resumed: number; readonly deferred: number }> {
  const paths = resolveAltitudePaths(options.env ?? process.env);
  using store = new TaskStore(paths.database, {
    ...(options.clock === undefined ? {} : { clock: options.clock }),
    ...(options.createId === undefined ? {} : { createId: options.createId }),
  });
  const now = options.clock?.() ?? Date.now();
  const sessions = new TaskAgentSessionStore(store.database, options.clock);
  return await resumeLocalTasksOnOpen({
    store,
    sessions,
    now,
    workerId: `tui-open:${crypto.randomUUID()}`,
    start: async (taskId) => await takeManagedTask(
      [taskId],
      options,
      stdout,
      { detachable: true, attach: false },
    ),
    onError: (taskId, error) => {
      (options.stderr ?? process.stderr).write(
        `Altitude deferred automatic resume for ${taskId}: ${error instanceof Error ? error.message : String(error)}\n`,
      );
    },
  });
}

async function rememberTuiInterruptedTask(
  taskId: string,
  options: CliOptions,
): Promise<boolean> {
  return await withStore(options, (store) => {
    if (store.getTask(taskId)?.status !== "paused") return false;
    const session = new TaskAgentSessionStore(store.database, options.clock).get(taskId);
    if (session === undefined || session.runCount < 1) return false;
    store.scheduleAction(planLocalResume(session, options.clock?.() ?? Date.now()));
    return true;
  });
}

async function runTuiCommand(options: CliOptions, stdout: Output): Promise<number> {
  if (options.startTui === undefined
    && (process.stdin.isTTY !== true || process.stdout.isTTY !== true)) {
    stdout.write(`Altitude's factory overview requires an interactive terminal.\n\n${HELP}`);
    return 2;
  }
  const paths = resolveAltitudePaths(options.env ?? process.env);
  let controlPlane: NodeUse | undefined;
  const ensureControlPlane = async () => {
    if (controlPlane !== undefined) return;
    const config = await readLocalConfig(paths.config);
    if (config === undefined) throw new Error("Altitude's local factory is not configured");
    const started = options.startTuiControlPlane === undefined
      ? await useTuiControlPlane({ paths, config, options, stdout })
      : await options.startTuiControlPlane();
    controlPlane = {
      external: options.startTuiControlPlane !== undefined,
      close: () => started.close(),
    };
  };
  try {
    const actions: AltitudeTuiActions = {
      configured: async () => await readLocalConfig(paths.config) !== undefined,
      configureClaude: async () => {
        const exitCode = await runSetupCommand([], options, stdout);
        if (exitCode !== 0) throw new Error(`Claude capsule setup exited with status ${exitCode}`);
      },
      resumeInterruptedTasks: async () => {
        await ensureControlPlane();
        return await resumeTasksInterruptedByTui(options, stdout);
      },
      listTasks: async () => {
        await ensureControlPlane();
        return await withStore(options, (store) => store.listTasks());
      },
      createTask: async (intent) => {
        await ensureControlPlane();
        return await withStore(options, (store) => {
          const taskId = options.createId?.() ?? crypto.randomUUID();
          store.append({
            taskId,
            type: "task.created",
            payload: {
              title: generatedTitle(intent),
              intent,
              intentLevel: "principles",
            },
            actor: OPERATOR,
          });
          return store.append({
            taskId,
            type: "task.state_changed",
            payload: { to: "queued", reason: "Created from the Altitude factory overview" },
            actor: OPERATOR,
          }).task;
        });
      },
      runTask: async (taskId) => {
        await ensureControlPlane();
        await makeTaskRunnable(taskId, options);
        const exitCode = await runManagedTask([taskId], options, stdout);
        if (exitCode !== 0 && exitCode !== 75) {
          throw new Error(`Managed task exited with status ${exitCode}`);
        }
      },
      takeTask: async (taskId) => {
        await ensureControlPlane();
        const exitCode = await takeManagedTask([taskId], options, stdout, { detachable: true });
        if (exitCode !== 0) throw new Error(`Claude Code exited with status ${exitCode}`);
        const status = await new TerminalSessionManager(paths.terminals).status(taskId, "agent");
        return status?.state === "running"
          ? "Claude is still working. Press enter to reattach; keep Altitude open, or set up a remote node for unattended work."
          : undefined;
      },
      openShell: async (taskId) => {
        await ensureControlPlane();
        await makeTaskRunnable(taskId, options);
        const exitCode = await openTaskShell([taskId], options, stdout);
        if (exitCode !== 0) throw new Error(`Task shell exited with status ${exitCode}`);
        const status = await new TerminalSessionManager(paths.terminals).status(taskId);
        return status?.state === "running"
          ? "Workbench is still running. Press s to reattach; exit it before running or sending this task."
          : "Portable workbench checkpointed.";
      },
      reviewTask: async (taskId, targetUrl) => {
        await ensureControlPlane();
        const result = await reviewManagedTask([taskId, "--url", targetUrl], options, stdout);
        if (result.exitCode !== 0) throw new Error(`Redmarker exited with status ${result.exitCode}`);
        return result.submitted
          ? "Visual intent added to the portable task capsule."
          : "Redmarker closed without submitting visual intent.";
      },
      resolveAttention: async (taskId, attentionId, resolution) => {
        await ensureControlPlane();
        const task = await persistAttentionResolution(options, taskId, attentionId, resolution);
        if (resolution.kind === "acknowledged") return "Update acknowledged.";
        const continued = await continueAfterOperatorInput({
          task,
          resolution,
          options,
          stdout,
        });
        if (continued.exitCode !== 0) {
          throw new Error(`Claude exited with status ${continued.exitCode} after the response was recorded`);
        }
        return continued.notice;
      },
    };
    return await (options.startTui ?? runAltitudeTui)(actions);
  } finally {
    try {
      if (controlPlane !== undefined) {
        const terminals = new TerminalSessionManager(paths.terminals);
        const tasks = await withStore(options, (store) => store.listTasks());
        for (const task of tasks) {
          const status = await terminals.status(task.id, "agent");
          if (status === undefined) continue;
          if (status.state === "running") await terminals.terminate(task.id, "agent");
          const exitCode = await takeManagedTask([task.id], options, stdout);
          if (exitCode !== 0 && exitCode !== 143) {
            (options.stderr ?? process.stderr).write(`Altitude could not cleanly pause ${task.id}; Claude exited ${exitCode}.\n`);
            continue;
          }
          await rememberTuiInterruptedTask(task.id, options);
        }
      }
    } finally {
      await controlPlane?.close();
    }
  }
}

export async function runCli(args: readonly string[], options: CliOptions = {}): Promise<number> {
  const stdout = options.stdout ?? process.stdout;
  const stderr = options.stderr ?? process.stderr;
  const command = args[0];

  try {
    if (command === undefined) return await runTuiCommand(options, stdout);
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
      case "config":
        return await runConfigCommand(args.slice(1), options, stdout);
      case "task":
        return await runTaskCommand(args.slice(1), options, stdout);
      case "attention":
        return await runAttentionCommand(args.slice(1), options, stdout);
      case "tracker":
        return await runTrackerCommand(args.slice(1), options, stdout);
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
      case "_identity-transcript":
        return await runIdentityTranscriptStdio(args[1]);
      case "_terminal-supervisor":
        return await runTerminalSupervisor(args[1]);
      case "_microsandbox-attach":
        return await runMicrosandboxAttach(args[1]);
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
