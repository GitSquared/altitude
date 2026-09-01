import { afterEach, describe, expect, test } from "bun:test";
import { lstatSync, mkdtempSync, rmSync, symlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { EndpointAddr, EndpointTicket, SecretKey } from "@number0/iroh";

import {
  FilesystemNodeRegistry,
  registeredNode,
} from "../src/node/registry.ts";
import {
  ALTITUDE_NODE_TICKET_PROTOCOL,
  encodeAltitudeNodeTicket,
} from "../src/node/iroh.ts";

const roots: string[] = [];
const tickets = new Map<string, string>();

afterEach(() => {
  for (const root of roots.splice(0)) {
    rmSync(root, { recursive: true, force: true });
  }
});

function direct(id: string) {
  let ticket = tickets.get(id);
  if (ticket === undefined) {
    const endpointId = SecretKey.generate().public();
    ticket = encodeAltitudeNodeTicket({
      schema: ALTITUDE_NODE_TICKET_PROTOCOL,
      nodeId: id,
      endpointId: endpointId.toString(),
      endpointTicket: EndpointTicket.fromAddr(new EndpointAddr(endpointId)).toString(),
      pairingToken: "a".repeat(43),
    });
    tickets.set(id, ticket);
  }
  return registeredNode({
    nodeId: id,
    ticket,
  });
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
    })).toThrow("node id is invalid");
  });
});
