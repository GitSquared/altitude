import { randomUUID } from "node:crypto";

import {
  decodeTaskBootstrapPayload,
  encodeTaskBootstrapPayload,
} from "./bootstrap.ts";
import {
  isSha256Digest,
  sha256,
  stableJson,
  utf8,
  type Sha256Digest,
} from "./canonical.ts";
import {
  CapsuleCheckpointNotFoundError,
  type CapsuleCheckpointReference,
  type FilesystemCapsuleCheckpointStore,
} from "./filesystem.ts";
import {
  capsuleManifestDigest,
  createCapsuleCheckpoint,
  type CapsuleCheckpoint,
  type CapsuleMemberContent,
} from "./manifest.ts";
import { portableCapsulePath } from "./paths.ts";
import type { FilesystemRepositorySeedCache } from "./seed-cache.ts";

export const CAPSULE_TRANSFER_PACKAGE_PROTOCOL =
  "altitude.capsule-transfer-package/v1" as const;
export const CAPSULE_TRANSFER_RECEIPT_PROTOCOL =
  "altitude.capsule-transfer/v1" as const;
export const CAPSULE_TRANSFER_MEMBER = portableCapsulePath(
  "state/transfer.json",
);

const MAX_TRANSFER_HEADER_BYTES = 1024 * 1024;
export const MAX_CAPSULE_TRANSFER_PACKAGE_BYTES = 1024 * 1024 * 1024;

export interface CapsuleTransferReceipt {
  readonly schema: typeof CAPSULE_TRANSFER_RECEIPT_PROTOCOL;
  readonly transferId: string;
  readonly preparedAt: string;
  readonly source: {
    readonly nodeId: string;
    readonly leaseId: string;
    readonly epoch: number;
    readonly checkpointDigest: Sha256Digest;
    readonly checkpointRevision: number;
  };
  readonly target: {
    readonly nodeId: string;
    readonly leaseId: string;
    readonly epoch: number;
  };
}

export interface DecodedCapsuleTransferPackage {
  readonly transfer: CapsuleTransferReceipt;
  readonly checkpoint: CapsuleCheckpoint;
  readonly checkpointDigest: Sha256Digest;
  readonly repositorySeedBundle: Uint8Array;
}

export interface TargetPreparedReceipt {
  readonly schema: "altitude.target-prepared/v1";
  readonly transferId: string;
  readonly targetNodeId: string;
  readonly checkpoint: CapsuleCheckpointReference;
}

export interface TargetActivationReceipt {
  readonly schema: "altitude.target-activation/v1";
  readonly transferId: string;
  readonly targetNodeId: string;
  readonly checkpoint: CapsuleCheckpointReference;
}

export interface PreparedCapsuleTransfer {
  readonly transferId: string;
  readonly sourceNodeId: string;
  readonly targetNodeId: string;
  readonly sourceCheckpoint: CapsuleCheckpointReference;
  readonly targetCheckpoint: CapsuleCheckpointReference;
  readonly targetLeaseId: string;
  readonly targetEpoch: number;
}

export interface CommittedCapsuleTransfer extends PreparedCapsuleTransfer {
  readonly activation: TargetActivationReceipt;
}

export interface CapsuleTransferTransport {
  prepare(payload: Uint8Array): Promise<TargetPreparedReceipt>;
  activate(input: {
    readonly transferId: string;
    readonly checkpoint: CapsuleCheckpointReference;
  }): Promise<TargetActivationReceipt>;
}

export class AmbiguousCapsuleActivationError extends Error {
  override readonly name = "AmbiguousCapsuleActivationError";

  constructor(
    readonly transfer: PreparedCapsuleTransfer,
    options: ErrorOptions = {},
  ) {
    super(
      "Capsule ownership committed to " + transfer.targetNodeId
        + ", but target activation was not acknowledged",
      options,
    );
  }
}

function requireIdentifier(value: unknown, label: string): asserts value is string {
  if (
    typeof value !== "string"
    || !/^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/u.test(value)
  ) {
    throw new Error(label + " is invalid");
  }
}

function requireEpoch(value: unknown, label: string): asserts value is number {
  if (!Number.isSafeInteger(value) || (value as number) < 0) {
    throw new Error(label + " is invalid");
  }
}

