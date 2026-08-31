import type {
  AgentAdapter,
  AgentEvent,
  AgentInteractiveSession,
  AgentRun,
  AgentRunCompletion,
  AgentTaskSession,
} from "../agent/protocol.ts";
import type { EventActor } from "../core/events.ts";
import type { Task, TaskStatus } from "../core/model.ts";
import type { IdentityControlProvisioner } from "../identity/control.ts";
import type { TaskAgentSessionStore } from "../storage/task-agent-session-store.ts";
import type { ScheduledActionInput, TaskStore } from "../storage/task-store.ts";
import { claudeTaskMcpConfig, type TaskMcpGrantStore } from "../tools/grants.ts";
import { planRateLimitRetry } from "./retry.ts";

const SYSTEM: EventActor = { kind: "system", component: "task-runner" };

export interface ManagedTaskRun {
  readonly run: AgentRun;
  readonly session: AgentTaskSession;
  readonly grantId: string;
  settle(): Promise<AgentRunCompletion>;
}

export interface ManagedInteractiveTake {
  readonly session: AgentTaskSession;
  readonly interactive: AgentInteractiveSession;
  /** Persist that the native child really spawned before it can be resumed. */
  started(): void;
  finish(): Promise<void>;
}

type McpProvisioner = Pick<IdentityControlProvisioner, "writeTaskMcpConfig">;

function taskPrompt(task: Task): string {
  return `You are the implementation agent for Altitude task ${task.id}: ${task.title}.

Canonical intent (${task.intentLevel}):
${task.intent}

Operating contract:
- Begin with the Altitude get_task tool. The durable task and its event history outrank this summary.
- You have no source checkout in this identity capsule. Read, search, edit, run checks, and inspect Git only through the task-scoped Altitude tools.
- Match the operator's stated level of intent. Do not silently climb to a broader product or architectural level.
- Trace, Update, Decision, and Review are additive. Use record_trace for low-level history, report_update for non-blocking checkpoints, request_decision only when operator input truly blocks progress, and request_review when checked work is ready.
- After a successful Decision or Review request, stop starting work and yield the turn.
- Never merge, deploy, release, publish, or weaken the capsule/identity boundary. Those remain operator actions.
- Preserve unrelated work. A passing command is evidence for that command, not proof of an untested path.

Work autonomously until you reach a genuine blocking Decision or a reviewable, checked result.`;
}

function asAgentSession(value: ReturnType<TaskAgentSessionStore["get"]>): AgentTaskSession {
  if (value === undefined || value.controlDirectory === undefined || value.mcp === undefined || value.grantId === undefined) {
    throw new Error("Task agent session has no complete MCP binding");
  }
  return {
    taskId: value.taskId,
    sessionId: value.sessionId,
    controlDirectory: value.controlDirectory,
    projectDirectoryName: value.projectDirectoryName,
    mcp: value.mcp,
  };
}

function allowedStartStatus(status: TaskStatus): boolean {
  return status === "queued" || status === "provisioning" || status === "running"
    || status === "paused" || status === "waiting_external";
}

export class TaskRunCoordinator {
  constructor(
    private readonly store: TaskStore,
    private readonly sessions: TaskAgentSessionStore,
    private readonly grants: TaskMcpGrantStore,
    private readonly provisioner: McpProvisioner,
    private readonly agent: Pick<AgentAdapter, "start" | "prepareInteractiveTake">
      & Partial<Pick<AgentAdapter, "prepareInteractiveStart">>,
    private readonly now: () => number = Date.now,
  ) {}

  async start(input: {
    readonly taskId: string;
    readonly identityId: string;
    readonly mcpEndpoint: string;
    readonly model?: string;
  }): Promise<ManagedTaskRun> {
    if (input.model !== undefined && (!input.model.trim() || input.model.length > 200 || input.model.includes("\0"))) {
      throw new Error("Agent model is invalid");
    }
    let task = this.store.getTask(input.taskId);
    if (task === undefined) throw new Error(`Task ${input.taskId} does not exist`);
    if (!allowedStartStatus(task.status)) {
      throw new Error(`Task ${task.id} cannot start while ${task.status}`);
    }
    if (task.status === "queued") {
      task = this.#transition(task.id, "provisioning", "Preparing task capsule and managed agent session");
    }
    let ownsRunningTransition = false;

    const existing = this.sessions.ensure(task.id, input.identityId);
    const grant = this.grants.issue({
      taskId: task.id,
      identityId: input.identityId,
      agentSessionId: existing.sessionId,
    });
    let session: AgentTaskSession;
    try {
      const control = await this.provisioner.writeTaskMcpConfig({
        identityId: input.identityId,
        taskId: task.id,
        config: claudeTaskMcpConfig(input.mcpEndpoint, grant.token),
      });
      const bound = this.sessions.bindMcp({
        taskId: task.id,
        grantId: grant.id,
        controlDirectory: control.controlDirectory,
        configPath: control.configPath,
      });
      if (existing.grantId !== undefined && existing.grantId !== grant.id) this.grants.revoke(existing.grantId);
      session = asAgentSession(bound);
      const current = this.store.getTask(task.id)!;
      if (current.status === "provisioning" || current.status === "paused" || current.status === "waiting_external") {
        task = this.#transition(task.id, "running", "Starting managed Claude turn");
        ownsRunningTransition = true;
      } else {
        task = current;
      }
      const run = await this.agent.start({
        ...session,
        displayName: task.title,
        prompt: taskPrompt(task),
        resume: existing.runCount > 0,
        ...(input.model === undefined ? {} : { model: input.model }),
      });
      this.sessions.markRunStarted(task.id);
      const activeTaskId = task.id;
      let settled: Promise<AgentRunCompletion> | undefined;
      return {
        run,
        session,
        grantId: grant.id,
        settle: () => settled ??= this.#settle(activeTaskId, run.completion, input.model),
      };
    } catch (error) {
      this.grants.revoke(grant.id);
      const current = this.store.getTask(task.id);
      if (current?.status === "provisioning" || (ownsRunningTransition && current?.status === "running")) {
        this.#transition(task.id, "failed", error instanceof Error ? error.message : String(error));
      }
      throw error;
    }
  }

