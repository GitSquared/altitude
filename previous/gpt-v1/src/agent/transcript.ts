import {
  compareCanonicalStrings,
  isSha256Digest,
  sha256,
  stableJson,
  utf8,
  type Sha256Digest,
} from "../capsule/canonical.ts";
import {
  portableCapsulePath,
  type PortableCapsulePath,
} from "../capsule/paths.ts";
import type { CapsuleMemberContent } from "../capsule/manifest.ts";

export const CLAUDE_TRANSCRIPT_PROTOCOL =
  "altitude.claude-transcript/v1" as const;
export const CLAUDE_TRANSCRIPT_WIRE_PROTOCOL =
  "altitude.claude-transcript-wire/v1" as const;
export const CLAUDE_TRANSCRIPT_MANIFEST_MEMBER = portableCapsulePath(
  "session/claude/transcript.json",
);
export const CLAUDE_TRANSCRIPT_FILE_PREFIX = "session/claude/files/";
export const MAX_CLAUDE_TRANSCRIPT_BYTES = 256 * 1024 * 1024;
export const MAX_CLAUDE_TRANSCRIPT_FILES = 4_096;

const MAX_HEADER_BYTES = 4 * 1024 * 1024;
const IDENTIFIER = /^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/u;
const SESSION_ID =
  /^[0-9a-f]{8}-[0-9a-f]{4}-[1-8][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/iu;
const PROJECT_DIRECTORY = /^[A-Za-z0-9._-]+$/u;

export interface ClaudeTranscriptFile {
  readonly path: string;
  readonly bytes: Uint8Array;
}

export interface ClaudeTranscriptBinding {
  readonly taskId: string;
  readonly sessionId: string;
  readonly projectDirectoryName: string;
}

export interface ClaudeTranscriptBundle {
  readonly schema: typeof CLAUDE_TRANSCRIPT_PROTOCOL;
  readonly taskId: string;
  readonly sessionId: string;
  readonly projectDirectoryName: string;
  readonly files: readonly ClaudeTranscriptFile[];
}

interface TranscriptFileDescriptor {
  readonly path: string;
  readonly size: number;
  readonly sha256: Sha256Digest;
}

interface TranscriptHeader {
  readonly schema:
    | typeof CLAUDE_TRANSCRIPT_PROTOCOL
    | typeof CLAUDE_TRANSCRIPT_WIRE_PROTOCOL;
  readonly taskId: string;
  readonly sessionId: string;
  readonly projectDirectoryName: string;
  readonly files: readonly TranscriptFileDescriptor[];
}

function record(value: unknown, label: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error(label + " is invalid");
  }
  return value as Record<string, unknown>;
}

function requireBinding(input: {
  readonly taskId: string;
  readonly sessionId: string;
  readonly projectDirectoryName: string;
}): void {
  if (!IDENTIFIER.test(input.taskId)) throw new Error("Transcript task id is invalid");
  if (!SESSION_ID.test(input.sessionId)) {
    throw new Error("Transcript Claude session id is invalid");
  }
  if (!PROJECT_DIRECTORY.test(input.projectDirectoryName)) {
    throw new Error("Transcript project directory is invalid");
  }
}

function relativeTranscriptPath(value: unknown): string {
  if (typeof value !== "string" || value.length === 0) {
    throw new Error("Transcript file path is invalid");
  }
  const member = portableCapsulePath(CLAUDE_TRANSCRIPT_FILE_PREFIX + value);
  const normalized = member.slice(CLAUDE_TRANSCRIPT_FILE_PREFIX.length);
  if (normalized !== value) throw new Error("Transcript file path is invalid");
  return value;
}

function descriptors(
  files: readonly ClaudeTranscriptFile[],
): readonly TranscriptFileDescriptor[] {
  return files.map((file) => ({
    path: file.path,
    size: file.bytes.byteLength,
    sha256: sha256(file.bytes),
  }));
}

function normalizedFiles(
  input: readonly ClaudeTranscriptFile[],
): readonly ClaudeTranscriptFile[] {
  if (input.length > MAX_CLAUDE_TRANSCRIPT_FILES) {
    throw new Error("Claude transcript has too many files");
  }
  const files = input.map((file) => ({
    path: relativeTranscriptPath(file.path),
    bytes: file.bytes.slice(),
  })).sort((left, right) => compareCanonicalStrings(left.path, right.path));
  let previous: string | undefined;
  let size = 0;
  for (const file of files) {
    if (file.path === previous) {
      throw new Error("Duplicate Claude transcript file " + file.path);
    }
    previous = file.path;
    size += file.bytes.byteLength;
    if (!Number.isSafeInteger(size) || size > MAX_CLAUDE_TRANSCRIPT_BYTES) {
      throw new Error("Claude transcript is too large");
    }
  }
  return files;
}

