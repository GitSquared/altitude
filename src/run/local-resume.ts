import type { StoredTaskAgentSession } from "../storage/task-agent-session-store.ts";
import type { TaskAgentSessionStore } from "../storage/task-agent-session-store.ts";
import type { ScheduledActionInput, TaskStore } from "../storage/task-store.ts";

export const LOCAL_RESUME_ACTION_KIND = "agent.resume-on-open" as const;
export const LOCAL_RESUME_PAYLOAD_FORMAT = "altitude.local-resume/v1" as const;

export type LocalResumePayload = Readonly<{
  schema: typeof LOCAL_RESUME_PAYLOAD_FORMAT;
  sessionId: string;
  runNumber: number;
}>;

function validSessionId(value: unknown): value is string {
  return typeof value === "string"
    && /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/iu.test(value);
}

export function parseLocalResumePayload(value: unknown): LocalResumePayload {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error("Local resume payload is invalid");
  }
  const payload = value as Partial<LocalResumePayload>;
  if (payload.schema !== LOCAL_RESUME_PAYLOAD_FORMAT
    || !validSessionId(payload.sessionId)
    || !Number.isSafeInteger(payload.runNumber)
    || payload.runNumber! < 1) {
    throw new Error("Local resume payload is invalid");
  }
  return {
    schema: LOCAL_RESUME_PAYLOAD_FORMAT,
    sessionId: payload.sessionId,
    runNumber: payload.runNumber!,
  };
}

export function planLocalResume(
  session: StoredTaskAgentSession,
  runAt: number,
): ScheduledActionInput {
  if (!Number.isSafeInteger(runAt) || runAt < 0) throw new Error("Local resume time is invalid");
  if (session.runCount < 1) throw new Error("A task without a started Claude run cannot resume");
  const payload: LocalResumePayload = {
    schema: LOCAL_RESUME_PAYLOAD_FORMAT,
    sessionId: session.sessionId,
    runNumber: session.runCount,
  };
  return {
    taskId: session.taskId,
    kind: LOCAL_RESUME_ACTION_KIND,
    payload,
    runAt,
    dedupeKey: `${LOCAL_RESUME_ACTION_KIND}:${session.sessionId}:${session.runCount}`,
  };
}

export async function resumeLocalTasksOnOpen(input: {
  readonly store: Pick<
    TaskStore,
    "claimScheduledActionsByKind" | "completeScheduledAction" | "deadLetterScheduledAction"
      | "getTask" | "retryScheduledAction"
  >;
  readonly sessions: Pick<TaskAgentSessionStore, "get">;
  readonly now: number;
  readonly workerId: string;
  readonly start: (taskId: string) => Promise<number>;
  readonly onError?: (taskId: string, error: unknown) => void;
}): Promise<{ readonly resumed: number; readonly deferred: number }> {
  const actions = input.store.claimScheduledActionsByKind(LOCAL_RESUME_ACTION_KIND, {
    workerId: input.workerId,
    now: input.now,
    limit: 100,
  });
  let resumed = 0;
  let deferred = 0;
  for (const action of actions) {
    try {
      const payload = parseLocalResumePayload(action.payload);
      const task = input.store.getTask(action.taskId);
      const session = input.sessions.get(action.taskId);
      if (task?.status !== "paused" || session === undefined
        || session.sessionId !== payload.sessionId
        || session.runCount !== payload.runNumber) {
        input.store.completeScheduledAction(action.id, input.workerId, input.now);
        continue;
      }
      const exitCode = await input.start(action.taskId);
      if (exitCode !== 0) throw new Error(`Claude restart exited with status ${exitCode}`);
      input.store.completeScheduledAction(action.id, input.workerId, input.now);
      resumed += 1;
    } catch (error) {
      deferred += 1;
      const message = error instanceof Error ? error.message : String(error);
      if (message === "Local resume payload is invalid") {
        input.store.deadLetterScheduledAction(action.id, input.workerId, message, input.now);
      } else {
        input.store.retryScheduledAction(action.id, input.workerId, input.now + 5_000, message);
      }
      input.onError?.(action.taskId, error);
    }
  }
  return { resumed, deferred };
}
