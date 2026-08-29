import type { StoredTaskAgentSession } from "../storage/task-agent-session-store.ts";
import type { TaskStore } from "../storage/task-store.ts";
import type { AgentRetryPayload } from "./retry.ts";

export type ManagedAgentRetryExecutor = (
  taskId: string,
  payload: AgentRetryPayload,
) => Promise<"executed" | "skipped">;

export function createManagedAgentRetryExecutor(input: {
  readonly tasks: Pick<TaskStore, "getTask">;
  readonly session: (taskId: string) => StoredTaskAgentSession | undefined;
  readonly run: (taskId: string, model?: string) => Promise<number>;
}): ManagedAgentRetryExecutor {
  return async (taskId, retry) => {
    const task = input.tasks.getTask(taskId);
    const session = input.session(taskId);
    if (task?.status !== "waiting_external" || session === undefined
      || session.sessionId !== retry.sessionId || session.runCount !== retry.runNumber) {
      return "skipped";
    }
    const exitCode = await input.run(taskId, retry.model);
    if (exitCode !== 0 && exitCode !== 75 && input.tasks.getTask(taskId)?.status === "waiting_external") {
      throw new Error(`Managed retry for task ${taskId} failed before advancing the task`);
    }
    return "executed";
  };
}
