import { randomBytes, randomUUID, timingSafeEqual } from "node:crypto";
import { chmod, lstat, mkdir, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";

import {
  Endpoint,
  EndpointAddr,
  EndpointId,
  EndpointTicket,
  SecretKey,
  type BiStream,
  type Connection,
  type RecvStream,
  type SendStream,
} from "@number0/iroh";

import type { CapsuleCheckpointReference } from "../capsule/filesystem.ts";
import {
  MAX_CAPSULE_TRANSFER_PACKAGE_BYTES,
  type CapsuleTransferTransport,
  type TargetActivationReceipt,
  type TargetPreparedReceipt,
} from "../capsule/transfer-runtime.ts";

export const ALTITUDE_IROH_PROTOCOL = "altitude.iroh-node/v1" as const;
export const ALTITUDE_IROH_ALPN = new TextEncoder().encode(ALTITUDE_IROH_PROTOCOL);
export const ALTITUDE_NODE_TICKET_PROTOCOL = "altitude.node-ticket/v1" as const;

const TICKET_PREFIX = "altitude-node:";
const MAX_HEADER_BYTES = 64 * 1024;
const STREAM_CHUNK_BYTES = 64 * 1024;
const SECRET_KEY_BYTES = 32;
const PAIRING_TOKEN_BYTES = 32;
const ENDPOINT_ID = /^[a-f0-9]{64}$/u;
const PAIRING_TOKEN = /^[A-Za-z0-9_-]{43}$/u;
const SAFE_NODE_ID = /^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/u;

export interface AltitudeNodeTicket {
  readonly schema: typeof ALTITUDE_NODE_TICKET_PROTOCOL;
  readonly nodeId: string;
  readonly endpointId: string;
  readonly endpointTicket: string;
  readonly pairingToken: string;
}

export interface NodeConnection {
  readonly nodeId: string;
  readonly ticket: string;
}

export interface IrohNodeHealth {
  readonly nodeId: string;
  readonly version: string;
}

export interface AltitudeNodeClient extends CapsuleTransferTransport {
  health(): Promise<IrohNodeHealth>;
  close(): Promise<void>;
}

type IrohNodeRequest =
  | Readonly<{
      schema: typeof ALTITUDE_IROH_PROTOCOL;
      requestId: string;
      pairingToken: string;
      operation: "health";
      bodyLength: 0;
    }>
  | Readonly<{
      schema: typeof ALTITUDE_IROH_PROTOCOL;
      requestId: string;
      pairingToken: string;
      operation: "transfer.prepare";
      bodyLength: number;
    }>
  | Readonly<{
      schema: typeof ALTITUDE_IROH_PROTOCOL;
      requestId: string;
      pairingToken: string;
      operation: "transfer.activate";
      bodyLength: 0;
      transferId: string;
      checkpoint: CapsuleCheckpointReference;
    }>;

type IrohNodeResponse = Readonly<{
  schema: typeof ALTITUDE_IROH_PROTOCOL;
  requestId: string;
  ok: true;
  result: unknown;
}> | Readonly<{
  schema: typeof ALTITUDE_IROH_PROTOCOL;
  requestId: string;
  ok: false;
  error: Readonly<{ code: string; message: string }>;
}>;

export interface IrohBiStream {
  readonly send: Pick<SendStream, "writeAll" | "finish">;
  readonly recv: Pick<RecvStream, "read">;
}

export interface IrohNodeHandlers {
  health(): Promise<IrohNodeHealth>;
  prepare(payload: Uint8Array): Promise<TargetPreparedReceipt>;
  activate(input: {
    readonly transferId: string;
    readonly checkpoint: CapsuleCheckpointReference;
  }): Promise<TargetActivationReceipt>;
}

function record(value: unknown, label: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error(`${label} is invalid`);
  }
  return value as Record<string, unknown>;
}

function requireNodeId(value: unknown): string {
  if (typeof value !== "string" || !SAFE_NODE_ID.test(value)) {
    throw new Error("Iroh node id is invalid");
  }
  return value;
}

function requireEndpointId(value: unknown): string {
  if (typeof value !== "string" || !ENDPOINT_ID.test(value)) {
    throw new Error("Iroh endpoint id is invalid");
  }
  return value;
}

function requirePairingToken(value: unknown): string {
  if (typeof value !== "string" || !PAIRING_TOKEN.test(value)) {
    throw new Error("Iroh pairing token is invalid");
  }
  return value;
}