export function createClaudeTranscriptBundle(input: {
  readonly taskId: string;
  readonly sessionId: string;
  readonly projectDirectoryName: string;
  readonly files: readonly ClaudeTranscriptFile[];
}): ClaudeTranscriptBundle {
  requireBinding(input);
  return {
    schema: CLAUDE_TRANSCRIPT_PROTOCOL,
    taskId: input.taskId,
    sessionId: input.sessionId,
    projectDirectoryName: input.projectDirectoryName,
    files: normalizedFiles(input.files),
  };
}

function parseHeader(
  value: unknown,
  expectedSchema: TranscriptHeader["schema"],
): Omit<TranscriptHeader, "schema"> {
  const header = record(value, "Claude transcript header");
  if (header.schema !== expectedSchema) {
    throw new Error("Claude transcript schema is unsupported");
  }
  const binding = {
    taskId: typeof header.taskId === "string" ? header.taskId : "",
    sessionId: typeof header.sessionId === "string" ? header.sessionId : "",
    projectDirectoryName: typeof header.projectDirectoryName === "string"
      ? header.projectDirectoryName
      : "",
  };
  requireBinding(binding);
  if (!Array.isArray(header.files)
    || header.files.length > MAX_CLAUDE_TRANSCRIPT_FILES) {
    throw new Error("Claude transcript file descriptors are invalid");
  }
  const files: TranscriptFileDescriptor[] = [];
  let previous: string | undefined;
  let total = 0;
  for (const raw of header.files) {
    const descriptor = record(raw, "Claude transcript file descriptor");
    const path = relativeTranscriptPath(descriptor.path);
    if (previous !== undefined
      && compareCanonicalStrings(previous, path) >= 0) {
      throw new Error("Claude transcript files are not uniquely sorted");
    }
    previous = path;
    if (!Number.isSafeInteger(descriptor.size)
      || (descriptor.size as number) < 0
      || !isSha256Digest(descriptor.sha256)) {
      throw new Error("Claude transcript file descriptor is invalid");
    }
    total += descriptor.size as number;
    if (!Number.isSafeInteger(total) || total > MAX_CLAUDE_TRANSCRIPT_BYTES) {
      throw new Error("Claude transcript is too large");
    }
    files.push({
      path,
      size: descriptor.size as number,
      sha256: descriptor.sha256,
    });
  }
  return { ...binding, files };
}

function headerFor(
  bundle: ClaudeTranscriptBundle,
  schema: TranscriptHeader["schema"],
): TranscriptHeader {
  const normalized = createClaudeTranscriptBundle(bundle);
  return {
    schema,
    taskId: normalized.taskId,
    sessionId: normalized.sessionId,
    projectDirectoryName: normalized.projectDirectoryName,
    files: descriptors(normalized.files),
  };
}

function concat(chunks: readonly Uint8Array[]): Uint8Array {
  const size = chunks.reduce((total, chunk) => total + chunk.byteLength, 0);
  if (!Number.isSafeInteger(size)
    || size > MAX_CLAUDE_TRANSCRIPT_BYTES + MAX_HEADER_BYTES + 4) {
    throw new Error("Claude transcript wire payload is too large");
  }
  const output = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) {
    output.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return output;
}

export function encodeClaudeTranscriptWire(
  bundle: ClaudeTranscriptBundle,
): Uint8Array {
  const normalized = createClaudeTranscriptBundle(bundle);
  const headerBytes = utf8(stableJson(
    headerFor(normalized, CLAUDE_TRANSCRIPT_WIRE_PROTOCOL),
  ));
  if (headerBytes.byteLength > MAX_HEADER_BYTES) {
    throw new Error("Claude transcript wire header is too large");
  }
  const prefix = new Uint8Array(4);
  new DataView(prefix.buffer).setUint32(0, headerBytes.byteLength, false);
  return concat([prefix, headerBytes, ...normalized.files.map((file) => file.bytes)]);
}

