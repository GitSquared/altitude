export type AgentWriterMode = "headless" | "interactive";

export interface AgentWriterLease {
  readonly taskId: string;
  readonly sessionId: string;
  readonly holderId: string;
  readonly mode: AgentWriterMode;
  readonly acquiredAtMs: number;
  /** Monotonic per task; prevents an old holder from releasing a newer lease. */
  readonly revision: number;
}

export type AgentWriterLeaseCandidate = Omit<AgentWriterLease, "revision">;

/** Implement this on the task ledger for crash-safe, cross-process ownership. */
export interface AgentWriterLeaseStore {
  acquire(candidate: AgentWriterLeaseCandidate): Promise<AgentWriterLease | undefined>;
  current(taskId: string): Promise<AgentWriterLease | undefined>;
  release(lease: AgentWriterLease): Promise<boolean>;
}

export class AgentWriterLeaseConflictError extends Error {
  constructor(readonly currentLease: AgentWriterLease) {
    super(
      `Task ${currentLease.taskId} already has a ${currentLease.mode} writer for session ${currentLease.sessionId}`,
    );
    this.name = "AgentWriterLeaseConflictError";
  }
}

/** Useful in tests and one-process prototypes; production should use the task ledger. */
export class InMemoryAgentWriterLeaseStore implements AgentWriterLeaseStore {
  readonly #leases = new Map<string, AgentWriterLease>();
  readonly #revisions = new Map<string, number>();

  acquire(candidate: AgentWriterLeaseCandidate): Promise<AgentWriterLease | undefined> {
    if (this.#leases.has(candidate.taskId)) return Promise.resolve(undefined);
    const revision = (this.#revisions.get(candidate.taskId) ?? 0) + 1;
    this.#revisions.set(candidate.taskId, revision);
    const lease: AgentWriterLease = { ...candidate, revision };
    this.#leases.set(candidate.taskId, lease);
    return Promise.resolve(lease);
  }

  current(taskId: string): Promise<AgentWriterLease | undefined> {
    return Promise.resolve(this.#leases.get(taskId));
  }

  release(lease: AgentWriterLease): Promise<boolean> {
    const current = this.#leases.get(lease.taskId);
    if (current?.holderId !== lease.holderId || current.revision !== lease.revision) {
      return Promise.resolve(false);
    }
    this.#leases.delete(lease.taskId);
    return Promise.resolve(true);
  }
}
