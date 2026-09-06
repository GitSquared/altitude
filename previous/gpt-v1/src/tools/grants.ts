import { createHash, randomUUID } from "node:crypto";

import type { Database } from "bun:sqlite";

export const TASK_MCP_TOOL_NAMES = [
  "get_task",
  "read_file",
  "write_file",
  "list_files",
  "search_text",
  "run_command",
  "git_status",
  "create_pull_request",
  "inspect_pull_request",
  "record_trace",
  "report_update",
  "request_decision",
  "request_review",
] as const;

export type TaskMcpToolName = (typeof TASK_MCP_TOOL_NAMES)[number];

export interface TaskMcpGrant {
  readonly id: string;
  readonly taskId: string;
  readonly identityId: string;
  readonly agentSessionId?: string;
  readonly allowedTools: readonly TaskMcpToolName[];
  readonly issuedAt: number;
  readonly expiresAt: number;
}

export interface IssuedTaskMcpGrant extends TaskMcpGrant {
  /** Bearer secret returned only at issuance; the database stores its hash. */
  readonly token: string;
}

type GrantRow = {
  id: string;
  task_id: string;
  identity_id: string;
  agent_session_id: string | null;
  allowed_tools_json: string;
  issued_at: number;
  expires_at: number;
};

function tokenHash(token: string): string {
  return createHash("sha256").update(token, "utf8").digest("hex");
}

function requireId(value: string, label: string): void {
  if (!/^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/u.test(value)) throw new Error(`${label} is invalid`);
}

function normalizeTools(tools: readonly TaskMcpToolName[]): TaskMcpToolName[] {
  if (tools.length === 0) throw new Error("At least one task MCP tool is required");
  const allowed = new Set<string>(TASK_MCP_TOOL_NAMES);
  const unique = [...new Set(tools)];
  if (unique.some((tool) => !allowed.has(tool))) throw new Error("Task MCP grant contains an unknown tool");
  return unique.sort((left, right) => left.localeCompare(right));
}

function decodeGrant(row: GrantRow): TaskMcpGrant {
  const tools = JSON.parse(row.allowed_tools_json) as unknown;
  if (!Array.isArray(tools) || tools.some((tool) => typeof tool !== "string")) {
    throw new Error(`Task MCP grant ${row.id} has invalid allowed tools`);
  }
  return {
    id: row.id,
    taskId: row.task_id,
    identityId: row.identity_id,
    ...(row.agent_session_id === null ? {} : { agentSessionId: row.agent_session_id }),
    allowedTools: normalizeTools(tools as TaskMcpToolName[]),
    issuedAt: row.issued_at,
    expiresAt: row.expires_at,
  };
}

export class TaskMcpGrantStore {
  constructor(
    private readonly database: Database,
    private readonly now: () => number = () => Date.now(),
  ) {}

  issue(input: {
    readonly taskId: string;
    readonly identityId: string;
    readonly agentSessionId?: string;
    readonly allowedTools?: readonly TaskMcpToolName[];
    readonly ttlMs?: number;
  }): IssuedTaskMcpGrant {
    requireId(input.taskId, "Task id");
    requireId(input.identityId, "Identity id");
    if (input.agentSessionId !== undefined) requireId(input.agentSessionId, "Agent session id");
    const allowedTools = normalizeTools(input.allowedTools ?? TASK_MCP_TOOL_NAMES);
    const ttlMs = input.ttlMs ?? 24 * 60 * 60_000;
    if (!Number.isSafeInteger(ttlMs) || ttlMs < 60_000 || ttlMs > 30 * 24 * 60 * 60_000) {
      throw new Error("Task MCP grant lifetime must be between one minute and thirty days");
    }
    const issuedAt = this.now();
    const grant: IssuedTaskMcpGrant = {
      id: randomUUID(),
      token: `${randomUUID()}${randomUUID()}`.replaceAll("-", ""),
      taskId: input.taskId,
      identityId: input.identityId,
      ...(input.agentSessionId === undefined ? {} : { agentSessionId: input.agentSessionId }),
      allowedTools,
      issuedAt,
      expiresAt: issuedAt + ttlMs,
    };
    this.database.query<unknown, [string, string, string, string, string | null, string, number, number]>(`
      INSERT INTO task_mcp_grants(
        id, token_hash, task_id, identity_id, agent_session_id,
        allowed_tools_json, issued_at, expires_at
      ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
    `).run(
      grant.id,
      tokenHash(grant.token),
      grant.taskId,
      grant.identityId,
      grant.agentSessionId ?? null,
      JSON.stringify(grant.allowedTools),
      grant.issuedAt,
      grant.expiresAt,
    );
    return grant;
  }

  authenticate(token: string): TaskMcpGrant | undefined {
    if (!/^[a-f0-9]{64}$/u.test(token)) return undefined;
    const row = this.database.query<GrantRow, [string, number]>(`
      SELECT id, task_id, identity_id, agent_session_id,
             allowed_tools_json, issued_at, expires_at
      FROM task_mcp_grants
      WHERE token_hash = ?1 AND revoked_at IS NULL AND expires_at > ?2
    `).get(tokenHash(token), this.now());
    return row === null ? undefined : decodeGrant(row);
  }

  revoke(grantId: string): boolean {
    requireId(grantId, "Grant id");
    return this.database.query<unknown, [number, string]>(`
      UPDATE task_mcp_grants SET revoked_at = ?1
      WHERE id = ?2 AND revoked_at IS NULL
    `).run(this.now(), grantId).changes === 1;
  }

  purgeExpired(): number {
    return this.database.query<unknown, [number]>(`
      DELETE FROM task_mcp_grants WHERE expires_at <= ?1 OR revoked_at IS NOT NULL
    `).run(this.now()).changes;
  }
}

export function claudeTaskMcpConfig(endpoint: string, token: string): unknown {
  const url = new URL(endpoint);
  if (!["http:", "https:"].includes(url.protocol) || url.username || url.password) {
    throw new Error("Task MCP endpoint must be credential-free HTTP(S)");
  }
  if (!/^[a-f0-9]{64}$/u.test(token)) throw new Error("Task MCP token is invalid");
  return {
    mcpServers: {
      altitude: {
        type: "http",
        url: url.toString(),
        headers: { Authorization: `Bearer ${token}` },
      },
    },
  };
}
