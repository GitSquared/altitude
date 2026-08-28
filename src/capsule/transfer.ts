import { sha256, stableJson, utf8, type Sha256Digest } from "./canonical.ts";

export type OwnershipStatus = "active" | "pending-activation" | "uncertain";

export interface OwnershipLease {
  readonly epoch: number;
  readonly leaseId: string;
  readonly ownerNodeId: string;
  readonly status: OwnershipStatus;
}

export type TransferPhase =
  | "owned"
  | "prepared"
  | "target-prepared"
  | "commit-issued"
  | "fail-stopped";

export interface TransferContext {
  readonly transferId: string;
  readonly sourceNodeId: string;
  readonly targetNodeId: string;
  readonly sourceEpoch: number;
  readonly targetEpoch: number;
  readonly targetLeaseId: string;
  readonly checkpointDigest: Sha256Digest;
}

export interface AppliedTransferCommand {
  readonly commandId: string;
  readonly fingerprint: Sha256Digest;
}

export interface CapsuleTransferState {
  readonly capsuleId: string;
  readonly phase: TransferPhase;
  readonly ownership: OwnershipLease;
  readonly transfer: TransferContext | null;
  readonly appliedCommands: readonly AppliedTransferCommand[];
  readonly failureReason: "ambiguous-activation-ack" | null;
}

interface CommandBase {
  readonly commandId: string;
  readonly expectedEpoch: number;
}

export interface BeginTransferCommand extends CommandBase {
  readonly type: "begin-transfer";
  readonly transferId: string;
  readonly sourceNodeId: string;
  readonly sourceLeaseId: string;
  readonly targetNodeId: string;
  readonly targetLeaseId: string;
  readonly checkpointDigest: Sha256Digest;
}

export interface RecordTargetPreparedCommand extends CommandBase {
  readonly type: "record-target-prepared";
  readonly transferId: string;
  readonly checkpointDigest: Sha256Digest;
}

export interface CommitTransferCommand extends CommandBase {
  readonly type: "commit-transfer";
  readonly transferId: string;
}

export interface RecordActivationAckCommand extends CommandBase {
  readonly type: "record-activation-ack";
  readonly transferId: string;
  readonly targetNodeId: string;
  readonly targetLeaseId: string;
}

export interface AbortTransferCommand extends CommandBase {
  readonly type: "abort-transfer";
  readonly transferId: string;
}

export interface MarkAckAmbiguousCommand extends CommandBase {
  readonly type: "mark-ack-ambiguous";
  readonly transferId: string;
}

export type TransferCommand =
  | BeginTransferCommand
  | RecordTargetPreparedCommand
  | CommitTransferCommand
  | RecordActivationAckCommand
  | AbortTransferCommand
  | MarkAckAmbiguousCommand;

export class StaleOwnershipEpochError extends Error {
  override readonly name = "StaleOwnershipEpochError";

  constructor(expected: number, actual: number) {
    super(`Ownership epoch ${expected} is stale; current epoch is ${actual}`);
  }
}

export class TransferProtocolError extends Error {
  override readonly name = "TransferProtocolError";
}

export class DuplicateTransferCommandError extends Error {
  override readonly name = "DuplicateTransferCommandError";
}

export function createOwnedTransferState(input: {
  readonly capsuleId: string;
  readonly ownerNodeId: string;
  readonly leaseId: string;
  readonly epoch?: number;
}): CapsuleTransferState {
  assertIdentifier(input.capsuleId, "capsuleId");
  assertIdentifier(input.ownerNodeId, "ownerNodeId");
  assertIdentifier(input.leaseId, "leaseId");
  const epoch = input.epoch ?? 0;
  assertEpoch(epoch);
  return {
    capsuleId: input.capsuleId,
    phase: "owned",
    ownership: {
      epoch,
      ownerNodeId: input.ownerNodeId,
      leaseId: input.leaseId,
      status: "active",
    },
    transfer: null,
    appliedCommands: [],
    failureReason: null,
  };
}

export function applyTransferCommand(
  state: CapsuleTransferState,
  command: TransferCommand,
): CapsuleTransferState {
  validateCommand(command);
  const fingerprint = transferCommandFingerprint(command);
  const duplicate = state.appliedCommands.find(
    (entry) => entry.commandId === command.commandId,
  );
  if (duplicate !== undefined) {
    if (duplicate.fingerprint !== fingerprint) {
      throw new DuplicateTransferCommandError(
        `Transfer command ${command.commandId} was reused with different input`,
      );
    }
    return state;
  }

  if (command.expectedEpoch !== state.ownership.epoch) {
    throw new StaleOwnershipEpochError(
      command.expectedEpoch,
      state.ownership.epoch,
    );
  }

  const transitioned = transition(state, command);
  return {
    ...transitioned,
    appliedCommands: [
      ...state.appliedCommands,
      { commandId: command.commandId, fingerprint },
    ],
  };
}