function requireRevision(value: unknown, label: string): asserts value is number {
  if (!Number.isSafeInteger(value) || (value as number) < 1) {
    throw new Error(label + " is invalid");
  }
}

function record(value: unknown, label: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error(label + " is invalid");
  }
  return value as Record<string, unknown>;
}

function parseInstant(value: unknown, label: string): string {
  if (
    typeof value !== "string"
    || !Number.isFinite(Date.parse(value))
    || new Date(Date.parse(value)).toISOString() !== value
  ) {
    throw new Error(label + " is invalid");
  }
  return value;
}

function reference(checkpoint: CapsuleCheckpoint): CapsuleCheckpointReference {
  return {
    capsuleId: checkpoint.manifest.capsuleId,
    revision: checkpoint.manifest.revision,
    digest: capsuleManifestDigest(checkpoint.manifest),
  };
}

function sameReference(
  left: CapsuleCheckpointReference,
  right: CapsuleCheckpointReference,
): boolean {
  return left.capsuleId === right.capsuleId
    && left.revision === right.revision
    && left.digest === right.digest;
}

function parseTransferReceipt(value: unknown): CapsuleTransferReceipt {
  const receipt = record(value, "Capsule transfer receipt");
  if (receipt.schema !== CAPSULE_TRANSFER_RECEIPT_PROTOCOL) {
    throw new Error("Capsule transfer receipt schema is unsupported");
  }
  requireIdentifier(receipt.transferId, "Transfer id");
  const source = record(receipt.source, "Capsule transfer source");
  const target = record(receipt.target, "Capsule transfer target");
  requireIdentifier(source.nodeId, "Source node id");
  requireIdentifier(source.leaseId, "Source lease id");
  requireEpoch(source.epoch, "Source epoch");
  requireRevision(source.checkpointRevision, "Source checkpoint revision");
  if (!isSha256Digest(source.checkpointDigest)) {
    throw new Error("Source checkpoint digest is invalid");
  }
  requireIdentifier(target.nodeId, "Target node id");
  requireIdentifier(target.leaseId, "Target lease id");
  requireEpoch(target.epoch, "Target epoch");
  if (target.nodeId === source.nodeId || target.epoch !== source.epoch + 1) {
    throw new Error("Capsule transfer target does not advance ownership");
  }
  return {
    schema: CAPSULE_TRANSFER_RECEIPT_PROTOCOL,
    transferId: receipt.transferId,
    preparedAt: parseInstant(
      receipt.preparedAt,
      "Transfer preparation time",
    ),
    source: {
      nodeId: source.nodeId,
      leaseId: source.leaseId,
      epoch: source.epoch,
      checkpointDigest: source.checkpointDigest,
      checkpointRevision: source.checkpointRevision,
    },
    target: {
      nodeId: target.nodeId,
      leaseId: target.leaseId,
      epoch: target.epoch,
    },
  };
}

function transferReceipt(checkpoint: CapsuleCheckpoint): CapsuleTransferReceipt {
  const member = checkpoint.members.find(
    (candidate) => candidate.path === CAPSULE_TRANSFER_MEMBER,
  );
  if (member === undefined) {
    throw new Error("Transferred checkpoint has no ownership receipt");
  }
  let value: unknown;
  try {
    value = JSON.parse(
      new TextDecoder("utf-8", { fatal: true }).decode(member.bytes),
    ) as unknown;
  } catch (error) {
    throw new Error("Capsule transfer receipt is not valid UTF-8 JSON", {
      cause: error,
    });
  }
  const receipt = parseTransferReceipt(value);
  const manifest = checkpoint.manifest;
  if (
    manifest.revision !== receipt.source.checkpointRevision + 1
    || manifest.checkpointedAt !== receipt.preparedAt
    || manifest.ownership.ownerNodeId !== receipt.target.nodeId
    || manifest.ownership.leaseId !== receipt.target.leaseId
    || manifest.ownership.epoch !== receipt.target.epoch
  ) {
    throw new Error(
      "Transferred checkpoint ownership does not match its receipt",
    );
  }
  return receipt;
}

