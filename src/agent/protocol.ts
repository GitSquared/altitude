import type { ProcessExit, ProcessLaunchSpec } from "../runtime/protocol.ts";

export const EXPERIMENTAL_AGENT_PROTOCOL = "altitude.agent/v1alpha1" as const;

export interface AgentCapabilities {
  readonly protocol: typeof EXPERIMENTAL_AGENT_PROTOCOL;
  readonly adapter: string;
  readonly fixedSessions: true;
  readonly resumableSessions: true;
  readonly cooperativeYield: true;
  readonly nativeInteractiveTake: true;
  readonly nodeLocalIdentity: true;
  readonly portableTaskTranscripts: true;
}

export type AgentAuthState = "authenticated" | "unauthenticated" | "unknown";

export interface AgentProbe {
  readonly installed: boolean;
  readonly version?: string;
  readonly auth: AgentAuthState;
}

export interface AgentMcpBoundary {
  /** Path as seen from inside the task/agent capsule. */
  readonly configPath: string;
  /** Fully-qualified MCP tool names, for example mcp__altitude__request_review. */
  readonly allowedTools: readonly string[];
}

export interface AgentTaskSession {
  readonly taskId: string;
  /** Claude requires an explicit UUID for a new fixed session. */
  readonly sessionId: string;
  /**
   * An identity-local control directory for this task. It contains no source
   * tree; code access is mediated by the task-scoped Altitude MCP boundary.
   */
  readonly controlDirectory: string;
  /** Stable across nodes so Claude resolves the same transcript directory. */
  readonly projectDirectoryName: string;
  readonly mcp: AgentMcpBoundary;
  readonly displayName?: string;
}

export interface AgentRunRequest extends AgentTaskSession {
  readonly prompt: string;
  readonly resume: boolean;
  readonly model?: string;
}

export interface AgentTakeRequest extends AgentTaskSession {}

export interface AgentInteractiveStartRequest extends AgentTaskSession {
  readonly prompt: string;
}

export type AgentRateLimitStatus = "allowed" | "warning" | "rejected" | "unknown";

interface ObservedAgentEvent {
  readonly observedAtMs: number;
}

export type AgentEvent =
  | (ObservedAgentEvent & {
      readonly type: "agent.init";
      readonly sessionId?: string;
      readonly model?: string;
      readonly tools: readonly string[];
    })
  | (ObservedAgentEvent & {
      readonly type: "agent.result";
      readonly sessionId?: string;
      readonly subtype?: string;
      readonly success: boolean;
      readonly text?: string;
      readonly durationMs?: number;
      readonly turns?: number;
    })
  | (ObservedAgentEvent & {
      readonly type: "agent.api_retry";
      readonly attempt?: number;
      readonly maximumAttempts?: number;
      readonly httpStatus?: number;
      readonly retryAfterMs?: number;
    })
  | (ObservedAgentEvent & {
      readonly type: "agent.rate_limit";
      readonly status: AgentRateLimitStatus;
      readonly limitType?: string;
      readonly utilization?: number;
      readonly resetsAtMs?: number;
      readonly errorCode?: string;
    })
  | (ObservedAgentEvent & {
      readonly type: "agent.message";
      readonly role: "assistant";
      readonly text: string;
      readonly partial: boolean;
    })
  | (ObservedAgentEvent & {
      readonly type: "agent.trace";
      readonly sourceType: string;
    })
  | (ObservedAgentEvent & {
      readonly type: "agent.protocol_error";
      readonly message: string;
    });

export type AgentRunOutcome =
  | "success"
  | "error"
  | "yielded"
  | "stopped"
  | "rate_limited"
  | "protocol_error";

export interface AgentRunCompletion {
  readonly outcome: AgentRunOutcome;
  readonly exit: ProcessExit;
  readonly result?: Extract<AgentEvent, { readonly type: "agent.result" }>;
  readonly rateLimit?: Extract<AgentEvent, { readonly type: "agent.rate_limit" }>;
  readonly detail?: string;
}

export interface AgentRun {
  readonly taskId: string;
  readonly sessionId: string;
  readonly launch: ProcessLaunchSpec & { readonly io: "piped" };
  readonly events: AsyncIterable<AgentEvent>;
  readonly completion: Promise<AgentRunCompletion>;
  /** Ask the live turn to checkpoint and finish at its next safe boundary. */
  requestYield(reason?: string): Promise<boolean>;
  stop(): Promise<void>;
}

export interface AgentInteractiveSession {
  readonly taskId: string;
  readonly sessionId: string;
  readonly launch: ProcessLaunchSpec & { readonly io: "interactive" };
  /** Must be called after the native TUI exits, even when it exits with an error. */
  release(): Promise<void>;
}

export type AgentTranscriptTransferPlan =
  | {
      readonly direction: "export";
      readonly taskId: string;
      readonly from: { readonly scope: "identity"; readonly path: string };
      readonly to: { readonly scope: "task-capsule"; readonly path: string };
      readonly identityMaterial: "excluded";
    }
  | {
      readonly direction: "import";
      readonly taskId: string;
      readonly from: { readonly scope: "task-capsule"; readonly path: string };
      readonly to: { readonly scope: "identity"; readonly path: string };
      readonly identityMaterial: "excluded";
    };

export interface AgentAdapter {
  readonly capabilities: AgentCapabilities;
  probe(): Promise<AgentProbe>;
  loginLaunchSpec(): ProcessLaunchSpec & { readonly io: "interactive" };
  start(request: AgentRunRequest): Promise<AgentRun>;
  prepareInteractiveStart(request: AgentInteractiveStartRequest): Promise<AgentInteractiveSession>;
  prepareInteractiveTake(request: AgentTakeRequest): Promise<AgentInteractiveSession>;
  transcriptExportPlan(session: AgentTaskSession): AgentTranscriptTransferPlan;
  transcriptImportPlan(session: AgentTaskSession): AgentTranscriptTransferPlan;
}
