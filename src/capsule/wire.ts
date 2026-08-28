import {
  capsuleManifestDigest,
  parseCapsuleManifest,
  verifyCapsuleMembers,
  type CapsuleCheckpoint,
  type CapsuleMemberContent,
} from "./manifest.ts";
import { isSha256Digest, stableJson, utf8, type Sha256Digest } from "./canonical.ts";

export const CAPSULE_WIRE_PROTOCOL = "altitude.capsule-wire/v1" as const;
export const MAX_CAPSULE_WIRE_BYTES = 1024 * 1024 * 1024;
const MAX_CAPSULE_WIRE_HEADER_BYTES = 4 * 1024 * 1024;

type CapsuleWireHeader = Readonly<{
  schema: typeof CAPSULE_WIRE_PROTOCOL;
  checkpointDigest: Sha256Digest;
  manifest: unknown;
}>;

function concat(chunks: readonly Uint8Array[]): Uint8Array {
  const size = chunks.reduce((total, chunk) => total + chunk.byteLength, 0);
  if (!Number.isSafeInteger(size) || size > MAX_CAPSULE_WIRE_BYTES) {
    throw new Error("Capsule wire payload is too large");
  }
  const output = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) {
    output.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return output;
}

/** A deterministic, length-delimited checkpoint envelope for capsule boundaries. */
export function encodeCapsuleWire(checkpoint: CapsuleCheckpoint): Uint8Array {
  verifyCapsuleMembers(checkpoint.manifest, checkpoint.members);
  const header: CapsuleWireHeader = {
    schema: CAPSULE_WIRE_PROTOCOL,
    checkpointDigest: capsuleManifestDigest(checkpoint.manifest),
    manifest: checkpoint.manifest,
  };
  const headerBytes = utf8(stableJson(header));
  if (headerBytes.byteLength > MAX_CAPSULE_WIRE_HEADER_BYTES) {
    throw new Error("Capsule wire header is too large");
  }
  const prefix = new Uint8Array(4);
  new DataView(prefix.buffer).setUint32(0, headerBytes.byteLength, false);
  const byPath = new Map(checkpoint.members.map((member) => [member.path, member.bytes] as const));
  return concat([
    prefix,
    headerBytes,
    ...checkpoint.manifest.members.map((descriptor) => byPath.get(descriptor.path)!),
  ]);
}

function record(value: unknown): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error("Capsule wire header is invalid");
  }
  return value as Record<string, unknown>;
}

export function decodeCapsuleWire(bytes: Uint8Array): CapsuleCheckpoint {
  if (bytes.byteLength < 4 || bytes.byteLength > MAX_CAPSULE_WIRE_BYTES) {
    throw new Error("Capsule wire payload size is invalid");
  }
  const headerSize = new DataView(bytes.buffer, bytes.byteOffset, 4).getUint32(0, false);
  if (headerSize < 2 || headerSize > MAX_CAPSULE_WIRE_HEADER_BYTES || 4 + headerSize > bytes.byteLength) {
    throw new Error("Capsule wire header size is invalid");
  }
  let decoded: unknown;
  try {
    decoded = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes.subarray(4, 4 + headerSize))) as unknown;
  } catch (error) {
    throw new Error("Capsule wire header is not valid UTF-8 JSON", { cause: error });
  }
  const header = record(decoded);
  if (header.schema !== CAPSULE_WIRE_PROTOCOL || !isSha256Digest(header.checkpointDigest)) {
    throw new Error("Capsule wire protocol or digest is invalid");
  }
  const manifest = parseCapsuleManifest(header.manifest);
  if (capsuleManifestDigest(manifest) !== header.checkpointDigest) {
    throw new Error("Capsule wire manifest digest mismatch");
  }
  let offset = 4 + headerSize;
  const members: CapsuleMemberContent[] = [];
  for (const descriptor of manifest.members) {
    if (offset + descriptor.size > bytes.byteLength) {
      throw new Error(`Capsule wire member ${descriptor.path} is truncated`);
    }
    members.push({ path: descriptor.path, bytes: bytes.slice(offset, offset + descriptor.size) });
    offset += descriptor.size;
  }
  if (offset !== bytes.byteLength) throw new Error("Capsule wire payload has trailing bytes");
  verifyCapsuleMembers(manifest, members);
  return { manifest, members };
}