function concat(chunks: readonly Uint8Array[]): Uint8Array {
  const size = chunks.reduce((total, chunk) => total + chunk.byteLength, 0);
  if (!Number.isSafeInteger(size) || size > MAX_CAPSULE_TRANSFER_PACKAGE_BYTES) {
    throw new Error("Capsule transfer package is too large");
  }
  const bytes = new Uint8Array(size);
  let offset = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, offset);
    offset += chunk.byteLength;
  }
  return bytes;
}

export function createOwnershipTransferCheckpoint(input: {
  readonly source: CapsuleCheckpoint;
  readonly transferId: string;
  readonly sourceNodeId: string;
  readonly sourceLeaseId: string;
  readonly targetNodeId: string;
  readonly targetLeaseId: string;
  readonly preparedAt: string;
  readonly additionalMembers?: readonly CapsuleMemberContent[];
}): CapsuleCheckpoint {
  requireIdentifier(input.transferId, "Transfer id");
  requireIdentifier(input.sourceNodeId, "Source node id");
  requireIdentifier(input.sourceLeaseId, "Source lease id");
  requireIdentifier(input.targetNodeId, "Target node id");
  requireIdentifier(input.targetLeaseId, "Target lease id");
  if (input.sourceNodeId === input.targetNodeId) {
    throw new Error("Source and target nodes must differ");
  }
  const sourceDigest = capsuleManifestDigest(input.source.manifest);
  const sourceOwnership = input.source.manifest.ownership;
  if (
    sourceOwnership.ownerNodeId !== input.sourceNodeId
    || sourceOwnership.leaseId !== input.sourceLeaseId
  ) {
    throw new Error(
      "Only the active source lease can prepare a capsule transfer",
    );
  }
  const preparedAt = parseInstant(
    input.preparedAt,
    "Transfer preparation time",
  );
  const receipt: CapsuleTransferReceipt = {
    schema: CAPSULE_TRANSFER_RECEIPT_PROTOCOL,
    transferId: input.transferId,
    preparedAt,
    source: {
      nodeId: input.sourceNodeId,
      leaseId: input.sourceLeaseId,
      epoch: sourceOwnership.epoch,
      checkpointDigest: sourceDigest,
      checkpointRevision: input.source.manifest.revision,
    },
    target: {
      nodeId: input.targetNodeId,
      leaseId: input.targetLeaseId,
      epoch: sourceOwnership.epoch + 1,
    },
  };
  const replacements = new Map<string, CapsuleMemberContent>();
  for (const member of input.additionalMembers ?? []) {
    if (
      member.path === CAPSULE_TRANSFER_MEMBER
      || replacements.has(member.path)
    ) {
      throw new Error("Duplicate transfer checkpoint member " + member.path);
    }
    if (member.path.startsWith("git/")) {
      throw new Error("Transfer metadata cannot replace the Git namespace");
    }
    replacements.set(member.path, {
      path: member.path,
      bytes: member.bytes.slice(),
    });
  }
  const members: CapsuleMemberContent[] = input.source.members
    .filter(
      (member) =>
        member.path !== CAPSULE_TRANSFER_MEMBER
        && !replacements.has(member.path),
    )
    .map((member) => ({ path: member.path, bytes: member.bytes.slice() }));
  members.push(...replacements.values(), {
    path: CAPSULE_TRANSFER_MEMBER,
    bytes: utf8(stableJson(receipt) + "\n"),
  });
  return createCapsuleCheckpoint({
    capsuleId: input.source.manifest.capsuleId,
    taskId: input.source.manifest.taskId,
    revision: input.source.manifest.revision + 1,
    checkpointedAt: preparedAt,
    ownership: {
      epoch: receipt.target.epoch,
      leaseId: receipt.target.leaseId,
      ownerNodeId: receipt.target.nodeId,
      status: "active",
    },
    repository: input.source.manifest.repository,
    members,
  });
}