export function decodeClaudeTranscriptWire(
  bytes: Uint8Array,
): ClaudeTranscriptBundle {
  if (bytes.byteLength < 4
    || bytes.byteLength > MAX_CLAUDE_TRANSCRIPT_BYTES + MAX_HEADER_BYTES + 4) {
    throw new Error("Claude transcript wire payload size is invalid");
  }
  const headerSize = new DataView(
    bytes.buffer,
    bytes.byteOffset,
    4,
  ).getUint32(0, false);
  if (headerSize < 2
    || headerSize > MAX_HEADER_BYTES
    || 4 + headerSize > bytes.byteLength) {
    throw new Error("Claude transcript wire header size is invalid");
  }
  let value: unknown;
  try {
    value = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(
      bytes.subarray(4, 4 + headerSize),
    )) as unknown;
  } catch (error) {
    throw new Error("Claude transcript wire header is not valid UTF-8 JSON", {
      cause: error,
    });
  }
  const header = parseHeader(value, CLAUDE_TRANSCRIPT_WIRE_PROTOCOL);
  let offset = 4 + headerSize;
  const files: ClaudeTranscriptFile[] = [];
  for (const descriptor of header.files) {
    if (offset + descriptor.size > bytes.byteLength) {
      throw new Error("Claude transcript file " + descriptor.path + " is truncated");
    }
    const fileBytes = bytes.slice(offset, offset + descriptor.size);
    offset += descriptor.size;
    if (sha256(fileBytes) !== descriptor.sha256) {
      throw new Error("Claude transcript file " + descriptor.path + " failed verification");
    }
    files.push({ path: descriptor.path, bytes: fileBytes });
  }
  if (offset !== bytes.byteLength) {
    throw new Error("Claude transcript wire payload has trailing bytes");
  }
  return createClaudeTranscriptBundle({ ...header, files });
}

export function claudeTranscriptCapsuleMembers(
  bundle: ClaudeTranscriptBundle,
): readonly CapsuleMemberContent[] {
  const normalized = createClaudeTranscriptBundle(bundle);
  const header = headerFor(normalized, CLAUDE_TRANSCRIPT_PROTOCOL);
  return [
    {
      path: CLAUDE_TRANSCRIPT_MANIFEST_MEMBER,
      bytes: utf8(stableJson(header) + "\n"),
    },
    ...normalized.files.map((file) => ({
      path: portableCapsulePath(CLAUDE_TRANSCRIPT_FILE_PREFIX + file.path),
      bytes: file.bytes.slice(),
    })),
  ];
}

export function decodeClaudeTranscriptCapsuleMembers(
  members: readonly CapsuleMemberContent[],
): ClaudeTranscriptBundle | undefined {
  const manifest = members.find(
    (member) => member.path === CLAUDE_TRANSCRIPT_MANIFEST_MEMBER,
  );
  const files = members.filter(
    (member) => member.path.startsWith(CLAUDE_TRANSCRIPT_FILE_PREFIX),
  );
  if (manifest === undefined) {
    if (files.length > 0) {
      throw new Error("Capsule has Claude transcript files without a manifest");
    }
    return undefined;
  }
  let value: unknown;
  try {
    value = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(
      manifest.bytes,
    )) as unknown;
  } catch (error) {
    throw new Error("Claude transcript capsule manifest is not valid UTF-8 JSON", {
      cause: error,
    });
  }
  const header = parseHeader(value, CLAUDE_TRANSCRIPT_PROTOCOL);
  const byPath = new Map<string, Uint8Array>();
  for (const file of files) {
    const relative = relativeTranscriptPath(
      file.path.slice(CLAUDE_TRANSCRIPT_FILE_PREFIX.length),
    );
    if (byPath.has(relative)) {
      throw new Error("Duplicate Claude transcript capsule file " + relative);
    }
    byPath.set(relative, file.bytes);
  }
  if (byPath.size !== header.files.length) {
    throw new Error("Claude transcript capsule file set is incomplete");
  }
  const decoded: ClaudeTranscriptFile[] = [];
  for (const descriptor of header.files) {
    const fileBytes = byPath.get(descriptor.path);
    if (fileBytes === undefined
      || fileBytes.byteLength !== descriptor.size
      || sha256(fileBytes) !== descriptor.sha256) {
      throw new Error("Claude transcript capsule file " + descriptor.path + " failed verification");
    }
    decoded.push({ path: descriptor.path, bytes: fileBytes });
  }
  return createClaudeTranscriptBundle({ ...header, files: decoded });
}

export function isClaudeTranscriptMember(path: PortableCapsulePath): boolean {
  return path === CLAUDE_TRANSCRIPT_MANIFEST_MEMBER
    || path.startsWith(CLAUDE_TRANSCRIPT_FILE_PREFIX);
}

export function assertResumableClaudeTranscript(
  bundle: ClaudeTranscriptBundle,
): void {
  const sessionFile = bundle.sessionId + ".jsonl";
  if (!bundle.files.some((file) =>
    file.path === sessionFile || file.path.endsWith("/" + sessionFile)
  )) {
    throw new Error(
      "Portable Claude transcript has no main session file for "
        + bundle.sessionId,
    );
  }
}
