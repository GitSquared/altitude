import { randomUUID } from "node:crypto";
import { chmod, lstat, mkdir, readFile, rename, rm, writeFile } from "node:fs/promises";
import { isAbsolute, join, parse, resolve } from "node:path";

import { isSha256Digest, sha256, stableJson, type Sha256Digest } from "./canonical.ts";

const SEED_METADATA_FORMAT = "altitude.repository-seed/v1" as const;
const MAX_SEED_BYTES = 1024 * 1024 * 1024;

type SeedMetadata = Readonly<{
  schema: typeof SEED_METADATA_FORMAT;
  checkpointDigest: Sha256Digest;
  seedDigest: Sha256Digest;
  size: number;
}>;

function key(digest: Sha256Digest): string {
  if (!isSha256Digest(digest)) throw new Error("Checkpoint digest is invalid");
  return digest.slice("sha256:".length);
}

async function regular(path: string, label: string): Promise<ReturnType<typeof lstat>> {
  const stat = await lstat(path);
  if (stat.isSymbolicLink() || !stat.isFile()) throw new Error(`${label} must be a regular file`);
  return stat;
}

export class FilesystemRepositorySeedCache {
  readonly root: string;

  constructor(root: string) {
    if (!isAbsolute(root)) throw new Error("Repository seed cache root must be absolute");
    this.root = resolve(root);
    if (this.root === parse(this.root).root) throw new Error("Repository seed cache cannot use a filesystem root");
  }

  async put(checkpointDigest: Sha256Digest, bytes: Uint8Array): Promise<void> {
    if (bytes.byteLength === 0 || bytes.byteLength > MAX_SEED_BYTES) throw new Error("Repository seed size is invalid");
    await mkdir(this.root, { recursive: true, mode: 0o700 });
    const rootStat = await lstat(this.root);
    if (rootStat.isSymbolicLink() || !rootStat.isDirectory()) throw new Error("Repository seed cache root is unsafe");
    const directory = join(this.root, key(checkpointDigest));
    await mkdir(directory, { mode: 0o700 }).catch(async (error: unknown) => {
      if (typeof error !== "object" || error === null || !("code" in error) || error.code !== "EEXIST") throw error;
      const stat = await lstat(directory);
      if (stat.isSymbolicLink() || !stat.isDirectory()) throw new Error("Repository seed directory is unsafe");
    });
    const seedPath = join(directory, "repository.bundle");
    const metadataPath = join(directory, "metadata.json");
    try {
      const existing = await this.read(checkpointDigest);
      if (sha256(existing) !== sha256(bytes)) throw new Error("Checkpoint is already bound to a different repository seed");
      return;
    } catch (error) {
      if (!(error instanceof Error) || !/does not exist/u.test(error.message)) throw error;
    }
    const temporarySeed = join(directory, `.seed-${randomUUID()}.tmp`);
    const temporaryMetadata = join(directory, `.metadata-${randomUUID()}.tmp`);
    const metadata: SeedMetadata = {
      schema: SEED_METADATA_FORMAT,
      checkpointDigest,
      seedDigest: sha256(bytes),
      size: bytes.byteLength,
    };
    try {
      await writeFile(temporarySeed, bytes, { flag: "wx", mode: 0o600 });
      await writeFile(temporaryMetadata, `${stableJson(metadata)}\n`, { flag: "wx", mode: 0o600 });
      await chmod(temporarySeed, 0o600);
      await chmod(temporaryMetadata, 0o600);
      await rename(temporarySeed, seedPath);
      await rename(temporaryMetadata, metadataPath);
    } finally {
      await rm(temporarySeed, { force: true }).catch(() => undefined);
      await rm(temporaryMetadata, { force: true }).catch(() => undefined);
    }
  }

  async read(checkpointDigest: Sha256Digest): Promise<Uint8Array> {
    const directory = join(this.root, key(checkpointDigest));
    const seedPath = join(directory, "repository.bundle");
    const metadataPath = join(directory, "metadata.json");
    let metadataBytes: Uint8Array;
    let seedBytes: Uint8Array;
    try {
      const metadataStat = await regular(metadataPath, "Repository seed metadata");
      const seedStat = await regular(seedPath, "Repository seed");
      if (metadataStat.size > 64 * 1024 || seedStat.size > MAX_SEED_BYTES) throw new Error("Repository seed cache entry is too large");
      [metadataBytes, seedBytes] = await Promise.all([
        readFile(metadataPath),
        readFile(seedPath),
      ]);
    } catch (error) {
      if (typeof error === "object" && error !== null && "code" in error && error.code === "ENOENT") {
        throw new Error(`Repository seed for ${checkpointDigest} does not exist`, { cause: error });
      }
      throw error;
    }
    const value = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(metadataBytes)) as Partial<SeedMetadata>;
    if (value.schema !== SEED_METADATA_FORMAT || value.checkpointDigest !== checkpointDigest
      || !isSha256Digest(value.seedDigest) || value.size !== seedBytes.byteLength
      || sha256(seedBytes) !== value.seedDigest) {
      throw new Error(`Repository seed for ${checkpointDigest} failed verification`);
    }
    return seedBytes;
  }
}