function transition(
  state: CapsuleTransferState,
  command: TransferCommand,
): CapsuleTransferState {
  switch (command.type) {
    case "begin-transfer": {
      requirePhase(state, "owned");
      if (
        command.sourceNodeId !== state.ownership.ownerNodeId ||
        command.sourceLeaseId !== state.ownership.leaseId
      ) {
        throw new TransferProtocolError("Only the active source lease can begin a transfer");
      }
      if (command.targetNodeId === command.sourceNodeId) {
        throw new TransferProtocolError("Source and target nodes must differ");
      }
      return {
        ...state,
        phase: "prepared",
        transfer: {
          transferId: command.transferId,
          sourceNodeId: command.sourceNodeId,
          targetNodeId: command.targetNodeId,
          sourceEpoch: state.ownership.epoch,
          targetEpoch: state.ownership.epoch + 1,
          targetLeaseId: command.targetLeaseId,
          checkpointDigest: command.checkpointDigest,
        },
      };
    }
    case "record-target-prepared": {
      requirePhase(state, "prepared");
      const transfer = requireTransfer(state, command.transferId);
      if (command.checkpointDigest !== transfer.checkpointDigest) {
        throw new TransferProtocolError("Target prepared a different checkpoint digest");
      }
      return { ...state, phase: "target-prepared" };
    }
    case "commit-transfer": {
      requirePhase(state, "target-prepared");
      const transfer = requireTransfer(state, command.transferId);
      return {
        ...state,
        phase: "commit-issued",
        ownership: {
          epoch: transfer.targetEpoch,
          ownerNodeId: transfer.targetNodeId,
          leaseId: transfer.targetLeaseId,
          status: "pending-activation",
        },
      };
    }
    case "record-activation-ack": {
      requirePhase(state, "commit-issued");
      const transfer = requireTransfer(state, command.transferId);
      if (
        command.targetNodeId !== transfer.targetNodeId ||
        command.targetLeaseId !== transfer.targetLeaseId
      ) {
        throw new TransferProtocolError("Activation ACK does not match the target lease");
      }
      return {
        ...state,
        phase: "owned",
        ownership: { ...state.ownership, status: "active" },
        transfer: null,
      };
    }
    case "abort-transfer": {
      if (state.phase !== "prepared" && state.phase !== "target-prepared") {
        throw new TransferProtocolError(
          "A transfer can only abort before ownership commit",
        );
      }
      requireTransfer(state, command.transferId);
      return { ...state, phase: "owned", transfer: null };
    }
    case "mark-ack-ambiguous": {
      requirePhase(state, "commit-issued");
      requireTransfer(state, command.transferId);
      return {
        ...state,
        phase: "fail-stopped",
        ownership: { ...state.ownership, status: "uncertain" },
        failureReason: "ambiguous-activation-ack",
      };
    }
  }
}

export function ownsRunnableLease(
  state: CapsuleTransferState,
  claim: { readonly nodeId: string; readonly leaseId: string; readonly epoch: number },
): boolean {
  return (
    state.phase === "owned" &&
    state.ownership.status === "active" &&
    state.ownership.ownerNodeId === claim.nodeId &&
    state.ownership.leaseId === claim.leaseId &&
    state.ownership.epoch === claim.epoch
  );
}

export function transferCommandFingerprint(command: TransferCommand): Sha256Digest {
  return sha256(utf8(stableJson(command)));
}

function requirePhase(state: CapsuleTransferState, phase: TransferPhase): void {
  if (state.phase !== phase) {
    throw new TransferProtocolError(
      `Transfer command requires phase ${phase}; current phase is ${state.phase}`,
    );
  }
}

function requireTransfer(
  state: CapsuleTransferState,
  transferId: string,
): TransferContext {
  if (state.transfer === null || state.transfer.transferId !== transferId) {
    throw new TransferProtocolError("Transfer id does not match the active transfer");
  }
  return state.transfer;
}

function validateCommand(command: TransferCommand): void {
  assertIdentifier(command.commandId, "commandId");
  assertEpoch(command.expectedEpoch);
  if ("transferId" in command) assertIdentifier(command.transferId, "transferId");
  if (command.type === "begin-transfer") {
    assertIdentifier(command.sourceNodeId, "sourceNodeId");
    assertIdentifier(command.sourceLeaseId, "sourceLeaseId");
    assertIdentifier(command.targetNodeId, "targetNodeId");
    assertIdentifier(command.targetLeaseId, "targetLeaseId");
    assertDigest(command.checkpointDigest);
  }
  if (command.type === "record-target-prepared") {
    assertDigest(command.checkpointDigest);
  }
  if (command.type === "record-activation-ack") {
    assertIdentifier(command.targetNodeId, "targetNodeId");
    assertIdentifier(command.targetLeaseId, "targetLeaseId");
  }
}

function assertIdentifier(value: string, label: string): void {
  if (!/^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$/u.test(value)) {
    throw new TransferProtocolError(`${label} is invalid`);
  }
}

function assertEpoch(value: number): void {
  if (!Number.isSafeInteger(value) || value < 0) {
    throw new TransferProtocolError("expectedEpoch must be a non-negative integer");
  }
}

function assertDigest(value: string): void {
  if (!/^sha256:[a-f0-9]{64}$/u.test(value)) {
    throw new TransferProtocolError("checkpointDigest is invalid");
  }
}
