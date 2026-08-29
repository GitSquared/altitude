import { afterEach, describe, expect, test } from "bun:test";
import { lstatSync, mkdtempSync, rmSync, symlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  FilesystemNodeRegistry,
  registeredNode,
} from "../src/node/registry.ts";

const roots: string[] = [];

afterEach(() => {
  for (const root of roots.splice(0)) {
    rmSync(root, { recursive: true, force: true });
  }
});

function direct(id: string) {
  return registeredNode({
    nodeId: id,
    user: "altitude",
    identityFile: `/state/nodes/${id}/id_ed25519`,
    knownHostsFile: `/state/nodes/${id}/known_hosts`,
    route: { kind: "ssh", host: `${id}.example`, port: 2222 },
  }, "/opt/altitude/bin/altitude");
}

describe("filesystem node registry", () => {
  test("atomically remembers private node routes and lists them by id", async () => {
    const root = mkdtempSync(join(tmpdir(), "altitude-node-registry-"));
    roots.push(root);
    const registry = new FilesystemNodeRegistry(join(root, "nodes"));
    await registry.put(direct("straylight"));
    await registry.put(direct("basement"));

    expect(await registry.get("straylight")).toEqual(direct("straylight"));
    expect((await registry.list()).map((node) => node.connection.nodeId))
      .toEqual(["basement", "straylight"]);
    expect(lstatSync(join(root, "nodes", "straylight.json")).mode & 0o777)
      .toBe(0o600);
    expect(lstatSync(join(root, "nodes")).mode & 0o777).toBe(0o700);
  });

  test("rejects mismatched records and symlink entries", async () => {
    const root = mkdtempSync(join(tmpdir(), "altitude-node-registry-"));
    roots.push(root);
    const nodeRoot = join(root, "nodes");
    const registry = new FilesystemNodeRegistry(nodeRoot);
    await registry.put(direct("straylight"));
    symlinkSync(join(nodeRoot, "straylight.json"), join(nodeRoot, "shadow.json"));

    await expect(registry.get("shadow")).rejects.toThrow("regular file");
    await expect(registry.list()).rejects.toThrow("not a regular file");
    expect(() => registeredNode({
      ...direct("straylight").connection,
      nodeId: "unsafe/id",
    })).toThrow("Unsafe node id");
  });
});