export function encodeCapsuleTransferPackage(input: {
  readonly checkpoint: CapsuleCheckpoint;
  readonly repositorySeedBundle: Uint8Array;
}): Uint8Array {
  const transfer = transferReceipt(input.checkpoint);
  const payload = encodeTaskBootstrapPayload(input);
  const checkpointDigest = capsuleManifestDigest(input.checkpoint.manifest);
  const header = {
    schema: CAPSULE_TRANSFER_PACKAGE_PROTOCOL,
    transferId: transfer.transferId,
    targetNodeId: transfer.target.nodeId,
    checkpointDigest,
    payload: { size: payload.byteLength, sha256: sha256(payload) },
  };
  const headerBytes = utf8(stableJson(header));
  if (headerBytes.byteLength > MAX_TRANSFER_HEADER_BYTES) {
    throw new Error("Capsule transfer package header is too large");
  }
  const prefix = new Uint8Array(4);
  new DataView(prefix.buffer).setUint32(0, headerBytes.byteLength, false);
  return concat([prefix, headerBytes, payload]);
}

export function decodeCapsuleTransferPackage(
  bytes: Uint8Array,
): DecodedCapsuleTransferPackage {
  if (bytes.byteLength < 4 || bytes.byteLength > MAX_CAPSULE_TRANSFER_PACKAGE_BYTES) {
    throw new Error("Capsule transfer package size is invalid");
  }
  const headerSize = new DataView(
    bytes.buffer,
    bytes.byteOffset,
    4,
  ).getUint32(0, false);
  if (
    headerSize < 2
    || headerSize > MAX_TRANSFER_HEADER_BYTES
    || 4 + headerSize > bytes.byteLength
  ) {
    throw new Error("Capsule transfer package header size is invalid");
  }
  let value: unknown;
  try {
    value = JSON.parse(
      new TextDecoder("utf-8", { fatal: true }).decode(
        bytes.subarray(4, 4 + headerSize),
      ),
    ) as unknown;
  } catch (error) {
    throw new Error(
      "Capsule transfer package header is not valid UTF-8 JSON",
      { cause: error },
    );
  }
  const header = record(value, "Capsule transfer package header");
  if (header.schema !== CAPSULE_TRANSFER_PACKAGE_PROTOCOL) {
    throw new Error("Capsule transfer package schema is unsupported");
  }
  requireIdentifier(header.transferId, "Transfer id");
  requireIdentifier(header.targetNodeId, "Target node id");
  if (!isSha256Digest(header.checkpointDigest)) {
    throw new Error("Transfer checkpoint digest is invalid");
  }
  const descriptor = record(header.payload, "Transfer package payload");
  if (
    !Number.isSafeInteger(descriptor.size)
    || (descriptor.size as number) < 1
    || !isSha256Digest(descriptor.sha256)
  ) {
    throw new Error("Transfer package payload descriptor is invalid");
  }
  const payload = bytes.slice(4 + headerSize);
  if (
    payload.byteLength !== descriptor.size
    || sha256(payload) !== descriptor.sha256
  ) {
    throw new Error("Transfer package payload failed verification");
  }
  const decoded = decodeTaskBootstrapPayload(payload);
  const transfer = transferReceipt(decoded.checkpoint);
  if (
    transfer.transferId !== header.transferId
    || transfer.target.nodeId !== header.targetNodeId
    || decoded.checkpointDigest !== header.checkpointDigest
  ) {
    throw new Error("Transfer package header does not match its checkpoint");
  }
  return { transfer, ...decoded };
}

async function activeOrUndefined(
  checkpoints: Pick<FilesystemCapsuleCheckpointStore, "readActive">,
  capsuleId: string,
): Promise<CapsuleCheckpoint | undefined> {
  try {
    return await checkpoints.readActive(capsuleId);
  } catch (error) {
    if (error instanceof CapsuleCheckpointNotFoundError) return undefined;
    throw error;
  }
}

export class FilesystemCapsuleTransferReceiver {
  constructor(
    private readonly checkpoints: Pick<
      FilesystemCapsuleCheckpointStore,
      "publish" | "read" | "readActive" | "activate"
    >,
    private readonly seeds: Pick<FilesystemRepositorySeedCache, "put" | "read">,
    readonly nodeId: string,
  ) {
    requireIdentifier(nodeId, "Target node id");
  }

