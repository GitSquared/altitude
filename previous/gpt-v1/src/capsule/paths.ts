import { isAbsolute, posix, relative, resolve } from "node:path";

declare const portablePathBrand: unique symbol;
declare const repositoryPathBrand: unique symbol;

export type PortableCapsulePath = string & {
  readonly [portablePathBrand]: true;
};

export type SafeRepositoryPath = string & {
  readonly [repositoryPathBrand]: true;
};

export class UnsafeCapsulePathError extends Error {
  override readonly name = "UnsafeCapsulePathError";
}

const PORTABLE_NAMESPACES = new Set([
  "artifacts",
  "git",
  "services",
  "session",
  "state",
  "workspace",
]);

const NODE_LOCAL_ROOTS = new Set([
  "cache",
  "credentials",
  "identity",
  "node-local",
  "runtime",
  "secrets",
]);

const CREDENTIAL_FILENAMES = new Set([
  ".credentials.json",
  ".git-credentials",
  ".netrc",
  ".npmrc",
  "credentials.json",
]);

function validateRelativePosixPath(value: string, label: string): string[] {
  if (value.length === 0) {
    throw new UnsafeCapsulePathError(`${label} cannot be empty`);
  }
  if (value.includes("\0")) {
    throw new UnsafeCapsulePathError(`${label} cannot contain NUL bytes`);
  }
  if (value.includes("\\")) {
    throw new UnsafeCapsulePathError(
      `${label} must use portable POSIX separators`,
    );
  }
  if (value.startsWith("/") || /^[A-Za-z]:/u.test(value)) {
    throw new UnsafeCapsulePathError(`${label} must be relative`);
  }
  if (value.endsWith("/") || value.includes("//")) {
    throw new UnsafeCapsulePathError(`${label} must be normalized`);
  }

  const segments = value.split("/");
  if (segments.some((segment) => segment === "" || segment === "." || segment === "..")) {
    throw new UnsafeCapsulePathError(`${label} cannot traverse directories`);
  }
  if (posix.normalize(value) !== value) {
    throw new UnsafeCapsulePathError(`${label} must be normalized`);
  }
  if (segments.some((segment) => /[\u0000-\u001f\u007f]/u.test(segment))) {
    throw new UnsafeCapsulePathError(`${label} cannot contain control characters`);
  }
  return segments;
}

export function portableCapsulePath(value: string): PortableCapsulePath {
  const segments = validateRelativePosixPath(value, "Capsule member path");
  const root = segments[0];
  if (root === undefined || !PORTABLE_NAMESPACES.has(root)) {
    const boundary = root !== undefined && NODE_LOCAL_ROOTS.has(root)
      ? " is node-local and cannot enter a capsule"
      : " is not a portable capsule namespace";
    throw new UnsafeCapsulePathError(`Capsule namespace ${JSON.stringify(root)}${boundary}`);
  }
  if (segments.length < 2) {
    throw new UnsafeCapsulePathError(
      "Capsule member paths must include a namespace and filename",
    );
  }

  const lower = segments.map((segment) => segment.toLowerCase());
  if (
    lower.includes(".ssh") ||
    lower.includes(".aws") ||
    lower.includes(".gnupg") ||
    lower.includes("node-local") ||
    lower.some((segment) => CREDENTIAL_FILENAMES.has(segment)) ||
    (lower.includes(".claude") && lower.includes(".credentials.json")) ||
    (lower.includes(".config") && lower.includes("gh") && lower.includes("hosts.yml")) ||
    (lower.includes(".docker") && lower.includes("config.json"))
  ) {
    throw new UnsafeCapsulePathError(
      `Capsule member ${JSON.stringify(value)} crosses the node-local credential boundary`,
    );
  }

  return value as PortableCapsulePath;
}

export function safeRepositoryPath(value: string): SafeRepositoryPath {
  const segments = validateRelativePosixPath(value, "Repository path");
  if (segments.some((segment) => segment.toLowerCase() === ".git")) {
    throw new UnsafeCapsulePathError(
      "Repository archive entries cannot address Git internals",
    );
  }
  return value as SafeRepositoryPath;
}

export function untrackedMemberPath(
  repositoryPath: SafeRepositoryPath,
): PortableCapsulePath {
  return portableCapsulePath(`git/untracked/${repositoryPath}`);
}

export function resolvePortablePath(root: string, path: PortableCapsulePath): string {
  const absoluteRoot = resolve(root);
  const candidate = resolve(absoluteRoot, ...path.split("/"));
  const fromRoot = relative(absoluteRoot, candidate);
  if (fromRoot === "" || fromRoot === ".." || fromRoot.startsWith(`..${posix.sep}`) || isAbsolute(fromRoot)) {
    throw new UnsafeCapsulePathError(`Path ${JSON.stringify(path)} escapes its capsule root`);
  }
  return candidate;
}

export const PORTABLE_CAPSULE_NAMESPACES = Object.freeze([
  ...PORTABLE_NAMESPACES,
]);

export interface NodeLocalCapsuleBindings {
  readonly capsuleId: string;
  readonly nodeId: string;
  readonly identityProfileId: string;
  readonly runtimeHandle: string | null;
  readonly cacheKeys: readonly string[];
  readonly secretReferences: readonly string[];
}
