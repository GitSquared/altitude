import { isDeepStrictEqual } from "node:util";

import type { AgentRunOutcome } from "../agent/protocol.ts";
import type { EventEnvelope } from "../core/events.ts";
import type { Task } from "../core/model.ts";
import { replayTask } from "../core/reducer.ts";
import type { IntentArtifactEnvelope } from "../intent/protocol.ts";
import { stableJson, utf8 } from "../capsule/canonical.ts";
import type { CapsuleMemberContent } from "../capsule/manifest.ts";
import { portableCapsulePath } from "../capsule/paths.ts";
import type {
  StoredTaskAgentSession,
  TaskAgentSessionStore,
} from "../storage/task-agent-session-store.ts";
import type { TaskStore } from "../storage/task-store.ts";

export const PORTABLE_TASK_STATE_FORMAT = "altitude.task-state/v1" as const;
export const PORTABLE_AGENT_SESSION_FORMAT =
  "altitude.agent-session/v1" as const;
export const PORTABLE_TASK_STATE_MEMBER = portableCapsulePath("state/task.json");
const MAX_PORTABLE_TASK_STATE_BYTES = 32 * 1024 * 1024;

export interface PortableTaskAgentSession {
  readonly schema: typeof PORTABLE_AGENT_SESSION_FORMAT;
  readonly taskId: string;
  readonly adapter: "claude-code-cli";
  readonly sessionId: string;
  readonly projectDirectoryName: string;
  readonly runCount: number;
  readonly consecutiveRateLimits: number;
  readonly lastOutcome?: AgentRunOutcome;
  readonly createdAt: number;
  readonly updatedAt: number;
}

export interface PortableTaskState {
  readonly schema: typeof PORTABLE_TASK_STATE_FORMAT;
  readonly task: Task;
  readonly events: readonly EventEnvelope[];
  readonly intentArtifacts: readonly IntentArtifactEnvelope[];
  readonly agentSession?: PortableTaskAgentSession;
}

const OUTCOMES = new Set<AgentRunOutcome>([
  "success",
  "error",
  "yielded",
  "stopped",
  "rate_limited",
  "protocol_error",
]);

function parseAgentSession(
  value: unknown,
  taskId: string,
): PortableTaskAgentSession {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error("Portable agent session is invalid");
  }
  const session = value as Record<string, unknown>;
  if (session.schema !== PORTABLE_AGENT_SESSION_FORMAT
    || session.taskId !== taskId
    || session.adapter !== "claude-code-cli"
    || typeof session.sessionId !== "string"
    || !/^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/iu.test(session.sessionId)
    || typeof session.projectDirectoryName !== "string"
    || !/^[A-Za-z0-9._-]+$/u.test(session.projectDirectoryName)
    || !Number.isSafeInteger(session.runCount)
    || (session.runCount as number) < 0
    || !Number.isSafeInteger(session.consecutiveRateLimits)
    || (session.consecutiveRateLimits as number) < 0
    || !Number.isSafeInteger(session.createdAt)
    || (session.createdAt as number) < 0
    || !Number.isSafeInteger(session.updatedAt)
    || (session.updatedAt as number) < (session.createdAt as number)
    || (session.lastOutcome !== undefined
      && (typeof session.lastOutcome !== "string"
        || !OUTCOMES.has(session.lastOutcome as AgentRunOutcome)))) {
    throw new Error("Portable agent session is invalid");
  }
  return {
    schema: PORTABLE_AGENT_SESSION_FORMAT,
    taskId,
    adapter: "claude-code-cli",
    sessionId: session.sessionId,
    projectDirectoryName: session.projectDirectoryName,
    runCount: session.runCount as number,
    consecutiveRateLimits: session.consecutiveRateLimits as number,
    ...(session.lastOutcome === undefined
      ? {}
      : { lastOutcome: session.lastOutcome as AgentRunOutcome }),
    createdAt: session.createdAt as number,
    updatedAt: session.updatedAt as number,
  };
}

