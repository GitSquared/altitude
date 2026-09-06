import { randomUUID } from "node:crypto";

import {
  AGENT_RETRY_ACTION_KIND,
  parseAgentRetryPayload,
  type AgentRetryPayload,
} from "../run/retry.ts";
import type { OutboxMessage, ScheduledAction, TaskStore } from "../storage/task-store.ts";

const DEFAULT_POLL_MS = 1_000;
const DEFAULT_LEASE_MS = 5 * 60_000;
const DEFAULT_MAX_ATTEMPTS = 8;
const MAX_WORKER_BACKOFF_MS = 5 * 60_000;

export type AgentRetryExecutor = (
  taskId: string,
  payload: AgentRetryPayload,
) => Promise<"executed" | "skipped">;

export interface ScheduledActionKindHandler<Payload> {
  readonly kind: string;
  parse(payload: unknown): Payload;
  execute(
    taskId: string,
    payload: Payload,
  ): Promise<"executed" | "skipped">;
}

export interface NodeWorkerOptions {
  readonly workerId?: string;
  readonly pollMs?: number;
  readonly leaseMs?: number;
  readonly maxAttempts?: number;
  readonly claimLimit?: number;
  readonly now?: () => number;
  readonly onError?: (error: unknown) => void;
}

type ScheduledActionStore = Pick<
  TaskStore,
  "claimScheduledActionsByKind" | "completeScheduledAction"
    | "retryScheduledAction" | "deadLetterScheduledAction"
    | "renewScheduledActionLease" | "recoverExpiredLeases"
>;

type OutboxStore = Pick<
  TaskStore,
  "claimOutboxByTopic" | "deliverOutbox" | "retryOutbox"
    | "renewOutboxLease" | "deadLetterOutbox" | "recoverExpiredLeases"
>;

export interface OutboxTopicHandler<Payload> {
  readonly topic: string;
  parse(payload: unknown): Payload;
  execute(taskId: string, payload: Payload): Promise<"executed" | "skipped">;
}

export interface NodeOutboxWorkerOptions {
  readonly workerId?: string;
  readonly pollMs?: number;
  readonly leaseMs?: number;
  readonly maxAttempts?: number;
  readonly claimLimit?: number;
  readonly now?: () => number;
  readonly onError?: (error: unknown) => void;
}

function positiveInteger(
  value: number,
  label: string,
  maximum = Number.MAX_SAFE_INTEGER,
): number {
  if (!Number.isSafeInteger(value) || value < 1 || value > maximum) {
    throw new Error(label + " is invalid");
  }
  return value;
}

