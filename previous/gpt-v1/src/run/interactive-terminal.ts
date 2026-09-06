import type { AgentWriterLeaseStore } from "../agent/writer-lease.ts";
import type { TaskAgentSessionStore } from "../storage/task-agent-session-store.ts";
import type { TaskStore } from "../storage/task-store.ts";
import type { TaskMcpGrantStore } from "../tools/grants.ts";
import type { TerminalSessionStatus } from "../terminal/session.ts";

type RecoverableLeaseStore = Pick<AgentWriterLeaseStore, "current"> & {
  expire(taskId: string, expectedRevision: number): Promise<boolean>;
};

/**
 * Reconcile an independently observed PTY exit with durable agent ownership.
 * The terminal status is the liveness evidence; the writer revision is the
 * fencing token that prevents an old supervisor from clearing a newer run.
 */
export async function reconcileExitedInteractiveAgent(input: {
  readonly taskId: string;
  readonly status: TerminalSessionStatus;
  readonly tasks: Pick<TaskStore, "getTask" | "append">;
  readonly sessions: Pick<TaskAgentSessionStore, "get">;
  readonly leases: RecoverableLeaseStore;
  readonly grants: Pick<TaskMcpGrantStore, "revoke">;
}): Promise<void> {
  if (input.status.state === "running" || input.status.state === "starting") {
    throw new Error("A live Claude terminal cannot be reconciled");
  }
  if (input.status.state === "failed" && input.status.childAlive === true) {
    throw new Error(`Task ${input.taskId} lost its terminal supervisor, but its Claude child still appears alive`);
  }
  const session = input.sessions.get(input.taskId);
  if (session === undefined) throw new Error(`Task ${input.taskId} has no durable Claude session`);
  const lease = await input.leases.current(input.taskId);
  if (lease !== undefined) {
    if (lease.mode !== "interactive" || lease.sessionId !== session.sessionId) {
      throw new Error(`Task ${input.taskId} terminal exit does not fence its current writer`);
    }
    if (!(await input.leases.expire(input.taskId, lease.revision))) {
      throw new Error(`Task ${input.taskId} writer changed during terminal reconciliation`);
    }
  }
  if (session.grantId !== undefined) input.grants.revoke(session.grantId);
  const current = input.tasks.getTask(input.taskId);
  if (current?.status === "taken") {
    input.tasks.append({
      taskId: input.taskId,
      type: "task.state_changed",
      payload: {
        to: "paused",
        reason: input.status.state === "exited"
          ? "Native Claude terminal exited"
          : `Native Claude terminal failed: ${input.status.error}`,
      },
      actor: { kind: "system", component: "terminal-supervisor" },
    });
  }
}
