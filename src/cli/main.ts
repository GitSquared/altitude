import type { Writable } from "node:stream";
import { join, resolve } from "node:path";

import type { AttentionResolution, EventActor } from "../core/events.ts";
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
  defaultLocalConfig,
  readLocalConfig,
  validateLocalConfig,
  writeLocalConfig,
  type AltitudeLocalConfig,
} from "../config/local.ts";
import { runDoctor, renderDoctor } from "../doctor.ts";
import { createAltitudeNodeService, nodeInstanceId, serveAltitudeNode } from "../node/server.ts";
import {
  NodeScheduledActionKindWorker,
  NodeScheduledActionWorker,
} from "../node/worker.ts";
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
  MAX_CAPSULE_TRANSFER_PACKAGE_BYTES,
  type PreparedCapsuleTransfer,
} from "../capsule/transfer-runtime.ts";
import {
  SshCapsuleTransferTransport,
  buildRemoteCommandSpec,
  type NodeConnection,
} from "../node/transport.ts";
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
  TASK_DISPATCH_ACTION_KIND,
  parseTaskDispatchPayload,
} from "../run/dispatch.ts";
import { activateTransferredTask } from "../run/transfer-activation.ts";
import { TaskRunCoordinator } from "../run/task-runner.ts";
import { VERSION } from "../version.ts";
import {
  isAltitudeSourceCheckout,
  prepareStandaloneImageContext,
} from "../runtime/standalone-images.ts";
import { runAltitudeTui, ProcessTuiTerminal, type TuiTerminal } from "../tui/index.ts";
import { runTerminalSupervisor, TerminalSessionManager } from "../terminal/session.ts";

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
  task shell <task-id>        Set up tools and config inside the task capsule
  task review <task-id>       Add visual intent with managed Redmarker
  task send <task-id>         Move a paused capsule to a registered node
  attention answer <task-id>  Answer the active Decision
  attention accept <task-id>  Accept the active Review and complete the task
  attention revise <task-id>  Return a Review with revision instructions
  attention ack <task-id>     Acknowledge an Update by --id
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
  altitude task send <task-id> --node <id> --user <user>
                     --identity-file <path> --known-hosts-file <path>
                     (--host <host> [--ssh-port <port>] | --pigeons-endpoint <id>)
                     [--remote-altitude <executable>] [--json]

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

const NODE_HELP = `Usage:
  altitude node add <id> --user <user> --identity-file <absolute-path>
                    --known-hosts-file <absolute-path>
                    (--host <host> [--ssh-port <port>] | --pigeons-endpoint <id>)
                    [--pigeons <executable>] [--remote-altitude <executable>]
                    [--json]
  altitude node list [--json]
  altitude node show <id> [--json]
  altitude node check <id> [--accept-new-host-key] [--json]
  altitude node serve [--host <address>] [--port <port>] [--runtime <runtime>]

Runtimes:
  apple-container, docker

The listener defaults to loopback. Use --host deliberately when capsules on a
native Linux node need to reach the service through the host gateway.
With the saved runtime and listener settings, the node also resumes durable
rate-limit retries and starts tasks received from another node.
Altitude does not install or manage an operating-system service; run this command
under whichever process supervisor belongs to the node.
`;

const SETUP_HELP = `Usage:
  altitude setup [--runtime <runtime>] [--identity <id>] [--source <path>]
                 [--identity-image <image>] [--task-image <image>]
                 [--network <name>] [--node-url <url>]
                 [--node-id <id>] [--node-host <address>] [--node-port <port>]
                 [--skip-build] [--json]

Setup builds the two OCI images, creates a source-free identity capsule, and
opens the official Claude Code login there when authentication is needed.
On Linux, an installed Altitude binary builds its capsules from itself; --source
selects an Altitude checkout explicitly (and is currently required elsewhere).
`;

