import { createHash } from "node:crypto";

import {
  McpServer,
  createMcpHandler,
  hostHeaderValidationResponse,
  originValidationResponse,
  type CallToolResult,
  type McpHttpHandler,
} from "@modelcontextprotocol/server";
import * as z from "zod/v4";

import type { EventActor } from "../core/events.ts";
import { INTENT_LEVELS, type AttentionEvidence } from "../core/model.ts";
import type { TaskStore } from "../storage/task-store.ts";
import { VERSION } from "../version.ts";
import {
  TASK_MCP_TOOL_NAMES,
  type TaskMcpGrant,
  type TaskMcpGrantStore,
  type TaskMcpToolName,
} from "./grants.ts";
import { TASK_TOOL_PROTOCOL, type TaskToolExecutor, type TaskToolRequest } from "./protocol.ts";

const MAX_MCP_REQUEST_BYTES = 2 * 1024 * 1024;

const pathSchema = z.string().min(1).max(4_096);
const evidenceSchema = z.object({
  label: z.string().min(1).max(200),
  uri: z.string().url().max(4_096),
  mediaType: z.string().min(1).max(200).optional(),
});

function textResult(value: Record<string, unknown>): CallToolResult {
  return {
    content: [{ type: "text", text: JSON.stringify(value, null, 2) }],
    structuredContent: value,
  };
}

function errorResult(error: unknown): CallToolResult {
  const detail = error instanceof Error ? error.message : String(error);
  return {
    isError: true,
    content: [{ type: "text", text: detail }],
    structuredContent: { ok: false, error: detail },
  };
}

async function execute(
  executor: TaskToolExecutor,
  grant: TaskMcpGrant,
  request: TaskToolRequest,
): Promise<CallToolResult> {
  try {
    const response = await executor.execute(grant.taskId, request);
    return response.ok
      ? textResult({ ok: true, result: response.result })
      : {
          isError: true,
          content: [{ type: "text", text: `${response.error.code}: ${response.error.message}` }],
          structuredContent: { ok: false, error: response.error },
        };
  } catch (error) {
    return errorResult(error);
  }
}

function actor(grant: TaskMcpGrant): EventActor {
  return {
    kind: "agent",
    adapter: "claude-code-cli",
    ...(grant.agentSessionId === undefined ? {} : { sessionId: grant.agentSessionId }),
  };
}

function attentionIdFor(
  grant: TaskMcpGrant,
  kind: "trace" | "update" | "decision" | "review",
  idempotencyKey: string | undefined,
): string {
  if (idempotencyKey === undefined) return crypto.randomUUID();
  const digest = createHash("sha256")
    .update(`${grant.id}\0${kind}\0${idempotencyKey}`, "utf8")
    .digest("hex")
    .slice(0, 32);
  return `mcp-${kind}-${digest}`;
}

function normalizedEvidence(
  evidence: readonly { readonly label: string; readonly uri: string; readonly mediaType?: string | undefined }[] | undefined,
): readonly AttentionEvidence[] | undefined {
  return evidence?.map((item) => ({
    label: item.label,
    uri: item.uri,
    ...(item.mediaType === undefined ? {} : { mediaType: item.mediaType }),
  }));
}

