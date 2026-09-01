import { describe, expect, test } from "bun:test";

import { CapsuleProcessLauncher } from "./process-launcher.ts";

describe("Microsandbox process launcher", () => {
  test("encodes an interactive capsule attachment without a host fallback", () => {
    const command = new CapsuleProcessLauncher().interactiveCommand({
      target: { kind: "identity-capsule", identityId: "claude-personal" },
      executable: "claude",
      args: ["/login"],
      cwd: "/identity",
      env: { CLAUDE_CONFIG_DIR: "/identity/claude" },
      io: "interactive",
      purpose: "Authenticate Claude inside the identity capsule",
    });
    const args = command.args ?? [];
    const payload = JSON.parse(Buffer.from(args.at(-1)!, "base64url").toString("utf8")) as {
      schema: string;
      capsuleId: string;
      executable: string;
      args: string[];
      cwd: string;
    };

    expect(args).toContain("_microsandbox-attach");
    expect(payload).toMatchObject({
      schema: "altitude.microsandbox-attach/v1",
      executable: "claude",
      args: ["/login"],
      cwd: "/identity",
    });
    expect(payload.capsuleId).toMatch(/^altitude-i-/u);
  });

  test("refuses host execution and paths outside the capsule boundary", () => {
    const launcher = new CapsuleProcessLauncher();
    expect(() => launcher.interactiveCommand({
      target: { kind: "host" },
      executable: "claude",
      args: [],
      io: "interactive",
      purpose: "Never run Claude on the host",
    })).toThrow("cannot silently execute on the host");
    expect(() => launcher.interactiveCommand({
      target: { kind: "task-capsule", taskId: "task-1" },
      executable: "git",
      args: ["status"],
      cwd: "/Users/operator/project",
      io: "interactive",
      purpose: "Inspect a task",
    })).toThrow("must remain inside /workspace");
  });
});
