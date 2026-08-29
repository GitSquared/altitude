import {
  createHash,
  randomBytes,
  randomUUID,
  timingSafeEqual,
} from "node:crypto";

import type { TaskStore } from "../storage/task-store.ts";
import type { IntentArtifactEnvelope } from "./protocol.ts";

const MAX_SUBMISSION_BYTES = 2 * 1024 * 1024;
const DEFAULT_GRANT_TTL_MS = 30 * 60_000;
const MAX_GRANT_TTL_MS = 2 * 60 * 60_000;

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
  readonly checkpointDigest: string;
  readonly checkpointRevision: number;
}

export interface IntentSubmissionStoreOptions {
  readonly clock?: () => number;
  readonly createSessionId?: () => string;
  readonly createArtifactId?: () => string;
  readonly createToken?: () => string;
}

type StoredGrantRow = {
  session_id: string;
  task_id: string;
  capsule_id: string;
  token_hash: string;
  base_url: string;
  head_sha: string;
  checkpoint_digest: string;
  checkpoint_revision: number;
  expires_at: number;
  consumed_at: number | null;
  revoked_at: number | null;
};

type ArtifactRow = {
  artifact_json: string;
};

function hashToken(token: string): string {
  return createHash("sha256").update(token, "utf8").digest("hex");
}

function requireIdentifier(value: string, label: string): void {
  if (!/^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/u.test(value)) {
    throw new Error(`${label} is invalid`);
  }
}

function requireUuid(value: string, label: string): void {
  if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/iu.test(value)) {
    throw new Error(`${label} is invalid`);
  }
}

function requireBaseUrl(value: string): string {
  let url: URL;
  try {
    url = new URL(value);
  } catch (error) {
    throw new Error("Intent artifact base URL is invalid", { cause: error });
  }
  if (!["http:", "https:"].includes(url.protocol) || url.username || url.password || url.href.length > 4_096) {
    throw new Error("Intent artifact base URL must be credential-free HTTP(S)");
  }
  return url.toString();
}

function requireHeadSha(value: string): void {
  if (!/^[a-f0-9]{40}(?:[a-f0-9]{24})?$/u.test(value)) {
    throw new Error("Intent artifact head SHA must be a full Git object ID");
  }
}

function requireCheckpoint(input: { readonly digest: string; readonly revision: number }): void {
  if (!/^sha256:[a-f0-9]{64}$/u.test(input.digest)) {
    throw new Error("Intent artifact checkpoint digest is invalid");
  }
  if (!Number.isSafeInteger(input.revision) || input.revision < 1) {
    throw new Error("Intent artifact checkpoint revision is invalid");
  }
}

function validReview(value: unknown): value is RedmarkerReviewV1 {
  if (typeof value !== "object" || value === null) return false;
  const review = value as Record<string, unknown>;
  if (review.schemaVersion !== "redmarker-review/v1") return false;
  if (typeof review.project !== "object" || review.project === null) return false;
  const project = review.project as Record<string, unknown>;
  if (typeof project.name !== "string" || project.name.length === 0 || project.name.length > 500) return false;
  if (typeof review.page !== "object" || review.page === null) return false;
  const page = review.page as Record<string, unknown>;
  if (
    typeof page.pathname !== "string"
    || page.pathname.length > 4_096
    || typeof page.title !== "string"
    || page.title.length > 2_000
  ) return false;
  if (typeof page.viewport !== "object" || page.viewport === null) return false;
  const viewport = page.viewport as Record<string, unknown>;
  if (
    typeof viewport.width !== "number"
    || !Number.isFinite(viewport.width)
    || viewport.width < 0
    || typeof viewport.height !== "number"
    || !Number.isFinite(viewport.height)
    || viewport.height < 0
  ) return false;
  return Array.isArray(review.marks) && review.marks.length > 0 && review.marks.length <= 100;
}