  async prepare(payload: Uint8Array): Promise<TargetPreparedReceipt> {
    const decoded = decodeCapsuleTransferPackage(payload);
    if (decoded.transfer.target.nodeId !== this.nodeId) {
      throw new Error(
        "Transfer targets " + decoded.transfer.target.nodeId
          + ", not " + this.nodeId,
      );
    }
    const prepared = reference(decoded.checkpoint);
    const current = await activeOrUndefined(
      this.checkpoints,
      prepared.capsuleId,
    );
    if (current !== undefined) {
      const currentReference = reference(current);
      if (
        !sameReference(currentReference, prepared)
        && (
          currentReference.digest
            !== decoded.transfer.source.checkpointDigest
          || current.manifest.revision
            !== decoded.transfer.source.checkpointRevision
          || current.manifest.ownership.ownerNodeId
            !== decoded.transfer.source.nodeId
          || current.manifest.ownership.leaseId
            !== decoded.transfer.source.leaseId
          || current.manifest.ownership.epoch
            !== decoded.transfer.source.epoch
        )
      ) {
        throw new Error(
          "Target capsule state does not match the transfer source",
        );
      }
    }
    const published = await this.checkpoints.publish(decoded.checkpoint);
    await this.seeds.put(published.digest, decoded.repositorySeedBundle);
    await this.checkpoints.read(published);
    await this.seeds.read(published.digest);
    return {
      schema: "altitude.target-prepared/v1",
      transferId: decoded.transfer.transferId,
      targetNodeId: this.nodeId,
      checkpoint: published,
    };
  }

  async activate(input: {
    readonly transferId: string;
    readonly checkpoint: CapsuleCheckpointReference;
  }): Promise<TargetActivationReceipt> {
    requireIdentifier(input.transferId, "Transfer id");
    const checkpoint = await this.checkpoints.read(input.checkpoint);
    const transfer = transferReceipt(checkpoint);
    if (
      transfer.transferId !== input.transferId
      || transfer.target.nodeId !== this.nodeId
    ) {
      throw new Error(
        "Target activation command does not match the staged transfer",
      );
    }
    const current = await activeOrUndefined(
      this.checkpoints,
      input.checkpoint.capsuleId,
    );
    if (
      current !== undefined
      && !sameReference(reference(current), input.checkpoint)
    ) {
      const currentReference = reference(current);
      if (
        currentReference.digest !== transfer.source.checkpointDigest
        || current.manifest.revision !== transfer.source.checkpointRevision
        || current.manifest.ownership.ownerNodeId !== transfer.source.nodeId
        || current.manifest.ownership.leaseId !== transfer.source.leaseId
        || current.manifest.ownership.epoch !== transfer.source.epoch
      ) {
        throw new Error(
          "Target capsule ownership changed after transfer preparation",
        );
      }
    }
    await this.seeds.read(input.checkpoint.digest);
    await this.checkpoints.activate(input.checkpoint);
    return {
      schema: "altitude.target-activation/v1",
      transferId: input.transferId,
      targetNodeId: this.nodeId,
      checkpoint: input.checkpoint,
    };
  }
}

export class InProcessCapsuleTransferTransport
implements CapsuleTransferTransport {
  constructor(private readonly receiver: FilesystemCapsuleTransferReceiver) {}

  prepare(payload: Uint8Array): Promise<TargetPreparedReceipt> {
    return this.receiver.prepare(payload);
  }

  activate(input: {
    readonly transferId: string;
    readonly checkpoint: CapsuleCheckpointReference;
  }): Promise<TargetActivationReceipt> {
    return this.receiver.activate(input);
  }
}

export class LocalToRemoteCapsuleTransfer {
  readonly #now: () => number;
  readonly #createId: () => string;

  constructor(
    private readonly checkpoints: Pick<
      FilesystemCapsuleCheckpointStore,
      "publish" | "read" | "readActive" | "activate"
    >,
    private readonly seeds: Pick<FilesystemRepositorySeedCache, "put" | "read">,
    readonly sourceNodeId: string,
    private readonly transport: CapsuleTransferTransport,
    options: {
      readonly now?: () => number;
      readonly createId?: () => string;
    } = {},
  ) {
    requireIdentifier(sourceNodeId, "Source node id");
    this.#now = options.now ?? Date.now;
    this.#createId = options.createId ?? randomUUID;
  }