  async take(input: {
    readonly taskId: string;
    readonly identityId: string;
    readonly mcpEndpoint: string;
  }): Promise<ManagedInteractiveTake> {
    let task = this.store.getTask(input.taskId);
    if (task === undefined) throw new Error(`Task ${input.taskId} does not exist`);
    if (task.status !== "queued" && task.status !== "provisioning" && task.status !== "running"
      && task.status !== "paused" && task.status !== "waiting_external") {
      throw new Error(`Task ${task.id} cannot be taken while ${task.status}`);
    }
    if (task.status === "queued") {
      task = this.#transition(task.id, "provisioning", "Preparing task capsule and native Claude session");
    }
    const existing = this.sessions.ensure(input.taskId, input.identityId);
    if (existing.identityId !== input.identityId) throw new Error(`Task ${input.taskId} is bound to another identity`);
    const grant = this.grants.issue({
      taskId: input.taskId,
      identityId: input.identityId,
      agentSessionId: existing.sessionId,
    });
    let session: AgentTaskSession;
    try {
      const control = await this.provisioner.writeTaskMcpConfig({
        identityId: input.identityId,
        taskId: input.taskId,
        config: claudeTaskMcpConfig(input.mcpEndpoint, grant.token),
      });
      session = asAgentSession(this.sessions.bindMcp({
        taskId: input.taskId,
        grantId: grant.id,
        controlDirectory: control.controlDirectory,
        configPath: control.configPath,
      }));
      if (existing.grantId !== undefined) this.grants.revoke(existing.grantId);
    } catch (error) {
      this.grants.revoke(grant.id);
      throw error;
    }
    const startsSession = existing.runCount === 0;
    const prepareStart = this.agent.prepareInteractiveStart;
    if (startsSession && prepareStart === undefined) {
      this.grants.revoke(grant.id);
      throw new Error("The configured coding harness cannot start a native interactive session");
    }
    const interactive = startsSession
      ? await prepareStart!.call(this.agent, {
          ...session,
          displayName: task.title,
          prompt: taskPrompt(task),
        })
      : await this.agent.prepareInteractiveTake({
          ...session,
          displayName: task.title,
        });
    try {
      this.#transition(task.id, "taken", "Operator entered the native Claude Code session");
    } catch (error) {
      await interactive.release();
      throw error;
    }
    let finished = false;
    let started = !startsSession;
    return {
      session,
      interactive,
      started: () => {
        if (started) return;
        started = true;
        this.sessions.markRunStarted(task.id);
      },
      finish: async () => {
        if (finished) return;
        finished = true;
        await interactive.release();
        if (this.store.getTask(task.id)?.status === "taken") {
          this.#transition(task.id, "paused", "Operator left the native Claude Code session");
        }
      },
    };
  }

  async #settle(
    taskId: string,
    completionPromise: Promise<AgentRunCompletion>,
    model?: string,
  ): Promise<AgentRunCompletion> {
    const completion = await completionPromise;
    const session = this.sessions.recordOutcome(taskId, completion.outcome);
    const task = this.store.getTask(taskId);
    if (task?.status !== "running") return completion;
    const retry = completion.outcome === "rate_limited"
      ? planRateLimitRetry({
          taskId,
          session,
          completion,
          now: this.now(),
          ...(model === undefined ? {} : { model }),
        })
      : undefined;
    const target: TaskStatus = completion.outcome === "rate_limited" ? "waiting_external"
      : completion.outcome === "success" || completion.outcome === "yielded" ? "paused"
        : completion.outcome === "stopped" ? "stopped" : "failed";
    const reason = completion.outcome === "success"
      ? "Agent turn ended without a blocking Decision or Review"
      : completion.outcome === "rate_limited"
        ? `Agent rate limit; automatic retry scheduled for ${new Date(retry!.runAt).toISOString()}`
        : `Agent turn ended: ${completion.outcome}`;
    this.#transition(taskId, target, reason, retry === undefined ? [] : [retry]);
    return completion;
  }

  #transition(
    taskId: string,
    to: TaskStatus,
    reason: string,
    scheduledActions: readonly ScheduledActionInput[] = [],
  ): Task {
    return this.store.append({
      taskId,
      type: "task.state_changed",
      payload: { to, reason },
      actor: SYSTEM,
    }, [], scheduledActions).task;
  }
}

export async function collectAgentEvents(events: AsyncIterable<AgentEvent>): Promise<AgentEvent[]> {
  const collected: AgentEvent[] = [];
  for await (const event of events) collected.push(event);
  return collected;
}