export function portableAgentSession(
  session: StoredTaskAgentSession,
): PortableTaskAgentSession {
  return parseAgentSession({
    schema: PORTABLE_AGENT_SESSION_FORMAT,
    taskId: session.taskId,
    adapter: session.adapter,
    sessionId: session.sessionId,
    projectDirectoryName: session.projectDirectoryName,
    runCount: session.runCount,
    consecutiveRateLimits: session.consecutiveRateLimits,
    ...(session.lastOutcome === undefined
      ? {}
      : { lastOutcome: session.lastOutcome }),
    createdAt: session.createdAt,
    updatedAt: session.updatedAt,
  }, session.taskId);
}

export function decodePortableTaskState(bytes: Uint8Array): PortableTaskState {
  if (bytes.byteLength === 0 || bytes.byteLength > MAX_PORTABLE_TASK_STATE_BYTES) {
    throw new Error("Portable task state size is invalid");
  }
  let value: unknown;
  try {
    value = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes)) as unknown;
  } catch (error) {
    throw new Error("Portable task state is not valid UTF-8 JSON", { cause: error });
  }
  if (typeof value !== "object" || value === null) {
    throw new Error("Portable task state is invalid");
  }
  const snapshot = value as Partial<PortableTaskState>;
  if (snapshot.schema !== PORTABLE_TASK_STATE_FORMAT
    || typeof snapshot.task !== "object" || snapshot.task === null
    || !Array.isArray(snapshot.events) || !Array.isArray(snapshot.intentArtifacts)) {
    throw new Error("Portable task state schema is unsupported");
  }
  let replayed: Task;
  try {
    replayed = replayTask(snapshot.events as EventEnvelope[]);
  } catch (error) {
    throw new Error("Portable task event history is invalid", { cause: error });
  }
  if (!isDeepStrictEqual(replayed, snapshot.task)) {
    throw new Error("Portable task projection does not match its event history");
  }
  for (const artifact of snapshot.intentArtifacts) {
    if (typeof artifact !== "object" || artifact === null
      || artifact.schema !== "altitude.intent-artifact/v1"
      || artifact.taskId !== replayed.id) {
      throw new Error("Portable intent artifact is invalid");
    }
  }
  const agentSession = snapshot.agentSession === undefined
    ? undefined
    : parseAgentSession(snapshot.agentSession, replayed.id);
  return {
    schema: PORTABLE_TASK_STATE_FORMAT,
    task: replayed,
    events: snapshot.events as EventEnvelope[],
    intentArtifacts: snapshot.intentArtifacts as IntentArtifactEnvelope[],
    ...(agentSession === undefined ? {} : { agentSession }),
  };
}

export function portableTaskStateMember(
  store: Pick<TaskStore, "getTask" | "getEvents" | "listIntentArtifacts">,
  taskId: string,
  sessions?: Pick<TaskAgentSessionStore, "get">,
): CapsuleMemberContent {
  const task = store.getTask(taskId);
  if (task === undefined) throw new Error(`Task ${taskId} does not exist`);
  const snapshot: PortableTaskState = {
    schema: PORTABLE_TASK_STATE_FORMAT,
    task,
    events: store.getEvents(taskId),
    intentArtifacts: store.listIntentArtifacts(taskId),
    ...(sessions?.get(taskId) === undefined
      ? {}
      : { agentSession: portableAgentSession(sessions.get(taskId)!) }),
  };
  const bytes = utf8(`${stableJson(snapshot)}\n`);
  if (bytes.byteLength > MAX_PORTABLE_TASK_STATE_BYTES) {
    throw new Error(`Portable task state for ${taskId} exceeds ${MAX_PORTABLE_TASK_STATE_BYTES} bytes`);
  }
  return { path: PORTABLE_TASK_STATE_MEMBER, bytes };
}
