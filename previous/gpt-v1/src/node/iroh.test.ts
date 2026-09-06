import { afterEach, describe, expect, test } from "bun:test";
import { EndpointAddr, EndpointTicket, SecretKey } from "@number0/iroh";
import { lstatSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  FilesystemCapsuleCheckpointStore,
  FilesystemCapsuleTransferReceiver,
  FilesystemRepositorySeedCache,
  LocalToRemoteCapsuleTransfer,
  capsuleManifestDigest,
  createCapsuleCheckpoint,
  portableCapsulePath,
  utf8,
} from "../capsule/index.ts";

import {
  ALTITUDE_NODE_TICKET_PROTOCOL,
  IrohNodeClient,
  bindPersistentIrohIdentity,
  createIrohNodeClient,
  encodeAltitudeNodeTicket,
  handleIrohNodeStream,
  loadOrCreateIrohMaterial,
  parseAltitudeNodeTicket,
  parseNodeConnection,
  serveIrohNode,
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

  test.skipIf(process.env.ALTITUDE_ACCEPTANCE_IROH !== "1")(
    "moves and activates a verified capsule over two native Iroh endpoints",
    async () => {
      const root = mkdtempSync(join(tmpdir(), "altitude-iroh-native-"));
      roots.push(root);
      const sourceCheckpoints = new FilesystemCapsuleCheckpointStore(join(root, "source-capsules"));
      const sourceSeeds = new FilesystemRepositorySeedCache(join(root, "source-seeds"));
      const targetCheckpoints = new FilesystemCapsuleCheckpointStore(join(root, "target-capsules"));
      const targetSeeds = new FilesystemRepositorySeedCache(join(root, "target-seeds"));
      const source = createCapsuleCheckpoint({
        capsuleId: "task-native",
        taskId: "task-native",
        revision: 1,
        checkpointedAt: "2026-09-01T10:00:00.000Z",
        ownership: {
          epoch: 1,
          leaseId: "lease-local",
          ownerNodeId: "local",
          status: "active",
        },
        repository: {
          kind: "git",
          remoteUrl: "https://github.com/gitsquared/altitude.git",
          defaultBranch: "main",
          branch: "gaby/native-acceptance",
          baseCommit: "a".repeat(40),
          headCommit: "b".repeat(40),
          bundleMember: null,
          stagedPatchMember: portableCapsulePath("git/staged.patch"),
          unstagedPatchMember: portableCapsulePath("git/unstaged.patch"),
          untracked: [],
        },
        members: [
          { path: portableCapsulePath("git/staged.patch"), bytes: new Uint8Array() },
          { path: portableCapsulePath("git/unstaged.patch"), bytes: new Uint8Array() },
          { path: portableCapsulePath("state/acceptance.txt"), bytes: utf8("native Iroh capsule\n") },
        ],
      });
      const sourceReference = await sourceCheckpoints.publish(source);
      await sourceCheckpoints.activate(sourceReference);
      const seed = utf8("native Iroh repository seed");
      await sourceSeeds.put(sourceReference.digest, seed);
      const receiver = new FilesystemCapsuleTransferReceiver(
        targetCheckpoints,
        targetSeeds,
        "remote",
      );
      const serverIdentity = await bindPersistentIrohIdentity(join(root, "server"));
      const server = await serveIrohNode({
        nodeId: "remote",
        identity: serverIdentity,
        handlers: {
          async health() { return { nodeId: "remote", version: "acceptance" }; },
          prepare: async (payload) => await receiver.prepare(payload),
          activate: async (input) => await receiver.activate(input),
        },
      });
      const clientIdentity = await bindPersistentIrohIdentity(join(root, "client"));
      const client = await createIrohNodeClient(
        clientIdentity.endpoint,
        { nodeId: "remote", ticket: server.ticket },
        async () => await clientIdentity.endpoint.close(),
      );
      try {
        expect(await client.health()).toEqual({ nodeId: "remote", version: "acceptance" });
        const coordinator = new LocalToRemoteCapsuleTransfer(
          sourceCheckpoints,
          sourceSeeds,
          "local",
          client,
          { now: () => Date.parse("2026-09-01T10:05:00.000Z") },
        );
        const prepared = await coordinator.prepare({
          taskId: "task-native",
          transferId: "transfer-native",
          targetNodeId: "remote",
          targetLeaseId: "lease-remote",
        });
        await expect(targetCheckpoints.readActive("task-native")).rejects.toThrow();
        const committed = await coordinator.commit(prepared);
        expect(committed.activation).toMatchObject({
          schema: "altitude.target-activation/v1",
          targetNodeId: "remote",
        });
        for (const checkpoints of [sourceCheckpoints, targetCheckpoints]) {
          const active = await checkpoints.readActive("task-native");
          expect(capsuleManifestDigest(active.manifest)).toBe(prepared.targetCheckpoint.digest);
          expect(active.manifest.ownership).toMatchObject({
            ownerNodeId: "remote",
            leaseId: "lease-remote",
            epoch: 2,
            status: "active",
          });
          expect(new TextDecoder().decode(active.members.find(
            (member) => member.path === portableCapsulePath("state/acceptance.txt"),
          )?.bytes)).toBe("native Iroh capsule\n");
        }
        expect(await targetSeeds.read(prepared.targetCheckpoint.digest)).toEqual(seed);
      } finally {
        await client.close();
        await server.close();
      }
    },
    20_000,
  );
});