function parseCheckpointReference(value: unknown): CapsuleCheckpointReference {
  const checkpoint = record(value, "Iroh checkpoint reference");
  if (typeof checkpoint.capsuleId !== "string"
    || !SAFE_NODE_ID.test(checkpoint.capsuleId)
    || !Number.isSafeInteger(checkpoint.revision)
    || (checkpoint.revision as number) < 1
    || typeof checkpoint.digest !== "string"
    || !/^sha256:[a-f0-9]{64}$/u.test(checkpoint.digest)) {
    throw new Error("Iroh checkpoint reference is invalid");
  }
  return checkpoint as unknown as CapsuleCheckpointReference;
}

export function encodeAltitudeNodeTicket(input: AltitudeNodeTicket): string {
  const parsed = parseAltitudeNodeTicket(input);
  return TICKET_PREFIX + Buffer.from(JSON.stringify(parsed), "utf8").toString("base64url");
}

export function parseAltitudeNodeTicket(value: unknown): AltitudeNodeTicket {
  let decoded: unknown = value;
  if (typeof value === "string") {
    if (!value.startsWith(TICKET_PREFIX) || value.length > 32 * 1024) {
      throw new Error("Altitude node ticket is invalid");
    }
    try {
      decoded = JSON.parse(Buffer.from(value.slice(TICKET_PREFIX.length), "base64url").toString("utf8")) as unknown;
    } catch (error) {
      throw new Error("Altitude node ticket is invalid", { cause: error });
    }
  }
  const ticket = record(decoded, "Altitude node ticket");
  if (ticket.schema !== ALTITUDE_NODE_TICKET_PROTOCOL
    || typeof ticket.endpointTicket !== "string"
    || !ticket.endpointTicket.trim()
    || ticket.endpointTicket.length > 16 * 1024) {
    throw new Error("Altitude node ticket is unsupported");
  }
  const nodeId = requireNodeId(ticket.nodeId);
  const endpointId = requireEndpointId(ticket.endpointId);
  const pairingToken = requirePairingToken(ticket.pairingToken);
  let native: EndpointTicket;
  try {
    native = EndpointTicket.fromString(ticket.endpointTicket);
  } catch (error) {
    throw new Error("Iroh endpoint ticket is invalid", { cause: error });
  }
  if (native.endpointAddr().id().toString() !== endpointId) {
    throw new Error("Iroh endpoint ticket does not match its endpoint id");
  }
  return {
    schema: ALTITUDE_NODE_TICKET_PROTOCOL,
    nodeId,
    endpointId,
    endpointTicket: native.toString(),
    pairingToken,
  };
}

export function parseNodeConnection(value: unknown): NodeConnection {
  const connection = record(value, "Node connection");
  const nodeId = requireNodeId(connection.nodeId);
  if (typeof connection.ticket !== "string") throw new Error("Node connection ticket is invalid");
  const ticket = parseAltitudeNodeTicket(connection.ticket);
  if (ticket.nodeId !== nodeId) throw new Error("Node connection ticket targets another node");
  return { nodeId, ticket: encodeAltitudeNodeTicket(ticket) };
}

function parseRequest(value: unknown): IrohNodeRequest {
  const request = record(value, "Iroh request");
  if (request.schema !== ALTITUDE_IROH_PROTOCOL
    || typeof request.requestId !== "string"
    || !SAFE_NODE_ID.test(request.requestId)
    || (request.operation !== "health"
      && request.operation !== "transfer.prepare"
      && request.operation !== "transfer.activate")
    || !Number.isSafeInteger(request.bodyLength)
    || (request.bodyLength as number) < 0
    || (request.bodyLength as number) > MAX_CAPSULE_TRANSFER_PACKAGE_BYTES) {
    throw new Error("Iroh request is invalid");
  }
  requirePairingToken(request.pairingToken);
  if (request.operation === "health" && request.bodyLength !== 0) {
    throw new Error("Iroh health request cannot carry a body");
  }
  if (request.operation === "transfer.prepare" && request.bodyLength === 0) {
    throw new Error("Iroh transfer package is empty");
  }
  if (request.operation === "transfer.activate") {
    if (request.bodyLength !== 0 || typeof request.transferId !== "string" || !SAFE_NODE_ID.test(request.transferId)) {
      throw new Error("Iroh activation request is invalid");
    }
    parseCheckpointReference(request.checkpoint);
  }
  return request as IrohNodeRequest;
}

function parseResponse(value: unknown, requestId: string): IrohNodeResponse {
  const response = record(value, "Iroh response");
  if (response.schema !== ALTITUDE_IROH_PROTOCOL || response.requestId !== requestId || typeof response.ok !== "boolean") {
    throw new Error("Iroh response is invalid or mismatched");
  }
  if (response.ok) {
    if (!("result" in response)) throw new Error("Iroh response has no result");
  } else {
    const error = record(response.error, "Iroh response error");
    if (typeof error.code !== "string" || typeof error.message !== "string") {
      throw new Error("Iroh response error is invalid");
    }
  }
  return response as IrohNodeResponse;
}

