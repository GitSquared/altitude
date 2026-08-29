import { describe, expect, test } from "bun:test";

import {
  CLAUDE_TRANSCRIPT_MANIFEST_MEMBER,
  claudeTranscriptCapsuleMembers,
  createClaudeTranscriptBundle,
  decodeClaudeTranscriptCapsuleMembers,
  decodeClaudeTranscriptWire,
  encodeClaudeTranscriptWire,
} from "../src/agent/transcript.ts";
import { utf8 } from "../src/capsule/canonical.ts";

const SESSION_ID = "01900000-0000-7000-8000-000000000001";

function bundle() {
  return createClaudeTranscriptBundle({
    taskId: "task-1",
    sessionId: SESSION_ID,
    projectDirectoryName: "altitude-task-1",
    files: [
      { path: "subagents/agent-a.jsonl", bytes: utf8("subagent\n") },
      { path: SESSION_ID + ".jsonl", bytes: utf8("operator\nassistant\n") },
    ],
  });
}

describe("portable Claude transcript", () => {
  test("round-trips one verified session through wire and capsule members", () => {
    const transcript = bundle();
    const wire = encodeClaudeTranscriptWire(transcript);
    expect(decodeClaudeTranscriptWire(wire)).toEqual(transcript);

    const members = claudeTranscriptCapsuleMembers(transcript);
    expect(members[0]?.path).toBe(CLAUDE_TRANSCRIPT_MANIFEST_MEMBER);
    expect(decodeClaudeTranscriptCapsuleMembers(members)).toEqual(transcript);
    expect(members.map((member) => member.path).join("\n"))
      .not.toContain("credentials");
  });

  test("rejects corrupt bytes, traversal, and orphaned transcript files", () => {
    const wire = encodeClaudeTranscriptWire(bundle());
    const corrupt = wire.slice();
    const last = corrupt.byteLength - 1;
    corrupt[last] = corrupt[last]! ^ 1;
    expect(() => decodeClaudeTranscriptWire(corrupt)).toThrow(
      "failed verification",
    );
    expect(() => createClaudeTranscriptBundle({
      taskId: "task-1",
      sessionId: SESSION_ID,
      projectDirectoryName: "altitude-task-1",
      files: [{ path: "../.credentials.json", bytes: utf8("secret") }],
    })).toThrow();
    expect(() => decodeClaudeTranscriptCapsuleMembers([
      {
        path: "session/claude/files/orphan.jsonl" as never,
        bytes: utf8("orphan"),
      },
    ])).toThrow("without a manifest");
  });
});
