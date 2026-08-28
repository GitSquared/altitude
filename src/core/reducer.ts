import type {
  ActiveBlockingAttention,
  Attention,
  OpenUpdate,
  Task,
  TaskStatus,
} from "./model.ts";
import { isIntentLevel, isTaskStatus } from "./model.ts";
import type { AttentionResolution, EventEnvelope } from "./events.ts";

export class DomainInvariantError extends Error {
  override readonly name = "DomainInvariantError";
}

const ALLOWED_TRANSITIONS: Readonly<Record<TaskStatus, readonly TaskStatus[]>> = {
  draft: ["queued", "stopped", "archived"],
  queued: ["provisioning", "paused", "stopped", "failed"],
  provisioning: ["running", "paused", "stopped", "failed"],
  running: ["waiting_external", "taken", "pausing", "paused", "stopped", "failed"],
  waiting_decision: [],
  waiting_review: [],
  waiting_external: ["running", "taken", "pausing", "paused", "stopped", "failed"],
  taken: ["running", "pausing", "paused", "stopped", "failed"],
  pausing: ["paused", "transferring", "stopped", "failed"],
  transferring: ["queued", "provisioning", "running", "paused", "stopped", "failed"],
  paused: ["queued", "provisioning", "running", "taken", "transferring", "stopped", "failed", "archived"],
  completed: ["archived"],
  stopped: ["queued", "archived"],
  failed: ["queued", "paused", "stopped", "archived"],
  archived: [],
};

function requireText(value: string, label: string): void {
  if (!value.trim()) throw new DomainInvariantError(`${label} must not be empty`);
}

function assertAttentionShape(attention: Attention): void {
  requireText(attention.id, "Attention id");
  requireText(attention.title, "Attention title");
  requireText(attention.summary, "Attention summary");
  if (attention.class === "decision") {
    requireText(attention.question, "Decision question");
    if (attention.options && attention.options.length < 2) {
      throw new DomainInvariantError("A Decision with options must provide at least two");
    }
  }
  if (attention.class === "review") {
    if (attention.artifacts.length === 0) throw new DomainInvariantError("A Review requires at least one artifact");
    if (attention.checks.length === 0) throw new DomainInvariantError("A Review requires completed checks");
  }
}

function resolveOpenUpdate(task: Task, attentionId: string, resolution: AttentionResolution): Task {
  const update = task.openUpdates.find((candidate) => candidate.id === attentionId);
  if (!update) throw new DomainInvariantError(`Attention ${attentionId} is not open`);
  if (resolution.kind !== "acknowledged") {
    throw new DomainInvariantError("An Update can only be acknowledged");
  }
  return { ...task, openUpdates: task.openUpdates.filter((candidate) => candidate.id !== attentionId) };
}

function resolveBlockingAttention(task: Task, attentionId: string, resolution: AttentionResolution): Task {
  const attention = task.activeAttention;
  if (!attention || attention.id !== attentionId) {
    throw new DomainInvariantError(`Attention ${attentionId} is not the active blocking request`);
  }

  if (attention.class === "decision") {
    if (resolution.kind !== "answered") throw new DomainInvariantError("A Decision requires an answer");
    requireText(resolution.answer, "Decision answer");
    return { ...task, activeAttention: null, status: attention.resumeStatus };
  }

  if (resolution.kind === "accepted") {
    return { ...task, activeAttention: null, status: "completed" };
  }
  if (resolution.kind === "revision_requested") {
    requireText(resolution.instructions, "Review revision instructions");
    return { ...task, activeAttention: null, status: attention.resumeStatus };
  }
  throw new DomainInvariantError("A Review must be accepted or request a revision");
}

function closeAttentionWithoutResolution(task: Task, attentionId: string): Task {
  if (task.activeAttention?.id === attentionId) {
    return {
      ...task,
      activeAttention: null,
      status: task.activeAttention.resumeStatus,
    };
  }
  if (task.openUpdates.some((candidate) => candidate.id === attentionId)) {
    return { ...task, openUpdates: task.openUpdates.filter((candidate) => candidate.id !== attentionId) };
  }
  throw new DomainInvariantError(`Attention ${attentionId} is not open`);
}

function raiseAttention(task: Task, attention: Attention, occurredAt: number): Task {
  assertAttentionShape(attention);
  if (task.activeAttention?.id === attention.id || task.openUpdates.some((candidate) => candidate.id === attention.id)) {
    throw new DomainInvariantError(`Attention ${attention.id} is already open`);
  }

  switch (attention.class) {
    case "trace":
      return task;
    case "update": {
      const update: OpenUpdate = { ...attention, raisedAt: occurredAt };
      return { ...task, openUpdates: [...task.openUpdates, update] };
    }
    case "decision":
    case "review": {
      if (task.activeAttention) {
        throw new DomainInvariantError("A task may have at most one active blocking Decision or Review");
      }
      if (task.status !== "running" && task.status !== "taken") {
        throw new DomainInvariantError(`Cannot raise blocking attention while task is ${task.status}`);
      }
      const activeAttention: ActiveBlockingAttention = {
        ...attention,
        raisedAt: occurredAt,
        resumeStatus: task.status,
      };
      return {
        ...task,
        activeAttention,
        status: attention.class === "decision" ? "waiting_decision" : "waiting_review",
      };
    }
  }
}