function equalToken(left: string, right: string): boolean {
  const a = Buffer.from(left, "utf8");
  const b = Buffer.from(right, "utf8");
  return a.byteLength === b.byteLength && timingSafeEqual(a, b);
}

async function writeBytes(send: IrohBiStream["send"], bytes: Uint8Array): Promise<void> {
  for (let offset = 0; offset < bytes.byteLength; offset += STREAM_CHUNK_BYTES) {
    await send.writeAll(Array.from(bytes.subarray(offset, Math.min(bytes.byteLength, offset + STREAM_CHUNK_BYTES))));
  }
}

async function readBytes(recv: IrohBiStream["recv"], size: number): Promise<Uint8Array> {
  if (!Number.isSafeInteger(size) || size < 0 || size > MAX_CAPSULE_TRANSFER_PACKAGE_BYTES) {
    throw new Error("Iroh stream body size is invalid");
  }
  const result = new Uint8Array(size);
  let offset = 0;
  while (offset < size) {
    const chunk = await recv.read(Math.min(STREAM_CHUNK_BYTES, size - offset));
    if (chunk.length === 0) throw new Error("Iroh stream ended before its declared body");
    if (chunk.length > size - offset) throw new Error("Iroh stream exceeded its declared body");
    result.set(chunk, offset);
    offset += chunk.length;
  }
  return result;
}

async function writeHeader(send: IrohBiStream["send"], value: unknown): Promise<void> {
  const header = new TextEncoder().encode(JSON.stringify(value));
  if (header.byteLength === 0 || header.byteLength > MAX_HEADER_BYTES) throw new Error("Iroh header is too large");
  const prefix = new Uint8Array(4);
  new DataView(prefix.buffer).setUint32(0, header.byteLength, false);
  await writeBytes(send, prefix);
  await writeBytes(send, header);
}

async function readHeader(recv: IrohBiStream["recv"]): Promise<unknown> {
  const prefix = await readBytes(recv, 4);
  const size = new DataView(prefix.buffer, prefix.byteOffset, prefix.byteLength).getUint32(0, false);
  if (size === 0 || size > MAX_HEADER_BYTES) throw new Error("Iroh header size is invalid");
  const bytes = await readBytes(recv, size);
  try {
    return JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes)) as unknown;
  } catch (error) {
    throw new Error("Iroh header is invalid JSON", { cause: error });
  }
}

function responseError(requestId: string, error: unknown): IrohNodeResponse {
  return {
    schema: ALTITUDE_IROH_PROTOCOL,
    requestId,
    ok: false,
    error: {
      code: "request_failed",
      message: error instanceof Error ? error.message : String(error),
    },
  };
}

export async function handleIrohNodeStream(
  stream: IrohBiStream,
  pairingToken: string,
  handlers: IrohNodeHandlers,
): Promise<void> {
  let requestId = "invalid";
  try {
    const request = parseRequest(await readHeader(stream.recv));
    requestId = request.requestId;
    if (!equalToken(request.pairingToken, requirePairingToken(pairingToken))) {
      throw new Error("Iroh pairing token was rejected");
    }
    const body = await readBytes(stream.recv, request.bodyLength);
    const result = request.operation === "health"
      ? await handlers.health()
      : request.operation === "transfer.prepare"
        ? await handlers.prepare(body)
        : await handlers.activate({
            transferId: request.transferId,
            checkpoint: parseCheckpointReference(request.checkpoint),
          });
    await writeHeader(stream.send, {
      schema: ALTITUDE_IROH_PROTOCOL,
      requestId,
      ok: true,
      result,
    } satisfies IrohNodeResponse);
  } catch (error) {
    await writeHeader(stream.send, responseError(requestId, error));
  } finally {
    await stream.send.finish();
  }
}

export class IrohNodeClient implements AltitudeNodeClient {
  readonly #ticket: AltitudeNodeTicket;

  constructor(
    connection: NodeConnection,
    private readonly openStream: () => Promise<IrohBiStream>,
    private readonly closeEndpoint: () => Promise<void> = async () => undefined,
  ) {
    const parsed = parseNodeConnection(connection);
    this.#ticket = parseAltitudeNodeTicket(parsed.ticket);
  }

