import { createHash, randomUUID, timingSafeEqual } from "node:crypto";

import type { IntentArtifactEnvelope } from "./protocol.ts";

const MAX_SUBMISSION_BYTES = 2 * 1024 * 1024;

class SubmissionError extends Error {
  constructor(readonly code: string) {
    super(code);
  }
}

export interface RedmarkerReviewV1 {
  readonly schemaVersion: "redmarker-review/v1";
  readonly project: { readonly name: string };
  readonly page: {
    readonly pathname: string;
    readonly title: string;
    readonly viewport: { readonly width: number; readonly height: number };
  };
  readonly marks: readonly unknown[];
}

export interface SubmissionGrant {
  readonly sessionId: string;
  readonly taskId: string;
  readonly capsuleId: string;
  readonly token: string;
  readonly expiresAt: string;
  readonly baseUrl: string;
  readonly headSha: string;
}

interface StoredGrant extends Omit<SubmissionGrant, "token"> {
  readonly tokenHash: Uint8Array;
  consumed: boolean;
}

function hashToken(token: string): Uint8Array {
  return createHash("sha256").update(token, "utf8").digest();
}

function validReview(value: unknown): value is RedmarkerReviewV1 {
  if (typeof value !== "object" || value === null) return false;
  const review = value as Record<string, unknown>;
  if (review.schemaVersion !== "redmarker-review/v1") return false;
  if (typeof review.project !== "object" || review.project === null) return false;
  if (typeof (review.project as Record<string, unknown>).name !== "string") return false;
  if (typeof review.page !== "object" || review.page === null) return false;
  const page = review.page as Record<string, unknown>;
  if (typeof page.pathname !== "string" || typeof page.title !== "string") return false;
  if (typeof page.viewport !== "object" || page.viewport === null) return false;
  const viewport = page.viewport as Record<string, unknown>;
  if (typeof viewport.width !== "number" || typeof viewport.height !== "number") return false;
  return Array.isArray(review.marks) && review.marks.length > 0 && review.marks.length <= 100;
}

function authorizedToken(provided: string, expectedHash: Uint8Array): boolean {
  if (provided.length < 32 || provided.length > 512) return false;
  const actualHash = hashToken(provided);
  return actualHash.byteLength === expectedHash.byteLength && timingSafeEqual(actualHash, expectedHash);
}

async function readJsonWithLimit(request: Request): Promise<unknown> {
  if (request.body === null) throw new SubmissionError("invalid_submission");
  const reader = request.body.getReader();
  const chunks: Uint8Array[] = [];
  let size = 0;
  try {
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      size += value.byteLength;
      if (size > MAX_SUBMISSION_BYTES) {
        await reader.cancel();
        throw new SubmissionError("submission_too_large");
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
  try {
    return JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(body)) as unknown;
  } catch (error) {
    if (error instanceof SubmissionError) throw error;
    throw new SubmissionError("invalid_submission");
  }
}

export class IntentSubmissionRegistry {
  private readonly grants = new Map<string, StoredGrant>();

  constructor(private readonly now: () => Date = () => new Date()) {}

  issue(input: {
    readonly taskId: string;
    readonly capsuleId: string;
    readonly baseUrl: string;
    readonly headSha: string;
    readonly ttlMs?: number;
  }): SubmissionGrant {
    const sessionId = randomUUID();
    const token = `${randomUUID()}${randomUUID()}`.replaceAll("-", "");
    const expiresAt = new Date(this.now().getTime() + (input.ttlMs ?? 30 * 60_000)).toISOString();
    const grant: StoredGrant = {
      sessionId,
      taskId: input.taskId,
      capsuleId: input.capsuleId,
      expiresAt,
      baseUrl: input.baseUrl,
      headSha: input.headSha,
      tokenHash: hashToken(token),
      consumed: false,
    };
    this.grants.set(sessionId, grant);
    return { ...input, sessionId, token, expiresAt };
  }

  consume(sessionId: string, token: string, payload: unknown): IntentArtifactEnvelope {
    const grant = this.grants.get(sessionId);
    if (grant === undefined || grant.consumed) throw new Error("submission_not_found");
    if (Date.parse(grant.expiresAt) <= this.now().getTime()) throw new Error("submission_expired");
    if (!authorizedToken(token, grant.tokenHash)) throw new Error("submission_unauthorized");
    if (!validReview(payload)) throw new Error("invalid_redmarker_review");

    const bytes = new TextEncoder().encode(JSON.stringify(payload)).byteLength;
    if (bytes > MAX_SUBMISSION_BYTES) throw new Error("submission_too_large");
    grant.consumed = true;

    return {
      schema: "altitude.intent-artifact/v1",
      artifactId: randomUUID(),
      taskId: grant.taskId,
      capsuleId: grant.capsuleId,
      tool: "redmarker",
      createdAt: this.now().toISOString(),
      base: { url: grant.baseUrl, headSha: grant.headSha },
      payloadSchema: payload.schemaVersion,
      payload,
    };
  }
}

export function createIntentSubmissionHandler(
  registry: IntentSubmissionRegistry,
  accept: (artifact: IntentArtifactEnvelope) => Promise<void>,
): (request: Request) => Promise<Response> {
  return async (request) => {
    if (request.method !== "POST") return Response.json({ ok: false, error: "method_not_allowed" }, { status: 405 });
    const url = new URL(request.url);
    const match = /^\/v1\/intent-tools\/redmarker\/([0-9a-f-]{36})$/.exec(url.pathname);
    if (match === null) return Response.json({ ok: false, error: "not_found" }, { status: 404 });
    const declared = Number(request.headers.get("content-length") ?? 0);
    if (Number.isFinite(declared) && declared > MAX_SUBMISSION_BYTES) {
      return Response.json({ ok: false, error: "submission_too_large" }, { status: 413 });
    }
    const mediaType = request.headers.get("content-type")?.split(";", 1)[0]?.trim().toLowerCase();
    if (mediaType !== "application/json") {
      return Response.json({ ok: false, error: "unsupported_media_type" }, { status: 415 });
    }
    const authorization = request.headers.get("authorization") ?? "";
    const token = authorization.startsWith("Bearer ") ? authorization.slice(7) : "";
    let payload: unknown;
    try {
      payload = await readJsonWithLimit(request);
      const artifact = registry.consume(match[1]!, token, payload);
      await accept(artifact);
      return Response.json({ ok: true, artifactId: artifact.artifactId }, { status: 202 });
    } catch (error) {
      const code = error instanceof SubmissionError
        ? error.code
        : error instanceof Error ? error.message : "invalid_submission";
      const status = code === "submission_unauthorized" ? 401
        : code === "submission_too_large" ? 413
          : code === "submission_not_found" ? 404
            : code === "submission_expired" ? 410
              : 400;
      return Response.json({ ok: false, error: code }, { status });
    }
  };
}