const CONFIG_HELP = `Usage:
  altitude config get [key] [--json]
  altitude config set <key> <value> [--json]
  altitude config path

Keys:
  runtime
  identity.id
  identity.image
  taskImage
  network
  node.id
  node.bindHost
  node.port
  node.capsuleMcpEndpoint

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
  readonly terminal?: TuiTerminal;
  readonly startTuiControlPlane?: () => Promise<Pick<NodeUse, "close">>;
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

const CONFIG_KEYS = [
  "runtime",
  "identity.id",
  "identity.image",
  "taskImage",
  "network",
  "node.id",
  "node.bindHost",
  "node.port",
  "node.capsuleMcpEndpoint",
] as const;

type ConfigKey = (typeof CONFIG_KEYS)[number];

function isConfigKey(value: string): value is ConfigKey {
  return (CONFIG_KEYS as readonly string[]).includes(value);
}

function configValue(config: AltitudeLocalConfig, key: ConfigKey): string | number {
  switch (key) {
    case "runtime": return config.runtime;
    case "identity.id": return config.identity.id;
    case "identity.image": return config.identity.image;
    case "taskImage": return config.taskImage;
    case "network": return config.network;
    case "node.id": return config.node.id;
    case "node.bindHost": return config.node.bindHost;
    case "node.port": return config.node.port;
    case "node.capsuleMcpEndpoint": return config.node.capsuleMcpEndpoint;
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
    case "runtime": return validateLocalConfig({ ...config, runtime: value });
    case "identity.id": return validateLocalConfig({ ...config, identity: { ...config.identity, id: value } });
    case "identity.image": return validateLocalConfig({ ...config, identity: { ...config.identity, image: value } });
    case "taskImage": return validateLocalConfig({ ...config, taskImage: value });
    case "network": return validateLocalConfig({ ...config, network: value });
    case "node.id": return validateLocalConfig({ ...config, node: { ...config.node, id: value } });
    case "node.bindHost": return validateLocalConfig({ ...config, node: { ...config.node, bindHost: value } });
    case "node.port": return validateLocalConfig({ ...config, node: { ...config.node, port: value } });
    case "node.capsuleMcpEndpoint": return validateLocalConfig({
      ...config,
      node: { ...config.node, capsuleMcpEndpoint: value },
    });
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
      return await reviewManagedTask(args.slice(1), options, stdout);
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
  const launcher = new CapsuleProcessLauncher(input.config.runtime);
  const grants = new TaskMcpGrantStore(store.database, input.options.clock);
  let server: ReturnType<typeof serveAltitudeNode> | undefined;
  try {
    const service = createAltitudeNodeService({
      runtime: input.config.runtime,
      store,
      grants,
      executor: new ProcessTaskToolExecutor(launcher),
      submissions: new IntentSubmissionStore(store, {
        ...(input.options.clock === undefined ? {} : { clock: input.options.clock }),
      }),
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
  const retryWorker = new NodeScheduledActionWorker(
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
  try {
    retryWorker.start();
    dispatchWorker.start();
  } catch (error) {
    await Promise.allSettled([retryWorker.close(), dispatchWorker.close(), server.close()]);
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
        await Promise.all([retryWorker.close(), dispatchWorker.close(), server.close()]);
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
    new TaskCapsuleManager(input.config.runtime, input.runner),
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
      network: input.config.network,
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
  const { paths, config, runner } = await managedContext(options);
  await requireSettledWorkbench(paths, taskId);
  using store = new TaskStore(paths.database, {
    ...(options.clock === undefined ? {} : { clock: options.clock }),
    ...(options.createId === undefined ? {} : { createId: options.createId }),
  });
  const task = store.getTask(taskId);
  if (task === undefined) throw new Error(`Task ${taskId} does not exist`);
  const launcher = new CapsuleProcessLauncher(config.runtime);
  const agent = await ensureManagedIdentity(config, runner, launcher, store, options);
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
    runner,
    launcher,
    agentSessions,
    transcripts,
    ...(options.clock === undefined ? {} : { clock: options.clock }),
  });
  const grants = new TaskMcpGrantStore(store.database, options.clock);
  const node = await useTaskNode({ config, store, grants, launcher, databasePath: paths.database });
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
  await requireSettledWorkbench(paths, taskId);
  using store = new TaskStore(paths.database, {
    ...(options.clock === undefined ? {} : { clock: options.clock }),
    ...(options.createId === undefined ? {} : { createId: options.createId }),
  });
  const task = store.getTask(taskId);
  if (task === undefined) throw new Error(`Task ${taskId} does not exist`);
  const launcher = new CapsuleProcessLauncher(config.runtime);
  const agent = await ensureManagedIdentity(config, runner, launcher, store, options);
  const agentSessions = new TaskAgentSessionStore(store.database, options.clock);
  const transcripts = new IdentityTranscriptProvisioner(
    launcher,
    config.identity.id,
  );
  const checkpointer = await prepareTaskCapsule({
    task,
    store,
    paths,
    config,
    runner,
    launcher,
    agentSessions,
    transcripts,
    ...(options.clock === undefined ? {} : { clock: options.clock }),
  });
  const grants = new TaskMcpGrantStore(store.database, options.clock);
  const node = await useTaskNode({ config, store, grants, launcher, databasePath: paths.database });
  try {
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
      mcpEndpoint: config.node.capsuleMcpEndpoint,
    });
    let exitCode = 1;
    let interactiveError: unknown;
    try {
      exitCode = await runner.runInteractive(
        launcher.interactiveCommand(take.interactive.launch),
        { started: () => take.started() },
      );
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

async function openTaskShell(args: readonly string[], options: CliOptions, stdout: Output): Promise<number> {
  const parsed = parseArguments(args);
  if (parsed.positionals.length !== 1) throw new CliUsageError("task shell requires exactly one task id");
  const taskId = parsed.positionals[0]!;
  const { paths, config, runner } = await managedContext(options);
  using store = new TaskStore(paths.database, {
    ...(options.clock === undefined ? {} : { clock: options.clock }),
    ...(options.createId === undefined ? {} : { createId: options.createId }),
  });
  const task = store.getTask(taskId);
  if (task === undefined) throw new Error(`Task ${taskId} does not exist`);
  const launcher = new CapsuleProcessLauncher(config.runtime);
  const agentSessions = new TaskAgentSessionStore(store.database, options.clock);
  const checkpointer = await prepareTaskCapsule({
    task,
    store,
    paths,
    config,
    runner,
    launcher,
    agentSessions,
    ...(options.clock === undefined ? {} : { clock: options.clock }),
  });
  stdout.write([
    `Entering the portable workbench for ${taskId}.`,
    "Tools and non-secret config saved under $HOME will travel with this task.",
    "Harness, Git hosting, and signing logins belong in node-local identity capsules.",
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
): Promise<number> {
  const parsed = parseArguments(args, new Set(["url", "redmarker"]));
  if (parsed.positionals.length !== 1) {
    throw new CliUsageError("task review requires exactly one task id");
  }
  const targetUrl = option(parsed, "url");
  if (targetUrl === undefined) throw new CliUsageError("task review requires --url <loopback-url>");
  const taskId = parsed.positionals[0]!;
  const { paths, config, runner } = await managedContext(options);
  await requireSettledWorkbench(paths, taskId);
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
  const agentSessions = new TaskAgentSessionStore(store.database, options.clock);
  const checkpointer = await prepareTaskCapsule({
    task,
    store,
    paths,
    config,
    runner,
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

async function sendManagedTask(
  args: readonly string[],
  options: CliOptions,
  stdout: Output,
): Promise<number> {
  const parsed = parseArguments(args, new Set([
    "node",
    "host",
    "ssh-port",
    "user",
    "identity-file",
    "known-hosts-file",
    "pigeons-endpoint",
    "pigeons",
    "remote-altitude",
  ]));
  if (parsed.positionals.length !== 1) {
    throw new CliUsageError("task send requires exactly one task id");
  }
  const required = (name: string): string => {
    const value = option(parsed, name);
    if (value === undefined) throw new CliUsageError("task send requires --" + name);
    return value;
  };
  const targetNodeId = required("node");
  const { paths, config, runner } = await managedContext(options);
  const taskId = parsed.positionals[0]!;
  await requireSettledWorkbench(paths, taskId);
  const inlineConnection = [
    "host",
    "ssh-port",
    "user",
    "identity-file",
    "known-hosts-file",
    "pigeons-endpoint",
    "pigeons",
  ].some((name) => option(parsed, name) !== undefined);
  let connection: NodeConnection;
  let remoteAltitude: string;
  if (!inlineConnection) {
    const remembered = await new FilesystemNodeRegistry(paths.nodes).get(
      targetNodeId,
    );
    if (remembered === undefined) {
      throw new Error(
        `Node ${targetNodeId} is not registered. Run \`altitude node add ${targetNodeId} ...\` first.`,
      );
    }
    connection = remembered.connection;
    remoteAltitude = option(parsed, "remote-altitude")
      ?? remembered.remoteAltitude;
  } else {
    const host = option(parsed, "host");
    const pigeonsEndpoint = option(parsed, "pigeons-endpoint");
    if ((host === undefined) === (pigeonsEndpoint === undefined)) {
      throw new CliUsageError(
        "task send requires exactly one of --host or --pigeons-endpoint",
      );
    }
    if (pigeonsEndpoint !== undefined
      && option(parsed, "ssh-port") !== undefined) {
      throw new CliUsageError("--ssh-port is only valid with --host");
    }
    connection = {
      nodeId: targetNodeId,
      user: required("user"),
      identityFile: resolve(required("identity-file")),
      knownHostsFile: resolve(required("known-hosts-file")),
      route: host !== undefined
        ? {
            kind: "ssh",
            host,
            ...(option(parsed, "ssh-port") === undefined
              ? {}
              : { port: portNumber(option(parsed, "ssh-port"))! }),
          }
        : {
            kind: "pigeons",
            endpointId: pigeonsEndpoint!,
            ...(option(parsed, "pigeons") === undefined
              ? {}
              : { pigeonsExecutable: option(parsed, "pigeons")! }),
          },
    };
    remoteAltitude = option(parsed, "remote-altitude") ?? "altitude";
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
  const coordinator = new LocalToRemoteCapsuleTransfer(
    checkpoints,
    seeds,
    config.node.id,
    new SshCapsuleTransferTransport(
      runner,
      connection,
      remoteAltitude,
    ),
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

async function readStdinBytes(limit: number): Promise<Uint8Array> {
  const reader = Bun.stdin.stream().getReader();
  const chunks: Uint8Array[] = [];
  let size = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      size += value.byteLength;
      if (size > limit) {
        await reader.cancel();
        throw new Error(`Transfer input exceeds ${limit} bytes`);
      }
      chunks.push(value);
    }
  } finally {
    reader.releaseLock();
  }
  const bytes = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return bytes;
}

