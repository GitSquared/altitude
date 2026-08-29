import { createHash, randomUUID } from "node:crypto";

import type { Database } from "bun:sqlite";

import type { AgentMcpBoundary, AgentRunOutcome } from "../agent/protocol.ts";
import type { PortableTaskAgentSession } from "../run/task-state.ts";
import { ALL_CLAUDE_ALTITUDE_TOOLS } from "../tools/mcp.ts";

export interface StoredTaskAgentSession {
  readonly taskId: string;
  readonly sessionId: string;
  readonly adapter: "claude-code-cli";
  readonly identityId: string;
  readonly projectDirectoryName: string;
  readonly controlDirectory?: string;
  readonly mcp?: AgentMcpBoundary;
  readonly grantId?: string;
  readonly runCount: number;
  readonly consecutiveRateLimits: number;
  readonly lastOutcome?: AgentRunOutcome;
  readonly createdAt: number;
  readonly updatedAt: number;
}

type SessionRow = {
  task_id: string;
  adapter: string;
  session_id: string;
  identity_id: string;
  project_directory_name: string;
  control_directory: string | null;
  mcp_config_path: string | null;
  grant_id: string | null;
  run_count: number;
  consecutive_rate_limits: number;
  last_outcome: AgentRunOutcome | null;
  created_at: number;
  updated_at: number;
};

function requireId(value: string, label: string): void {
  if (!/^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/u.test(value)) throw new Error(`${label} is invalid`);
}

function projectDirectory(taskId: string): string {
  return `altitude-${createHash("sha256").update(taskId, "utf8").digest("hex").slice(0, 24)}`;
}

const AGENT_OUTCOMES = new Set<AgentRunOutcome>([
  "success",
  "error",
  "yielded",
  "stopped",
  "rate_limited",
  "protocol_error",
]);

function decode(row: SessionRow | null): StoredTaskAgentSession | undefined {
  if (row === null) return undefined;
  if (row.adapter !== "claude-code-cli") throw new Error(`Unsupported stored agent adapter ${row.adapter}`);
  if ((row.control_directory === null) !== (row.mcp_config_path === null)
    || (row.control_directory === null) !== (row.grant_id === null)) {
    throw new Error(`Stored task session ${row.task_id} has an incomplete MCP binding`);
  }
  return {
    taskId: row.task_id,
    sessionId: row.session_id,
    adapter: "claude-code-cli",
    identityId: row.identity_id,
    projectDirectoryName: row.project_directory_name,
    ...(row.control_directory === null ? {} : {
      controlDirectory: row.control_directory,
      mcp: {
        configPath: row.mcp_config_path!,
        allowedTools: ALL_CLAUDE_ALTITUDE_TOOLS,
      },
    }),
    ...(row.grant_id === null ? {} : { grantId: row.grant_id }),
    runCount: row.run_count,
    consecutiveRateLimits: row.consecutive_rate_limits,
    ...(row.last_outcome === null ? {} : { lastOutcome: row.last_outcome }),
    createdAt: row.created_at,
    updatedAt: row.updated_at,
  };
}

export class TaskAgentSessionStore {
  constructor(
    private readonly database: Database,
    private readonly now: () => number = () => Date.now(),
    private readonly createSessionId: () => string = () => randomUUID(),
  ) {}

  get(taskId: string): StoredTaskAgentSession | undefined {
    requireId(taskId, "Task id");
    return decode(this.database.query<SessionRow, [string]>(`
      SELECT task_id, adapter, session_id, identity_id, project_directory_name,
             control_directory, mcp_config_path, grant_id, run_count,
             consecutive_rate_limits, last_outcome, created_at, updated_at
      FROM task_agent_sessions WHERE task_id = ?1
    `).get(taskId));
  }