function message(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function workerRetryAt(now: number, attempts: number): number {
  const delay = Math.min(
    MAX_WORKER_BACKOFF_MS,
    5_000 * 2 ** Math.min(Math.max(0, attempts - 1), 10),
  );
  return now + delay;
}

/** Delivers one durable outbox topic with bounded retries and restart recovery. */
export class NodeOutboxTopicWorker<Payload> {
  readonly workerId: string;
  readonly #pollMs: number;
  readonly #leaseMs: number;
  readonly #maxAttempts: number;
  readonly #claimLimit: number;
  readonly #now: () => number;
  readonly #onError: (error: unknown) => void;
  #timer: ReturnType<typeof setInterval> | undefined;
  #active: Promise<number> | undefined;
  #closed = false;

  constructor(
    private readonly store: OutboxStore,
    private readonly handler: OutboxTopicHandler<Payload>,
    options: NodeOutboxWorkerOptions = {},
  ) {
    if (!handler.topic.trim()) throw new Error("Outbox topic is required");
    this.workerId = options.workerId
      ?? `altitude-node-${handler.topic}-${randomUUID()}`;
    if (!this.workerId.trim()) throw new Error("Outbox worker id is required");
    this.#pollMs = positiveInteger(options.pollMs ?? DEFAULT_POLL_MS, "Outbox poll interval");
    this.#leaseMs = positiveInteger(options.leaseMs ?? DEFAULT_LEASE_MS, "Outbox lease duration");
    this.#maxAttempts = positiveInteger(
      options.maxAttempts ?? DEFAULT_MAX_ATTEMPTS,
      "Outbox maximum attempts",
      100,
    );
    this.#claimLimit = positiveInteger(options.claimLimit ?? 4, "Outbox claim limit", 100);
    this.#now = options.now ?? Date.now;
    this.#onError = options.onError ?? (() => undefined);
  }

  start(): void {
    if (this.#closed) throw new Error("Outbox worker is closed");
    if (this.#timer !== undefined) return;
    this.store.recoverExpiredLeases(this.#now());
    this.#timer = setInterval(() => this.#kick(), this.#pollMs);
    this.#kick();
  }

  async close(): Promise<void> {
    if (this.#closed) return;
    this.#closed = true;
    if (this.#timer !== undefined) clearInterval(this.#timer);
    this.#timer = undefined;
    await this.#active?.catch(() => undefined);
  }

  async runOnce(): Promise<number> {
    if (this.#closed) return 0;
    const messages = this.store.claimOutboxByTopic(this.handler.topic, {
      workerId: this.workerId,
      now: this.#now(),
      leaseMs: this.#leaseMs,
      limit: this.#claimLimit,
    });
    const results = await Promise.allSettled(messages.map(async (item) => await this.#execute(item)));
    for (const result of results) {
      if (result.status === "rejected") this.#onError(result.reason);
    }
    return messages.length;
  }

  #kick(): void {
    if (this.#closed || this.#active !== undefined) return;
    this.#active = this.runOnce()
      .catch((error) => {
        this.#onError(error);
        return 0;
      })
      .finally(() => {
        this.#active = undefined;
      });
  }

  async #execute(item: OutboxMessage): Promise<void> {
    let retryable = true;
    let renewalError: unknown;
    const renewalEvery = Math.max(1_000, Math.min(60_000, Math.floor(this.#leaseMs / 3)));
    const renewal = setInterval(() => {
      try {
        this.store.renewOutboxLease(item.id, this.workerId, this.#now(), this.#leaseMs);
      } catch (error) {
        renewalError = error;
      }
    }, renewalEvery);
    try {
      if (item.topic !== this.handler.topic) {
        retryable = false;
        throw new Error("Outbox topic changed after it was claimed");
      }
      let payload: Payload;
      try {
        payload = this.handler.parse(item.payload);
      } catch (error) {
        retryable = false;
        throw error;
      }
      await this.handler.execute(item.taskId, payload);
      if (renewalError !== undefined) throw renewalError;
      this.store.deliverOutbox(item.id, this.workerId, this.#now());
    } catch (error) {
      const detail = message(error);
      if (!retryable || item.attempts >= this.#maxAttempts) {
        this.store.deadLetterOutbox(item.id, this.workerId, detail, this.#now());
      } else {
        const now = this.#now();
        this.store.retryOutbox(item.id, this.workerId, workerRetryAt(now, item.attempts), detail);
      }
      this.#onError(error);
    } finally {
      clearInterval(renewal);
    }
  }
}

/** Claims one durable action kind and keeps its lease alive during long work. */
export class NodeScheduledActionKindWorker<Payload> {
  readonly workerId: string;
  readonly #pollMs: number;
  readonly #leaseMs: number;
  readonly #maxAttempts: number;
  readonly #claimLimit: number;
  readonly #now: () => number;
  readonly #onError: (error: unknown) => void;
  #timer: ReturnType<typeof setInterval> | undefined;
  #active: Promise<number> | undefined;
  #closed = false;

  constructor(
    private readonly store: ScheduledActionStore,
    private readonly handler: ScheduledActionKindHandler<Payload>,
    options: NodeWorkerOptions = {},
  ) {
    if (!handler.kind.trim()) throw new Error("Scheduled action kind is required");
    this.workerId = options.workerId
      ?? `altitude-node-${handler.kind}-${randomUUID()}`;
    if (!this.workerId.trim()) {
      throw new Error("Scheduled action worker id is required");
    }
    this.#pollMs = positiveInteger(
      options.pollMs ?? DEFAULT_POLL_MS,
      "Scheduled action poll interval",
    );
    this.#leaseMs = positiveInteger(
      options.leaseMs ?? DEFAULT_LEASE_MS,
      "Scheduled action lease duration",
    );
    this.#maxAttempts = positiveInteger(
      options.maxAttempts ?? DEFAULT_MAX_ATTEMPTS,
      "Scheduled action maximum attempts",
      100,
    );
    this.#claimLimit = positiveInteger(
      options.claimLimit ?? 4,
      "Scheduled action claim limit",
      100,
    );
    this.#now = options.now ?? Date.now;
    this.#onError = options.onError ?? (() => undefined);
  }

  start(): void {
    if (this.#closed) throw new Error("Scheduled action worker is closed");
    if (this.#timer !== undefined) return;
    this.store.recoverExpiredLeases(this.#now());
    this.#timer = setInterval(() => this.#kick(), this.#pollMs);
    this.#kick();
  }

  async close(): Promise<void> {
    if (this.#closed) return;
    this.#closed = true;
    if (this.#timer !== undefined) clearInterval(this.#timer);
    this.#timer = undefined;
    await this.#active?.catch(() => undefined);
  }

  async runOnce(): Promise<number> {
    if (this.#closed) return 0;
    const actions = this.store.claimScheduledActionsByKind(this.handler.kind, {
      workerId: this.workerId,
      now: this.#now(),
      leaseMs: this.#leaseMs,
      limit: this.#claimLimit,
    });
    const results = await Promise.allSettled(
      actions.map(async (action) => await this.#execute(action)),
    );
    for (const result of results) {
      if (result.status === "rejected") this.#onError(result.reason);
    }
    return actions.length;
  }

  #kick(): void {
    if (this.#closed || this.#active !== undefined) return;
    this.#active = this.runOnce()
      .catch((error) => {
        this.#onError(error);
        return 0;
      })
      .finally(() => {
        this.#active = undefined;
      });
  }

  async #execute(action: ScheduledAction): Promise<void> {
    let renewalError: unknown;
    let retryable = true;
    const renewalEvery = Math.max(
      1_000,
      Math.min(60_000, Math.floor(this.#leaseMs / 3)),
    );
    const renewal = setInterval(() => {
      try {
        this.store.renewScheduledActionLease(
          action.id,
          this.workerId,
          this.#now(),
          this.#leaseMs,
        );
      } catch (error) {
        renewalError = error;
      }
    }, renewalEvery);
    try {
      if (action.kind !== this.handler.kind) {
        retryable = false;
        throw new Error("Scheduled action kind changed after it was claimed");
      }
      let payload: Payload;
      try {
        payload = this.handler.parse(action.payload);
      } catch (error) {
        retryable = false;
        throw error;
      }
      await this.handler.execute(action.taskId, payload);
      if (renewalError !== undefined) throw renewalError;
      this.store.completeScheduledAction(
        action.id,
        this.workerId,
        this.#now(),
      );
    } catch (error) {
      const detail = message(error);
      if (!retryable || action.attempts >= this.#maxAttempts) {
        this.store.deadLetterScheduledAction(
          action.id,
          this.workerId,
          detail,
          this.#now(),
        );
      } else {
        const now = this.#now();
        this.store.retryScheduledAction(
          action.id,
          this.workerId,
          workerRetryAt(now, action.attempts),
          detail,
        );
      }
      this.#onError(error);
    } finally {
      clearInterval(renewal);
    }
  }
}

export class AgentRetryWorker {
  readonly #worker: NodeScheduledActionKindWorker<AgentRetryPayload>;

  constructor(
    store: ScheduledActionStore,
    executeAgentRetry: AgentRetryExecutor,
    options: NodeWorkerOptions = {},
  ) {
    this.#worker = new NodeScheduledActionKindWorker(store, {
      kind: AGENT_RETRY_ACTION_KIND,
      parse: parseAgentRetryPayload,
      execute: executeAgentRetry,
    }, options);
  }

  get workerId(): string {
    return this.#worker.workerId;
  }

  start(): void {
    this.#worker.start();
  }

  close(): Promise<void> {
    return this.#worker.close();
  }

  runOnce(): Promise<number> {
    return this.#worker.runOnce();
  }
}