async function runTransferReceiverCommand(
  args: readonly string[],
  options: CliOptions,
  stdout: Output,
): Promise<number> {
  const operation = args[0];
  if ((operation !== "prepare" && operation !== "activate") || args.length !== 1) {
    throw new CliUsageError("_transfer requires prepare or activate");
  }
  const paths = resolveAltitudePaths(options.env ?? process.env);
  await ensureAltitudePaths(paths);
  const config = await readLocalConfig(paths.config);
  if (config === undefined) throw new Error("Target Altitude node is not set up");
  const checkpoints = new FilesystemCapsuleCheckpointStore(paths.capsules);
  const receiver = new FilesystemCapsuleTransferReceiver(
    checkpoints,
    new FilesystemRepositorySeedCache(join(paths.cache, "repository-seeds")),
    config.node.id,
  );
  if (operation === "prepare") {
    writeJson(stdout, await receiver.prepare(
      await readStdinBytes(MAX_CAPSULE_TRANSFER_PACKAGE_BYTES),
    ));
    return 0;
  }
  const bytes = await readStdinBytes(64 * 1024);
  let request: unknown;
  try {
    request = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes)) as unknown;
  } catch (error) {
    throw new Error("Target activation request is invalid JSON", { cause: error });
  }
  if (typeof request !== "object" || request === null) {
    throw new Error("Target activation request is invalid");
  }
  const value = request as Parameters<typeof receiver.activate>[0];
  const activation = await receiver.activate(value);
  const checkpoint = await checkpoints.read(activation.checkpoint);
  const stateMember = checkpoint.members.find(
    (member) => member.path === PORTABLE_TASK_STATE_MEMBER,
  );
  if (stateMember === undefined) {
    throw new Error("Transferred capsule has no portable task state");
  }
  using store = new TaskStore(paths.database, {
    ...(options.clock === undefined ? {} : { clock: options.clock }),
    ...(options.createId === undefined ? {} : { createId: options.createId }),
  });
  const portableState = decodePortableTaskState(stateMember.bytes);
  activateTransferredTask({
    store,
    sessions: new TaskAgentSessionStore(store.database, options.clock),
    portableState,
    targetIdentityId: config.identity.id,
    targetNodeId: config.node.id,
    transferId: activation.transferId,
    checkpoint: activation.checkpoint,
    activatedAt: (options.clock ?? Date.now)(),
  });
  writeJson(stdout, activation);
  return 0;
}

