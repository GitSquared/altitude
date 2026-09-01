import { afterEach, describe, expect, test } from "bun:test";
import { EndpointAddr, EndpointTicket, SecretKey } from "@number0/iroh";
import { lstatSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  ALTITUDE_NODE_TICKET_PROTOCOL,
  IrohNodeClient,
  encodeAltitudeNodeTicket,
  handleIrohNodeStream,
  loadOrCreateIrohMaterial,
  parseAltitudeNodeTicket,
  parseNodeConnection,
  type IrohBiStream,
  type IrohNodeHandlers,
} from "./iroh.ts";

const endpointId = SecretKey.generate().public();
const nativeTicket = EndpointTicket.fromAddr(new EndpointAddr(endpointId));
const pairingToken = "a".repeat(43);
const roots: string[] = [];

afterEach(() => {
  for (const root of roots.splice(0)) rmSync(root, { recursive: true, force: true });
});

function ticket(nodeId = "straylight"): string {
  return encodeAltitudeNodeTicket({
    schema: ALTITUDE_NODE_TICKET_PROTOCOL,
    nodeId,
    endpointId: endpointId.toString(),
    endpointTicket: nativeTicket.toString(),
    pairingToken,
  });
}

class Channel {
  readonly #chunks: Uint8Array[] = [];
  readonly #waiters: Array<() => void> = [];
  #finished = false;

  async write(bytes: readonly number[]): Promise<void> {
    this.#chunks.push(Uint8Array.from(bytes));
    this.#wake();
  }

  finish(): void {
    this.#finished = true;
    this.#wake();
  }

  async read(limit: number): Promise<number[]> {
    while (this.#chunks.length === 0 && !this.#finished) {
      await new Promise<void>((resolve) => this.#waiters.push(resolve));
    }
    const first = this.#chunks.shift();
    if (first === undefined) return [];
    if (first.byteLength <= limit) return Array.from(first);
    this.#chunks.unshift(first.subarray(limit));
    return Array.from(first.subarray(0, limit));
  }

  #wake(): void {
    for (const waiter of this.#waiters.splice(0)) waiter();
  }
}

function streamPair(): readonly [IrohBiStream, IrohBiStream] {
  const clientToServer = new Channel();
  const serverToClient = new Channel();
  return [
    {
      send: {
        writeAll: (bytes) => clientToServer.write(bytes),
        finish: async () => clientToServer.finish(),
      },
      recv: { read: (limit) => serverToClient.read(limit) },
    },
    {
      send: {
        writeAll: (bytes) => serverToClient.write(bytes),
        finish: async () => serverToClient.finish(),
      },
      recv: { read: (limit) => clientToServer.read(limit) },
    },
  ];
}

function checkpoint() {
  return {
    capsuleId: "task-1",
    revision: 2,
    digest: `sha256:${"a".repeat(64)}` as const,
  };
}

describe("Altitude Iroh route", () => {
  test("keeps node identity and pairing capability private and stable", async () => {
    const root = mkdtempSync(join(tmpdir(), "altitude-iroh-identity-"));
    roots.push(root);
    const identityRoot = join(root, "identity");
    const first = await loadOrCreateIrohMaterial(identityRoot);
    const second = await loadOrCreateIrohMaterial(identityRoot);

    expect(second).toEqual(first);
    expect(first.secretKey).toHaveLength(32);
    expect(first.pairingToken).toHaveLength(43);
    expect(lstatSync(identityRoot).mode & 0o777).toBe(0o700);
    expect(lstatSync(join(identityRoot, "iroh-secret.key")).mode & 0o777).toBe(0o600);
    expect(lstatSync(join(identityRoot, "iroh-pairing.token")).mode & 0o777).toBe(0o600);
  });

  test("encodes one self-contained private pairing ticket", () => {
    const encoded = ticket();
    const parsed = parseAltitudeNodeTicket(encoded);
    expect(parsed).toMatchObject({
      schema: ALTITUDE_NODE_TICKET_PROTOCOL,
      nodeId: "straylight",
      endpointId: endpointId.toString(),
      pairingToken,
    });
    expect(parseNodeConnection({ nodeId: "straylight", ticket: encoded }))
      .toEqual({ nodeId: "straylight", ticket: encoded });
    expect(() => parseNodeConnection({ nodeId: "other", ticket: encoded }))
      .toThrow("another node");
  });

  test("routes health and binary capsule transfer over framed bidirectional streams", async () => {
    const preparedPayloads: Uint8Array[] = [];
    const handlers: IrohNodeHandlers = {
      async health() {
        return { nodeId: "straylight", version: "0.0.0" };
      },
      async prepare(payload) {
        preparedPayloads.push(payload);
        return {
          schema: "altitude.target-prepared/v1",
          transferId: "transfer-1",
          targetNodeId: "straylight",
          checkpoint: checkpoint(),
        };
      },
      async activate(input) {
        expect(input).toEqual({ transferId: "transfer-1", checkpoint: checkpoint() });
        return {
          schema: "altitude.target-activation/v1",
          transferId: input.transferId,
          targetNodeId: "straylight",
          checkpoint: input.checkpoint,
        };
      },
    };
    const client = new IrohNodeClient(
      { nodeId: "straylight", ticket: ticket() },
      async () => {
        const [clientStream, serverStream] = streamPair();
        void handleIrohNodeStream(serverStream, pairingToken, handlers);
        return clientStream;
      },
    );

    expect(await client.health()).toEqual({ nodeId: "straylight", version: "0.0.0" });
    const payload = Uint8Array.from({ length: 200_000 }, (_, index) => index % 251);
    expect(await client.prepare(payload)).toMatchObject({ transferId: "transfer-1" });
    expect(preparedPayloads).toEqual([payload]);
    expect(await client.activate({ transferId: "transfer-1", checkpoint: checkpoint() }))
      .toMatchObject({ schema: "altitude.target-activation/v1", targetNodeId: "straylight" });
  });

  test("rejects a client whose pairing capability does not match", async () => {
    const handlers: IrohNodeHandlers = {
      async health() { return { nodeId: "straylight", version: "0.0.0" }; },
      async prepare() { throw new Error("not used"); },
      async activate() { throw new Error("not used"); },
    };
    const wrong = encodeAltitudeNodeTicket({
      ...parseAltitudeNodeTicket(ticket()),
      pairingToken: "b".repeat(43),
    });
    const client = new IrohNodeClient({ nodeId: "straylight", ticket: wrong }, async () => {
      const [clientStream, serverStream] = streamPair();
      void handleIrohNodeStream(serverStream, pairingToken, handlers);
      return clientStream;
    });
    await expect(client.health()).rejects.toThrow("pairing token was rejected");
  });
});