  async #request(operation: IrohNodeRequest["operation"], input: {
    readonly body?: Uint8Array;
    readonly transferId?: string;
    readonly checkpoint?: CapsuleCheckpointReference;
  } = {}): Promise<unknown> {
    const requestId = randomUUID();
    const body = input.body ?? new Uint8Array();
    const request = {
      schema: ALTITUDE_IROH_PROTOCOL,
      requestId,
      pairingToken: this.#ticket.pairingToken,
      operation,
      bodyLength: body.byteLength,
      ...(input.transferId === undefined ? {} : { transferId: input.transferId }),
      ...(input.checkpoint === undefined ? {} : { checkpoint: input.checkpoint }),
    } as IrohNodeRequest;
    parseRequest(request);
    const stream = await this.openStream();
    await writeHeader(stream.send, request);
    await writeBytes(stream.send, body);
    await stream.send.finish();
    const response = parseResponse(await readHeader(stream.recv), requestId);
    if (!response.ok) throw new Error(`Iroh node request failed: ${response.error.message}`);
    return response.result;
  }

  async health(): Promise<IrohNodeHealth> {
    const result = record(await this.#request("health"), "Iroh health result");
    if (result.nodeId !== this.#ticket.nodeId || typeof result.version !== "string" || !result.version) {
      throw new Error("Iroh health response targets another node");
    }
    return { nodeId: result.nodeId, version: result.version };
  }

  async prepare(payload: Uint8Array): Promise<TargetPreparedReceipt> {
    const result = record(await this.#request("transfer.prepare", { body: payload }), "Iroh prepare result");
    if (result.schema !== "altitude.target-prepared/v1"
      || typeof result.transferId !== "string"
      || result.targetNodeId !== this.#ticket.nodeId) {
      throw new Error("Iroh prepare receipt is invalid");
    }
    return {
      schema: "altitude.target-prepared/v1",
      transferId: result.transferId,
      targetNodeId: result.targetNodeId,
      checkpoint: parseCheckpointReference(result.checkpoint),
    };
  }

  async activate(input: {
    readonly transferId: string;
    readonly checkpoint: CapsuleCheckpointReference;
  }): Promise<TargetActivationReceipt> {
    const result = record(await this.#request("transfer.activate", input), "Iroh activation result");
    if (result.schema !== "altitude.target-activation/v1"
      || result.transferId !== input.transferId
      || result.targetNodeId !== this.#ticket.nodeId) {
      throw new Error("Iroh activation receipt is invalid");
    }
    return {
      schema: "altitude.target-activation/v1",
      transferId: result.transferId,
      targetNodeId: result.targetNodeId,
      checkpoint: parseCheckpointReference(result.checkpoint),
    };
  }

  close(): Promise<void> {
    return this.closeEndpoint();
  }
}

export async function createIrohNodeClient(
  endpoint: Endpoint,
  connection: NodeConnection,
  closeEndpoint: () => Promise<void> = async () => undefined,
): Promise<IrohNodeClient> {
  const parsed = parseNodeConnection(connection);
  const ticket = parseAltitudeNodeTicket(parsed.ticket);
  const endpointAddr = EndpointTicket.fromString(ticket.endpointTicket).endpointAddr();
  if (endpointAddr.id().toString() !== ticket.endpointId) {
    throw new Error("Iroh node endpoint identity changed");
  }
  return new IrohNodeClient(parsed, async () => {
    const remote = await endpoint.connect(endpointAddr, Array.from(ALTITUDE_IROH_ALPN));
    return await remote.openBi();
  }, closeEndpoint);
}

async function readPrivateBytes(path: string, size: number, label: string): Promise<Uint8Array | undefined> {
  let stat;
  try {
    stat = await lstat(path);
  } catch (error) {
    if (typeof error === "object" && error !== null && "code" in error && error.code === "ENOENT") return undefined;
    throw error;
  }
  if (stat.isSymbolicLink() || !stat.isFile() || stat.size !== size) {
    throw new Error(`${label} is unsafe or invalid`);
  }
  return new Uint8Array(await readFile(path));
}

async function createPrivateBytes(path: string, bytes: Uint8Array): Promise<boolean> {
  try {
    await writeFile(path, bytes, { flag: "wx", mode: 0o600 });
    await chmod(path, 0o600);
    return true;
  } catch (error) {
    if (typeof error === "object" && error !== null && "code" in error && error.code === "EEXIST") {
      return false;
    }
    throw error;
  }
}

export interface PersistentIrohMaterial {
  readonly secretKey: Uint8Array;
  readonly pairingToken: string;
}

export interface PersistentIrohIdentity {
  readonly endpoint: Endpoint;
  readonly pairingToken: string;
}

export async function loadOrCreateIrohMaterial(root: string): Promise<PersistentIrohMaterial> {
  await mkdir(root, { recursive: true, mode: 0o700 });
  const rootStat = await lstat(root);
  if (rootStat.isSymbolicLink() || !rootStat.isDirectory()) throw new Error("Iroh identity directory is unsafe");
  await chmod(root, 0o700);
  const keyPath = join(root, "iroh-secret.key");
  const tokenPath = join(root, "iroh-pairing.token");
  let keyBytes = await readPrivateBytes(keyPath, SECRET_KEY_BYTES, "Iroh secret key");
  if (keyBytes === undefined) {
    const generated = Uint8Array.from(SecretKey.generate().toBytes());
    keyBytes = await createPrivateBytes(keyPath, generated)
      ? generated
      : await readPrivateBytes(keyPath, SECRET_KEY_BYTES, "Iroh secret key");
  }
  let tokenBytes = await readPrivateBytes(tokenPath, PAIRING_TOKEN_BYTES, "Iroh pairing token");
  if (tokenBytes === undefined) {
    const generated = randomBytes(PAIRING_TOKEN_BYTES);
    tokenBytes = await createPrivateBytes(tokenPath, generated)
      ? generated
      : await readPrivateBytes(tokenPath, PAIRING_TOKEN_BYTES, "Iroh pairing token");
  }
  if (keyBytes === undefined || tokenBytes === undefined) {
    throw new Error("Iroh identity creation raced without producing durable material");
  }
  return {
    secretKey: keyBytes,
    pairingToken: Buffer.from(tokenBytes).toString("base64url"),
  };
}

export async function bindPersistentIrohIdentity(root: string): Promise<PersistentIrohIdentity> {
  const material = await loadOrCreateIrohMaterial(root);
  const endpoint = await Endpoint.bind({
    secretKey: Array.from(material.secretKey),
    alpns: [Array.from(ALTITUDE_IROH_ALPN)],
  });
  return { endpoint, pairingToken: material.pairingToken };
}

export async function nodeTicketFor(
  nodeId: string,
  identity: PersistentIrohIdentity,
  onlineTimeoutMs = 5_000,
): Promise<string> {
  let timeout: ReturnType<typeof setTimeout> | undefined;
  await Promise.race([
    identity.endpoint.online().catch(() => undefined),
    new Promise<void>((resolve) => {
      timeout = setTimeout(resolve, onlineTimeoutMs);
    }),
  ]).finally(() => {
    if (timeout !== undefined) clearTimeout(timeout);
  });
  const endpointTicket = EndpointTicket.fromAddr(identity.endpoint.addr());
  return encodeAltitudeNodeTicket({
    schema: ALTITUDE_NODE_TICKET_PROTOCOL,
    nodeId: requireNodeId(nodeId),
    endpointId: identity.endpoint.id().toString(),
    endpointTicket: endpointTicket.toString(),
    pairingToken: requirePairingToken(identity.pairingToken),
  });
}

export interface RunningIrohNode {
  readonly ticket: string;
  close(): Promise<void>;
}

async function serveConnection(
  connection: Connection,
  pairingToken: string,
  handlers: IrohNodeHandlers,
): Promise<void> {
  while (true) {
    try {
      const stream: BiStream = await connection.acceptBi();
      void handleIrohNodeStream(stream, pairingToken, handlers);
    } catch {
      return;
    }
  }
}

export async function serveIrohNode(input: {
  readonly nodeId: string;
  readonly identity: PersistentIrohIdentity;
  readonly handlers: IrohNodeHandlers;
}): Promise<RunningIrohNode> {
  const ticket = await nodeTicketFor(input.nodeId, input.identity);
  let closed = false;
  const accepting = (async () => {
    while (!closed) {
      const incoming = await input.identity.endpoint.acceptNext();
      if (incoming === null) return;
      void incoming.accept()
        .then((accepting) => accepting.connect())
        .then((connection) => serveConnection(connection, input.identity.pairingToken, input.handlers))
        .catch(() => undefined);
    }
  })();
  return {
    ticket,
    async close(): Promise<void> {
      if (closed) return;
      closed = true;
      await input.identity.endpoint.close();
      await accepting;
    },
  };
}

export function endpointAddrFromTicket(ticket: AltitudeNodeTicket): EndpointAddr {
  const parsed = parseAltitudeNodeTicket(ticket);
  const native = EndpointTicket.fromString(parsed.endpointTicket);
  const id = EndpointId.fromString(parsed.endpointId);
  const addr = native.endpointAddr();
  if (!addr.id().equals(id)) throw new Error("Iroh endpoint ticket identity changed");
  return addr;
}
