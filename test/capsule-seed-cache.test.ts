import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { FilesystemRepositorySeedCache } from "../src/capsule/seed-cache.ts";
import { sha256 } from "../src/capsule/canonical.ts";

const directories: string[] = [];
afterEach(() => {
  for (const directory of directories.splice(0)) rmSync(directory, { recursive: true, force: true });
});

describe("node-local repository seed cache", () => {
  test("persists and verifies seed material by portable checkpoint digest", async () => {
    const root = mkdtempSync(join(tmpdir(), "altitude-seed-cache-"));
    directories.push(root);
    const cache = new FilesystemRepositorySeedCache(join(root, "seeds"));
    const checkpoint = sha256(new TextEncoder().encode("checkpoint"));
    const seed = new Uint8Array([1, 2, 3, 4]);
    await cache.put(checkpoint, seed);
    expect(await cache.read(checkpoint)).toEqual(seed);
    await cache.put(checkpoint, seed);
    expect(cache.put(checkpoint, new Uint8Array([9]))).rejects.toThrow("different repository seed");
  });

  test("detects a corrupted cached bundle", async () => {
    const root = mkdtempSync(join(tmpdir(), "altitude-seed-corrupt-"));
    directories.push(root);
    const cacheRoot = join(root, "seeds");
    const cache = new FilesystemRepositorySeedCache(cacheRoot);
    const checkpoint = sha256(new TextEncoder().encode("checkpoint"));
    await cache.put(checkpoint, new Uint8Array([1, 2, 3]));
    writeFileSync(join(cacheRoot, checkpoint.slice("sha256:".length), "repository.bundle"), "corrupt");
    expect(cache.read(checkpoint)).rejects.toThrow("failed verification");
  });
});