function registeredNodeRoute(record: RegisteredNode): string {
  const route = record.connection.route;
  return route.kind === "ssh"
    ? `${record.connection.user}@${route.host}${route.port === undefined ? "" : `:${route.port}`}`
    : `${record.connection.user}@pigeons:${route.endpointId.slice(0, 12)}`;
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
          `${node.connection.nodeId}  ${registeredNodeRoute(node)}  ${node.remoteAltitude}\n`,
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
      stdout.write(`Identity: ${node.connection.identityFile}\n`);
      stdout.write(`Known hosts: ${node.connection.knownHostsFile}\n`);
      stdout.write(`Remote Altitude: ${node.remoteAltitude}\n`);
    }
    return 0;
  }

  if (subcommand === "check") {
    const parsed = parseArguments(
      args,
      new Set(),
      new Set(["json", "accept-new-host-key"]),
    );
    if (parsed.positionals.length !== 1) {
      throw new CliUsageError("node check requires exactly one node id");
    }
    const id = parsed.positionals[0]!;
    const node = await registry.get(id);
    if (node === undefined) throw new Error(`Node ${id} is not registered`);
    const runner = options.commandRunner ?? new BunCommandRunner();
    const result = await runner.run(buildRemoteCommandSpec(
      node.connection,
      [node.remoteAltitude, "version"],
      { acceptNewHostKey: parsed.options.get("accept-new-host-key") === true },
    ));
    if (result.exitCode !== 0) {
      throw new Error(
        `Node ${id} check failed (${result.exitCode}): ${result.stderr.trim()}`,
      );
    }
    const version = result.stdout.trim();
    if (!version) throw new Error(`Node ${id} returned no Altitude version`);
    if (wantsJson(parsed)) writeJson(stdout, { nodeId: id, version });
    else stdout.write(`${id} is reachable with Altitude ${version}.\n`);
    return 0;
  }

  const parsed = parseArguments(args, new Set([
    "host",
    "ssh-port",
    "user",
    "identity-file",
    "known-hosts-file",
    "pigeons-endpoint",
    "pigeons",
    "remote-altitude",
  ]));
  if (parsed.positionals.length !== 1) {
    throw new CliUsageError("node add requires exactly one node id");
  }
  const required = (name: string): string => {
    const value = option(parsed, name);
    if (value === undefined) throw new CliUsageError("node add requires --" + name);
    return value;
  };
  const id = parsed.positionals[0]!;
  const host = option(parsed, "host");
  const pigeonsEndpoint = option(parsed, "pigeons-endpoint");
  if ((host === undefined) === (pigeonsEndpoint === undefined)) {
    throw new CliUsageError(
      "node add requires exactly one of --host or --pigeons-endpoint",
    );
  }
  if (pigeonsEndpoint !== undefined && option(parsed, "ssh-port") !== undefined) {
    throw new CliUsageError("--ssh-port is only valid with --host");
  }
  if (host !== undefined && option(parsed, "pigeons") !== undefined) {
    throw new CliUsageError("--pigeons is only valid with --pigeons-endpoint");
  }
  const node = await registry.put(registeredNode({
    nodeId: id,
    user: required("user"),
    identityFile: resolve(required("identity-file")),
    knownHostsFile: resolve(required("known-hosts-file")),
    route: host !== undefined
      ? {
          kind: "ssh",
          host,
          ...(option(parsed, "ssh-port") === undefined
            ? {}
            : { port: portNumber(option(parsed, "ssh-port"))! }),
        }
      : {
          kind: "pigeons",
          endpointId: pigeonsEndpoint!,
          ...(option(parsed, "pigeons") === undefined
            ? {}
            : { pigeonsExecutable: option(parsed, "pigeons")! }),
        },
  }, option(parsed, "remote-altitude") ?? "altitude"));
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
  const retryWorker = !workerCompatible ? undefined : new NodeScheduledActionWorker(
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
  const dispatchWorker = !workerCompatible || config === undefined
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
  stdout.write(
    `Altitude node listening at ${server.url.toString()} (${runtime}`
      + (retryWorker === undefined ? "" : ", retry and dispatch workers active")
      + ")\n",
  );
  if (config !== undefined && !workerCompatible) {
    stdout.write("Automatic retries are inactive because node runtime/listener overrides differ from the saved setup.\n");
  }
  try {
    retryWorker?.start();
    dispatchWorker?.start();
    await waitForShutdownSignal();
  } finally {
    await Promise.all([retryWorker?.close(), dispatchWorker?.close()]);
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
      id: option(parsed, "node-id") ?? base.node.id,
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
      "node-id",
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
  const buildImages = parsed.options.get("skip-build") !== true;
  const requestedSource = option(parsed, "source");
  const candidateSource = resolve(requestedSource ?? process.cwd());
  const sourceAvailable = buildImages
    ? await isAltitudeSourceCheckout(candidateSource)
    : false;
  if (buildImages && requestedSource !== undefined && !sourceAvailable) {
    throw new Error(`${candidateSource} is not an Altitude source checkout`);
  }
  const standalone = buildImages && !sourceAvailable
    ? await prepareStandaloneImageContext({
        parentDirectory: join(paths.cache, "image-builds"),
        executable: process.execPath,
      })
    : undefined;
  let result;
  try {
    result = await setupAltitude({
      config,
      sourceDirectory: standalone?.directory
        ?? candidateSource,
      buildImages,
      ...(json ? {} : { progress: (item) => stdout.write(`→ ${item.message}\n`) }),
    }, {
      runtimeProbe: runtime,
      images: new CapsuleImageBuilder(config.runtime, runner),
      identity: new IdentityCapsuleManager(config.runtime, runner),
      claude,
      interactiveCommand: (spec) => launcher.interactiveCommand(spec),
      runner,
    });
  } finally {
    await standalone?.cleanup();
  }
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

async function runTuiCommand(options: CliOptions, stdout: Output): Promise<number> {
  const terminal = options.terminal ?? new ProcessTuiTerminal();
  if (!terminal.interactive) {
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
    return await runAltitudeTui(terminal, {
      configured: async () => await readLocalConfig(paths.config) !== undefined,
      configureClaude: async () => {
        const exitCode = await runSetupCommand([], options, stdout);
        if (exitCode !== 0) throw new Error(`Claude capsule setup exited with status ${exitCode}`);
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
        const exitCode = await takeManagedTask([taskId], options, stdout);
        if (exitCode !== 0) throw new Error(`Claude Code exited with status ${exitCode}`);
      },
      openShell: async (taskId) => {
        await ensureControlPlane();
        await makeTaskRunnable(taskId, options);
        const exitCode = await openTaskShell([taskId], options, stdout);
        if (exitCode !== 0) throw new Error(`Task shell exited with status ${exitCode}`);
      },
    });
  } finally {
    await controlPlane?.close();
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
      case "_transfer":
        return await runTransferReceiverCommand(args.slice(1), options, stdout);
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
