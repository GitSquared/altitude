import { createHash } from "node:crypto";

export type Sha256Digest = `sha256:${string}`;

export function sha256(bytes: Uint8Array): Sha256Digest {
  return `sha256:${createHash("sha256").update(bytes).digest("hex")}`;
}

export function utf8(value: string): Uint8Array {
  return new TextEncoder().encode(value);
}

export function stableJson(value: unknown): string {
  return JSON.stringify(normalize(value));
}

export function compareCanonicalStrings(left: string, right: string): number {
  return left < right ? -1 : left > right ? 1 : 0;
}

function normalize(value: unknown): unknown {
  if (
    value === null ||
    typeof value === "string" ||
    typeof value === "boolean"
  ) {
    return value;
  }

  if (typeof value === "number") {
    if (!Number.isFinite(value)) {
      throw new TypeError("Canonical JSON cannot contain non-finite numbers");
    }
    return value;
  }

  if (Array.isArray(value)) {
    return value.map(normalize);
  }

  if (typeof value === "object") {
    const object = value as Record<string, unknown>;
    const normalized: Record<string, unknown> = {};
    for (const key of Object.keys(object).sort()) {
      const child = object[key];
      if (child === undefined) {
        throw new TypeError(
          `Canonical JSON cannot contain undefined at key ${JSON.stringify(key)}`,
        );
      }
      normalized[key] = normalize(child);
    }
    return normalized;
  }

  throw new TypeError(`Canonical JSON cannot encode ${typeof value}`);
}

export function isSha256Digest(value: unknown): value is Sha256Digest {
  return (
    typeof value === "string" && /^sha256:[a-f0-9]{64}$/u.test(value)
  );
}
