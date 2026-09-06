/**
 * The process boundary is intentionally smaller than a container/runtime API.
 * An adapter describes where an unmodified executable must run; the node owns
 * the mechanics of entering that capsule and attaching a terminal.
 */
export type ProcessExecutionTarget =
  | { readonly kind: "host" }
  | {
      readonly kind: "identity-capsule";
      readonly identityId: string;
      /** Selects a task-scoped MCP/tool binding without exposing its workspace. */
      readonly taskId?: string;
    }
  | {
      readonly kind: "task-capsule";
      readonly taskId: string;
    };

export interface ProcessLaunchSpec {
  readonly target: ProcessExecutionTarget;
  readonly executable: string;
  readonly args: readonly string[];
  readonly cwd?: string;
  readonly env?: Readonly<Record<string, string>>;
  readonly io: "piped" | "interactive";
  readonly purpose: string;
}

export interface ProcessExit {
  readonly exitCode: number;
  readonly signal?: string;
}

/** A piped child process. Interactive specs are handed to the operator shell. */
export interface RunningProcess {
  readonly stdout: AsyncIterable<Uint8Array>;
  readonly stderr: AsyncIterable<Uint8Array>;
  readonly exited: Promise<ProcessExit>;
  writeStdin(chunk: string | Uint8Array): Promise<void>;
  closeStdin(): Promise<void>;
  terminate(signal?: NodeJS.Signals): Promise<void>;
}

/**
 * Implemented by Altitude's Microsandbox adapter or a test double.
 * A launcher must honor `target`; silently falling back to the host would
 * cross Altitude's identity boundary.
 */
export interface ProcessLauncher {
  launch(spec: ProcessLaunchSpec & { readonly io: "piped" }): Promise<RunningProcess>;
}