  ensure(taskId: string, identityId: string): StoredTaskAgentSession {
    requireId(taskId, "Task id");
    requireId(identityId, "Identity id");
    const existing = this.get(taskId);
    if (existing !== undefined) {
      if (existing.identityId !== identityId) {
        throw new Error(`Task ${taskId} is bound to identity ${existing.identityId}, not ${identityId}`);
      }
      return existing;
    }
    const now = this.now();
    const sessionId = this.createSessionId();
    if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/iu.test(sessionId)) {
      throw new Error("Agent session id must be a UUID");
    }
    this.database.query<unknown, [string, string, string, string, string, number, number]>(`
      INSERT INTO task_agent_sessions(
        task_id, adapter, session_id, identity_id, project_directory_name,
        created_at, updated_at
      ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
    `).run(taskId, "claude-code-cli", sessionId, identityId, projectDirectory(taskId), now, now);
    return this.get(taskId)!;
  }

  importPortable(
    snapshot: PortableTaskAgentSession,
    targetIdentityId: string,
  ): StoredTaskAgentSession {
    requireId(snapshot.taskId, "Task id");
    requireId(targetIdentityId, "Identity id");
    if (snapshot.schema !== "altitude.agent-session/v1"
      || snapshot.adapter !== "claude-code-cli"
      || !/^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/iu.test(snapshot.sessionId)
      || !/^[A-Za-z0-9._-]+$/u.test(snapshot.projectDirectoryName)
      || !Number.isSafeInteger(snapshot.runCount) || snapshot.runCount < 0
      || !Number.isSafeInteger(snapshot.consecutiveRateLimits)
      || snapshot.consecutiveRateLimits < 0
      || !Number.isSafeInteger(snapshot.createdAt) || snapshot.createdAt < 0
      || !Number.isSafeInteger(snapshot.updatedAt)
      || snapshot.updatedAt < snapshot.createdAt
      || (snapshot.lastOutcome !== undefined
        && !AGENT_OUTCOMES.has(snapshot.lastOutcome))) {
      throw new Error("Portable task agent session is invalid");
    }
    return this.database.transaction(() => {
      const existing = this.get(snapshot.taskId);
      if (existing === undefined) {
        this.database.query<unknown, [string, string, string, string, number, number, AgentRunOutcome | null, number, number]>(`
          INSERT INTO task_agent_sessions(
            task_id, adapter, session_id, identity_id,
            project_directory_name, run_count, consecutive_rate_limits,
            last_outcome, created_at, updated_at
          ) VALUES (?1, 'claude-code-cli', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
        `).run(
          snapshot.taskId,
          snapshot.sessionId,
          targetIdentityId,
          snapshot.projectDirectoryName,
          snapshot.runCount,
          snapshot.consecutiveRateLimits,
          snapshot.lastOutcome ?? null,
          snapshot.createdAt,
          snapshot.updatedAt,
        );
        return this.get(snapshot.taskId)!;
      }
      if (existing.sessionId !== snapshot.sessionId
        || existing.adapter !== snapshot.adapter
        || existing.projectDirectoryName !== snapshot.projectDirectoryName) {
        throw new Error(
          `Target task ${snapshot.taskId} is bound to another agent session`,
        );
      }
      if (existing.runCount === snapshot.runCount
        && existing.lastOutcome !== undefined
        && snapshot.lastOutcome !== undefined
        && existing.lastOutcome !== snapshot.lastOutcome) {
        throw new Error("Target agent session outcome diverges from the capsule");
      }
      if (existing.grantId !== undefined) {
        this.database.query<unknown, [number, string]>(`
          UPDATE task_mcp_grants
          SET revoked_at = COALESCE(revoked_at, ?1)
          WHERE id = ?2
        `).run(this.now(), existing.grantId);
      }
      const existingIsAhead = existing.runCount > snapshot.runCount;
      const sameRun = existing.runCount === snapshot.runCount;
      this.database.query<unknown, [string, number, number, AgentRunOutcome | null, number, number, string]>(`
        UPDATE task_agent_sessions
        SET identity_id = ?1,
            control_directory = NULL,
            mcp_config_path = NULL,
            grant_id = NULL,
            run_count = ?2,
            consecutive_rate_limits = ?3,
            last_outcome = ?4,
            created_at = ?5,
            updated_at = ?6
        WHERE task_id = ?7
      `).run(
        targetIdentityId,
        existingIsAhead ? existing.runCount : snapshot.runCount,
        existingIsAhead
          ? existing.consecutiveRateLimits
          : sameRun
          ? Math.max(existing.consecutiveRateLimits, snapshot.consecutiveRateLimits)
          : snapshot.consecutiveRateLimits,
        (existingIsAhead ? existing.lastOutcome : snapshot.lastOutcome)
          ?? existing.lastOutcome
          ?? null,
        Math.min(existing.createdAt, snapshot.createdAt),
        Math.max(existing.updatedAt, snapshot.updatedAt),
        snapshot.taskId,
      );
      return this.get(snapshot.taskId)!;
    }).immediate();
  }

  bindMcp(input: {
    readonly taskId: string;
    readonly grantId: string;
    readonly controlDirectory: string;
    readonly configPath: string;
  }): StoredTaskAgentSession {
    requireId(input.taskId, "Task id");
    requireId(input.grantId, "Grant id");
    if (!input.controlDirectory.startsWith("/identity/control/tasks/") || !input.configPath.startsWith(`${input.controlDirectory}/`)) {
      throw new Error("Task MCP binding must remain in its identity control directory");
    }
    const changed = this.database.query<unknown, [string, string, string, number, string]>(`
      UPDATE task_agent_sessions
      SET grant_id = ?1, control_directory = ?2, mcp_config_path = ?3, updated_at = ?4
      WHERE task_id = ?5
    `).run(input.grantId, input.controlDirectory, input.configPath, this.now(), input.taskId).changes;
    if (changed !== 1) throw new Error(`Task ${input.taskId} has no agent session`);
    return this.get(input.taskId)!;
  }

  markRunStarted(taskId: string): StoredTaskAgentSession {
    requireId(taskId, "Task id");
    const changed = this.database.query<unknown, [number, string]>(`
      UPDATE task_agent_sessions
      SET run_count = run_count + 1, updated_at = ?1
      WHERE task_id = ?2 AND grant_id IS NOT NULL
    `).run(this.now(), taskId).changes;
    if (changed !== 1) throw new Error(`Task ${taskId} has no bound agent session`);
    return this.get(taskId)!;
  }

  recordOutcome(taskId: string, outcome: AgentRunOutcome): StoredTaskAgentSession {
    requireId(taskId, "Task id");
    const changed = this.database.query<unknown, [AgentRunOutcome, AgentRunOutcome, number, string]>(`
      UPDATE task_agent_sessions
      SET last_outcome = ?1,
          consecutive_rate_limits = CASE WHEN ?2 = 'rate_limited' THEN consecutive_rate_limits + 1 ELSE 0 END,
          updated_at = ?3
      WHERE task_id = ?4
    `).run(outcome, outcome, this.now(), taskId).changes;
    if (changed !== 1) throw new Error(`Task ${taskId} has no agent session`);
    return this.get(taskId)!;
  }
}