function registerTaskTools(
  server: McpServer,
  grant: TaskMcpGrant,
  store: TaskStore,
  executor: TaskToolExecutor,
): void {
  const allowed = new Set<TaskMcpToolName>(grant.allowedTools);
  const has = (name: TaskMcpToolName): boolean => allowed.has(name);

  if (has("get_task")) server.registerTool("get_task", {
    title: "Read task intent",
    description: "Read the canonical task, intention level, attention state, and durable event history before acting.",
    annotations: { readOnlyHint: true, idempotentHint: true },
  }, async () => {
    const task = store.getTask(grant.taskId);
    if (task === undefined) return errorResult(new Error(`Task ${grant.taskId} does not exist`));
    return textResult({
      ok: true,
      task,
      events: store.getEvents(grant.taskId).slice(-100),
      intentArtifacts: store.listIntentArtifacts(grant.taskId).slice(-20),
    });
  });

  if (has("read_file")) server.registerTool("read_file", {
    title: "Read source file",
    description: "Read UTF-8 source from the isolated task capsule. Returns a SHA-256 token for conflict-safe writes.",
    inputSchema: z.object({
      path: pathSchema,
      startLine: z.number().int().positive().optional(),
      endLine: z.number().int().positive().optional(),
    }),
    annotations: { readOnlyHint: true, idempotentHint: true },
  }, (input) => execute(executor, grant, { schema: TASK_TOOL_PROTOCOL, operation: "read_file", ...input }));

  if (has("write_file")) server.registerTool("write_file", {
    title: "Write source file",
    description: "Atomically write UTF-8 source in the task capsule. Pass the SHA-256 returned by read_file to reject lost updates, or null to require a new file.",
    inputSchema: z.object({
      path: pathSchema,
      content: z.string().max(2 * 1024 * 1024),
      expectedSha256: z.string().regex(/^sha256:[a-f0-9]{64}$/u).nullable().optional(),
      createDirectories: z.boolean().optional(),
    }),
    annotations: { readOnlyHint: false, destructiveHint: true, idempotentHint: false },
  }, (input) => execute(executor, grant, { schema: TASK_TOOL_PROTOCOL, operation: "write_file", ...input }));

  if (has("list_files")) server.registerTool("list_files", {
    title: "List source tree",
    description: "List a bounded portion of the task capsule source tree without following symlinks or exposing .git internals.",
    inputSchema: z.object({
      path: pathSchema.optional(),
      depth: z.number().int().min(0).max(8).optional(),
      limit: z.number().int().min(1).max(1_000).optional(),
    }),
    annotations: { readOnlyHint: true, idempotentHint: true },
  }, (input) => execute(executor, grant, { schema: TASK_TOOL_PROTOCOL, operation: "list_files", ...input }));

  if (has("search_text")) server.registerTool("search_text", {
    title: "Search source text",
    description: "Search the task capsule with a fixed-string ripgrep query and bounded results.",
    inputSchema: z.object({
      query: z.string().min(1).max(1_024),
      path: pathSchema.optional(),
      glob: z.string().min(1).max(256).optional(),
      limit: z.number().int().min(1).max(1_000).optional(),
    }),
    annotations: { readOnlyHint: true, idempotentHint: true },
  }, (input) => execute(executor, grant, { schema: TASK_TOOL_PROTOCOL, operation: "search_text", ...input }));

  if (has("run_command")) server.registerTool("run_command", {
    title: "Run task command",
    description: "Run one executable with an argv array inside the isolated task capsule. No shell interpolation and no identity credentials are available.",
    inputSchema: z.object({
      executable: z.string().min(1).max(256),
      args: z.array(z.string().max(16_384)).max(256).optional(),
      cwd: pathSchema.optional(),
      env: z.record(z.string(), z.string().max(16_384)).optional(),
      timeoutMs: z.number().int().min(1_000).max(600_000).optional(),
    }),
    annotations: { readOnlyHint: false, destructiveHint: true, idempotentHint: false },
  }, (input) => execute(executor, grant, { schema: TASK_TOOL_PROTOCOL, operation: "run_command", ...input }));

  if (has("git_status")) server.registerTool("git_status", {
    title: "Read Git status",
    description: "Read stable porcelain Git status from the task capsule.",
    annotations: { readOnlyHint: true, idempotentHint: true },
  }, () => execute(executor, grant, { schema: TASK_TOOL_PROTOCOL, operation: "git_status" }));

  if (has("record_trace")) server.registerTool("record_trace", {
    title: "Record trace",
    description: "Append low-level operational history without interrupting the operator or changing task execution.",
    inputSchema: z.object({
      title: z.string().min(1).max(200),
      summary: z.string().min(1).max(20_000),
      evidence: z.array(evidenceSchema).max(20).optional(),
      idempotencyKey: z.string().min(1).max(200).optional(),
    }),
    annotations: { readOnlyHint: false, destructiveHint: false, idempotentHint: false },
  }, async (input) => {
    try {
      const result = store.append({
        taskId: grant.taskId,
        type: "attention.raised",
        payload: { attention: { id: attentionIdFor(grant, "trace", input.idempotencyKey), class: "trace", title: input.title, summary: input.summary, ...(input.evidence === undefined ? {} : { evidence: normalizedEvidence(input.evidence)! }) } },
        actor: actor(grant),
        ...(input.idempotencyKey === undefined ? {} : { idempotencyKey: `mcp:trace:${input.idempotencyKey}` }),
      });
      return textResult({ ok: true, task: result.task });
    } catch (error) {
      return errorResult(error);
    }
  });

  if (has("report_update")) server.registerTool("report_update", {
    title: "Report update",
    description: "Add a non-blocking Update. Work continues; the operator may acknowledge it independently later.",
    inputSchema: z.object({
      title: z.string().min(1).max(200),
      summary: z.string().min(1).max(20_000),
      evidence: z.array(evidenceSchema).max(20).optional(),
      idempotencyKey: z.string().min(1).max(200).optional(),
    }),
    annotations: { readOnlyHint: false, destructiveHint: false, idempotentHint: false },
  }, async (input) => {
    try {
      const attentionId = attentionIdFor(grant, "update", input.idempotencyKey);
      const result = store.append({
        taskId: grant.taskId,
        type: "attention.raised",
        payload: { attention: { id: attentionId, class: "update", title: input.title, summary: input.summary, ...(input.evidence === undefined ? {} : { evidence: normalizedEvidence(input.evidence)! }) } },
        actor: actor(grant),
        ...(input.idempotencyKey === undefined ? {} : { idempotencyKey: `mcp:update:${input.idempotencyKey}` }),
      });
      return textResult({ ok: true, attentionId, task: result.task });
    } catch (error) {
      return errorResult(error);
    }
  });

  if (has("request_decision")) server.registerTool("request_decision", {
    title: "Request decision",
    description: "Raise the task's single blocking Decision at the right level of intent. Stop starting new work after this succeeds.",
    inputSchema: z.object({
      title: z.string().min(1).max(200),
      summary: z.string().min(1).max(20_000),
      question: z.string().min(1).max(10_000),
      recommendation: z.string().min(1).max(10_000).optional(),
      options: z.array(z.object({
        label: z.string().min(1).max(200),
        value: z.string().min(1).max(2_000),
        tradeoff: z.string().min(1).max(5_000).optional(),
      })).min(2).max(10).optional(),
      originalIntent: z.string().min(1).max(20_000).optional(),
      requestedIntentLevel: z.enum(INTENT_LEVELS).optional(),
      consequence: z.string().min(1).max(10_000).optional(),
      evidence: z.array(evidenceSchema).max(20).optional(),
      idempotencyKey: z.string().min(1).max(200).optional(),
    }),
    annotations: { readOnlyHint: false, destructiveHint: false, idempotentHint: false },
  }, async (input) => {
    try {
      const attentionId = attentionIdFor(grant, "decision", input.idempotencyKey);
      const evidence = normalizedEvidence(input.evidence);
      const options = input.options?.map((item) => ({
        label: item.label,
        value: item.value,
        ...(item.tradeoff === undefined ? {} : { tradeoff: item.tradeoff }),
      }));
      const result = store.append({
        taskId: grant.taskId,
        type: "attention.raised",
        payload: { attention: {
          id: attentionId,
          class: "decision",
          title: input.title,
          summary: input.summary,
          question: input.question,
          ...(input.recommendation === undefined ? {} : { recommendation: input.recommendation }),
          ...(options === undefined ? {} : { options }),
          ...(input.originalIntent === undefined ? {} : { originalIntent: input.originalIntent }),
          ...(input.requestedIntentLevel === undefined ? {} : { requestedIntentLevel: input.requestedIntentLevel }),
          ...(input.consequence === undefined ? {} : { consequence: input.consequence }),
          ...(evidence === undefined ? {} : { evidence }),
        } },
        actor: actor(grant),
        ...(input.idempotencyKey === undefined ? {} : { idempotencyKey: `mcp:decision:${input.idempotencyKey}` }),
      });
      return textResult({ ok: true, attentionId, task: result.task, instruction: "Yield this turn and wait for the operator's answer." });
    } catch (error) {
      return errorResult(error);
    }
  });

  if (has("request_review")) server.registerTool("request_review", {
    title: "Request review",
    description: "Raise the task's blocking Review only after checks and reviewable evidence exist. Acceptance is the only route to completion.",
    inputSchema: z.object({
      title: z.string().min(1).max(200),
      summary: z.string().min(1).max(20_000),
      artifacts: z.array(z.object({
        kind: z.enum(["pull_request", "preview", "document", "visual_review", "other"]),
        label: z.string().min(1).max(200),
        uri: z.string().url().max(4_096),
      })).min(1).max(20),
      checks: z.array(z.string().min(1).max(2_000)).min(1).max(100),
      caveats: z.array(z.string().min(1).max(5_000)).max(50).optional(),
      evidence: z.array(evidenceSchema).max(20).optional(),
      idempotencyKey: z.string().min(1).max(200).optional(),
    }),
    annotations: { readOnlyHint: false, destructiveHint: false, idempotentHint: false },
  }, async (input) => {
    try {
      const attentionId = attentionIdFor(grant, "review", input.idempotencyKey);
      const evidence = normalizedEvidence(input.evidence);
      const result = store.append({
        taskId: grant.taskId,
        type: "attention.raised",
        payload: { attention: {
          id: attentionId,
          class: "review",
          title: input.title,
          summary: input.summary,
          artifacts: input.artifacts,
          checks: input.checks,
          ...(input.caveats === undefined ? {} : { caveats: input.caveats }),
          ...(evidence === undefined ? {} : { evidence }),
        } },
        actor: actor(grant),
        ...(input.idempotencyKey === undefined ? {} : { idempotencyKey: `mcp:review:${input.idempotencyKey}` }),
      });
      return textResult({ ok: true, attentionId, task: result.task, instruction: "Yield this turn and wait for Review." });
    } catch (error) {
      return errorResult(error);
    }
  });
}

