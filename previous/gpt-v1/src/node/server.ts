import { createHash } from "node:crypto";

import {
  hostHeaderValidationResponse,
  originValidationResponse,
} from "@modelcontextprotocol/server";

import {
  createIntentSubmissionHandler,
  IntentSubmissionStore,
} from "../intent/submission.ts";
import { MICROSANDBOX_HOST_IPV4 } from "../runtime/microsandbox.ts";
import type { TaskStore } from "../storage/task-store.ts";
import { createLinearWebhookHandler } from "../tracker/webhook.ts";
import type { TaskMcpGrantStore } from "../tools/grants.ts";
import { createTaskMcpEndpoint, type TaskMcpEndpoint } from "../tools/mcp.ts";
import type { TaskToolExecutor } from "../tools/protocol.ts";
import { VERSION } from "../version.ts";

export const DEFAULT_NODE_PORT = 43_100;

export interface AltitudeNodeService {
  fetch(request: Request): Promise<Response>;
  close(): Promise<void>;
}

export interface RunningAltitudeNode {
  readonly hostname: string;
  readonly port: number;
  readonly url: URL;
  close(): Promise<void>;
}

export function nodeInstanceId(databasePath: string): string {
  if (!databasePath.trim() || databasePath.includes("\0")) throw new Error("Node database path is invalid");
  return `node-${createHash("sha256").update(databasePath, "utf8").digest("hex").slice(0, 24)}`;
}

function json(value: unknown, init?: ResponseInit): Response {
  return Response.json(value, {
    ...init,
    headers: {
      "cache-control": "no-store",
      ...init?.headers,
    },
  });
}

/**
 * The node owns durable task state and the only bridge into task capsules.
 * Keeping this as a fetch handler makes the protocol independently testable;
 * the Bun listener is only a deployment detail.
 */
export function createAltitudeNodeService(input: {
  readonly store: TaskStore;
  readonly grants: TaskMcpGrantStore;
  readonly executor: TaskToolExecutor;
  readonly submissions?: IntentSubmissionStore;
  readonly instanceId?: string;
  readonly allowedHostnames?: readonly string[];
  readonly linearWebhookSecret?: string;
}): AltitudeNodeService {
  const allowedHostnames = [...(input.allowedHostnames ?? [
    "127.0.0.1",
    "localhost",
    "[::1]",
    MICROSANDBOX_HOST_IPV4,
  ])];
  const submissions = input.submissions ?? new IntentSubmissionStore(input.store);
  const submit = createIntentSubmissionHandler(submissions);
  const mcp: TaskMcpEndpoint = createTaskMcpEndpoint({
    grants: input.grants,
    store: input.store,
    executor: input.executor,
    allowedHostnames,
  });
  const linearWebhook = input.linearWebhookSecret === undefined
    ? undefined
    : createLinearWebhookHandler({ secret: input.linearWebhookSecret, store: input.store });

  return {
    async fetch(request): Promise<Response> {
      const url = new URL(request.url);
      if (url.pathname === "/healthz") {
        if (request.method !== "GET" && request.method !== "HEAD") {
          return json({ error: "method_not_allowed" }, {
            status: 405,
            headers: { allow: "GET, HEAD" },
          });
        }
        const body = request.method === "HEAD"
          ? null
          : JSON.stringify({
              ok: true,
              service: "altitude-node",
              version: VERSION,
              instanceId: input.instanceId ?? "unbound",
            });
        return new Response(body, {
          status: 200,
          headers: {
            "cache-control": "no-store",
            "content-type": "application/json; charset=utf-8",
          },
        });
      }
      if (url.pathname.startsWith("/v1/intent-tools/")) {
        const rejected = hostHeaderValidationResponse(request, allowedHostnames)
          ?? originValidationResponse(request, allowedHostnames);
        if (rejected !== undefined) return rejected;
        return await submit(request);
      }
      if (url.pathname === "/v1/tracker/linear/webhook" && linearWebhook !== undefined) {
        return await linearWebhook(request);
      }
      if (url.pathname === "/mcp") return await mcp.fetch(request);
      return json({ error: "not_found" }, { status: 404 });
    },
    close: () => mcp.close(),
  };
}

export function serveAltitudeNode(input: {
  readonly service: AltitudeNodeService;
  readonly hostname?: string;
  readonly port?: number;
}): RunningAltitudeNode {
  const hostname = input.hostname ?? "127.0.0.1";
  const port = input.port ?? DEFAULT_NODE_PORT;
  const server = Bun.serve({
    hostname,
    port,
    fetch: (request) => input.service.fetch(request),
  });
  let closed = false;
  return {
    hostname: server.hostname ?? hostname,
    port: server.port ?? port,
    url: server.url,
    async close(): Promise<void> {
      if (closed) return;
      closed = true;
      await input.service.close();
      await server.stop(true);
    },
  };
}
