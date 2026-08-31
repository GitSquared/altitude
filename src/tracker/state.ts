import type { Database } from "bun:sqlite";

import type { AttentionClass } from "../core/model.ts";
import type { LinearTaskBinding } from "./linear.ts";
import type { TrackerAttentionCorrelation } from "./protocol.ts";

export type StoredLinearTaskBinding = LinearTaskBinding & Readonly<{
  appUserId?: string;
  mentionTokens: readonly string[];
}>;

type BindingRow = {
  task_id: string;
  issue_id: string;
  issue_identifier: string | null;
  agent_session_id: string | null;
  app_user_id: string | null;
  mention_tokens_json: string;
};

type CorrelationRow = {
  task_id: string;
  attention_id: string;
  attention_class: AttentionClass;
  issue_id: string;
  issue_comment_id: string;
  agent_session_id: string | null;
};

function required(value: string, label: string): string {
  const normalized = value.trim();
  if (!normalized || normalized.length > 200) {
    throw new Error(`${label} must be between 1 and 200 characters`);
  }
  return normalized;
}

function optional(value: string | undefined, label: string): string | undefined {
  return value === undefined ? undefined : required(value, label);
}

function tokens(values: readonly string[] | undefined): string[] {
  const normalized = [...new Set((values ?? ["@altitude"])
    .map((value) => required(value, "Linear mention token")))]
    .slice(0, 20);
  if (normalized.length === 0) throw new Error("At least one Linear mention token is required");
  return normalized;
}

function decodeBinding(row: BindingRow): StoredLinearTaskBinding {
  const mentionTokens = JSON.parse(row.mention_tokens_json) as unknown;
  if (!Array.isArray(mentionTokens) || !mentionTokens.every((value) => typeof value === "string")) {
    throw new Error("Stored Linear mention tokens are invalid");
  }
  return {
    taskId: row.task_id,
    issueId: row.issue_id,
    ...(row.issue_identifier === null ? {} : { issueIdentifier: row.issue_identifier }),
    ...(row.agent_session_id === null ? {} : { agentSessionId: row.agent_session_id }),
    ...(row.app_user_id === null ? {} : { appUserId: row.app_user_id }),
    mentionTokens,
  };
}

function decodeCorrelation(row: CorrelationRow): TrackerAttentionCorrelation {
  return {
    provider: "linear",
    taskId: row.task_id,
    attentionId: row.attention_id,
    attentionClass: row.attention_class,
    issueId: row.issue_id,
    issueCommentId: row.issue_comment_id,
    ...(row.agent_session_id === null ? {} : { agentSessionId: row.agent_session_id }),
  };
}

/** Node-local lookup data. No Linear token or webhook secret is accepted here. */
export class LinearTrackerStateStore {
  constructor(
    private readonly database: Database,
    private readonly now: () => number = Date.now,
  ) {}

  bind(input: LinearTaskBinding & Readonly<{
    appUserId?: string;
    mentionTokens?: readonly string[];
  }>): StoredLinearTaskBinding {
    const taskId = required(input.taskId, "Task id");
    const issueId = required(input.issueId, "Linear issue id");
    const issueIdentifier = optional(input.issueIdentifier, "Linear issue identifier");
    const agentSessionId = optional(input.agentSessionId, "Linear Agent Session id");
    const appUserId = optional(input.appUserId, "Linear app user id");
    const mentionTokens = tokens(input.mentionTokens);
    const now = this.now();
    this.database.query<unknown, [string, string, string | null, string | null, string | null, string, number, number]>(`
      INSERT INTO linear_task_bindings(
        task_id, issue_id, issue_identifier, agent_session_id, app_user_id,
        mention_tokens_json, created_at, updated_at
      ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
      ON CONFLICT(task_id) DO UPDATE SET
        issue_id = excluded.issue_id,
        issue_identifier = excluded.issue_identifier,
        agent_session_id = excluded.agent_session_id,
        app_user_id = excluded.app_user_id,
        mention_tokens_json = excluded.mention_tokens_json,
        updated_at = excluded.updated_at
    `).run(
      taskId,
      issueId,
      issueIdentifier ?? null,
      agentSessionId ?? null,
      appUserId ?? null,
      JSON.stringify(mentionTokens),
      now,
      now,
    );
    return this.getByIssueId(issueId)!;
  }

  getByIssueId(issueId: string): StoredLinearTaskBinding | undefined {
    const row = this.database.query<BindingRow, [string]>(
      "SELECT * FROM linear_task_bindings WHERE issue_id = ?1",
    ).get(required(issueId, "Linear issue id"));
    return row === null ? undefined : decodeBinding(row);
  }

  getByTaskId(taskId: string): StoredLinearTaskBinding | undefined {
    const row = this.database.query<BindingRow, [string]>(
      "SELECT * FROM linear_task_bindings WHERE task_id = ?1",
    ).get(required(taskId, "Task id"));
    return row === null ? undefined : decodeBinding(row);
  }

  recordCorrelation(input: TrackerAttentionCorrelation): TrackerAttentionCorrelation {
    if (input.provider !== "linear") throw new Error("Linear state received a non-Linear correlation");
    const binding = this.database.query<{ issue_id: string }, [string]>(
      "SELECT issue_id FROM linear_task_bindings WHERE task_id = ?1",
    ).get(required(input.taskId, "Task id"));
    if (binding === null) throw new Error(`Task ${input.taskId} has no Linear binding`);
    if (binding.issue_id !== input.issueId) {
      throw new Error("Linear attention correlation does not match the task binding");
    }
    const now = this.now();
    this.database.query<unknown, [string, string, string, string, string, string | null, number, number]>(`
      INSERT INTO linear_attention_correlations(
        task_id, attention_id, attention_class, issue_id, issue_comment_id,
        agent_session_id, created_at, updated_at
      ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
      ON CONFLICT(task_id, attention_id) DO UPDATE SET
        attention_class = excluded.attention_class,
        issue_id = excluded.issue_id,
        issue_comment_id = excluded.issue_comment_id,
        agent_session_id = excluded.agent_session_id,
        updated_at = excluded.updated_at
    `).run(
      required(input.taskId, "Task id"),
      required(input.attentionId, "Attention id"),
      input.attentionClass,
      required(input.issueId, "Linear issue id"),
      required(input.issueCommentId, "Linear issue comment id"),
      optional(input.agentSessionId, "Linear Agent Session id") ?? null,
      now,
      now,
    );
    return input;
  }

  listCorrelations(taskId: string): TrackerAttentionCorrelation[] {
    return this.database.query<CorrelationRow, [string]>(`
      SELECT task_id, attention_id, attention_class, issue_id,
             issue_comment_id, agent_session_id
      FROM linear_attention_correlations
      WHERE task_id = ?1
      ORDER BY created_at, attention_id
    `).all(required(taskId, "Task id")).map(decodeCorrelation);
  }
}
