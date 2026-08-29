import type { AgentRunCompletion } from "../agent/protocol.ts";
import type { StoredTaskAgentSession } from "../storage/task-agent-session-store.ts";
import type { ScheduledActionInput } from "../storage/task-store.ts";

export const AGENT_RETRY_ACTION_KIND = "agent.retry" as const;
export const AGENT_RETRY_PAYLOAD_FORMAT = "altitude.agent-retry/v1" as const;

const MIN_RETRY_DELAY_MS = 5_000;
const BASE_RETRY_DELAY_MS = 30_000;
const MAX_BACKOFF_DELAY_MS = 15 * 60_000;
const MAX_RESET_DELAY_MS = 6 * 60 * 60_000;

export interface AgentRetryPayload {
  readonly schema: typeof AGENT_RETRY_PAYLOAD_FORMAT;
  readonly sessionId: string;
  readonly runNumber: number;
  readonly model?: string;
}

function safeTime(value: number): number {
  if (!Number.isSafeInteger(value) || value < 0) throw new Error("Agent retry time is invalid");
  return value;
}

export function rateLimitRetryAt(
  now: number,
  consecutiveRateLimits: number,
  resetsAtMs?: number,
): number {
  safeTime(now);
  if (!Number.isSafeInteger(consecutiveRateLimits) || consecutiveRateLimits < 1) {
    throw new Error("Consecutive rate-limit count must be positive");
  }
  const exponent = Math.min(consecutiveRateLimits - 1, 10);
  const backoff = Math.min(MAX_BACKOFF_DELAY_MS, BASE_RETRY_DELAY_MS * 2 ** exponent);
  let delay = backoff;
  if (resetsAtMs !== undefined && Number.isSafeInteger(resetsAtMs) && resetsAtMs > now) {
    delay = Math.min(MAX_RESET_DELAY_MS, Math.max(MIN_RETRY_DELAY_MS, resetsAtMs - now + 1_000));
  }
  return safeTime(now + Math.max(MIN_RETRY_DELAY_MS, delay));
}

export function planRateLimitRetry(input: {
  readonly taskId: string;
  readonly session: StoredTaskAgentSession;
  readonly completion: AgentRunCompletion;
  readonly now: number;
  readonly model?: string;
}): ScheduledActionInput {
  if (input.completion.outcome !== "rate_limited") throw new Error("Only a rate-limited run can schedule an agent retry");
  if (input.model !== undefined && (!input.model.trim() || input.model.length > 200 || input.model.includes("\0"))) {
    throw new Error("Agent retry model is invalid");
  }
  const payload: AgentRetryPayload = {
    schema: AGENT_RETRY_PAYLOAD_FORMAT,
    sessionId: input.session.sessionId,
    runNumber: input.session.runCount,
    ...(input.model === undefined ? {} : { model: input.model }),
  };
  return {
    taskId: input.taskId,
    kind: AGENT_RETRY_ACTION_KIND,
    payload,
    runAt: rateLimitRetryAt(input.now, input.session.consecutiveRateLimits, input.completion.rateLimit?.resetsAtMs),
    dedupeKey: `${AGENT_RETRY_ACTION_KIND}:${input.session.sessionId}:${input.session.runCount}`,
  };
}

function record(value: unknown): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw new Error("Agent retry payload is invalid");
  return value as Record<string, unknown>;
}

export function parseAgentRetryPayload(value: unknown): AgentRetryPayload {
  const payload = record(value);
  if (payload.schema !== AGENT_RETRY_PAYLOAD_FORMAT
    || typeof payload.sessionId !== "string"
    || !/^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/iu.test(payload.sessionId)
    || !Number.isSafeInteger(payload.runNumber) || (payload.runNumber as number) < 1) {
    throw new Error("Agent retry payload is invalid");
  }
  if (payload.model !== undefined && (typeof payload.model !== "string" || !payload.model.trim()
    || payload.model.length > 200 || payload.model.includes("\0"))) {
    throw new Error("Agent retry model is invalid");
  }
  return {
    schema: AGENT_RETRY_PAYLOAD_FORMAT,
    sessionId: payload.sessionId,
    runNumber: payload.runNumber as number,
    ...(payload.model === undefined ? {} : { model: payload.model as string }),
  };
}