  async prepare(input: {
    readonly taskId: string;
    readonly targetNodeId: string;
    readonly targetLeaseId?: string;
    readonly transferId?: string;
    readonly additionalMembers?: readonly CapsuleMemberContent[];
  }): Promise<PreparedCapsuleTransfer> {
    requireIdentifier(input.taskId, "Task id");
    requireIdentifier(input.targetNodeId, "Target node id");
    const source = await this.checkpoints.readActive(input.taskId);
    if (source.manifest.taskId !== input.taskId) {
      throw new Error("Active capsule is bound to another task");
    }
    if (source.manifest.ownership.ownerNodeId !== this.sourceNodeId) {
      throw new Error(
        "Capsule is owned by " + source.manifest.ownership.ownerNodeId
          + ", not " + this.sourceNodeId,
      );
    }
    const sourceCheckpoint = reference(source);
    const repositorySeedBundle = await this.seeds.read(
      sourceCheckpoint.digest,
    );
    const transferId = input.transferId ?? this.#createId();
    const targetLeaseId = input.targetLeaseId ?? this.#createId();
    const target = createOwnershipTransferCheckpoint({
      source,
      transferId,
      sourceNodeId: this.sourceNodeId,
      sourceLeaseId: source.manifest.ownership.leaseId,
      targetNodeId: input.targetNodeId,
      targetLeaseId,
      preparedAt: new Date(this.#now()).toISOString(),
      ...(input.additionalMembers === undefined
        ? {}
        : { additionalMembers: input.additionalMembers }),
    });
    const targetCheckpoint = await this.checkpoints.publish(target);
    await this.seeds.put(targetCheckpoint.digest, repositorySeedBundle);
    const receipt = await this.transport.prepare(
      encodeCapsuleTransferPackage({
        checkpoint: target,
        repositorySeedBundle,
      }),
    );
    if (
      receipt.schema !== "altitude.target-prepared/v1"
      || receipt.transferId !== transferId
      || receipt.targetNodeId !== input.targetNodeId
      || !sameReference(receipt.checkpoint, targetCheckpoint)
    ) {
      throw new Error(
        "Target prepared receipt does not match the transfer",
      );
    }
    return {
      transferId,
      sourceNodeId: this.sourceNodeId,
      targetNodeId: input.targetNodeId,
      sourceCheckpoint,
      targetCheckpoint,
      targetLeaseId,
      targetEpoch: target.manifest.ownership.epoch,
    };
  }

  async commit(
    prepared: PreparedCapsuleTransfer,
  ): Promise<CommittedCapsuleTransfer> {
    if (
      prepared.sourceNodeId !== this.sourceNodeId
      || prepared.sourceCheckpoint.capsuleId
        !== prepared.targetCheckpoint.capsuleId
      || prepared.targetCheckpoint.revision
        !== prepared.sourceCheckpoint.revision + 1
    ) {
      throw new Error(
        "Prepared capsule transfer is not bound to this source",
      );
    }
    const current = reference(
      await this.checkpoints.readActive(
        prepared.sourceCheckpoint.capsuleId,
      ),
    );
    if (sameReference(current, prepared.sourceCheckpoint)) {
      await this.checkpoints.activate(prepared.targetCheckpoint);
    } else if (!sameReference(current, prepared.targetCheckpoint)) {
      throw new Error("Source capsule changed after transfer preparation");
    }
    try {
      const activation = await this.transport.activate({
        transferId: prepared.transferId,
        checkpoint: prepared.targetCheckpoint,
      });
      if (
        activation.schema !== "altitude.target-activation/v1"
        || activation.transferId !== prepared.transferId
        || activation.targetNodeId !== prepared.targetNodeId
        || !sameReference(
          activation.checkpoint,
          prepared.targetCheckpoint,
        )
      ) {
        throw new Error(
          "Target activation ACK does not match the transfer",
        );
      }
      return { ...prepared, activation };
    } catch (error) {
      throw new AmbiguousCapsuleActivationError(prepared, { cause: error });
    }
  }
}
