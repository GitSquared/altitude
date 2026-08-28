import type { Database } from "bun:sqlite";

import type {
  AgentWriterLease,
  AgentWriterLeaseCandidate,
  AgentWriterLeaseStore,
  AgentWriterMode,
} from "../agent/writer-lease.ts";

type AgentWriterLeaseRow = Readonly<{
  task_id: string;
  session_id: string;
  holder_id: string;
  mode: AgentWriterMode;
  acquired_at: number;
  revision: number;
}>;

type AgentWriterRevisionRow = Readonly<{
  revision: number;
  holder_id: string | null;
}>;

function requireNonEmpty(value: string, label: string): void {
  if (!value.trim()) throw new Error(`${label} must not be empty`);
}

function requireTime(value: number, label: string): void {
  if (!Number.isSafeInteger(value) || value < 0) {
    throw new Error(`${label} must be a non-negative integer timestamp`);
  }
}

function requireRevision(value: number): void {
  if (!Number.isSafeInteger(value) || value < 1) {
    throw new Error("Writer lease revision must be a positive integer");
  }
}

function decodeLease(row: AgentWriterLeaseRow | null): AgentWriterLease | undefined {
  if (!row) return undefined;
  return {
    taskId: row.task_id,
    sessionId: row.session_id,
    holderId: row.holder_id,
    mode: row.mode,
    acquiredAtMs: row.acquired_at,
    revision: row.revision,
  };
}

/**
 * Crash-safe writer ownership backed by the task ledger.
 *
 * Released rows deliberately remain in the table with their last revision. A
 * later acquisition increments that revision, making leases fencing tokens
 * rather than advisory locks.
 */
export class SqliteAgentWriterLeaseStore implements AgentWriterLeaseStore {
  constructor(readonly database: Database) {}

  acquire(candidate: AgentWriterLeaseCandidate): Promise<AgentWriterLease | undefined> {
    this.validateCandidate(candidate);

    const lease = this.database.transaction(() => {
      const prior = this.database.query<AgentWriterRevisionRow, [string]>(`
        SELECT revision, holder_id
        FROM agent_writer_leases
        WHERE task_id = ?1
      `).get(candidate.taskId);

      if (prior?.holder_id !== null && prior?.holder_id !== undefined) return undefined;

      const revision = (prior?.revision ?? 0) + 1;
      if (prior === null) {
        this.database.query<unknown, [string, string, string, AgentWriterMode, number, number]>(`
          INSERT INTO agent_writer_leases(
            task_id, session_id, holder_id, mode, acquired_at, revision
          ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
        `).run(
          candidate.taskId,
          candidate.sessionId,
          candidate.holderId,
          candidate.mode,
          candidate.acquiredAtMs,
          revision,
        );
      } else {
        const changed = this.database.query<unknown, [string, string, AgentWriterMode, number, number, string, number]>(`
          UPDATE agent_writer_leases
          SET session_id = ?1,
              holder_id = ?2,
              mode = ?3,
              acquired_at = ?4,
              revision = ?5
          WHERE task_id = ?6
            AND revision = ?7
            AND holder_id IS NULL
        `).run(
          candidate.sessionId,
          candidate.holderId,
          candidate.mode,
          candidate.acquiredAtMs,
          revision,
          candidate.taskId,
          prior.revision,
        ).changes;
        if (changed !== 1) return undefined;
      }

      return { ...candidate, revision };
    }).immediate();

    return Promise.resolve(lease);
  }

  current(taskId: string): Promise<AgentWriterLease | undefined> {
    requireNonEmpty(taskId, "Task id");
    return Promise.resolve(decodeLease(this.database.query<AgentWriterLeaseRow, [string]>(`
      SELECT task_id, session_id, holder_id, mode, acquired_at, revision
      FROM agent_writer_leases
      WHERE task_id = ?1 AND holder_id IS NOT NULL
    `).get(taskId)));
  }

  release(lease: AgentWriterLease): Promise<boolean> {
    this.validateLease(lease);
    return Promise.resolve(this.clearExact(lease.taskId, lease.revision, lease));
  }

  /**
   * Clear an abandoned writer using its fencing revision.
   *
   * This method does not decide whether a process is dead. Callers must obtain
   * independent liveness evidence before recovery. A stale revision can never
   * clear a newer acquisition.
   */
  expire(taskId: string, expectedRevision: number): Promise<boolean> {
    requireNonEmpty(taskId, "Task id");
    requireRevision(expectedRevision);
    return Promise.resolve(this.clearExact(taskId, expectedRevision));
  }

  private clearExact(taskId: string, revision: number, lease?: AgentWriterLease): boolean {
    if (lease === undefined) {
      return this.database.query<unknown, [string, number]>(`
        UPDATE agent_writer_leases
        SET session_id = NULL,
            holder_id = NULL,
            mode = NULL,
            acquired_at = NULL
        WHERE task_id = ?1
          AND revision = ?2
          AND holder_id IS NOT NULL
      `).run(taskId, revision).changes === 1;
    }

    return this.database
      .query<unknown, [string, number, string, string, AgentWriterMode, number]>(`
        UPDATE agent_writer_leases
        SET session_id = NULL,
            holder_id = NULL,
            mode = NULL,
            acquired_at = NULL
        WHERE task_id = ?1
          AND revision = ?2
          AND session_id = ?3
          AND holder_id = ?4
          AND mode = ?5
          AND acquired_at = ?6
      `)
      .run(
        lease.taskId,
        lease.revision,
        lease.sessionId,
        lease.holderId,
        lease.mode,
        lease.acquiredAtMs,
      ).changes === 1;
  }

  private validateCandidate(candidate: AgentWriterLeaseCandidate): void {
    requireNonEmpty(candidate.taskId, "Task id");
    requireNonEmpty(candidate.sessionId, "Session id");
    requireNonEmpty(candidate.holderId, "Writer holder id");
    requireTime(candidate.acquiredAtMs, "Writer lease acquisition time");
  }

  private validateLease(lease: AgentWriterLease): void {
    this.validateCandidate(lease);
    requireRevision(lease.revision);
  }
}