export function assertTaskInvariants(task: Task): void {
  requireText(task.id, "Task id");
  requireText(task.title, "Task title");
  requireText(task.intent, "Task intent");
  if (!isTaskStatus(task.status)) throw new DomainInvariantError(`Unknown task status: ${String(task.status)}`);
  if (!isIntentLevel(task.intentLevel)) throw new DomainInvariantError(`Unknown intent level: ${String(task.intentLevel)}`);
  if (!Number.isSafeInteger(task.revision) || task.revision < 1) {
    throw new DomainInvariantError("Task revision must be a positive integer");
  }
  if (!Number.isSafeInteger(task.createdAt) || !Number.isSafeInteger(task.updatedAt) || task.updatedAt < task.createdAt) {
    throw new DomainInvariantError("Task timestamps are invalid");
  }
  const updateIds = new Set<string>();
  for (const update of task.openUpdates) {
    assertAttentionShape(update);
    if (updateIds.has(update.id)) throw new DomainInvariantError(`Update ${update.id} is duplicated`);
    updateIds.add(update.id);
  }
  if (task.activeAttention) {
    assertAttentionShape(task.activeAttention);
    if (updateIds.has(task.activeAttention.id)) {
      throw new DomainInvariantError(`Attention ${task.activeAttention.id} is duplicated`);
    }
    const expected = task.activeAttention.class === "decision" ? "waiting_decision" : "waiting_review";
    if (task.status !== expected) {
      throw new DomainInvariantError(`Active ${task.activeAttention.class} attention requires status ${expected}`);
    }
  } else if (task.status === "waiting_decision" || task.status === "waiting_review") {
    throw new DomainInvariantError(`${task.status} requires active blocking attention`);
  }
}

export function reduceTask(current: Task | undefined, event: EventEnvelope): Task {
  if (!current) {
    if (event.type !== "task.created") {
      throw new DomainInvariantError(`First event for task ${event.taskId} must be task.created`);
    }
    if (event.sequence !== 1) throw new DomainInvariantError("A new task must begin at sequence 1");
    requireText(event.taskId, "Task id");
    requireText(event.payload.title, "Task title");
    requireText(event.payload.intent, "Task intent");
    if (!isIntentLevel(event.payload.intentLevel)) throw new DomainInvariantError("Task intent level is invalid");
    const created: Task = {
      id: event.taskId,
      title: event.payload.title.trim(),
      intent: event.payload.intent.trim(),
      intentLevel: event.payload.intentLevel,
      status: "draft",
      revision: 1,
      createdAt: event.occurredAt,
      updatedAt: event.occurredAt,
      activeAttention: null,
      openUpdates: [],
    };
    assertTaskInvariants(created);
    return created;
  }

  if (event.taskId !== current.id) throw new DomainInvariantError("Event task id does not match the projection");
  if (event.sequence !== current.revision + 1) {
    throw new DomainInvariantError(`Expected event sequence ${current.revision + 1}, received ${event.sequence}`);
  }
  if (event.type === "task.created") throw new DomainInvariantError(`Task ${current.id} already exists`);

  let next: Task;
  switch (event.type) {
    case "task.state_changed": {
      if (current.activeAttention) {
        throw new DomainInvariantError("Resolve, supersede, or withdraw blocking attention before changing task state");
      }
      if (event.payload.to === "waiting_decision" || event.payload.to === "waiting_review") {
        throw new DomainInvariantError("Waiting states are entered by raising blocking attention");
      }
      if (!ALLOWED_TRANSITIONS[current.status].includes(event.payload.to)) {
        throw new DomainInvariantError(`Invalid task transition ${current.status} -> ${event.payload.to}`);
      }
      next = { ...current, status: event.payload.to };
      break;
    }
    case "task.intent_changed":
      if (!isIntentLevel(event.payload.level)) throw new DomainInvariantError("Task intent level is invalid");
      if (event.payload.intent !== undefined) requireText(event.payload.intent, "Task intent");
      next = {
        ...current,
        intentLevel: event.payload.level,
        ...(event.payload.intent === undefined ? {} : { intent: event.payload.intent.trim() }),
      };
      break;
    case "task.renamed":
      requireText(event.payload.title, "Task title");
      next = { ...current, title: event.payload.title.trim() };
      break;
    case "attention.raised":
      next = raiseAttention(current, event.payload.attention, event.occurredAt);
      break;
    case "attention.resolved":
      next = current.activeAttention?.id === event.payload.attentionId
        ? resolveBlockingAttention(current, event.payload.attentionId, event.payload.resolution)
        : resolveOpenUpdate(current, event.payload.attentionId, event.payload.resolution);
      break;
    case "attention.superseded":
    case "attention.withdrawn":
      requireText(event.payload.reason, "Attention closure reason");
      next = closeAttentionWithoutResolution(current, event.payload.attentionId);
      break;
  }

  next = {
    ...next,
    revision: event.sequence,
    updatedAt: Math.max(current.updatedAt, event.occurredAt),
  };
  assertTaskInvariants(next);
  return next;
}

/** Rebuild a projection solely from its append-only event stream. */
export function replayTask(events: Iterable<EventEnvelope>): Task {
  let task: Task | undefined;
  for (const event of events) task = reduceTask(task, event);
  if (!task) throw new DomainInvariantError("Cannot replay an empty task event stream");
  return task;
}
