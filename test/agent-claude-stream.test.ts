import { describe, expect, test } from "bun:test";

import { encodeClaudeUserInput, parseClaudeStreamLine } from "../src/agent/claude-stream.ts";

describe("Claude CLI stream protocol", () => {
  test("normalizes lifecycle, retry, rate-limit, and result events", () => {
    expect(parseClaudeStreamLine(JSON.stringify({
      type: "system",
      subtype: "init",
      session_id: "01900000-0000-7000-8000-000000000001",
      model: "claude-sonnet-5",
      tools: ["mcp__altitude__bash"],
    }), 10)).toEqual({
      type: "agent.init",
      observedAtMs: 10,
      sessionId: "01900000-0000-7000-8000-000000000001",
      model: "claude-sonnet-5",
      tools: ["mcp__altitude__bash"],
    });

    expect(parseClaudeStreamLine(JSON.stringify({
      type: "system",
      subtype: "api_retry",
      attempt: 2,
      max_retries: 4,
      error_status: 529,
      retry_delay_ms: 1_500,
    }), 11)).toEqual({
      type: "agent.api_retry",
      observedAtMs: 11,
      attempt: 2,
      maximumAttempts: 4,
      httpStatus: 529,
      retryAfterMs: 1_500,
    });

    expect(parseClaudeStreamLine(JSON.stringify({
      type: "rate_limit_event",
      rate_limit_info: {
        status: "rejected",
        rateLimitType: "five_hour",
        utilization: 1,
        resetsAt: 1_900_000_000,
      },
    }), 12)).toEqual({
      type: "agent.rate_limit",
      observedAtMs: 12,
      status: "rejected",
      limitType: "five_hour",
      utilization: 1,
      resetsAtMs: 1_900_000_000_000,
    });

    expect(parseClaudeStreamLine(JSON.stringify({
      type: "result",
      subtype: "success",
      is_error: false,
      session_id: "01900000-0000-7000-8000-000000000001",
      result: "Ready for review.",
      duration_ms: 42,
      num_turns: 3,
    }), 13)).toEqual({
      type: "agent.result",
      observedAtMs: 13,
      subtype: "success",
      success: true,
      sessionId: "01900000-0000-7000-8000-000000000001",
      text: "Ready for review.",
      durationMs: 42,
      turns: 3,
    });
  });

  test("keeps raw payloads out of unrecognized operational events", () => {
    const event = parseClaudeStreamLine(JSON.stringify({
      type: "tool_progress",
      secret_payload: "do-not-copy-this-into-the-ledger",
    }), 20);

    expect(event).toEqual({ type: "agent.trace", observedAtMs: 20, sourceType: "tool_progress" });
    expect(JSON.stringify(event)).not.toContain("do-not-copy");
  });

  test("encodes a streaming user message without session credentials", () => {
    expect(JSON.parse(encodeClaudeUserInput("Build the capsule."))).toEqual({
      type: "user",
      message: { role: "user", content: [{ type: "text", text: "Build the capsule." }] },
      parent_tool_use_id: null,
    });
  });

  test("rejects malformed NDJSON", () => {
    expect(() => parseClaudeStreamLine("not json", 0)).toThrow("invalid NDJSON");
    expect(() => parseClaudeStreamLine("[]", 0)).toThrow("non-object");
    expect(() => parseClaudeStreamLine("{}", 0)).toThrow("without a type");
  });
});