function authorizedToken(provided: string, expectedHash: string): boolean {
  if (provided.length < 32 || provided.length > 512 || !/^[a-f0-9]{64}$/u.test(expectedHash)) return false;
  const actual = Buffer.from(hashToken(provided), "hex");
  const expected = Buffer.from(expectedHash, "hex");
  return actual.byteLength === expected.byteLength && timingSafeEqual(actual, expected);
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

/** Durable, one-use grants whose consumption and artifact write share one transaction. */
export class IntentSubmissionStore {
  readonly #clock: () => number;
  readonly #createSessionId: () => string;
  readonly #createArtifactId: () => string;
  readonly #createToken: () => string;

  constructor(
    private readonly tasks: Pick<TaskStore, "database" | "getTask">,
    options: IntentSubmissionStoreOptions = {},
  ) {
    this.#clock = options.clock ?? Date.now;
    this.#createSessionId = options.createSessionId ?? randomUUID;
    this.#createArtifactId = options.createArtifactId ?? randomUUID;
    this.#createToken = options.createToken ?? (() => randomBytes(32).toString("hex"));
  }

  issue(input: {
    readonly taskId: string;
    readonly capsuleId: string;
    readonly baseUrl: string;
    readonly headSha: string;
    readonly checkpointDigest: string;
    readonly checkpointRevision: number;
    readonly ttlMs?: number;
  }): SubmissionGrant {
    requireIdentifier(input.taskId, "Task id");
    requireIdentifier(input.capsuleId, "Capsule id");
    if (this.tasks.getTask(input.taskId) === undefined) {
      throw new Error(`Task ${input.taskId} does not exist`);
    }
    const baseUrl = requireBaseUrl(input.baseUrl);
    requireHeadSha(input.headSha);
    requireCheckpoint({ digest: input.checkpointDigest, revision: input.checkpointRevision });
    const ttlMs = input.ttlMs ?? DEFAULT_GRANT_TTL_MS;
    if (!Number.isSafeInteger(ttlMs) || ttlMs < 1_000 || ttlMs > MAX_GRANT_TTL_MS) {
      throw new Error(`Intent submission TTL must be between 1000 and ${MAX_GRANT_TTL_MS} milliseconds`);
    }
    const sessionId = this.#createSessionId();
    const token = this.#createToken();
    requireUuid(sessionId, "Intent submission session id");
    if (token.length < 32 || token.length > 512 || /[\u0000-\u001f\u007f]/u.test(token)) {
      throw new Error("Intent submission token generator returned an invalid token");
    }
    const createdAt = this.#clock();
    const expiresAt = createdAt + ttlMs;
    this.tasks.database.query<
      unknown,
      [string, string, string, string, string, string, string, number, number, number]
    >(`
      INSERT INTO intent_submission_grants(
        session_id, task_id, capsule_id, token_hash, base_url, head_sha,
        checkpoint_digest, checkpoint_revision, created_at, expires_at
      ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
    `).run(
      sessionId,
      input.taskId,
      input.capsuleId,
      hashToken(token),
      baseUrl,
      input.headSha,
      input.checkpointDigest,
      input.checkpointRevision,
      createdAt,
      expiresAt,
    );
    return {
      sessionId,
      taskId: input.taskId,
      capsuleId: input.capsuleId,
      token,
      expiresAt: new Date(expiresAt).toISOString(),
      baseUrl,
      headSha: input.headSha,
      checkpointDigest: input.checkpointDigest,
      checkpointRevision: input.checkpointRevision,
    };
  }

  consume(sessionId: string, token: string, payload: unknown): IntentArtifactEnvelope {
    if (!validReview(payload)) throw new SubmissionError("invalid_redmarker_review");
    const payloadJson = JSON.stringify(payload);
    if (new TextEncoder().encode(payloadJson).byteLength > MAX_SUBMISSION_BYTES) {
      throw new SubmissionError("submission_too_large");
    }
    const now = this.#clock();
    return this.tasks.database.transaction(() => {
      const grant = this.tasks.database.query<StoredGrantRow, [string]>(`
        SELECT session_id, task_id, capsule_id, token_hash, base_url, head_sha,
               checkpoint_digest, checkpoint_revision, expires_at, consumed_at, revoked_at
        FROM intent_submission_grants
        WHERE session_id = ?1
      `).get(sessionId);
      if (grant === null || grant.consumed_at !== null || grant.revoked_at !== null) {
        throw new SubmissionError("submission_not_found");
      }
      if (grant.expires_at <= now) throw new SubmissionError("submission_expired");
      if (!authorizedToken(token, grant.token_hash)) throw new SubmissionError("submission_unauthorized");

      const artifact: IntentArtifactEnvelope = {
        schema: "altitude.intent-artifact/v1",
        artifactId: this.#createArtifactId(),
        taskId: grant.task_id,
        capsuleId: grant.capsule_id,
        tool: "redmarker",
        createdAt: new Date(now).toISOString(),
        base: {
          url: grant.base_url,
          headSha: grant.head_sha,
          checkpointDigest: grant.checkpoint_digest,
          checkpointRevision: grant.checkpoint_revision,
        },
        payloadSchema: payload.schemaVersion,
        payload,
      };
      this.tasks.database.query<
        unknown,
        [string, string, string, string, string, string, string, string, string, number, number, string]
      >(`
        INSERT INTO intent_artifacts(
          id, session_id, task_id, capsule_id, tool, payload_schema,
          base_url, head_sha, checkpoint_digest, checkpoint_revision,
          created_at, artifact_json
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
      `).run(
        artifact.artifactId,
        grant.session_id,
        artifact.taskId,
        artifact.capsuleId,
        artifact.tool,
        artifact.payloadSchema,
        artifact.base.url,
        artifact.base.headSha,
        artifact.base.checkpointDigest,
        artifact.base.checkpointRevision,
        now,
        JSON.stringify(artifact),
      );
      const consumed = this.tasks.database.query<unknown, [number, string]>(`
        UPDATE intent_submission_grants
        SET consumed_at = ?1
        WHERE session_id = ?2 AND consumed_at IS NULL AND revoked_at IS NULL
      `).run(now, sessionId);
      if (consumed.changes !== 1) throw new SubmissionError("submission_not_found");
      return artifact;
    }).immediate();
  }

  revoke(sessionId: string): boolean {
    const result = this.tasks.database.query<unknown, [number, string]>(`
      UPDATE intent_submission_grants
      SET revoked_at = ?1
      WHERE session_id = ?2 AND consumed_at IS NULL AND revoked_at IS NULL
    `).run(this.#clock(), sessionId);
    return result.changes === 1;
  }

  artifactForSession(sessionId: string): IntentArtifactEnvelope | undefined {
    const row = this.tasks.database.query<ArtifactRow, [string]>(`
      SELECT artifact_json FROM intent_artifacts WHERE session_id = ?1
    `).get(sessionId);
    return row === null ? undefined : JSON.parse(row.artifact_json) as IntentArtifactEnvelope;
  }
}

function response(value: unknown, init?: ResponseInit): Response {
  return Response.json(value, {
    ...init,
    headers: { "cache-control": "no-store", ...init?.headers },
  });
}

export function createIntentSubmissionHandler(
  submissions: IntentSubmissionStore,
): (request: Request) => Promise<Response> {
  return async (request) => {
    if (request.method !== "POST") {
      return response({ ok: false, error: "method_not_allowed" }, {
        status: 405,
        headers: { allow: "POST" },
      });
    }
    const url = new URL(request.url);
    const match = /^\/v1\/intent-tools\/redmarker\/([0-9a-f-]{36})$/.exec(url.pathname);
    if (match === null) return response({ ok: false, error: "not_found" }, { status: 404 });
    const declared = Number(request.headers.get("content-length") ?? 0);
    if (Number.isFinite(declared) && declared > MAX_SUBMISSION_BYTES) {
      return response({ ok: false, error: "submission_too_large" }, { status: 413 });
    }
    const mediaType = request.headers.get("content-type")?.split(";", 1)[0]?.trim().toLowerCase();
    if (mediaType !== "application/json") {
      return response({ ok: false, error: "unsupported_media_type" }, { status: 415 });
    }
    const authorization = request.headers.get("authorization") ?? "";
    const token = authorization.startsWith("Bearer ") ? authorization.slice(7) : "";
    try {
      const payload = await readJsonWithLimit(request);
      const artifact = submissions.consume(match[1]!, token, payload);
      return response({ ok: true, artifactId: artifact.artifactId }, { status: 202 });
    } catch (error) {
      const code = error instanceof SubmissionError ? error.code : "submission_unavailable";
      const status = code === "submission_unauthorized" ? 401
        : code === "submission_too_large" ? 413
          : code === "submission_not_found" ? 404
            : code === "submission_expired" ? 410
              : code === "submission_unavailable" ? 503
                : 400;
      return response({ ok: false, error: code }, { status });
    }
  };
}
