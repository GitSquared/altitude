import type { IntentToolAdapter, IntentToolLaunch } from "./protocol.ts";

function requireCredentialFreeUrl(value: string, label: string): URL {
  const url = new URL(value);
  if (!['http:', 'https:'].includes(url.protocol)) throw new Error(`${label} must use HTTP(S)`);
  if (url.username || url.password) throw new Error(`${label} must not contain credentials`);
  return url;
}

function isLoopback(hostname: string): boolean {
  return hostname === "localhost"
    || hostname === "127.0.0.1"
    || hostname === "[::1]";
}

export class RedmarkerIntentTool implements IntentToolAdapter {
  readonly kind = "redmarker" as const;

  constructor(private readonly executable = "redmarker") {
    if (!executable.trim() || executable.includes("\0")) {
      throw new Error("Redmarker executable is invalid");
    }
  }

  launch(input: {
    readonly taskId: string;
    readonly capsuleId: string;
    readonly targetUrl: string;
    readonly submissionUrl: string;
    readonly submissionToken: string;
  }): IntentToolLaunch {
    const target = requireCredentialFreeUrl(input.targetUrl, "Redmarker target");
    const submission = requireCredentialFreeUrl(input.submissionUrl, "Redmarker submission");
    if (!isLoopback(target.hostname)) {
      throw new Error("Redmarker target must use a loopback endpoint");
    }
    if (!isLoopback(submission.hostname)) {
      throw new Error("Redmarker submission must use a loopback endpoint");
    }
    return {
      tool: "redmarker",
      executable: this.executable,
      args: ["--url", target.toString()],
      environment: {
        REDMARKER_ALTITUDE_TASK_ID: input.taskId,
        REDMARKER_ALTITUDE_CAPSULE_ID: input.capsuleId,
        REDMARKER_SUBMIT_URL: submission.toString(),
        REDMARKER_SUBMIT_TOKEN: input.submissionToken,
      },
    };
  }
}
