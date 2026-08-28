import type { AgentEvent, AgentRateLimitStatus } from "./protocol.ts";

type JsonObject = Record<string, unknown>;

function object(value: unknown): JsonObject | undefined {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as JsonObject
    : undefined;
}

function string(value: unknown): string | undefined {
  return typeof value === "string" && value.length > 0 ? value : undefined;
}

function finiteNumber(value: unknown): number | undefined {
  return typeof value === "number" && Number.isFinite(value) ? value : undefined;
}

function integer(value: unknown): number | undefined {
  const number = finiteNumber(value);
  return number !== undefined && Number.isInteger(number) ? number : undefined;
}

function strings(value: unknown): readonly string[] {
  return Array.isArray(value) ? value.filter((entry): entry is string => typeof entry === "string") : [];
}

function textFromContent(value: unknown): string | undefined {
  if (typeof value === "string") return value.trim() || undefined;
  if (!Array.isArray(value)) return undefined;
  const text = value
    .map((entry) => object(entry))
    .filter((entry): entry is JsonObject => entry !== undefined && entry.type === "text")
    .map((entry) => string(entry.text))
    .filter((entry): entry is string => entry !== undefined)
    .join("\n")
    .trim();
  return text || undefined;
}

function normalizeRateStatus(value: unknown): AgentRateLimitStatus {
  switch (value) {
    case "allowed": return "allowed";
    case "allowed_warning":
    case "warning": return "warning";
    case "rejected": return "rejected";
    default: return "unknown";
  }
}

function epochMilliseconds(value: unknown): number | undefined {
  const number = finiteNumber(value);
  if (number === undefined || number <= 0) return undefined;
  // Accept either Unix seconds or milliseconds across Claude CLI versions.
  return number < 1_000_000_000_000 ? Math.round(number * 1_000) : Math.round(number);
}

/**
 * Normalize the small set of Claude stream events Altitude's lifecycle needs.
 * Unknown events intentionally retain only their type; raw model/tool payloads
 * belong in Claude's transcript, not in the operational event ledger.
 */
export function parseClaudeStreamLine(line: string, observedAtMs: number): AgentEvent {
  let decoded: unknown;
  try {
    decoded = JSON.parse(line);
  } catch {
    throw new Error("Claude emitted invalid NDJSON");
  }
  const message = object(decoded);
  if (message === undefined) throw new Error("Claude emitted a non-object NDJSON event");
  const type = string(message.type);
  if (type === undefined) throw new Error("Claude emitted an event without a type");

  if (type === "system" && message.subtype === "init") {
    const sessionId = string(message.session_id);
    const model = string(message.model);
    return {
      type: "agent.init",
      observedAtMs,
      ...(sessionId === undefined ? {} : { sessionId }),
      ...(model === undefined ? {} : { model }),
      tools: strings(message.tools),
    };
  }

  if (type === "system" && message.subtype === "api_retry") {
    const attempt = integer(message.attempt);
    const maximumAttempts = integer(message.max_retries);
    const httpStatus = integer(message.error_status);
    const retryAfterMs = finiteNumber(message.retry_delay_ms);
    return {
      type: "agent.api_retry",
      observedAtMs,
      ...(attempt === undefined ? {} : { attempt }),
      ...(maximumAttempts === undefined ? {} : { maximumAttempts }),
      ...(httpStatus === undefined ? {} : { httpStatus }),
      ...(retryAfterMs === undefined ? {} : { retryAfterMs }),
    };
  }

  if (type === "rate_limit_event") {
    const info = object(message.rate_limit_info) ?? {};
    const limitType = string(info.rate_limit_type) ?? string(info.rateLimitType);
    const resetsAtMs = epochMilliseconds(info.resets_at ?? info.resetsAt);
    const errorCode = string(info.error_code) ?? string(info.errorCode);
    const utilization = finiteNumber(info.utilization);
    return {
      type: "agent.rate_limit",
      observedAtMs,
      status: normalizeRateStatus(info.status),
      ...(limitType === undefined ? {} : { limitType }),
      ...(utilization === undefined ? {} : { utilization }),
      ...(resetsAtMs === undefined ? {} : { resetsAtMs }),
      ...(errorCode === undefined ? {} : { errorCode }),
    };
  }

  if (type === "result") {
    const subtype = string(message.subtype);
    const isError = message.is_error === true || subtype?.startsWith("error") === true;
    const resultText = string(message.result);
    const sessionId = string(message.session_id);
    const durationMs = finiteNumber(message.duration_ms);
    const turns = integer(message.num_turns);
    return {
      type: "agent.result",
      observedAtMs,
      ...(sessionId === undefined ? {} : { sessionId }),
      ...(subtype === undefined ? {} : { subtype }),
      success: !isError,
      ...(resultText === undefined ? {} : { text: resultText }),
      ...(durationMs === undefined ? {} : { durationMs }),
      ...(turns === undefined ? {} : { turns }),
    };
  }

  if (type === "assistant") {
    const assistantMessage = object(message.message);
    const text = textFromContent(assistantMessage?.content);
    if (text !== undefined) {
      return { type: "agent.message", observedAtMs, role: "assistant", text, partial: false };
    }
  }

  if (type === "stream_event") {
    const event = object(message.event);
    const delta = object(event?.delta);
    if (event?.type === "content_block_delta" && delta?.type === "text_delta") {
      const text = string(delta.text);
      if (text !== undefined) {
        return { type: "agent.message", observedAtMs, role: "assistant", text, partial: true };
      }
    }
  }

  return { type: "agent.trace", observedAtMs, sourceType: type };
}

export function encodeClaudeUserInput(content: string): string {
  return `${JSON.stringify({
    type: "user",
    message: { role: "user", content: [{ type: "text", text: content }] },
    parent_tool_use_id: null,
  })}\n`;
}