function grantFromContext(value: unknown): TaskMcpGrant {
  if (typeof value !== "object" || value === null || !("grant" in value)) {
    throw new Error("Task MCP request has no authenticated grant");
  }
  return (value as { grant: TaskMcpGrant }).grant;
}

async function parseBoundedJson(request: Request): Promise<unknown> {
  if (request.body === null) throw new Error("MCP request body is required");
  const reader = request.clone().body!.getReader();
  const chunks: Uint8Array[] = [];
  let size = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      size += value.byteLength;
      if (size > MAX_MCP_REQUEST_BYTES) {
        await reader.cancel();
        throw new Error("MCP request is too large");
      }
      chunks.push(value);
    }
  } finally {
    reader.releaseLock();
  }
  const body = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) {
    body.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(body)) as unknown;
}

export interface TaskMcpEndpoint {
  fetch(request: Request): Promise<Response>;
  close(): Promise<void>;
}

export function createTaskMcpEndpoint(input: {
  readonly grants: TaskMcpGrantStore;
  readonly store: TaskStore;
  readonly executor: TaskToolExecutor;
  readonly allowedHostnames?: readonly string[];
  readonly path?: string;
}): TaskMcpEndpoint {
  const allowedHostnames = [...(input.allowedHostnames ?? [
    "127.0.0.1",
    "localhost",
    "[::1]",
    "host.docker.internal",
    "host.container.internal",
  ])];
  const path = input.path ?? "/mcp";
  const handler: McpHttpHandler = createMcpHandler(({ authInfo }) => {
    const grant = grantFromContext(authInfo?.extra);
    const server = new McpServer({ name: "altitude", version: VERSION });
    registerTaskTools(server, grant, input.store, input.executor);
    return server;
  });

  return {
    async fetch(request): Promise<Response> {
      if (new URL(request.url).pathname !== path) return Response.json({ error: "not_found" }, { status: 404 });
      const rejected = hostHeaderValidationResponse(request, allowedHostnames)
        ?? originValidationResponse(request, allowedHostnames);
      if (rejected !== undefined) return rejected;
      const authorization = request.headers.get("authorization") ?? "";
      const token = authorization.startsWith("Bearer ") ? authorization.slice(7) : "";
      const grant = input.grants.authenticate(token);
      if (grant === undefined) {
        return Response.json({ error: "unauthorized" }, {
          status: 401,
          headers: { "www-authenticate": 'Bearer realm="altitude-task"' },
        });
      }
      let parsedBody: unknown;
      if (request.method === "POST") {
        const mediaType = request.headers.get("content-type")?.split(";", 1)[0]?.trim().toLowerCase();
        if (mediaType !== "application/json") return Response.json({ error: "unsupported_media_type" }, { status: 415 });
        try {
          parsedBody = await parseBoundedJson(request);
        } catch (error) {
          const message = error instanceof Error ? error.message : String(error);
          return Response.json({ error: message === "MCP request is too large" ? "request_too_large" : "invalid_json" }, {
            status: message === "MCP request is too large" ? 413 : 400,
          });
        }
      }
      return await handler.fetch(request, {
        ...(parsedBody === undefined ? {} : { parsedBody }),
        authInfo: {
          token,
          clientId: grant.identityId,
          scopes: [...grant.allowedTools],
          expiresAt: Math.floor(grant.expiresAt / 1_000),
          extra: { grant },
        },
      });
    },
    close: () => handler.close(),
  };
}

export const ALL_CLAUDE_ALTITUDE_TOOLS = TASK_MCP_TOOL_NAMES.map((name) => `mcp__altitude__${name}`);
