import { describe, expect, test } from "bun:test";

import {
  AGENT_RETRY_PAYLOAD_FORMAT,
  parseAgentRetryPayload,
  rateLimitRetryAt,
} from "../src/run/retry.ts";

describe("agent retry policy", () => {
  test("uses bounded exponential backoff and honors a provider reset hint", () => {
    expect(rateLimitRetryAt(1_000, 1)).toBe(31_000);
    expect(rateLimitRetryAt(1_000, 2)).toBe(61_000);
    expect(rateLimitRetryAt(1_000, 20)).toBe(901_000);
    expect(rateLimitRetryAt(1_000, 1, 50_000)).toBe(51_000);
    expect(rateLimitRetryAt(1_000, 1, 100_000_000)).toBe(21_601_000);
  });

  test("accepts only a fixed session and run generation", () => {
    expect(parseAgentRetryPayload({
      schema: AGENT_RETRY_PAYLOAD_FORMAT,
      sessionId: "01900000-0000-7000-8000-000000000001",
      runNumber: 3,
      model: "claude-sonnet",
    })).toMatchObject({ runNumber: 3, model: "claude-sonnet" });
    expect(() => parseAgentRetryPayload({
      schema: AGENT_RETRY_PAYLOAD_FORMAT,
      sessionId: "not-a-session",
      runNumber: 3,
    })).toThrow("invalid");
  });
});
