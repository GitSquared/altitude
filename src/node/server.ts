import type { RuntimeKind } from "../runtime/capsule-runtime.ts";
import type { TaskStore } from "../storage/task-store.ts";
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
  readonly runtime: RuntimeKind;
  readonly store: TaskStore;
  readonly grants: TaskMcpGrantStore;
  readonly executor: TaskToolExecutor;
  readonly allowedHostnames?: readonly string[];
}): AltitudeNodeService {
  const mcp: TaskMcpEndpoint = createTaskMcpEndpoint({
    grants: input.grants,
    store: input.store,
    executor: input.executor,
    ...(input.allowedHostnames === undefined ? {} : { allowedHostnames: input.allowedHostnames }),
  });

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
          : JSON.stringify({ ok: true, service: "altitude-node", version: VERSION, runtime: input.runtime });
        return new Response(body, {
          status: 200,
          headers: {
            "cache-control": "no-store",
            "content-type": "application/json; charset=utf-8",
          },
        });
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
