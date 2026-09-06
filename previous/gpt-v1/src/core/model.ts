export const TASK_STATUSES = [
  "draft",
  "queued",
  "provisioning",
  "running",
  "waiting_decision",
  "waiting_review",
  "waiting_external",
  "taken",
  "pausing",
  "transferring",
  "paused",
  "completed",
  "stopped",
  "failed",
  "archived",
] as const;

export type TaskStatus = (typeof TASK_STATUSES)[number];

/**
 * The expression levels from "Attention is All I Have", ordered from the
 * operator retaining the most implementation ownership to delegating it.
 * This is deliberately independent from attention class: a Decision can be
 * requested at any expression level.
 */
export const INTENT_LEVELS = [
  "fully_owned",
  "precise_instructions",
  "option_selection",
  "detailed_plan",
  "architecture_sketch",
  "principles",
  "brainstorming",
  "fully_delegated",
] as const;

export type IntentLevel = (typeof INTENT_LEVELS)[number];

export const ATTENTION_CLASSES = ["trace", "update", "decision", "review"] as const;
export type AttentionClass = (typeof ATTENTION_CLASSES)[number];
export type BlockingAttentionClass = Extract<AttentionClass, "decision" | "review">;

export type AttentionOption = Readonly<{
  label: string;
  value: string;
  tradeoff?: string;
}>;

export type AttentionEvidence = Readonly<{
  label: string;
  uri: string;
  mediaType?: string;
}>;

export type ReviewArtifact = Readonly<{
  kind: "pull_request" | "preview" | "document" | "visual_review" | "other";
  label: string;
  uri: string;
}>;

type AttentionBase = Readonly<{
  id: string;
  title: string;
  summary: string;
  evidence?: readonly AttentionEvidence[];
}>;

export type TraceAttention = AttentionBase & Readonly<{
  class: "trace";
}>;

export type UpdateAttention = AttentionBase & Readonly<{
  class: "update";
}>;

export type DecisionAttention = AttentionBase & Readonly<{
  class: "decision";
  question: string;
  recommendation?: string;
  options?: readonly AttentionOption[];
  originalIntent?: string;
  requestedIntentLevel?: IntentLevel;
  consequence?: string;
}>;

export type ReviewAttention = AttentionBase & Readonly<{
  class: "review";
  artifacts: readonly ReviewArtifact[];
  checks: readonly string[];
  caveats?: readonly string[];
}>;

export type Attention = TraceAttention | UpdateAttention | DecisionAttention | ReviewAttention;

export type OpenUpdate = UpdateAttention & Readonly<{
  raisedAt: number;
}>;

export type ActiveBlockingAttention = (DecisionAttention | ReviewAttention) & Readonly<{
  raisedAt: number;
  resumeStatus: "running" | "taken";
}>;

/** The complete, rebuildable projection of a capsule's operational task state. */
export type Task = Readonly<{
  id: string;
  title: string;
  intent: string;
  intentLevel: IntentLevel;
  status: TaskStatus;
  revision: number;
  createdAt: number;
  updatedAt: number;
  activeAttention: ActiveBlockingAttention | null;
  openUpdates: readonly OpenUpdate[];
}>;

export function isTaskStatus(value: unknown): value is TaskStatus {
  return typeof value === "string" && (TASK_STATUSES as readonly string[]).includes(value);
}

export function isIntentLevel(value: unknown): value is IntentLevel {
  return typeof value === "string" && (INTENT_LEVELS as readonly string[]).includes(value);
}

export function isAttentionClass(value: unknown): value is AttentionClass {
  return typeof value === "string" && (ATTENTION_CLASSES as readonly string[]).includes(value);
}
